//! UPower's aggregate display device, updated by property signals without polling.
use gtk::{gio, prelude::*};
const SERVICE: &str = "org.freedesktop.UPower";
const PATH: &str = "/org/freedesktop/UPower/devices/DisplayDevice";
const IFACE: &str = "org.freedesktop.UPower.Device";

pub fn watch_on_battery(app: &gtk::Application, changed: impl Fn(Option<bool>) + 'static) {
    watch_power(app, gio::BusType::System, changed);
}
fn watch_power(
    app: &gtk::Application,
    bus: gio::BusType,
    changed: impl Fn(Option<bool>) + 'static,
) {
    let app = app.downgrade();
    gio::DBusProxy::for_bus(
        bus,
        gio::DBusProxyFlags::DO_NOT_AUTO_START | gio::DBusProxyFlags::GET_INVALIDATED_PROPERTIES,
        None,
        SERVICE,
        "/org/freedesktop/UPower",
        SERVICE,
        gio::Cancellable::NONE,
        move |result| {
            let Some(app) = app.upgrade() else { return };
            let Ok(proxy) = result else {
                changed(None);
                return;
            };
            let changed = std::rc::Rc::new(changed);
            let update = std::rc::Rc::new(move |proxy: &gio::DBusProxy| {
                changed(
                    proxy
                        .name_owner()
                        .and_then(|_| proxy.cached_property("OnBattery"))
                        .and_then(|v| v.get::<bool>()),
                );
            });
            update(&proxy);
            let properties = update.clone();
            proxy.connect_local("g-properties-changed", false, move |values| {
                if let Ok(proxy) = values[0].get::<gio::DBusProxy>() {
                    properties(&proxy);
                }
                None
            });
            proxy.connect_notify_local(Some("g-name-owner"), move |proxy, _| update(proxy));
            app.connect_shutdown(move |_| {
                let _ = &proxy;
            });
        },
    );
}

fn update(label: &gtk::Label, proxy: &gio::DBusProxy) {
    let present = proxy
        .cached_property("IsPresent")
        .and_then(|v| v.get::<bool>())
        .unwrap_or(false);
    label.set_visible(proxy.name_owner().is_some() && present);
    let percentage = proxy
        .cached_property("Percentage")
        .and_then(|v| v.get::<f64>())
        .filter(|v| v.is_finite() && (0.0..=100.0).contains(v));
    let state = proxy
        .cached_property("State")
        .and_then(|v| v.get::<u32>())
        .unwrap_or(0);
    let description = match state {
        1 => "Charging",
        2 => "Discharging",
        3 => "Empty",
        4 => "Fully charged",
        5 => "Waiting to charge",
        6 => "Waiting to discharge",
        _ => "Battery state unknown",
    };
    let warning = proxy
        .cached_property("WarningLevel")
        .and_then(|v| v.get::<u32>())
        .unwrap_or(0);
    let prefix = if warning >= 4 {
        "Critical battery "
    } else if warning == 3 {
        "Low battery "
    } else if state == 1 {
        "Charging "
    } else {
        ""
    };
    label.set_label(&percentage.map_or_else(
        || "Battery —".into(),
        |value| format!("{prefix}{value:.0}%"),
    ));
    let seconds = proxy
        .cached_property(if state == 1 {
            "TimeToFull"
        } else {
            "TimeToEmpty"
        })
        .and_then(|v| v.get::<i64>())
        .unwrap_or(0);
    let tooltip = if (state == 1 || state == 2) && seconds > 0 {
        let minutes = (seconds / 60).max(1);
        format!(
            "{description} · {}h {}m {}",
            minutes / 60,
            minutes % 60,
            if state == 1 {
                "until full"
            } else {
                "remaining"
            }
        )
    } else {
        description.into()
    };
    label.set_tooltip_text(Some(&tooltip));
}
pub fn widget() -> gtk::Label {
    for_bus(gio::BusType::System)
}
fn for_bus(bus: gio::BusType) -> gtk::Label {
    let label = gtk::Label::new(None);
    label.set_visible(false);
    let weak = label.downgrade();
    gio::DBusProxy::for_bus(
        bus,
        gio::DBusProxyFlags::DO_NOT_AUTO_START | gio::DBusProxyFlags::GET_INVALIDATED_PROPERTIES,
        None,
        SERVICE,
        PATH,
        IFACE,
        gio::Cancellable::NONE,
        move |result| {
            let (Some(label), Ok(proxy)) = (weak.upgrade(), result) else {
                return;
            };
            update(&label, &proxy);
            let weak = label.downgrade();
            proxy.connect_local("g-properties-changed", false, move |values| {
                if let (Some(label), Ok(proxy)) =
                    (weak.upgrade(), values[0].get::<gio::DBusProxy>())
                {
                    update(&label, &proxy);
                }
                None
            });
            let weak = label.downgrade();
            proxy.connect_notify_local(Some("g-name-owner"), move |proxy, _| {
                if let Some(label) = weak.upgrade() {
                    update(&label, proxy);
                }
            });
            label.connect_destroy(move |_| {
                let _ = &proxy;
            });
        },
    );
    label
}

#[cfg(test)]
mod tests {
    use super::*;
    use gtk::glib;
    use std::{
        cell::Cell,
        collections::BTreeMap,
        rc::Rc,
        time::{Duration, Instant},
    };
    #[test]
    #[ignore = "requires private D-Bus and GTK display"]
    fn live_battery_properties_and_restart() {
        assert_eq!(
            std::env::var("WM_NETWORK_TEST_PRIVATE_BUS").as_deref(),
            Ok("1")
        );
        gtk::init().unwrap();
        let context = glib::MainContext::default();
        let _guard = context.acquire().unwrap();
        let connection = gio::bus_get_sync(gio::BusType::Session, gio::Cancellable::NONE).unwrap();
        let name_call = |method: &str, params: glib::Variant| {
            connection
                .call_sync(
                    Some("org.freedesktop.DBus"),
                    "/org/freedesktop/DBus",
                    "org.freedesktop.DBus",
                    method,
                    Some(&params),
                    None,
                    gio::DBusCallFlags::NONE,
                    1000,
                    gio::Cancellable::NONE,
                )
                .unwrap();
        };
        name_call("RequestName", (SERVICE, 4u32).to_variant());
        let reads = Rc::new(Cell::new(0));
        let power_info = gio::DBusNodeInfo::for_xml("<node><interface name='org.freedesktop.UPower'><property name='OnBattery' type='b' access='read'/></interface></node>").unwrap();
        let power_registration = connection
            .register_object("/org/freedesktop/UPower", &power_info.interfaces()[0])
            .property(|_, _, _, _, _| false.to_variant())
            .build()
            .unwrap();
        let power_state = Rc::new(Cell::new(None));
        let observed = power_state.clone();
        let app = gtk::Application::new(
            Some("org.customwm.power.test"),
            gio::ApplicationFlags::NON_UNIQUE,
        );
        watch_power(&app, gio::BusType::Session, move |value| {
            observed.set(Some(value))
        });
        let count = reads.clone();
        let info = gio::DBusNodeInfo::for_xml("<node><interface name='org.freedesktop.UPower.Device'><property name='IsPresent' type='b' access='read'/><property name='Percentage' type='d' access='read'/><property name='State' type='u' access='read'/><property name='WarningLevel' type='u' access='read'/><property name='TimeToFull' type='x' access='read'/><property name='TimeToEmpty' type='x' access='read'/></interface></node>").unwrap();
        let registration = connection
            .register_object(PATH, &info.interfaces()[0])
            .property(move |_, _, _, _, name| {
                count.set(count.get() + 1);
                match name {
                    "IsPresent" => true.to_variant(),
                    "Percentage" => 62.4f64.to_variant(),
                    "State" => 2u32.to_variant(),
                    "WarningLevel" => 0u32.to_variant(),
                    _ => 5400i64.to_variant(),
                }
            })
            .build()
            .unwrap();
        let label = for_bus(gio::BusType::Session);
        let wait = |predicate: &dyn Fn() -> bool| {
            let deadline = Instant::now() + Duration::from_secs(3);
            while !predicate() {
                while context.pending() {
                    context.iteration(false);
                }
                assert!(Instant::now() < deadline, "battery did not settle");
                std::thread::sleep(Duration::from_millis(5));
            }
        };
        let emit = |properties: BTreeMap<&str, glib::Variant>| {
            connection
                .emit_signal(
                    None,
                    PATH,
                    "org.freedesktop.DBus.Properties",
                    "PropertiesChanged",
                    Some(&(IFACE, properties, Vec::<String>::new()).to_variant()),
                )
                .unwrap();
        };
        wait(&|| label.is_visible() && label.label() == "62%");
        wait(&|| power_state.get() == Some(Some(false)));
        connection
            .emit_signal(
                None,
                "/org/freedesktop/UPower",
                "org.freedesktop.DBus.Properties",
                "PropertiesChanged",
                Some(
                    &(
                        SERVICE,
                        BTreeMap::from([("OnBattery", true.to_variant())]),
                        Vec::<String>::new(),
                    )
                        .to_variant(),
                ),
            )
            .unwrap();
        wait(&|| power_state.get() == Some(Some(true)));
        assert_eq!(
            label.tooltip_text().as_deref(),
            Some("Discharging · 1h 30m remaining")
        );
        let initial_reads = reads.get();
        emit(BTreeMap::from([
            ("State", 1u32.to_variant()),
            ("Percentage", 70f64.to_variant()),
        ]));
        wait(&|| label.label() == "Charging 70%");
        assert_eq!(
            label.tooltip_text().as_deref(),
            Some("Charging · 1h 30m until full")
        );
        emit(BTreeMap::from([
            ("WarningLevel", 4u32.to_variant()),
            ("Percentage", 8f64.to_variant()),
        ]));
        wait(&|| label.label() == "Critical battery 8%");
        emit(BTreeMap::from([("WarningLevel", 3u32.to_variant())]));
        wait(&|| label.label() == "Low battery 8%");
        emit(BTreeMap::from([("Percentage", f64::NAN.to_variant())]));
        wait(&|| label.label() == "Battery —");
        emit(BTreeMap::from([("IsPresent", false.to_variant())]));
        wait(&|| !label.is_visible());
        assert_eq!(
            reads.get(),
            initial_reads,
            "property signals do not request snapshots"
        );
        name_call("ReleaseName", (SERVICE,).to_variant());
        wait(&|| power_state.get() == Some(None));
        name_call("RequestName", (SERVICE, 4u32).to_variant());
        wait(&|| label.is_visible() && label.label() == "62%" && reads.get() > initial_reads);
        wait(&|| power_state.get() == Some(Some(false)));
        name_call("ReleaseName", (SERVICE,).to_variant());
        wait(&|| !label.is_visible());
        connection.unregister_object(registration).unwrap();
        connection.unregister_object(power_registration).unwrap();
    }
}
