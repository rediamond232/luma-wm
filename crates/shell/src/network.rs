//! Event-driven NetworkManager status and asynchronous user-requested controls.
use gtk::{gio, glib, prelude::*};
use gtk4_layer_shell::{KeyboardMode, LayerShell};
use std::{cell::Cell, rc::Rc};

struct Controls {
    network: gtk::Switch,
    wireless: gtk::Switch,
    details: gtk::Label,
    error: gtk::Label,
    updating: Cell<bool>,
    busy: Cell<bool>,
}

impl Controls {
    fn update(&self, proxy: &gio::DBusProxy) {
        self.updating.set(true);
        let property = |name| proxy.cached_property(name).and_then(|v| v.get::<bool>());
        let available = proxy.name_owner().is_some();
        let network = property("NetworkingEnabled");
        let wireless = property("WirelessEnabled");
        let hardware = property("WirelessHardwareEnabled").unwrap_or(false);
        self.network.set_active(network.unwrap_or(false));
        self.network.set_state(network.unwrap_or(false));
        self.wireless.set_active(wireless.unwrap_or(false));
        self.wireless.set_state(wireless.unwrap_or(false));
        self.network
            .set_sensitive(available && network.is_some() && !self.busy.get());
        self.wireless.set_sensitive(
            available
                && hardware
                && network == Some(true)
                && wireless.is_some()
                && !self.busy.get(),
        );
        self.details.set_label(if !available {
            "NetworkManager is unavailable"
        } else if !hardware {
            "Wi-Fi is unavailable or blocked by hardware"
        } else if network == Some(false) {
            "Networking is off"
        } else {
            "Wi-Fi settings are controlled by NetworkManager"
        });
        self.updating.set(false);
    }
}

fn connect_control(controls: &Rc<Controls>, proxy: &gio::DBusProxy, wireless: bool) {
    let switch = if wireless {
        &controls.wireless
    } else {
        &controls.network
    };
    let weak = Rc::downgrade(controls);
    let proxy = proxy.clone();
    switch.connect_state_set(move |_, enabled| {
        let Some(controls) = weak.upgrade() else {
            return glib::Propagation::Stop;
        };
        if controls.updating.get() || controls.busy.replace(true) {
            return glib::Propagation::Stop;
        }
        controls.error.set_label("");
        controls.update(&proxy);
        let (method, parameters) = if wireless {
            (
                "org.freedesktop.DBus.Properties.Set",
                (
                    "org.freedesktop.NetworkManager",
                    "WirelessEnabled",
                    enabled.to_variant(),
                )
                    .to_variant(),
            )
        } else {
            ("Enable", (enabled,).to_variant())
        };
        let weak = Rc::downgrade(&controls);
        let result_proxy = proxy.clone();
        let owner = proxy.name_owner();
        proxy.call(
            method,
            Some(&parameters),
            gio::DBusCallFlags::NONE,
            10_000,
            gio::Cancellable::NONE,
            move |result| {
                let Some(controls) = weak.upgrade() else {
                    return;
                };
                controls.busy.set(false);
                if result_proxy.name_owner() == owner {
                    if let Err(error) = result {
                        controls
                            .error
                            .set_label(&format!("Could not change network settings: {error}"));
                    }
                }
                controls.update(&result_proxy);
            },
        );
        glib::Propagation::Stop
    });
}

fn label(state: u32, connectivity: u32) -> &'static str {
    match state {
        10 => "Network off",
        20 => "Offline",
        30 => "Disconnecting…",
        40 => "Connecting…",
        50..=70 if connectivity == 2 => "Sign in to network",
        50..=70 if connectivity == 3 => "Limited network",
        50 => "Local network",
        60 => "Site network",
        70 => "Online",
        _ => "Network unknown",
    }
}

fn update(button: &gtk::MenuButton, proxy: &gio::DBusProxy) {
    if proxy.name_owner().is_none() {
        button.set_label("Network unavailable");
        button.set_tooltip_text(Some("NetworkManager is not running."));
        return;
    }
    let property = |name| {
        proxy
            .cached_property(name)
            .and_then(|v| v.get::<u32>())
            .unwrap_or(0)
    };
    let status = label(property("State"), property("Connectivity"));
    button.set_label(status);
    button.set_tooltip_text(Some(&format!("{status} · Open network controls")));
}

pub fn button() -> gtk::MenuButton {
    button_for_bus(gio::BusType::System)
}

fn button_for_bus(bus: gio::BusType) -> gtk::MenuButton {
    let button = gtk::MenuButton::new();
    button.set_label("Network…");
    let popover = gtk::Popover::new();
    let content = gtk::Box::new(gtk::Orientation::Vertical, 10);
    content.set_margin_top(12);
    content.set_margin_bottom(12);
    content.set_margin_start(12);
    content.set_margin_end(12);
    content.set_width_request(300);
    let controls = Rc::new(Controls {
        network: gtk::Switch::new(),
        wireless: gtk::Switch::new(),
        details: gtk::Label::new(None),
        error: gtk::Label::new(None),
        updating: Cell::new(false),
        busy: Cell::new(false),
    });
    controls.network.set_sensitive(false);
    controls.wireless.set_sensitive(false);
    for (title, switch) in [
        ("Networking", &controls.network),
        ("Wi-Fi", &controls.wireless),
    ] {
        let row = gtk::Box::new(gtk::Orientation::Horizontal, 12);
        let label = gtk::Label::new(Some(title));
        label.set_hexpand(true);
        label.set_xalign(0.0);
        switch.set_valign(gtk::Align::Center);
        switch.update_property(&[gtk::accessible::Property::Label(title)]);
        row.append(&label);
        row.append(switch);
        content.append(&row);
    }
    for label in [&controls.details, &controls.error] {
        label.set_wrap(true);
        label.set_max_width_chars(36);
        label.set_xalign(0.0);
        content.append(label);
    }
    let settings = gtk::Button::with_label("Connection settings…");
    settings.connect_clicked(|button| {
        if let Err(error) = std::process::Command::new("nm-connection-editor").spawn() {
            button.set_tooltip_text(Some(&format!(
                "Could not open connection settings: {error}"
            )));
        }
    });
    content.append(&settings);
    popover.set_child(Some(&content));
    button.set_popover(Some(&popover));
    popover_keyboard_focus(&button, &popover);
    let weak = button.downgrade();
    gio::DBusProxy::for_bus(
        bus,
        gio::DBusProxyFlags::DO_NOT_AUTO_START | gio::DBusProxyFlags::GET_INVALIDATED_PROPERTIES,
        None,
        "org.freedesktop.NetworkManager",
        "/org/freedesktop/NetworkManager",
        "org.freedesktop.NetworkManager",
        gio::Cancellable::NONE,
        move |result| {
            let Some(button) = weak.upgrade() else { return };
            let proxy = match result {
                Ok(proxy) => proxy,
                Err(error) => {
                    button.set_label("Network unavailable");
                    controls.details.set_label("NetworkManager is unavailable");
                    controls
                        .error
                        .set_label(&format!("Could not read network status: {error}"));
                    button
                        .set_tooltip_text(Some(&format!("Could not read network status: {error}")));
                    return;
                }
            };
            update(&button, &proxy);
            controls.update(&proxy);
            if let Some(content) = button
                .popover()
                .and_then(|popover| popover.child())
                .and_then(|child| child.downcast::<gtk::Box>().ok())
            {
                let wifi = crate::wifi::panel(&proxy);
                content.insert_child_after(&wifi, Some(&controls.error));
            }
            connect_control(&controls, &proxy, false);
            connect_control(&controls, &proxy, true);
            let controls_changed = Rc::downgrade(&controls);
            let weak = button.downgrade();
            proxy.connect_local("g-properties-changed", false, move |values| {
                if let Some(button) = weak.upgrade() {
                    if let Ok(proxy) = values[0].get::<gio::DBusProxy>() {
                        update(&button, &proxy);
                        if let Some(controls) = controls_changed.upgrade() {
                            controls.update(&proxy);
                        }
                    }
                }
                None
            });
            let weak = button.downgrade();
            let controls_owner = Rc::downgrade(&controls);
            proxy.connect_notify_local(Some("g-name-owner"), move |proxy, _| {
                if let Some(button) = weak.upgrade() {
                    update(&button, proxy);
                    if let Some(controls) = controls_owner.upgrade() {
                        controls.update(proxy);
                    }
                }
            });
            // Widget owns proxy lifetime; proxy callbacks only hold weak widgets.
            button.connect_destroy(move |_| {
                let _ = (&proxy, &controls);
            });
        },
    );
    button
}

pub(crate) fn popover_keyboard_focus(button: &impl IsA<gtk::Widget>, popover: &gtk::Popover) {
    let button = button.as_ref();
    let previous_mode = Rc::new(Cell::new(None));
    let weak = button.downgrade();
    let previous = previous_mode.clone();
    popover.connect_map(move |_| {
        if let Some(window) = weak
            .upgrade()
            .and_then(|button| button.root())
            .and_then(|root| root.downcast::<gtk::Window>().ok())
            .filter(|window| window.is_layer_window())
        {
            previous.set(Some(window.keyboard_mode()));
            window.set_keyboard_mode(KeyboardMode::OnDemand);
        }
    });
    let weak = button.downgrade();
    popover.connect_unmap(move |_| {
        if let Some(mode) = previous_mode.take() {
            if let Some(window) = weak
                .upgrade()
                .and_then(|button| button.root())
                .and_then(|root| root.downcast::<gtk::Window>().ok())
                .filter(|window| window.is_layer_window())
            {
                window.set_keyboard_mode(mode);
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use gtk::glib;
    #[test]
    #[ignore = "requires a private test D-Bus and GTK display"]
    fn live_properties_and_service_restart() {
        assert_eq!(
            std::env::var("WM_NETWORK_TEST_PRIVATE_BUS").as_deref(),
            Ok("1")
        );
        gtk::init().unwrap();
        let context = glib::MainContext::default();
        let _guard = context.acquire().unwrap();
        let connection = gio::bus_get_sync(gio::BusType::Session, gio::Cancellable::NONE).unwrap();
        let name = "org.freedesktop.NetworkManager";
        let path = "/org/freedesktop/NetworkManager";
        let info = gio::DBusNodeInfo::for_xml("<node><interface name='org.freedesktop.NetworkManager'><method name='Enable'><arg type='b' direction='in'/></method><property name='State' type='u' access='read'/><property name='Connectivity' type='u' access='read'/><property name='NetworkingEnabled' type='b' access='read'/><property name='WirelessEnabled' type='b' access='readwrite'/><property name='WirelessHardwareEnabled' type='b' access='read'/></interface></node>").unwrap();
        let state = std::rc::Rc::new(std::cell::Cell::new(70u32));
        let state_for_property = state.clone();
        let network = Rc::new(Cell::new(true));
        let wireless = Rc::new(Cell::new(true));
        let writes = Rc::new(Cell::new(0));
        let deny = Rc::new(Cell::new(false));
        let network_property = network.clone();
        let wireless_property = wireless.clone();
        let wireless_set = wireless.clone();
        let wireless_writes = writes.clone();
        let network_set = network.clone();
        let network_writes = writes.clone();
        let deny_request = deny.clone();
        let changed_bool = move |connection: &gio::DBusConnection, property: &str, value: bool| {
            let changed = std::collections::HashMap::from([(property, value.to_variant())]);
            connection
                .emit_signal(
                    None,
                    path,
                    "org.freedesktop.DBus.Properties",
                    "PropertiesChanged",
                    Some(&(name, changed, Vec::<String>::new()).to_variant()),
                )
                .unwrap();
        };
        let registration = connection
            .register_object(path, &info.interfaces()[0])
            .property(move |_, _, _, _, property| {
                if property == "State" {
                    state_for_property.get().to_variant()
                } else if property == "NetworkingEnabled" {
                    network_property.get().to_variant()
                } else if property == "WirelessEnabled" {
                    wireless_property.get().to_variant()
                } else if property == "WirelessHardwareEnabled" {
                    true.to_variant()
                } else {
                    4u32.to_variant()
                }
            })
            .set_property(move |connection, _, _, _, property, value| {
                assert_eq!(property, "WirelessEnabled");
                let enabled = value.get::<bool>().unwrap();
                wireless_set.set(enabled);
                wireless_writes.set(wireless_writes.get() + 1);
                changed_bool(&connection, property, enabled);
                true
            })
            .method_call(move |connection, _, _, _, method, parameters, invocation| {
                assert_eq!(method, "Enable");
                network_writes.set(network_writes.get() + 1);
                if deny_request.get() {
                    invocation.return_dbus_error(
                        "org.freedesktop.NetworkManager.PermissionDenied",
                        "test denial",
                    );
                } else {
                    let enabled = parameters.get::<(bool,)>().unwrap().0;
                    network_set.set(enabled);
                    changed_bool(&connection, "NetworkingEnabled", enabled);
                    invocation.return_value(Some(&().to_variant()));
                }
            })
            .build()
            .unwrap();
        let request_name = || {
            let result = connection
                .call_sync(
                    Some("org.freedesktop.DBus"),
                    "/org/freedesktop/DBus",
                    "org.freedesktop.DBus",
                    "RequestName",
                    Some(&(name, 4u32).to_variant()),
                    None,
                    gio::DBusCallFlags::NONE,
                    1000,
                    gio::Cancellable::NONE,
                )
                .unwrap();
            assert_eq!(result.get::<(u32,)>(), Some((1,)));
        };
        request_name();
        let button = button_for_bus(gio::BusType::Session);
        let wait_label = |expected: &str| {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
            while button.label().as_deref() != Some(expected) {
                while context.pending() {
                    context.iteration(false);
                }
                assert!(
                    std::time::Instant::now() < deadline,
                    "expected {expected}, got {:?}",
                    button.label()
                );
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
        };
        wait_label("Online");
        let content = button.popover().unwrap().child().unwrap();
        let network_switch = content
            .first_child()
            .unwrap()
            .last_child()
            .unwrap()
            .downcast::<gtk::Switch>()
            .unwrap();
        let wireless_switch = content
            .first_child()
            .unwrap()
            .next_sibling()
            .unwrap()
            .last_child()
            .unwrap()
            .downcast::<gtk::Switch>()
            .unwrap();
        let error = content
            .last_child()
            .unwrap()
            .prev_sibling()
            .unwrap()
            .prev_sibling()
            .unwrap()
            .downcast::<gtk::Label>()
            .unwrap();
        let wait = |predicate: &dyn Fn() -> bool| {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
            while !predicate() {
                while context.pending() {
                    context.iteration(false);
                }
                assert!(
                    std::time::Instant::now() < deadline,
                    "network control did not settle"
                );
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
        };
        assert_eq!(
            writes.get(),
            0,
            "initial synchronization must not change networking"
        );
        wireless_switch.set_active(false);
        wait(&|| !wireless.get() && wireless_switch.is_sensitive());
        assert!(!wireless_switch.state());
        deny.set(true);
        network_switch.set_active(false);
        wait(&|| !error.label().is_empty() && network_switch.is_sensitive());
        assert!(
            network.get() && network_switch.is_active(),
            "denial must restore actual state"
        );
        deny.set(false);
        network_switch.set_active(false);
        wait(&|| !network.get() && network_switch.is_sensitive());
        assert!(!wireless_switch.is_sensitive());
        assert!(error.label().is_empty());
        assert_eq!(writes.get(), 3);
        state.set(20);
        let changed = std::collections::HashMap::from([("State", 20u32.to_variant())]);
        connection
            .emit_signal(
                None,
                path,
                "org.freedesktop.DBus.Properties",
                "PropertiesChanged",
                Some(&(name, changed, Vec::<String>::new()).to_variant()),
            )
            .unwrap();
        wait_label("Offline");
        connection
            .call_sync(
                Some("org.freedesktop.DBus"),
                "/org/freedesktop/DBus",
                "org.freedesktop.DBus",
                "ReleaseName",
                Some(&(name,).to_variant()),
                None,
                gio::DBusCallFlags::NONE,
                1000,
                gio::Cancellable::NONE,
            )
            .unwrap();
        wait_label("Network unavailable");
        state.set(70);
        request_name();
        wait_label("Online");
        drop(button);
        connection.unregister_object(registration).unwrap();
    }
    #[test]
    fn connectivity_does_not_override_disconnected_or_unknown_states() {
        assert_eq!(label(20, 4), "Offline");
        assert_eq!(label(40, 2), "Connecting…");
        assert_eq!(label(70, 2), "Sign in to network");
        assert_eq!(label(70, 3), "Limited network");
        assert_eq!(label(50, 0), "Local network");
        assert_eq!(label(60, 0), "Site network");
        assert_eq!(label(70, 4), "Online");
        assert_eq!(label(0, 4), "Network unknown");
    }
}
