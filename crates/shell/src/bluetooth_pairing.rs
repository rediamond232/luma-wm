//! A private BlueZ agent for a single user-initiated pairing operation.
use gtk::{gio, glib, prelude::*};
use std::{
    cell::{Cell, RefCell},
    rc::Rc,
};
const AGENT: &str = "/org/luma/Pairing";
const XML: &str = r#"<node><interface name="org.bluez.Agent1">
<method name="Release"/><method name="Cancel"/>
<method name="RequestPinCode"><arg type="o" direction="in"/><arg type="s" direction="out"/></method>
<method name="RequestPasskey"><arg type="o" direction="in"/><arg type="u" direction="out"/></method>
<method name="DisplayPinCode"><arg type="o" direction="in"/><arg type="s" direction="in"/></method>
<method name="DisplayPasskey"><arg type="o" direction="in"/><arg type="u" direction="in"/><arg type="q" direction="in"/></method>
<method name="RequestConfirmation"><arg type="o" direction="in"/><arg type="u" direction="in"/></method>
<method name="RequestAuthorization"><arg type="o" direction="in"/></method>
<method name="AuthorizeService"><arg type="o" direction="in"/><arg type="s" direction="in"/></method>
</interface></node>"#;
#[derive(Clone, Copy)]
enum Answer {
    Pin,
    Passkey,
    Confirm,
}
fn answer(kind: Answer, text: &str) -> Option<glib::Variant> {
    match kind {
        Answer::Pin
            if (1..=16).contains(&text.chars().count()) && !text.chars().any(char::is_control) =>
        {
            Some((text,).to_variant())
        }
        Answer::Passkey
            if !text.is_empty() && text.len() <= 6 && text.bytes().all(|b| b.is_ascii_digit()) =>
        {
            Some((text.parse::<u32>().ok()?,).to_variant())
        }
        Answer::Confirm => Some(().to_variant()),
        _ => None,
    }
}
pub(super) struct Pairing {
    pub active: Cell<bool>,
    connection: RefCell<Option<gio::DBusConnection>>,
    registration: RefCell<Option<gio::RegistrationId>>,
    pending: RefCell<Option<(gio::DBusMethodInvocation, Answer)>>,
    cancel: gio::Cancellable,
    owner: String,
    device: String,
    title: String,
    panel: gtk::Box,
    message: gtk::Label,
    entry: gtk::Entry,
    accept: gtk::Button,
    error: gtk::Label,
}
impl Drop for Pairing {
    fn drop(&mut self) {
        self.cancel.cancel();
        if let Some((invocation, _)) = self.pending.get_mut().take() {
            invocation.return_dbus_error("org.bluez.Error.Canceled", "Pairing closed");
        }
        if let Some(connection) = self.connection.get_mut().take() {
            if let Some(registration) = self.registration.get_mut().take() {
                let _ = connection.unregister_object(registration);
            }
            Self::close(&connection, &self.owner, &self.device, self.active.get());
        }
    }
}
impl Pairing {
    fn close(connection: &gio::DBusConnection, owner: &str, device: &str, cancel: bool) {
        if cancel {
            let retained = connection.clone();
            connection.call(
                Some(owner),
                device,
                "org.bluez.Device1",
                "CancelPairing",
                None,
                None,
                gio::DBusCallFlags::NONE,
                3000,
                gio::Cancellable::NONE,
                move |_| {
                    retained.close(gio::Cancellable::NONE, |_| {});
                },
            );
        } else {
            connection.close(gio::Cancellable::NONE, |_| {});
        }
    }
    pub fn device_path(&self) -> &str {
        &self.device
    }
    pub fn device_removed(&self) {
        self.finish(Some("Pairing canceled: Bluetooth device disappeared"), true);
    }
    pub fn cancel(&self) {
        self.finish(Some("Pairing canceled"), true);
    }
    fn finish(&self, error: Option<&str>, cancel: bool) {
        if !self.active.replace(false) {
            return;
        }
        self.cancel.cancel();
        if let Some((invocation, _)) = self.pending.borrow_mut().take() {
            invocation.return_dbus_error("org.bluez.Error.Canceled", "Pairing ended");
        }
        self.entry.set_text("");
        self.panel.set_visible(false);
        self.error
            .set_label(error.unwrap_or("Paired. Select Connect to use the device."));
        if let Some(connection) = self.connection.borrow_mut().take() {
            if let Some(registration) = self.registration.borrow_mut().take() {
                let _ = connection.unregister_object(registration);
            }
            Self::close(&connection, &self.owner, &self.device, cancel);
        }
    }
    fn respond(&self) {
        let kind = self.pending.borrow().as_ref().map(|(_, kind)| *kind);
        let Some(kind) = kind else {
            return;
        };
        let Some(value) = answer(kind, self.entry.text().as_str()) else {
            self.message.set_label(match kind {
                Answer::Passkey => "Enter a passkey containing 1–6 digits.",
                _ => "Enter a PIN containing 1–16 characters.",
            });
            self.entry.grab_focus();
            return;
        };
        if let Some((invocation, _)) = self.pending.borrow_mut().take() {
            invocation.return_value(Some(&value));
        }
        self.entry.set_text("");
        self.entry.set_visible(false);
        self.accept.set_visible(false);
        self.message
            .set_label(&format!("Pairing with {}…", self.title));
    }
    fn request(
        &self,
        sender: Option<&str>,
        method: &str,
        args: &glib::Variant,
        invocation: gio::DBusMethodInvocation,
    ) {
        if sender != Some(self.owner.as_str()) || !self.active.get() {
            invocation.return_dbus_error(
                "org.bluez.Error.Rejected",
                "No active pairing for this sender",
            );
            return;
        }
        if method == "Cancel" {
            if let Some((pending, _)) = self.pending.borrow_mut().take() {
                pending.return_dbus_error("org.bluez.Error.Canceled", "Agent request canceled");
            }
            self.entry.set_text("");
            self.entry.set_visible(false);
            self.accept.set_visible(false);
            self.message.set_label("Waiting for pairing result…");
            invocation.return_value(None);
            return;
        }
        if method == "Release" {
            invocation.return_value(None);
            self.finish(Some("Pairing ended by Bluetooth service"), false);
            return;
        }
        if args
            .child_value(0)
            .get::<glib::variant::ObjectPath>()
            .as_ref()
            .map(|p| p.as_str())
            != Some(self.device.as_str())
            || self.pending.borrow().is_some()
        {
            invocation.return_dbus_error("org.bluez.Error.Rejected", "Unexpected pairing request");
            return;
        }
        let (message, kind) = match method {
            "RequestPinCode" => ("Enter the device PIN.".to_string(), Some(Answer::Pin)),
            "RequestPasskey" => (
                "Enter the device passkey.".to_string(),
                Some(Answer::Passkey),
            ),
            "RequestConfirmation" | "DisplayPasskey" => {
                let passkey = args.child_value(1).get::<u32>().unwrap_or(u32::MAX);
                if passkey > 999999 {
                    invocation.return_dbus_error("org.bluez.Error.Rejected", "Invalid passkey");
                    return;
                }
                if method == "RequestConfirmation" {
                    (
                        format!("Does the device show {passkey:06}?"),
                        Some(Answer::Confirm),
                    )
                } else {
                    let entered = args.child_value(2).get::<u16>().unwrap_or(0).min(6);
                    (
                        format!(
                            "Type {passkey:06} on the device, then Enter. ({entered}/6 digits entered)"
                        ),
                        None,
                    )
                }
            }
            "DisplayPinCode" => {
                let pin = args.child_value(1).get::<String>().unwrap_or_default();
                if answer(Answer::Pin, &pin).is_none() {
                    invocation.return_dbus_error("org.bluez.Error.Rejected", "Invalid PIN");
                    return;
                }
                (format!("Type {pin} on the device, then Enter."), None)
            }
            "RequestAuthorization" => ("Allow this device to pair?".into(), Some(Answer::Confirm)),
            "AuthorizeService" => {
                let service = args.child_value(1).get::<String>().unwrap_or_default();
                let service: String = service
                    .chars()
                    .take(64)
                    .filter(|c| !c.is_control())
                    .collect();
                (
                    format!("Allow service {service} for this device?"),
                    Some(Answer::Confirm),
                )
            }
            _ => {
                invocation.return_dbus_error("org.bluez.Error.Rejected", "Unsupported request");
                return;
            }
        };
        self.message
            .set_label(&format!("{}\n{message}", self.title));
        self.entry.set_text("");
        self.entry
            .set_visible(matches!(kind, Some(Answer::Pin | Answer::Passkey)));
        self.accept.set_visible(kind.is_some());
        if let Some(kind) = kind {
            self.pending.replace(Some((invocation, kind)));
            if gtk::prelude::WidgetExt::is_visible(&self.entry) {
                self.entry.grab_focus();
            } else {
                self.accept.grab_focus();
            }
        } else {
            invocation.return_value(None);
        }
    }
    pub fn start(
        bus: gio::BusType,
        device: &gio::DBusProxy,
        title: &str,
        panel: &gtk::Box,
        error: &gtk::Label,
    ) -> Rc<Self> {
        while let Some(child) = panel.first_child() {
            panel.remove(&child);
        }
        let message = gtk::Label::new(Some(&format!("Pairing with {title}…")));
        message.set_wrap(true);
        message.set_max_width_chars(36);
        let entry = gtk::Entry::new();
        entry.set_max_length(16);
        entry.set_visibility(false);
        entry.set_visible(false);
        entry.update_property(&[gtk::accessible::Property::Label("Bluetooth PIN or passkey")]);
        let accept = gtk::Button::with_label("Confirm");
        accept.set_visible(false);
        let cancel = gtk::Button::with_label("Cancel pairing");
        for widget in [
            message.upcast_ref::<gtk::Widget>(),
            entry.upcast_ref(),
            accept.upcast_ref(),
            cancel.upcast_ref(),
        ] {
            panel.append(widget);
        }
        panel.set_visible(true);
        error.set_label("");
        let state = Rc::new(Self {
            active: Cell::new(true),
            connection: RefCell::new(None),
            registration: RefCell::new(None),
            pending: RefCell::new(None),
            cancel: gio::Cancellable::new(),
            owner: device.name_owner().unwrap_or_default().to_string(),
            device: device.object_path().to_string(),
            title: title.into(),
            panel: panel.clone(),
            message,
            entry,
            accept,
            error: error.clone(),
        });
        let weak = Rc::downgrade(&state);
        cancel.connect_clicked(move |_| {
            if let Some(state) = weak.upgrade() {
                state.cancel();
            }
        });
        let weak = Rc::downgrade(&state);
        state.accept.connect_clicked(move |_| {
            if let Some(state) = weak.upgrade() {
                state.respond();
            }
        });
        let weak = Rc::downgrade(&state);
        state.entry.connect_activate(move |_| {
            if let Some(state) = weak.upgrade() {
                state.respond();
            }
        });
        let weak = Rc::downgrade(&state);
        glib::MainContext::default().spawn_local(async move {
            let result = async {
                let address = gio::dbus_address_get_for_bus_sync(bus, gio::Cancellable::NONE)?;
                gio::DBusConnection::for_address_future(
                    &address,
                    gio::DBusConnectionFlags::AUTHENTICATION_CLIENT
                        | gio::DBusConnectionFlags::MESSAGE_BUS_CONNECTION,
                    None,
                )
                .await
            }
            .await;
            let Some(state) = weak.upgrade() else {
                if let Ok(connection) = result {
                    connection.close(gio::Cancellable::NONE, |_| {});
                }
                return;
            };
            let connection = match result {
                Ok(connection) => connection,
                Err(error) => {
                    state.finish(Some(&error.to_string()), false);
                    return;
                }
            };
            connection.set_exit_on_close(false);
            if !state.active.get() {
                connection.close(gio::Cancellable::NONE, |_| {});
                return;
            }
            let info = gio::DBusNodeInfo::for_xml(XML).unwrap();
            let handler = Rc::downgrade(&state);
            let registration = connection
                .register_object(AGENT, &info.interfaces()[0])
                .method_call(move |_, sender, _, _, method, args, invocation| {
                    if let Some(state) = handler.upgrade() {
                        state.request(sender, method, &args, invocation);
                    } else {
                        invocation.return_dbus_error("org.bluez.Error.Canceled", "Pairing closed");
                    }
                })
                .build();
            let registration = match registration {
                Ok(registration) => registration,
                Err(error) => {
                    connection.close(gio::Cancellable::NONE, |_| {});
                    state.finish(Some(&error.to_string()), false);
                    return;
                }
            };
            state.registration.replace(Some(registration));
            state.connection.replace(Some(connection.clone()));
            let owner = state.owner.clone();
            drop(state);
            let result = connection
                .call_future(
                    Some(&owner),
                    "/org/bluez",
                    "org.bluez.AgentManager1",
                    "RegisterAgent",
                    Some(
                        &(
                            glib::variant::ObjectPath::try_from(AGENT).unwrap(),
                            "KeyboardDisplay",
                        )
                            .to_variant(),
                    ),
                    None,
                    gio::DBusCallFlags::NONE,
                    5000,
                )
                .await;
            let Some(state) = weak.upgrade().filter(|s| s.active.get()) else {
                return;
            };
            if let Err(error) = result {
                state.finish(Some(&error.to_string()), false);
                return;
            }
            let done = Rc::downgrade(&state);
            connection.call(
                Some(&state.owner),
                &state.device,
                "org.bluez.Device1",
                "Pair",
                None,
                None,
                gio::DBusCallFlags::NONE,
                120_000,
                Some(&state.cancel),
                move |result| {
                    if let Some(state) = done.upgrade() {
                        match result {
                            Ok(_) => state.finish(None, false),
                            Err(error) => {
                                state.finish(Some(&format!("Pairing failed: {error}")), true)
                            }
                        }
                    }
                },
            );
        });
        state
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn pin_and_passkey_validation() {
        assert_eq!(
            answer(Answer::Passkey, "001387").unwrap().get::<(u32,)>(),
            Some((1387,))
        );
        for value in ["", "1234567", "-1", "1.2", "１２", "1\n"] {
            assert!(answer(Answer::Passkey, value).is_none());
        }
        assert!(answer(Answer::Pin, "abcd1234").is_some());
        for value in ["", "12345678901234567", "bad\n"] {
            assert!(answer(Answer::Pin, value).is_none());
        }
    }
    #[test]
    #[ignore = "requires private D-Bus and nested GTK display"]
    fn live_pairing_prompts_and_cleanup() {
        use gtk4_layer_shell::{Edge, KeyboardMode, Layer, LayerShell};
        assert_eq!(
            std::env::var("WM_NETWORK_TEST_PRIVATE_BUS").as_deref(),
            Ok("1")
        );
        gtk::init().unwrap();
        crate::css(&wm_core::Config::load().unwrap());
        let context = glib::MainContext::default();
        let _guard = context.acquire().unwrap();
        let connection = gio::bus_get_sync(gio::BusType::Session, gio::Cancellable::NONE).unwrap();
        connection
            .call_sync(
                Some("org.freedesktop.DBus"),
                "/org/freedesktop/DBus",
                "org.freedesktop.DBus",
                "RequestName",
                Some(&("org.bluez", 4u32).to_variant()),
                None,
                gio::DBusCallFlags::NONE,
                1000,
                gio::Cancellable::NONE,
            )
            .unwrap();
        let peer = Rc::new(RefCell::new(String::new()));
        let registered = peer.clone();
        let info = gio::DBusNodeInfo::for_xml("<node><interface name='org.bluez.AgentManager1'><method name='RegisterAgent'><arg type='o' direction='in'/><arg type='s' direction='in'/></method></interface></node>").unwrap();
        let manager = connection
            .register_object("/org/bluez", &info.interfaces()[0])
            .method_call(move |_, sender, _, _, method, args, invocation| {
                assert_eq!(method, "RegisterAgent");
                let (path, capability) = args.get::<(glib::variant::ObjectPath, String)>().unwrap();
                assert_eq!(path.as_str(), AGENT);
                assert_eq!(capability, "KeyboardDisplay");
                registered.replace(sender.unwrap().into());
                invocation.return_value(None);
            })
            .build()
            .unwrap();
        let pair_call = Rc::new(RefCell::new(None::<gio::DBusMethodInvocation>));
        let pending = pair_call.clone();
        let cancels = Rc::new(Cell::new(0));
        let counted = cancels.clone();
        let info = gio::DBusNodeInfo::for_xml("<node><interface name='org.bluez.Device1'><method name='Pair'/><method name='CancelPairing'/></interface></node>").unwrap();
        let device_registration = connection
            .register_object("/device", &info.interfaces()[0])
            .method_call(move |_, _, _, _, method, _, invocation| {
                if method == "Pair" {
                    assert!(pending.borrow().is_none());
                    pending.replace(Some(invocation));
                } else {
                    counted.set(counted.get() + 1);
                    if let Some(pair) = pending.borrow_mut().take() {
                        pair.return_dbus_error(
                            "org.bluez.Error.AuthenticationCanceled",
                            "Canceled",
                        );
                    }
                    invocation.return_value(None);
                }
            })
            .build()
            .unwrap();
        let device = context
            .block_on(gio::DBusProxy::new_future(
                &connection,
                gio::DBusProxyFlags::DO_NOT_LOAD_PROPERTIES,
                None,
                Some("org.bluez"),
                "/device",
                "org.bluez.Device1",
            ))
            .unwrap();
        let panel = gtk::Box::new(gtk::Orientation::Vertical, 6);
        let error = gtk::Label::new(None);
        let body = gtk::Box::new(gtk::Orientation::Vertical, 6);
        body.set_width_request(320);
        body.set_margin_top(12);
        body.set_margin_bottom(12);
        body.set_margin_start(12);
        body.set_margin_end(12);
        body.append(&panel);
        body.append(&error);
        let popover = gtk::Popover::builder().child(&body).build();
        let button = gtk::MenuButton::builder()
            .label("Bluetooth")
            .popover(&popover)
            .build();
        crate::network::popover_keyboard_focus(&button, &popover);
        let window = gtk::Window::new();
        window.init_layer_shell();
        window.set_layer(Layer::Top);
        window.set_anchor(Edge::Top, true);
        window.set_anchor(Edge::Left, true);
        window.set_keyboard_mode(KeyboardMode::None);
        window.set_default_size(340, 40);
        window.set_child(Some(&button));
        window.present();
        let wait = |predicate: &dyn Fn() -> bool| {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(6);
            while !predicate() {
                while context.pending() {
                    context.iteration(false);
                }
                assert!(
                    std::time::Instant::now() < deadline,
                    "pairing did not settle"
                );
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
        };
        let input = |args: &[&str]| {
            assert!(
                std::process::Command::new("xdotool")
                    .env("DISPLAY", std::env::var("WM_TEST_HOST_DISPLAY").unwrap())
                    .args(args)
                    .status()
                    .unwrap()
                    .success()
            );
        };
        wait(&|| button.is_mapped() && button.width() >= 100);
        context.block_on(glib::timeout_future(std::time::Duration::from_millis(250)));
        input(&[
            "mousemove",
            "--window",
            &std::env::var("WM_TEST_HOST_WINDOW").unwrap(),
            "100",
            "20",
            "click",
            "1",
        ]);
        wait(&|| popover.is_mapped());
        let path = glib::variant::ObjectPath::try_from("/device").unwrap();
        let request = |method: &str, args: glib::Variant| {
            let result = Rc::new(RefCell::new(None));
            let received = result.clone();
            connection.call(
                Some(&peer.borrow()),
                AGENT,
                "org.bluez.Agent1",
                method,
                Some(&args),
                None,
                gio::DBusCallFlags::NONE,
                5000,
                gio::Cancellable::NONE,
                move |reply| {
                    received.replace(Some(reply));
                },
            );
            result
        };
        let gone = |peer: &str| {
            !connection
                .call_sync(
                    Some("org.freedesktop.DBus"),
                    "/org/freedesktop/DBus",
                    "org.freedesktop.DBus",
                    "NameHasOwner",
                    Some(&(peer,).to_variant()),
                    None,
                    gio::DBusCallFlags::NONE,
                    1000,
                    gio::Cancellable::NONE,
                )
                .unwrap()
                .get::<(bool,)>()
                .unwrap()
                .0
        };
        for method in ["RequestConfirmation", "RequestPinCode", "RequestPasskey"] {
            let state = Pairing::start(
                gio::BusType::Session,
                &device,
                "Test keyboard",
                &panel,
                &error,
            );
            wait(&|| pair_call.borrow().is_some());
            assert_ne!(*peer.borrow(), connection.unique_name().unwrap());
            let args = if method == "RequestConfirmation" {
                (path.clone(), 1387u32).to_variant()
            } else {
                (path.clone(),).to_variant()
            };
            let reply = request(method, args);
            wait(&|| state.pending.borrow().is_some() && state.accept.is_mapped());
            context.block_on(glib::timeout_future(std::time::Duration::from_millis(200)));
            if method == "RequestConfirmation" {
                assert!(state.message.text().contains("001387"));
                assert!(state.accept.has_focus());
                input(&["key", "space"]);
            } else {
                assert!(
                    gtk::prelude::GtkWindowExt::focus(&window).is_some_and(|focus| focus
                        == state.entry
                        || focus.is_ancestor(&state.entry))
                );
                input(&[
                    "type",
                    "--clearmodifiers",
                    if method == "RequestPinCode" {
                        "abc123"
                    } else {
                        "001387"
                    },
                ]);
                input(&["key", "Return"]);
            }
            wait(&|| reply.borrow().is_some());
            let value = reply
                .borrow_mut()
                .take()
                .unwrap()
                .unwrap_or_else(|error| panic!("{method}: {error}"));
            if method == "RequestPasskey" {
                assert_eq!(value.get::<(u32,)>(), Some((1387,)));
            }
            if method == "RequestPinCode" {
                assert_eq!(value.get::<(String,)>(), Some(("abc123".into(),)));
            }
            assert!(state.entry.text().is_empty());
            pair_call.borrow_mut().take().unwrap().return_value(None);
            wait(&|| !state.active.get());
            wait(&|| gone(&peer.borrow()));
            assert!(!panel.is_visible());
        }
        let state = Pairing::start(
            gio::BusType::Session,
            &device,
            "Test keyboard",
            &panel,
            &error,
        );
        wait(&|| pair_call.borrow().is_some());
        let reply = request("DisplayPasskey", (path.clone(), 1387u32, 3u16).to_variant());
        wait(&|| reply.borrow().is_some());
        assert!(reply.borrow().as_ref().unwrap().is_ok());
        assert!(state.message.text().contains("001387") && state.message.text().contains("3/6"));
        let canceled_display = request("Cancel", ().to_variant());
        wait(&|| canceled_display.borrow().is_some());
        assert!(
            state.active.get(),
            "ending an agent prompt must not cancel the Pair operation"
        );
        let address =
            gio::dbus_address_get_for_bus_sync(gio::BusType::Session, gio::Cancellable::NONE)
                .unwrap();
        let stranger = context
            .block_on(gio::DBusConnection::for_address_future(
                &address,
                gio::DBusConnectionFlags::AUTHENTICATION_CLIENT
                    | gio::DBusConnectionFlags::MESSAGE_BUS_CONNECTION,
                None,
            ))
            .unwrap();
        let spoofed = context.block_on(stranger.call_future(
            Some(&peer.borrow()),
            AGENT,
            "org.bluez.Agent1",
            "RequestConfirmation",
            Some(&(path.clone(), 1387u32).to_variant()),
            None,
            gio::DBusCallFlags::NONE,
            5000,
        ));
        assert!(spoofed.is_err(), "only the captured BlueZ owner may prompt");
        assert!(state.pending.borrow().is_none());
        stranger.close(gio::Cancellable::NONE, |_| {});
        let bad = request(
            "RequestConfirmation",
            (
                glib::variant::ObjectPath::try_from("/unrelated").unwrap(),
                1387u32,
            )
                .to_variant(),
        );
        wait(&|| bad.borrow().is_some());
        assert!(bad.borrow().as_ref().unwrap().is_err());
        let pending = request("RequestAuthorization", (path.clone(),).to_variant());
        wait(&|| state.pending.borrow().is_some());
        if let Ok(path) = std::env::var("WM_NETWORK_TEST_SCREENSHOT") {
            context.block_on(glib::timeout_future(std::time::Duration::from_millis(200)));
            assert!(
                std::process::Command::new("import")
                    .env("DISPLAY", std::env::var("WM_TEST_HOST_DISPLAY").unwrap())
                    .args([
                        "-window",
                        &std::env::var("WM_TEST_HOST_WINDOW").unwrap(),
                        &path
                    ])
                    .status()
                    .unwrap()
                    .success()
            );
        }
        panel
            .last_child()
            .unwrap()
            .downcast::<gtk::Button>()
            .unwrap()
            .emit_clicked();
        wait(&|| !state.active.get() && cancels.get() == 1 && pending.borrow().is_some());
        assert!(pending.borrow().as_ref().unwrap().is_err());
        wait(&|| gone(&peer.borrow()));
        let state = Pairing::start(
            gio::BusType::Session,
            &device,
            "Test keyboard",
            &panel,
            &error,
        );
        wait(&|| pair_call.borrow().is_some());
        drop(state);
        wait(&|| cancels.get() == 2 && gone(&peer.borrow()));
        button.popdown();
        wait(&|| !popover.is_visible());
        assert_eq!(window.keyboard_mode(), KeyboardMode::None);
        window.close();
        connection.unregister_object(device_registration).unwrap();
        connection.unregister_object(manager).unwrap();
        std::fs::write(std::env::var("WM_NETWORK_TEST_RECEIPT").unwrap(), "ok").unwrap();
    }
}
