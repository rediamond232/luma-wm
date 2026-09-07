//! StatusNotifier watcher registry. The tray host/UI is a separate component.
use gtk::{gio, glib, prelude::*};
use std::{
    cell::{Cell, RefCell},
    collections::BTreeMap,
    rc::Rc,
};
const NAME: &str = "org.kde.StatusNotifierWatcher";
const PATH: &str = "/StatusNotifierWatcher";
const XML: &str = r#"<node><interface name="org.kde.StatusNotifierWatcher">
<method name="RegisterStatusNotifierItem"><arg type="s" direction="in"/></method>
<method name="RegisterStatusNotifierHost"><arg type="s" direction="in"/></method>
<property name="RegisteredStatusNotifierItems" type="as" access="read"/>
<property name="IsStatusNotifierHostRegistered" type="b" access="read"/>
<property name="ProtocolVersion" type="i" access="read"/>
<signal name="StatusNotifierItemRegistered"><arg type="s"/></signal>
<signal name="StatusNotifierItemUnregistered"><arg type="s"/></signal>
<signal name="StatusNotifierHostRegistered"/><signal name="StatusNotifierHostUnregistered"/>
</interface></node>"#;
#[derive(Default)]
struct Registry {
    items: BTreeMap<String, (String, String)>,
    hosts: BTreeMap<String, (String, String)>,
    pending: BTreeMap<u64, (String, String)>,
}
fn emit(connection: &gio::DBusConnection, signal: &str, id: Option<&str>) {
    let args = id.map_or_else(|| ().to_variant(), |id| (id,).to_variant());
    let _ = connection.emit_signal(None, PATH, NAME, signal, Some(&args));
}
pub fn start(app: &gtk::Application) {
    let weak = app.downgrade();
    gio::bus_get(
        gio::BusType::Session,
        gio::Cancellable::NONE,
        move |result| {
            let (Some(app), Ok(connection)) = (weak.upgrade(), result) else {
                return;
            };
            let registry = Rc::new(RefCell::new(Registry::default()));
            let properties = registry.clone();
            let methods = registry.clone();
            let sequence = Cell::new(0u64);
            let info = gio::DBusNodeInfo::for_xml(XML).unwrap();
            let registration = connection
                .register_object(PATH, &info.interfaces()[0])
                .property(move |_, _, _, _, property| match property {
                    "RegisteredStatusNotifierItems" => properties
                        .borrow()
                        .items
                        .keys()
                        .cloned()
                        .collect::<Vec<_>>()
                        .to_variant(),
                    "IsStatusNotifierHostRegistered" => {
                        (!properties.borrow().hosts.is_empty()).to_variant()
                    }
                    _ => 0i32.to_variant(),
                })
                .method_call(move |connection, sender, _, _, method, args, invocation| {
                    let host = method == "RegisterStatusNotifierHost";
                    let Some((value,)) = args.get::<(String,)>() else {
                        return;
                    };
                    let owner = sender.unwrap_or_default().to_string();
                    let (service, path) = if value.starts_with('/') && !host {
                        if glib::variant::ObjectPath::try_from(value.as_str()).is_err() {
                            invocation.return_dbus_error(
                                "org.freedesktop.DBus.Error.InvalidArgs",
                                "Invalid tray object path",
                            );
                            return;
                        }
                        (owner.clone(), value)
                    } else if gio::dbus_is_name(&value) {
                        (value, "/StatusNotifierItem".into())
                    } else {
                        invocation.return_dbus_error(
                            "org.freedesktop.DBus.Error.InvalidArgs",
                            "Invalid tray service name",
                        );
                        return;
                    };
                    if methods.borrow().pending.len() >= 64 {
                        invocation.return_dbus_error(
                            "org.freedesktop.DBus.Error.LimitsExceeded",
                            "Too many pending tray registrations",
                        );
                        return;
                    }
                    let request = sequence.get().wrapping_add(1);
                    sequence.set(request);
                    methods
                        .borrow_mut()
                        .pending
                        .insert(request, (service.clone(), owner.clone()));
                    let registry = methods.clone();
                    let retained = connection.clone();
                    connection.call(
                        Some("org.freedesktop.DBus"),
                        "/org/freedesktop/DBus",
                        "org.freedesktop.DBus",
                        "GetNameOwner",
                        Some(&(service.clone(),).to_variant()),
                        None,
                        gio::DBusCallFlags::NONE,
                        2000,
                        gio::Cancellable::NONE,
                        move |result| {
                            let mut registry = registry.borrow_mut();
                            let valid = registry.pending.remove(&request).is_some()
                                && result
                                    .ok()
                                    .and_then(|v| v.get::<(String,)>())
                                    .is_some_and(|(actual,)| actual == owner);
                            if !valid {
                                invocation.return_dbus_error(
                                    "org.freedesktop.DBus.Error.AccessDenied",
                                    "Tray service is not owned by the caller",
                                );
                                return;
                            }
                            let id = if host {
                                service.clone()
                            } else {
                                format!("{service}{path}")
                            };
                            let entries = if host {
                                &mut registry.hosts
                            } else {
                                &mut registry.items
                            };
                            if entries.contains_key(&id) {
                                invocation.return_value(Some(&().to_variant()));
                                return;
                            }
                            if entries.len() >= 64 {
                                invocation.return_dbus_error(
                                    "org.freedesktop.DBus.Error.LimitsExceeded",
                                    "Too many tray registrations",
                                );
                                return;
                            }
                            let first = entries.is_empty();
                            entries.insert(id.clone(), (service, owner));
                            drop(registry);
                            if !host {
                                emit(&retained, "StatusNotifierItemRegistered", Some(&id));
                            } else if first {
                                emit(&retained, "StatusNotifierHostRegistered", None);
                            }
                            invocation.return_value(Some(&().to_variant()));
                        },
                    );
                })
                .build();
            let Ok(registration) = registration else {
                return;
            };
            let changes = registry.clone();
            let retained = connection.clone();
            let subscription = connection.subscribe_to_signal(
                Some("org.freedesktop.DBus"),
                Some("org.freedesktop.DBus"),
                Some("NameOwnerChanged"),
                Some("/org/freedesktop/DBus"),
                None,
                gio::DBusSignalFlags::NONE,
                move |signal| {
                    let Some((name, old, new)) =
                        signal.parameters.get::<(String, String, String)>()
                    else {
                        return;
                    };
                    if old.is_empty() || old == new {
                        return;
                    }
                    let mut registry = changes.borrow_mut();
                    let gone = |service: &str, owner: &str| {
                        service == name || (name.starts_with(':') && owner == old)
                    };
                    registry
                        .pending
                        .retain(|_, (service, owner)| !gone(service, owner));
                    let removed: Vec<_> = registry
                        .items
                        .iter()
                        .filter(|(_, (service, owner))| gone(service, owner))
                        .map(|(id, _)| id.clone())
                        .collect();
                    for id in &removed {
                        registry.items.remove(id);
                    }
                    let had_hosts = !registry.hosts.is_empty();
                    registry
                        .hosts
                        .retain(|_, (service, owner)| !gone(service, owner));
                    let lost_hosts = had_hosts && registry.hosts.is_empty();
                    drop(registry);
                    for id in removed {
                        emit(&retained, "StatusNotifierItemUnregistered", Some(&id));
                    }
                    if lost_hosts {
                        emit(&retained, "StatusNotifierHostUnregistered", None);
                    }
                },
            );
            let owner = gio::bus_own_name_on_connection(
                &connection,
                NAME,
                gio::BusNameOwnerFlags::NONE,
                |_, _| {},
                |_, _| {},
            );
            let cleanup = RefCell::new(Some((registration, owner, subscription)));
            app.connect_shutdown(move |_| {
                if let Some((registration, owner, subscription)) = cleanup.borrow_mut().take() {
                    drop(subscription);
                    gio::bus_unown_name(owner);
                    let _ = connection.unregister_object(registration);
                }
            });
        },
    );
}
