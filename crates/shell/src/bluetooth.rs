//! Bluetooth controls backed by BlueZ's event-driven object manager.
use gtk::{gio, glib, prelude::*};
use std::{
    cell::{Cell, RefCell},
    collections::HashMap,
    rc::Rc,
};

const BLUEZ: &str = "org.bluez";
const ADAPTER: &str = "org.bluez.Adapter1";
const DEVICE: &str = "org.bluez.Device1";

fn boolean(proxy: &gio::DBusProxy, property: &str) -> bool {
    proxy
        .cached_property(property)
        .and_then(|v| v.get::<bool>())
        .unwrap_or(false)
}
fn name(proxy: &gio::DBusProxy) -> String {
    ["Alias", "Name", "Address"]
        .iter()
        .find_map(|name| {
            proxy
                .cached_property(name)
                .and_then(|v| v.get::<String>())
                .filter(|v| !v.is_empty())
        })
        .unwrap_or_else(|| "Bluetooth device".into())
        .chars()
        .take(64)
        .map(|c| if c.is_control() { '�' } else { c })
        .collect()
}
fn proxy(object: &gio::DBusObject, interface: &str) -> Option<gio::DBusProxy> {
    gio::prelude::DBusObjectExt::interface(object, interface)?
        .downcast()
        .ok()
}

struct Bluetooth {
    bus: gio::BusType,
    pairing: RefCell<Option<Rc<crate::bluetooth_pairing::Pairing>>>,
    pairing_panel: gtk::Box,
    button: glib::WeakRef<gtk::MenuButton>,
    rows: gtk::Box,
    error: gtk::Label,
    manager: gio::DBusObjectManagerClient,
    pending: RefCell<HashMap<String, u64>>,
    next_request: Cell<u64>,
    queued: Cell<bool>,
    discoveries: RefCell<HashMap<String, gio::DBusProxy>>,
}

impl Drop for Bluetooth {
    fn drop(&mut self) {
        for (_, proxy) in self.discoveries.get_mut().drain() {
            proxy.call(
                "StopDiscovery",
                None,
                gio::DBusCallFlags::NONE,
                5000,
                gio::Cancellable::NONE,
                |_| {},
            );
        }
    }
}

impl Bluetooth {
    fn menu_open(&self) -> bool {
        self.button
            .upgrade()
            .and_then(|button| button.popover())
            .is_some_and(|popover| popover.is_mapped())
    }

    fn stop_discoveries(self: &Rc<Self>) {
        let proxies: Vec<_> = self.discoveries.borrow().values().cloned().collect();
        for proxy in proxies {
            self.discovery(&proxy, false);
        }
    }

    fn discovery(self: &Rc<Self>, proxy: &gio::DBusProxy, start: bool) {
        let path = proxy.object_path().to_string();
        if self.pending.borrow().contains_key(&path) || self.manager.name_owner().is_none() {
            return;
        }
        let request = self.next_request.get().wrapping_add(1);
        self.next_request.set(request);
        self.pending.borrow_mut().insert(path.clone(), request);
        self.error.set_label("");
        self.update();
        let weak = Rc::downgrade(self);
        let retained_proxy = proxy.clone();
        let owner = self.manager.name_owner();
        proxy.call(
            if start {
                "StartDiscovery"
            } else {
                "StopDiscovery"
            },
            None,
            gio::DBusCallFlags::NONE,
            5000,
            gio::Cancellable::NONE,
            move |result| {
                let Some(state) = weak.upgrade() else {
                    if start && result.is_ok() {
                        retained_proxy.call(
                            "StopDiscovery",
                            None,
                            gio::DBusCallFlags::NONE,
                            5000,
                            gio::Cancellable::NONE,
                            |_| {},
                        );
                    }
                    return;
                };
                if state.manager.name_owner() != owner
                    || state.pending.borrow().get(&path) != Some(&request)
                {
                    return;
                }
                state.pending.borrow_mut().remove(&path);
                match result {
                    Ok(_) => {
                        if start {
                            state
                                .discoveries
                                .borrow_mut()
                                .insert(path, retained_proxy.clone());
                        } else {
                            state.discoveries.borrow_mut().remove(&path);
                        }
                        if start && !state.menu_open() {
                            state.discovery(&retained_proxy, false);
                        }
                    }
                    Err(error) => state
                        .error
                        .set_label(&format!("Bluetooth discovery failed: {error}")),
                }
                state.update();
            },
        );
    }
    fn update(self: &Rc<Self>) {
        let Some(button) = self.button.upgrade() else {
            return;
        };
        while let Some(child) = self.rows.first_child() {
            self.rows.remove(&child);
        }
        if self.manager.name_owner().is_none() {
            button.set_label("Bluetooth unavailable");
            self.rows
                .append(&gtk::Label::new(Some("BlueZ is not running")));
            return;
        }
        let objects = self.manager.objects();
        if let Some(pairing) = self.pairing.borrow().as_ref() {
            if pairing.active.get()
                && !objects.iter().any(|object| {
                    object.object_path().as_str() == pairing.device_path()
                        && proxy(object, DEVICE).is_some()
                })
            {
                pairing.device_removed();
            }
        }
        let mut adapters: Vec<_> = objects
            .iter()
            .filter_map(|object| proxy(object, ADAPTER))
            .collect();
        adapters.sort_by_key(|proxy| proxy.object_path());
        adapters.truncate(16);
        let mut devices: Vec<_> = objects
            .iter()
            .filter_map(|object| proxy(object, DEVICE))
            .collect();
        devices.sort_by_key(|proxy| (!boolean(proxy, "Connected"), name(proxy)));
        devices.truncate(64);
        let connected = devices
            .iter()
            .filter(|proxy| boolean(proxy, "Connected"))
            .count();
        button.set_label(&if connected > 0 {
            format!("BT {connected}")
        } else if adapters.iter().any(|proxy| boolean(proxy, "Powered")) {
            "Bluetooth".into()
        } else {
            "BT off".into()
        });
        if adapters.is_empty() {
            self.rows
                .append(&gtk::Label::new(Some("No Bluetooth adapter")));
        }
        for adapter in &adapters {
            let powered = boolean(adapter, "Powered");
            let row = self.row(&name(adapter), if powered { "Turn off" } else { "Turn on" });
            row.set_sensitive(
                !self
                    .pending
                    .borrow()
                    .contains_key(adapter.object_path().as_str()),
            );
            let weak = Rc::downgrade(self);
            let power_adapter = adapter.clone();
            row.connect_clicked(move |_| {
                if let Some(state) = weak.upgrade() {
                    state.call(
                        &power_adapter,
                        "org.freedesktop.DBus.Properties.Set",
                        Some((ADAPTER, "Powered", (!powered).to_variant()).to_variant()),
                    );
                }
            });
            let discovering = self
                .discoveries
                .borrow()
                .contains_key(adapter.object_path().as_str());
            let row = self.row(
                &name(adapter),
                if discovering {
                    "Stop discovery"
                } else {
                    "Find devices"
                },
            );
            row.set_sensitive(
                powered
                    && !self
                        .pending
                        .borrow()
                        .contains_key(adapter.object_path().as_str()),
            );
            let weak = Rc::downgrade(self);
            let adapter = adapter.clone();
            row.connect_clicked(move |_| {
                if let Some(state) = weak.upgrade() {
                    state.discovery(&adapter, !discovering);
                }
            });
        }
        if devices.is_empty() {
            self.rows.append(&gtk::Label::new(Some(
                "No known devices. Use Find devices to scan.",
            )));
        }
        for device in devices {
            let connected = boolean(&device, "Connected");
            let paired = boolean(&device, "Paired") || boolean(&device, "Bonded");
            let row = self.row(
                &name(&device),
                if connected {
                    "Disconnect"
                } else if paired {
                    "Connect"
                } else {
                    "Pair"
                },
            );
            let adapter_path = device
                .cached_property("Adapter")
                .and_then(|v| v.get::<glib::variant::ObjectPath>());
            let powered = adapters.iter().any(|a| {
                adapter_path
                    .as_ref()
                    .is_some_and(|path| a.object_path().as_str() == path.as_str())
                    && boolean(a, "Powered")
            });
            row.set_sensitive(
                powered
                    && !boolean(&device, "Blocked")
                    && !self
                        .pending
                        .borrow()
                        .contains_key(device.object_path().as_str()),
            );
            let weak = Rc::downgrade(self);
            row.connect_clicked(move |_| {
                if let Some(state) = weak.upgrade() {
                    if !connected && !paired {
                        if state
                            .pairing
                            .borrow()
                            .as_ref()
                            .is_some_and(|p| p.active.get())
                        {
                            state
                                .error
                                .set_label("Finish or cancel the current pairing first.");
                            return;
                        }
                        let pairing = crate::bluetooth_pairing::Pairing::start(
                            state.bus,
                            &device,
                            &name(&device),
                            &state.pairing_panel,
                            &state.error,
                        );
                        state.pairing.replace(Some(pairing));
                        return;
                    }
                    state.call(
                        &device,
                        if connected { "Disconnect" } else { "Connect" },
                        None,
                    );
                }
            });
        }
    }

    fn row(&self, title: &str, action: &str) -> gtk::Button {
        let button = gtk::Button::new();
        let label = gtk::Label::new(Some(&format!("{title} · {action}")));
        label.set_xalign(0.0);
        label.set_max_width_chars(36);
        label.set_ellipsize(gtk::pango::EllipsizeMode::End);
        button.set_child(Some(&label));
        button.set_tooltip_text(Some(title));
        self.rows.append(&button);
        button
    }

    fn call(
        self: &Rc<Self>,
        proxy: &gio::DBusProxy,
        method: &str,
        parameters: Option<glib::Variant>,
    ) {
        let path = proxy.object_path().to_string();
        if self.manager.name_owner().is_none() || self.pending.borrow().contains_key(&path) {
            return;
        }
        let request = self.next_request.get().wrapping_add(1);
        self.next_request.set(request);
        self.pending.borrow_mut().insert(path.clone(), request);
        self.error.set_label("");
        self.update();
        let weak = Rc::downgrade(self);
        let owner = self.manager.name_owner();
        proxy.call(
            method,
            parameters.as_ref(),
            gio::DBusCallFlags::NONE,
            20_000,
            gio::Cancellable::NONE,
            move |result| {
                let Some(state) = weak.upgrade() else { return };
                if state.pending.borrow().get(&path) != Some(&request) {
                    return;
                }
                state.pending.borrow_mut().remove(&path);
                if state.manager.name_owner() == owner {
                    if let Err(error) = result {
                        state
                            .error
                            .set_label(&format!("Bluetooth request failed: {error}"));
                    }
                    // Closing the menu may have deferred discovery cleanup while
                    // this adapter request held its pending slot.
                    if !state.menu_open() {
                        state.stop_discoveries();
                    }
                }
                state.update();
            },
        );
    }

    fn queue(self: &Rc<Self>) {
        if self.queued.replace(true) {
            return;
        }
        let weak = Rc::downgrade(self);
        glib::timeout_add_local_once(std::time::Duration::from_millis(100), move || {
            if let Some(state) = weak.upgrade() {
                state.queued.set(false);
                state.update();
            }
        });
    }
}

pub fn button() -> gtk::MenuButton {
    button_for_bus(gio::BusType::System)
}

fn button_for_bus(bus: gio::BusType) -> gtk::MenuButton {
    let button = gtk::MenuButton::new();
    button.set_label("Bluetooth…");
    let content = gtk::Box::new(gtk::Orientation::Vertical, 8);
    content.set_margin_top(12);
    content.set_margin_bottom(12);
    content.set_margin_start(12);
    content.set_margin_end(12);
    content.set_width_request(320);
    let rows = gtk::Box::new(gtk::Orientation::Vertical, 4);
    let scroll = gtk::ScrolledWindow::builder()
        .max_content_height(280)
        .propagate_natural_height(true)
        .hscrollbar_policy(gtk::PolicyType::Never)
        .child(&rows)
        .build();
    let error = gtk::Label::new(None);
    error.set_wrap(true);
    error.set_max_width_chars(36);
    content.append(&scroll);
    content.append(&error);
    let pairing_panel = gtk::Box::new(gtk::Orientation::Vertical, 6);
    pairing_panel.set_visible(false);
    content.append(&pairing_panel);
    let settings = gtk::Button::with_label("Bluetooth settings…");
    content.append(&settings);
    let weak_error = error.downgrade();
    settings.connect_clicked(move |_| {
        if let Err(error) = std::process::Command::new("blueman-manager").spawn() {
            if let Some(label) = weak_error.upgrade() {
                label.set_label(&format!("Could not open Bluetooth settings: {error}"));
            }
        }
    });
    let popover = gtk::Popover::new();
    popover.set_child(Some(&content));
    button.set_popover(Some(&popover));
    crate::network::popover_keyboard_focus(&button, &popover);
    let weak = button.downgrade();
    glib::MainContext::default().spawn_local(async move {
        let result = gio::DBusObjectManagerClient::new_for_bus_future(
            bus,
            gio::DBusObjectManagerClientFlags::DO_NOT_AUTO_START,
            BLUEZ,
            "/",
        )
        .await;
        let Some(button) = weak.upgrade() else { return };
        let manager = match result {
            Ok(manager) => manager,
            Err(reason) => {
                button.set_label("Bluetooth unavailable");
                error.set_label(&format!("Could not read Bluetooth status: {reason}"));
                return;
            }
        };
        let state = Rc::new(Bluetooth {
            bus,
            pairing: RefCell::new(None),
            pairing_panel,
            button: button.downgrade(),
            rows,
            error,
            manager: manager.clone(),
            pending: RefCell::new(HashMap::new()),
            next_request: Cell::new(0),
            queued: Cell::new(false),
            discoveries: RefCell::new(HashMap::new()),
        });
        for signal in [
            "object-added",
            "object-removed",
            "interface-added",
            "interface-removed",
            "interface-proxy-properties-changed",
        ] {
            let weak = Rc::downgrade(&state);
            manager.connect_local(signal, false, move |_| {
                if let Some(state) = weak.upgrade() {
                    state.queue();
                }
                None
            });
        }
        let weak = Rc::downgrade(&state);
        manager.connect_notify_local(Some("name-owner"), move |_, _| {
            if let Some(state) = weak.upgrade() {
                if let Some(pairing) = state.pairing.borrow_mut().take() {
                    pairing.cancel();
                }
                state.pending.borrow_mut().clear();
                state.discoveries.borrow_mut().clear();
                state.error.set_label("");
                state.update();
            }
        });
        state.update();
        let weak = Rc::downgrade(&state);
        button.popover().unwrap().connect_unmap(move |_| {
            if let Some(state) = weak.upgrade() {
                state.stop_discoveries();
                if let Some(pairing) = state.pairing.borrow_mut().take() {
                    pairing.cancel();
                }
            }
        });
        button.connect_destroy(move |_| {
            let _ = &state;
        });
    });
    button
}

#[cfg(test)]
mod tests {
    use super::*;
    use glib::variant::ObjectPath;
    type Properties = HashMap<String, glib::Variant>;
    type Objects = HashMap<ObjectPath, HashMap<String, Properties>>;
    fn path(value: &str) -> ObjectPath {
        ObjectPath::try_from(value).unwrap()
    }

    #[test]
    #[ignore = "requires private D-Bus and GTK display"]
    fn live_power_connections_and_restart() {
        assert_eq!(
            std::env::var("WM_NETWORK_TEST_PRIVATE_BUS").as_deref(),
            Ok("1")
        );
        gtk::init().unwrap();
        let context = glib::MainContext::default();
        let _guard = context.acquire().unwrap();
        let connection = gio::bus_get_sync(gio::BusType::Session, gio::Cancellable::NONE).unwrap();
        let request_name = || {
            connection
                .call_sync(
                    Some("org.freedesktop.DBus"),
                    "/org/freedesktop/DBus",
                    "org.freedesktop.DBus",
                    "RequestName",
                    Some(&(BLUEZ, 4u32).to_variant()),
                    None,
                    gio::DBusCallFlags::NONE,
                    1000,
                    gio::Cancellable::NONE,
                )
                .unwrap()
        };
        request_name();
        let powered = Rc::new(Cell::new(true));
        let connected = Rc::new(Cell::new(false));
        let power_snapshot = powered.clone();
        let connected_snapshot = connected.clone();
        let snapshots = Rc::new(Cell::new(0));
        let counted = snapshots.clone();
        let info = gio::DBusNodeInfo::for_xml("<node><interface name='org.freedesktop.DBus.ObjectManager'><method name='GetManagedObjects'><arg type='a{oa{sa{sv}}}' direction='out'/></method></interface></node>").unwrap();
        let root_registration = connection
            .register_object("/", &info.interfaces()[0])
            .method_call(move |_, _, _, _, _, _, invocation| {
                counted.set(counted.get() + 1);
                let objects: Objects = HashMap::from([
                    (
                        path("/adapter"),
                        HashMap::from([(
                            ADAPTER.into(),
                            HashMap::from([
                                ("Powered".into(), power_snapshot.get().to_variant()),
                                ("Alias".into(), "Test adapter".to_variant()),
                            ]),
                        )]),
                    ),
                    (
                        path("/adapter/device"),
                        HashMap::from([(
                            DEVICE.into(),
                            HashMap::from([
                                ("Paired".into(), true.to_variant()),
                                ("Connected".into(), connected_snapshot.get().to_variant()),
                                ("Adapter".into(), path("/adapter").to_variant()),
                                ("Alias".into(), "Test headphones".to_variant()),
                            ]),
                        )]),
                    ),
                ]);
                invocation.return_value(Some(&(objects,).to_variant()));
            })
            .build()
            .unwrap();
        let power = powered.clone();
        let power_writes = Rc::new(Cell::new(0));
        let writes = power_writes.clone();
        let starts = Rc::new(Cell::new(0));
        let stops = Rc::new(Cell::new(0));
        let start_calls = starts.clone();
        let stop_calls = stops.clone();
        let info = gio::DBusNodeInfo::for_xml("<node><interface name='org.bluez.Adapter1'><method name='StartDiscovery'/><method name='StopDiscovery'/><property name='Powered' type='b' access='readwrite'/></interface></node>").unwrap();
        let adapter_registration = connection
            .register_object("/adapter", &info.interfaces()[0])
            .method_call(move |_, _, _, _, method, _, invocation| {
                match method {
                    "StartDiscovery" => start_calls.set(start_calls.get() + 1),
                    "StopDiscovery" => stop_calls.set(stop_calls.get() + 1),
                    _ => panic!("unexpected discovery method"),
                }
                invocation.return_value(Some(&().to_variant()));
            })
            .set_property(move |connection, _, _, _, property, value| {
                assert_eq!(property, "Powered");
                power.set(value.get::<bool>().unwrap());
                writes.set(writes.get() + 1);
                let changed = HashMap::from([("Powered", value)]);
                connection
                    .emit_signal(
                        None,
                        "/adapter",
                        "org.freedesktop.DBus.Properties",
                        "PropertiesChanged",
                        Some(&(ADAPTER, changed, Vec::<String>::new()).to_variant()),
                    )
                    .unwrap();
                true
            })
            .build()
            .unwrap();
        let device_connected = connected.clone();
        let deny = Rc::new(Cell::new(false));
        let deny_request = deny.clone();
        let info = gio::DBusNodeInfo::for_xml("<node><interface name='org.bluez.Device1'><method name='Connect'/><method name='Disconnect'/></interface></node>").unwrap();
        let device_registration = connection
            .register_object("/adapter/device", &info.interfaces()[0])
            .method_call(move |connection, _, _, _, method, _, invocation| {
                if deny_request.get() {
                    invocation.return_dbus_error("org.bluez.Error.Failed", "test failure");
                    return;
                }
                assert!(method == "Connect" || method == "Disconnect");
                device_connected.set(method == "Connect");
                let changed = HashMap::from([("Connected", device_connected.get().to_variant())]);
                connection
                    .emit_signal(
                        None,
                        "/adapter/device",
                        "org.freedesktop.DBus.Properties",
                        "PropertiesChanged",
                        Some(&(DEVICE, changed, Vec::<String>::new()).to_variant()),
                    )
                    .unwrap();
                invocation.return_value(Some(&().to_variant()));
            })
            .build()
            .unwrap();
        let agent_count = Rc::new(Cell::new(0));
        let counted = agent_count.clone();
        let info = gio::DBusNodeInfo::for_xml("<node><interface name='org.bluez.AgentManager1'><method name='RegisterAgent'><arg type='o' direction='in'/><arg type='s' direction='in'/></method></interface></node>").unwrap();
        let agent_registration = connection
            .register_object("/org/bluez", &info.interfaces()[0])
            .method_call(move |_, _, _, _, method, args, invocation| {
                assert_eq!(method, "RegisterAgent");
                assert_eq!(
                    args.get::<(ObjectPath, String)>().unwrap().1,
                    "KeyboardDisplay"
                );
                counted.set(counted.get() + 1);
                invocation.return_value(None);
            })
            .build()
            .unwrap();
        let pair_count = Rc::new(Cell::new(0));
        let counted = pair_count.clone();
        let held_pair = Rc::new(RefCell::new(None::<gio::DBusMethodInvocation>));
        let held = held_pair.clone();
        let canceled_pairs = Rc::new(Cell::new(0));
        let canceled = canceled_pairs.clone();
        let info = gio::DBusNodeInfo::for_xml(
            "<node><interface name='org.bluez.Device1'><method name='Pair'/><method name='CancelPairing'/></interface></node>",
        )
        .unwrap();
        let new_device_registration = connection
            .register_object("/adapter/new", &info.interfaces()[0])
            .method_call(move |connection, _, _, _, method, _, invocation| {
                if method == "CancelPairing" {
                    canceled.set(canceled.get() + 1);
                    if let Some(pair) = held.borrow_mut().take() {
                        pair.return_dbus_error(
                            "org.bluez.Error.AuthenticationCanceled",
                            "Canceled by user",
                        );
                    }
                    invocation.return_value(None);
                    return;
                }
                assert_eq!(method, "Pair");
                counted.set(counted.get() + 1);
                if counted.get() > 1 {
                    assert!(held.borrow().is_none());
                    held.replace(Some(invocation));
                    return;
                }
                connection
                    .emit_signal(
                        None,
                        "/adapter/new",
                        "org.freedesktop.DBus.Properties",
                        "PropertiesChanged",
                        Some(
                            &(
                                DEVICE,
                                HashMap::from([("Paired", true.to_variant())]),
                                Vec::<String>::new(),
                            )
                                .to_variant(),
                        ),
                    )
                    .unwrap();
                invocation.return_value(None);
            })
            .build()
            .unwrap();
        let button = button_for_bus(gio::BusType::Session);
        let wait = |predicate: &dyn Fn() -> bool| {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(4);
            while !predicate() {
                while context.pending() {
                    context.iteration(false);
                }
                assert!(
                    std::time::Instant::now() < deadline,
                    "Bluetooth control did not settle"
                );
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
        };
        wait(&|| button.label().as_deref() == Some("Bluetooth"));
        let content = button
            .popover()
            .unwrap()
            .child()
            .unwrap()
            .downcast::<gtk::Box>()
            .unwrap();
        let scroll = content
            .first_child()
            .unwrap()
            .downcast::<gtk::ScrolledWindow>()
            .unwrap();
        let rows = scroll
            .child()
            .unwrap()
            .downcast::<gtk::Viewport>()
            .unwrap()
            .child()
            .unwrap()
            .downcast::<gtk::Box>()
            .unwrap();
        let error = scroll
            .next_sibling()
            .unwrap()
            .downcast::<gtk::Label>()
            .unwrap();
        assert_eq!(power_writes.get(), 0);
        let window = gtk::Window::builder().child(&button).build();
        if std::env::var_os("WM_NETWORK_TEST_INPUT").is_some() {
            use gtk4_layer_shell::{Edge, Layer, LayerShell};
            window.init_layer_shell();
            window.set_layer(Layer::Top);
            window.set_anchor(Edge::Top, true);
            window.set_anchor(Edge::Left, true);
            window.set_default_size(340, 40);
        }
        window.present();
        wait(&|| window.is_mapped() && button.width() > 0);
        context.block_on(glib::timeout_future(std::time::Duration::from_millis(250)));
        if std::env::var_os("WM_NETWORK_TEST_INPUT").is_some() {
            assert!(
                std::process::Command::new("xdotool")
                    .env("DISPLAY", std::env::var("WM_TEST_HOST_DISPLAY").unwrap())
                    .args([
                        "mousemove",
                        "--window",
                        &std::env::var("WM_TEST_HOST_WINDOW").unwrap(),
                        "100",
                        "20",
                        "click",
                        "1"
                    ])
                    .status()
                    .unwrap()
                    .success()
            );
        } else {
            button.popup();
        }
        wait(&|| button.popover().unwrap().is_mapped());
        rows.first_child()
            .unwrap()
            .next_sibling()
            .unwrap()
            .downcast::<gtk::Button>()
            .unwrap()
            .emit_clicked();
        wait(&|| {
            starts.get() == 1
                && rows
                    .first_child()
                    .unwrap()
                    .next_sibling()
                    .unwrap()
                    .is_sensitive()
        });
        assert_eq!(stops.get(), 0, "open menu retains its discovery session");
        let interfaces = HashMap::from([(
            DEVICE,
            HashMap::from([
                ("Alias", "Test mouse".to_variant()),
                ("Adapter", path("/adapter").to_variant()),
                ("Paired", false.to_variant()),
            ]),
        )]);
        connection
            .emit_signal(
                None,
                "/",
                "org.freedesktop.DBus.ObjectManager",
                "InterfacesAdded",
                Some(&(path("/adapter/new"), interfaces).to_variant()),
            )
            .unwrap();
        wait(&|| {
            rows.last_child().is_some_and(|row| {
                row.downcast_ref::<gtk::Button>()
                    .and_then(|b| b.child())
                    .and_then(|c| c.downcast::<gtk::Label>().ok())
                    .is_some_and(|l| l.text().contains("Test mouse · Pair"))
            })
        });
        assert!(rows.last_child().unwrap().is_sensitive());
        rows.last_child()
            .unwrap()
            .downcast::<gtk::Button>()
            .unwrap()
            .emit_clicked();
        wait(&|| pair_count.get() == 1 && error.text().contains("Paired."));
        assert_eq!(agent_count.get(), 1);
        wait(&|| {
            rows.last_child().is_some_and(|row| {
                row.downcast_ref::<gtk::Button>()
                    .and_then(|b| b.child())
                    .and_then(|c| c.downcast::<gtk::Label>().ok())
                    .is_some_and(|l| l.text().contains("Test mouse · Connect"))
            })
        });
        connection
            .emit_signal(
                None,
                "/adapter/new",
                "org.freedesktop.DBus.Properties",
                "PropertiesChanged",
                Some(
                    &(
                        DEVICE,
                        HashMap::from([("Paired", false.to_variant())]),
                        Vec::<String>::new(),
                    )
                        .to_variant(),
                ),
            )
            .unwrap();
        wait(&|| {
            rows.last_child().is_some_and(|row| {
                row.downcast_ref::<gtk::Button>()
                    .and_then(|b| b.child())
                    .and_then(|c| c.downcast::<gtk::Label>().ok())
                    .is_some_and(|l| l.text().contains("Test mouse · Pair"))
            })
        });
        rows.last_child()
            .unwrap()
            .downcast::<gtk::Button>()
            .unwrap()
            .emit_clicked();
        wait(&|| pair_count.get() == 2 && held_pair.borrow().is_some());
        let pairing_panel = error.next_sibling().unwrap();
        assert!(pairing_panel.is_visible());
        // Real Escape must cancel the pending Pair and release this menu's scan.
        if std::env::var_os("WM_NETWORK_TEST_INPUT").is_some() {
            assert!(
                std::process::Command::new("xdotool")
                    .env("DISPLAY", std::env::var("WM_TEST_HOST_DISPLAY").unwrap())
                    .args(["key", "Escape"])
                    .status()
                    .unwrap()
                    .success()
            );
        } else {
            button.popdown();
        }
        wait(&|| canceled_pairs.get() == 1 && held_pair.borrow().is_none() && stops.get() == 1);
        assert!(!pairing_panel.is_visible());
        assert!(error.text().contains("Pairing canceled"));
        assert!(!button.popover().unwrap().is_visible());
        button.popup();
        wait(&|| button.popover().unwrap().is_mapped());
        rows.last_child()
            .unwrap()
            .downcast::<gtk::Button>()
            .unwrap()
            .emit_clicked();
        wait(&|| pair_count.get() == 3 && held_pair.borrow().is_some());
        connection
            .emit_signal(
                None,
                "/",
                "org.freedesktop.DBus.ObjectManager",
                "InterfacesRemoved",
                Some(&(path("/adapter/new"), vec![DEVICE]).to_variant()),
            )
            .unwrap();
        wait(&|| {
            rows.last_child().is_some_and(|row| {
                row.downcast_ref::<gtk::Button>()
                    .and_then(|b| b.child())
                    .and_then(|c| c.downcast::<gtk::Label>().ok())
                    .is_some_and(|l| l.text().contains("Test headphones"))
            })
        });
        wait(&|| canceled_pairs.get() == 2 && held_pair.borrow().is_none());
        assert!(!pairing_panel.is_visible());
        assert!(error.text().contains("device disappeared"));
        button.popdown();
        wait(&|| stops.get() == 1);
        // A completed start after the menu closes must release immediately.
        rows.first_child()
            .unwrap()
            .next_sibling()
            .unwrap()
            .downcast::<gtk::Button>()
            .unwrap()
            .emit_clicked();
        wait(&|| starts.get() == 2 && stops.get() == 2);
        rows.last_child()
            .unwrap()
            .downcast::<gtk::Button>()
            .unwrap()
            .emit_clicked();
        wait(&|| button.label().as_deref() == Some("BT 1"));
        assert!(connected.get());
        deny.set(true);
        rows.last_child()
            .unwrap()
            .downcast::<gtk::Button>()
            .unwrap()
            .emit_clicked();
        wait(&|| !error.label().is_empty());
        assert!(rows.last_child().unwrap().is_sensitive());
        assert!(connected.get());
        deny.set(false);
        rows.last_child()
            .unwrap()
            .downcast::<gtk::Button>()
            .unwrap()
            .emit_clicked();
        wait(&|| !connected.get() && button.label().as_deref() == Some("Bluetooth"));
        button.popup();
        wait(&|| button.popover().unwrap().is_mapped());
        rows.first_child()
            .unwrap()
            .next_sibling()
            .unwrap()
            .downcast::<gtk::Button>()
            .unwrap()
            .emit_clicked();
        wait(&|| starts.get() == 3 && rows.first_child().unwrap().is_sensitive());
        rows.first_child()
            .unwrap()
            .downcast::<gtk::Button>()
            .unwrap()
            .emit_clicked();
        // Close before dispatching the power reply: cleanup must wait for the
        // adapter's pending request, then release the discovery session.
        button.popdown();
        wait(&|| button.label().as_deref() == Some("BT off") && stops.get() == 3);
        assert!(!powered.get());
        assert_eq!(power_writes.get(), 1);
        assert!(!rows.last_child().unwrap().is_sensitive());
        assert_eq!(
            snapshots.get(),
            1,
            "property changes should update cached objects without snapshots"
        );
        connection
            .call_sync(
                Some("org.freedesktop.DBus"),
                "/org/freedesktop/DBus",
                "org.freedesktop.DBus",
                "ReleaseName",
                Some(&(BLUEZ,).to_variant()),
                None,
                gio::DBusCallFlags::NONE,
                1000,
                gio::Cancellable::NONE,
            )
            .unwrap();
        wait(&|| button.label().as_deref() == Some("Bluetooth unavailable"));
        request_name();
        wait(&|| button.label().as_deref() == Some("BT off"));
        assert_eq!(snapshots.get(), 2);
        window.close();
        drop(button);
        connection.unregister_object(device_registration).unwrap();
        connection.unregister_object(adapter_registration).unwrap();
        connection.unregister_object(root_registration).unwrap();
        connection.unregister_object(agent_registration).unwrap();
        connection
            .unregister_object(new_device_registration)
            .unwrap();
        if let Ok(receipt) = std::env::var("WM_NETWORK_TEST_RECEIPT") {
            std::fs::write(receipt, "ok").unwrap();
        }
    }
}
