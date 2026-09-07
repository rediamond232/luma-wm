//! Wi-Fi discovery from NetworkManager's object snapshot, refreshed by signals.
use glib::variant::ObjectPath;
use gtk::{gio, glib, prelude::*};
use std::{
    cell::{Cell, RefCell},
    collections::HashMap,
    rc::Rc,
};

const NM: &str = "org.freedesktop.NetworkManager";
const DEVICE: &str = "org.freedesktop.NetworkManager.Device";
const WIRELESS: &str = "org.freedesktop.NetworkManager.Device.Wireless";
const AP: &str = "org.freedesktop.NetworkManager.AccessPoint";
const ACTIVE: &str = "org.freedesktop.NetworkManager.Connection.Active";
type Properties = HashMap<String, glib::Variant>;
type Objects = HashMap<ObjectPath, HashMap<String, Properties>>;
type Settings = HashMap<String, Properties>;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Security {
    Open,
    Psk,
    Sae,
    Owe,
    Unsupported,
}

fn security(flags: u32, wpa: u32, rsn: u32) -> Security {
    let keys = wpa | rsn;
    if keys & 0x100 != 0 {
        Security::Psk
    } else if keys & 0x400 != 0 {
        Security::Sae
    } else if keys & 0x800 != 0 {
        Security::Owe
    } else if flags & 1 == 0 && keys == 0 {
        Security::Open
    } else {
        Security::Unsupported
    }
}

fn settings(point: &AccessPoint, password: &str, remember: bool) -> Result<Settings, &'static str> {
    match point.security {
        Security::Unsupported => {
            return Err("Use connection settings for this network's security type");
        }
        Security::Psk
            if !((8..=63).contains(&password.len()) && password.is_ascii()
                || password.len() == 64 && password.bytes().all(|b| b.is_ascii_hexdigit())) =>
        {
            return Err("Enter 8–63 ASCII characters or a 64-digit hexadecimal key");
        }
        Security::Sae if password.is_empty() => return Err("Enter the network password"),
        _ => {}
    }
    let mut settings = HashMap::from([
        (
            "connection".into(),
            HashMap::from([
                (
                    "id".into(),
                    String::from_utf8_lossy(&point.ssid)
                        .to_string()
                        .to_variant(),
                ),
                ("type".into(), "802-11-wireless".to_variant()),
                ("autoconnect".into(), remember.to_variant()),
            ]),
        ),
        (
            "802-11-wireless".into(),
            HashMap::from([
                ("ssid".into(), point.ssid.to_variant()),
                ("mode".into(), "infrastructure".to_variant()),
            ]),
        ),
    ]);
    let key = match point.security {
        Security::Psk => Some("wpa-psk"),
        Security::Sae => Some("sae"),
        Security::Owe => Some("owe"),
        _ => None,
    };
    if let Some(key) = key {
        let mut values = HashMap::from([("key-mgmt".into(), key.to_variant())]);
        if matches!(point.security, Security::Psk | Security::Sae) {
            values.insert("psk".into(), password.to_variant());
        }
        settings.insert("802-11-wireless-security".into(), values);
    }
    Ok(settings)
}

#[derive(Clone, Debug)]
struct AccessPoint {
    device: ObjectPath,
    path: ObjectPath,
    ssid: Vec<u8>,
    strength: u8,
    secured: bool,
    active: bool,
    security: Security,
}

fn access_points(objects: &Objects) -> Vec<AccessPoint> {
    let mut networks = HashMap::<(Vec<u8>, u32, u32, u32), AccessPoint>::new();
    for (device, interfaces) in objects {
        if interfaces
            .get(DEVICE)
            .and_then(|p| p.get("DeviceType"))
            .and_then(|v| v.get::<u32>())
            != Some(2)
        {
            continue;
        }
        let Some(wireless) = interfaces.get(WIRELESS) else {
            continue;
        };
        let active = wireless
            .get("ActiveAccessPoint")
            .and_then(|v| v.get::<ObjectPath>());
        let paths = wireless
            .get("AccessPoints")
            .and_then(|v| v.get::<Vec<ObjectPath>>())
            .unwrap_or_default();
        for path in paths {
            let Some(properties) = objects.get(&path).and_then(|p| p.get(AP)) else {
                continue;
            };
            let ssid = properties
                .get("Ssid")
                .and_then(|v| v.get::<Vec<u8>>())
                .unwrap_or_default();
            if ssid.is_empty() || ssid.len() > 32 {
                continue;
            }
            let flag = |name| {
                properties
                    .get(name)
                    .and_then(|v| v.get::<u32>())
                    .unwrap_or(0)
            };
            let point = AccessPoint {
                device: device.clone(),
                active: active.as_ref() == Some(&path),
                path,
                ssid: ssid.clone(),
                strength: properties
                    .get("Strength")
                    .and_then(|v| v.get::<u8>())
                    .unwrap_or(0)
                    .min(100),
                secured: flag("Flags") & 1 != 0 || flag("WpaFlags") != 0 || flag("RsnFlags") != 0,
                security: security(flag("Flags"), flag("WpaFlags"), flag("RsnFlags")),
            };
            let key = (ssid, flag("Flags") & 1, flag("WpaFlags"), flag("RsnFlags"));
            if networks
                .get(&key)
                .is_none_or(|old| (point.active, point.strength) > (old.active, old.strength))
            {
                networks.insert(key, point);
            }
        }
    }
    let mut networks: Vec<_> = networks.into_values().collect();
    networks.sort_by(|a, b| {
        b.active
            .cmp(&a.active)
            .then(b.strength.cmp(&a.strength))
            .then(a.ssid.cmp(&b.ssid))
    });
    networks.truncate(64);
    networks
}

struct Panel {
    root: glib::WeakRef<gtk::Box>,
    list: gtk::Box,
    message: gtk::Label,
    manager: gio::DBusProxy,
    loading: Cell<bool>,
    again: Cell<bool>,
    connecting: Cell<bool>,
    devices: RefCell<Vec<ObjectPath>>,
    scanning: Cell<bool>,
    scan_button: gtk::Button,
    refresh_scheduled: Cell<bool>,
    form: gtk::Box,
    connection_path: RefCell<Option<ObjectPath>>,
    progress: gtk::Label,
}

impl Panel {
    fn connection_state(&self, path: &str, state: u32, reason: u32) {
        if self.connection_path.borrow().as_deref().map(|p| p.as_ref()) != Some(path) {
            return;
        }
        match state {
            1 => self.progress.set_label("Connecting…"),
            2 => {
                self.progress.set_label("Connected to Wi-Fi");
            }
            3 => self.progress.set_label("Disconnecting…"),
            4 => {
                let message = match reason {
                    2 => "Disconnected by request",
                    3 | 14 => "Wi-Fi device disconnected or removed",
                    4 | 7 | 8 => "Network service could not complete the connection",
                    5 => "Could not obtain a valid network address",
                    6 => "Connection timed out",
                    9 => "A password or other credentials are required",
                    10 => "Authentication failed. Check the network credentials",
                    11 => "Connection profile was removed",
                    _ => "Wi-Fi connection failed",
                };
                self.progress.set_label(message);
                self.connection_path.borrow_mut().take();
            }
            _ => {}
        }
    }

    fn watch_connection(self: &Rc<Self>, path: ObjectPath) {
        *self.connection_path.borrow_mut() = Some(path.clone());
        self.progress.set_label("Connecting…");
        let weak = Rc::downgrade(self);
        let owner = self.manager.name_owner();
        self.manager.connection().call(
            Some(NM),
            &path.clone(),
            "org.freedesktop.DBus.Properties",
            "GetAll",
            Some(&(ACTIVE,).to_variant()),
            None,
            gio::DBusCallFlags::NONE,
            5000,
            gio::Cancellable::NONE,
            move |result| {
                let Some(panel) = weak.upgrade() else { return };
                if panel.manager.name_owner() != owner {
                    return;
                }
                if let Ok(result) = result {
                    if let Some((properties,)) = result.get::<(Properties,)>() {
                        if let Some(state) = properties.get("State").and_then(|v| v.get::<u32>()) {
                            panel.connection_state(&path, state, 0);
                        }
                    }
                } else if panel.connection_path.borrow().as_ref() == Some(&path) {
                    panel
                        .progress
                        .set_label("Could not read Wi-Fi connection status");
                }
            },
        );
    }
    fn clear_form(&self) {
        while let Some(child) = self.form.first_child() {
            if let Some(entry) = child.downcast_ref::<gtk::PasswordEntry>() {
                entry.set_text("");
            }
            self.form.remove(&child);
        }
    }

    fn new_connection(self: &Rc<Self>, point: AccessPoint) {
        self.clear_form();
        if point.security == Security::Unsupported {
            return;
        }
        let title = gtk::Label::new(Some(&format!(
            "Connect to {}",
            String::from_utf8_lossy(&point.ssid)
        )));
        title.set_wrap(true);
        title.set_max_width_chars(34);
        self.form.append(&title);
        let password = gtk::PasswordEntry::builder().show_peek_icon(true).build();
        password.update_property(&[gtk::accessible::Property::Label("Wi-Fi password")]);
        let needs_password = matches!(point.security, Security::Psk | Security::Sae);
        password.set_visible(needs_password);
        self.form.append(&password);
        let remember = gtk::CheckButton::with_label("Remember this network");
        remember.set_active(true);
        self.form.append(&remember);
        let connect = gtk::Button::with_label("Connect");
        self.form.append(&connect);
        let weak = Rc::downgrade(self);
        if needs_password {
            password.grab_focus();
        }
        connect.connect_clicked(move |button| {
            let Some(panel) = weak.upgrade() else { return };
            if !panel.usable() || panel.connecting.get() { return; }
            let settings = match settings(&point, password.text().as_str(), remember.is_active()) {
                Ok(settings) => settings,
                Err(error) => { panel.message.set_label(error); return; }
            };
            password.set_text("");
            panel.connecting.set(true); button.set_sensitive(false); panel.list.set_sensitive(false);
            panel.connection_path.borrow_mut().take(); panel.progress.set_label("");
            panel.message.set_label("Requesting a connection…");
            let options = HashMap::from([("persist", if remember.is_active() { "disk" } else { "volatile" }.to_variant())]);
            let parameters = (settings, point.device.clone(), point.path.clone(), options).to_variant();
            let weak = Rc::downgrade(&panel); let button = button.downgrade();
            panel.manager.call("AddAndActivateConnection2", Some(&parameters), gio::DBusCallFlags::NONE, 15_000,
                gio::Cancellable::NONE, move |result| {
                    let Some(panel) = weak.upgrade() else { return };
                    panel.connecting.set(false); panel.list.set_sensitive(panel.usable());
                    if let Some(button) = button.upgrade() { button.set_sensitive(true); }
                    match result {
                        Ok(reply) => {
                            panel.message.set_label("");
                            panel.clear_form();
                            if let Some((_, active, _)) = reply.get::<(ObjectPath, ObjectPath, Properties)>() { panel.watch_connection(active); }
                            else { panel.progress.set_label("NetworkManager returned an invalid connection response"); }
                        }
                        Err(_) => panel.message.set_label("Could not create the connection. Check the password and authorization, then try again."),
                    }
                });
        });
    }
    fn queue_refresh(self: &Rc<Self>) {
        if !self.root.upgrade().is_some_and(|root| root.is_mapped())
            || self.refresh_scheduled.replace(true)
        {
            return;
        }
        let weak = Rc::downgrade(self);
        glib::timeout_add_local_once(std::time::Duration::from_millis(100), move || {
            if let Some(panel) = weak.upgrade() {
                panel.refresh_scheduled.set(false);
                panel.refresh();
            }
        });
    }
    fn scan(self: &Rc<Self>) {
        if !self.usable() || self.scanning.get() {
            return;
        }
        let devices = self.devices.borrow().clone();
        if devices.is_empty() {
            self.message.set_label("No Wi-Fi adapter is available");
            return;
        }
        self.scanning.set(true);
        self.scan_button.set_sensitive(false);
        self.message.set_label("Requesting a Wi-Fi scan…");
        let remaining = Rc::new(Cell::new(devices.len()));
        let failed = Rc::new(Cell::new(false));
        for device in devices {
            let weak = Rc::downgrade(self);
            let remaining = remaining.clone();
            let failed = failed.clone();
            let owner = self.manager.name_owner();
            let options = HashMap::<String, glib::Variant>::new();
            self.manager.connection().call(
                Some(NM),
                &device,
                WIRELESS,
                "RequestScan",
                Some(&(options,).to_variant()),
                None,
                gio::DBusCallFlags::NONE,
                5000,
                gio::Cancellable::NONE,
                move |result| {
                    let Some(panel) = weak.upgrade() else { return };
                    if panel.manager.name_owner() == owner {
                        if let Err(error) = result {
                            failed.set(true);
                            panel
                                .message
                                .set_label(&format!("Could not request Wi-Fi scan: {error}"));
                        }
                    } else {
                        failed.set(true);
                    }
                    remaining.set(remaining.get() - 1);
                    if remaining.get() == 0 {
                        panel.scanning.set(false);
                        panel
                            .scan_button
                            .set_sensitive(panel.usable() && !panel.devices.borrow().is_empty());
                        if !failed.get() {
                            panel
                                .message
                                .set_label("Scan requested. Results update as networks are found.");
                        }
                    }
                },
            );
        }
    }
    fn usable(&self) -> bool {
        self.manager.name_owner().is_some()
            && ["NetworkingEnabled", "WirelessEnabled"].iter().all(|name| {
                self.manager
                    .cached_property(name)
                    .and_then(|value| value.get::<bool>())
                    == Some(true)
            })
    }
    fn refresh(self: &Rc<Self>) {
        if !self.root.upgrade().is_some_and(|root| root.is_mapped()) {
            return;
        }
        if self.loading.replace(true) {
            self.again.set(true);
            return;
        }
        let weak = Rc::downgrade(self);
        let owner = self.manager.name_owner();
        self.manager.connection().call(
            Some(NM),
            "/org/freedesktop",
            "org.freedesktop.DBus.ObjectManager",
            "GetManagedObjects",
            None,
            None,
            gio::DBusCallFlags::NONE,
            5000,
            gio::Cancellable::NONE,
            move |result| {
                let Some(panel) = weak.upgrade() else { return };
                panel.loading.set(false);
                if panel.manager.name_owner() != owner {
                    panel.refresh();
                    return;
                }
                match result.and_then(|v| {
                    v.get::<(Objects,)>().map(|v| v.0).ok_or_else(|| {
                        glib::Error::new(
                            gio::IOErrorEnum::InvalidData,
                            "Invalid Wi-Fi device response",
                        )
                    })
                }) {
                    Ok(objects) => {
                        let mut devices: Vec<_> = objects
                            .iter()
                            .filter(|(_, interfaces)| {
                                interfaces.contains_key(WIRELESS)
                                    && interfaces
                                        .get(DEVICE)
                                        .and_then(|p| p.get("DeviceType"))
                                        .and_then(|v| v.get::<u32>())
                                        == Some(2)
                            })
                            .map(|(path, _)| path.clone())
                            .collect();
                        devices.sort();
                        devices.truncate(16);
                        *panel.devices.borrow_mut() = devices;
                        panel.render(access_points(&objects));
                    }
                    Err(error) => {
                        panel.devices.borrow_mut().clear();
                        panel.scan_button.set_sensitive(false);
                        while let Some(child) = panel.list.first_child() {
                            panel.list.remove(&child);
                        }
                        panel
                            .message
                            .set_label(&format!("Could not list Wi-Fi networks: {error}"));
                    }
                }
                if panel.again.replace(false) {
                    panel.refresh();
                }
            },
        );
    }

    fn render(self: &Rc<Self>, networks: Vec<AccessPoint>) {
        self.scan_button.set_sensitive(
            self.usable() && !self.scanning.get() && !self.devices.borrow().is_empty(),
        );
        self.list
            .set_sensitive(!self.connecting.get() && self.usable());
        while let Some(child) = self.list.first_child() {
            self.list.remove(&child);
        }
        if !self.connecting.get() && !self.scanning.get() {
            self.message.set_label(if networks.is_empty() {
                "No visible Wi-Fi networks"
            } else {
                "Select a network to use its saved connection"
            });
        }
        for point in networks {
            let name: String = String::from_utf8_lossy(&point.ssid)
                .chars()
                .map(|c| if c.is_control() { '�' } else { c })
                .collect();
            let button = gtk::Button::new();
            let label = gtk::Label::new(Some(&format!(
                "{name}  · {}% · {}",
                point.strength,
                if point.active {
                    "Connected"
                } else if point.secured {
                    "Protected"
                } else {
                    "Open"
                }
            )));
            label.set_ellipsize(gtk::pango::EllipsizeMode::End);
            label.set_max_width_chars(34);
            label.set_xalign(0.0);
            button.set_child(Some(&label));
            button.set_tooltip_text(Some(&name));
            button.set_sensitive(!point.active);
            let weak = Rc::downgrade(self);
            button.connect_clicked(move |_| {
                let Some(panel) = weak.upgrade() else { return };
                if !panel.usable() || panel.connecting.replace(true) { return; }
                panel.clear_form();
                panel.connection_path.borrow_mut().take(); panel.progress.set_label("");
                panel.list.set_sensitive(false);
                panel.message.set_label("Connecting with saved settings…");
                let parameters = (ObjectPath::try_from("/").unwrap(), point.device.clone(), point.path.clone()).to_variant();
                let weak = Rc::downgrade(&panel);
                let point = point.clone();
                panel.manager.call("ActivateConnection", Some(&parameters), gio::DBusCallFlags::NONE, 15_000,
                    gio::Cancellable::NONE, move |result| {
                        let Some(panel) = weak.upgrade() else { return };
                        panel.connecting.set(false); panel.list.set_sensitive(panel.usable());
                        match result {
                            Ok(reply) => {
                                panel.message.set_label("");
                                if let Some((active,)) = reply.get::<(ObjectPath,)>() { panel.watch_connection(active); }
                                else { panel.progress.set_label("NetworkManager returned an invalid connection response"); }
                            },
                            Err(error) => {
                                panel.message.set_label(&format!("Could not use a saved connection. Configure this network below or open connection settings. {error}"));
                                panel.new_connection(point);
                            },
                        }
                    });
            });
            self.list.append(&button);
        }
    }
}

pub fn panel(manager: &gio::DBusProxy) -> gtk::Box {
    panel_inner(manager).0
}

fn panel_inner(manager: &gio::DBusProxy) -> (gtk::Box, Rc<Panel>) {
    let root = gtk::Box::new(gtk::Orientation::Vertical, 8);
    let list = gtk::Box::new(gtk::Orientation::Vertical, 4);
    let message = gtk::Label::new(Some("Open this menu to load Wi-Fi networks"));
    message.set_wrap(true);
    message.set_max_width_chars(36);
    message.set_xalign(0.0);
    let scroll = gtk::ScrolledWindow::builder()
        .max_content_height(240)
        .propagate_natural_height(true)
        .hscrollbar_policy(gtk::PolicyType::Never)
        .child(&list)
        .build();
    root.append(&message);
    let progress = gtk::Label::new(None);
    progress.set_wrap(true);
    progress.set_max_width_chars(36);
    progress.set_xalign(0.0);
    root.append(&progress);
    let form = gtk::Box::new(gtk::Orientation::Vertical, 6);
    root.append(&form);
    root.append(&scroll);
    let refresh = gtk::Button::with_label("Scan for networks");
    refresh.set_sensitive(false);
    root.append(&refresh);
    let panel = Rc::new(Panel {
        root: root.downgrade(),
        list,
        message,
        manager: manager.clone(),
        loading: Cell::new(false),
        again: Cell::new(false),
        connecting: Cell::new(false),
        devices: RefCell::new(Vec::new()),
        scanning: Cell::new(false),
        scan_button: refresh.clone(),
        refresh_scheduled: Cell::new(false),
        form,
        connection_path: RefCell::new(None),
        progress,
    });
    let weak = Rc::downgrade(&panel);
    root.connect_map(move |_| {
        if let Some(panel) = weak.upgrade() {
            panel.refresh();
        }
    });
    let weak = Rc::downgrade(&panel);
    root.connect_unmap(move |_| {
        if let Some(panel) = weak.upgrade() {
            panel.clear_form();
        }
    });
    let weak = Rc::downgrade(&panel);
    refresh.connect_clicked(move |_| {
        if let Some(panel) = weak.upgrade() {
            panel.scan();
        }
    });
    let weak = Rc::downgrade(&panel);
    let connection = manager.connection();
    let subscription = connection.subscribe_to_signal(
        Some(NM),
        None,
        None,
        None,
        None,
        gio::DBusSignalFlags::NONE,
        move |signal| {
            let interface = signal.interface_name;
            let parameters = signal.parameters;
            if interface == "org.freedesktop.DBus.ObjectManager"
                && signal.signal_name == "InterfacesRemoved"
            {
                if let (Some(panel), Some((path, interfaces))) = (
                    weak.upgrade(),
                    parameters.get::<(ObjectPath, Vec<String>)>(),
                ) {
                    if interfaces.iter().any(|name| name == ACTIVE) {
                        panel.connection_state(&path, 4, 0);
                    }
                }
            }
            if interface == ACTIVE && signal.signal_name == "StateChanged" {
                if let (Some(panel), Some((state, reason))) =
                    (weak.upgrade(), parameters.get::<(u32, u32)>())
                {
                    panel.connection_state(signal.object_path, state, reason);
                }
            }
            let relevant = interface == "org.freedesktop.DBus.ObjectManager"
                || interface == WIRELESS
                || (signal.signal_name == "PropertiesChanged"
                    && parameters
                        .child_value(0)
                        .get::<String>()
                        .is_some_and(|s| s == WIRELESS || s == AP || s == NM));
            if relevant {
                if let Some(panel) = weak.upgrade() {
                    panel.queue_refresh();
                }
            }
        },
    );
    let weak = Rc::downgrade(&panel);
    manager.connect_notify_local(Some("g-name-owner"), move |_, _| {
        if let Some(panel) = weak.upgrade() {
            if panel.connection_path.borrow_mut().take().is_some() {
                panel
                    .progress
                    .set_label("Network service restarted; connection status is unavailable");
            }
            panel.clear_form();
            while let Some(child) = panel.list.first_child() {
                panel.list.remove(&child);
            }
            panel.devices.borrow_mut().clear();
            panel.scan_button.set_sensitive(false);
            panel.refresh();
        }
    });
    let retained = panel.clone();
    root.connect_destroy(move |_| {
        let _ = (&retained, &subscription);
    });
    (root, panel)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    fn path(value: &str) -> ObjectPath {
        ObjectPath::try_from(value).unwrap()
    }
    fn fixture() -> Objects {
        HashMap::from([
            (
                path("/device"),
                HashMap::from([
                    (
                        DEVICE.into(),
                        HashMap::from([("DeviceType".into(), 2u32.to_variant())]),
                    ),
                    (
                        WIRELESS.into(),
                        HashMap::from([
                            ("ActiveAccessPoint".into(), path("/ap1").to_variant()),
                            (
                                "AccessPoints".into(),
                                vec![path("/ap1"), path("/ap2"), path("/ap3")].to_variant(),
                            ),
                        ]),
                    ),
                ]),
            ),
            (
                path("/ap1"),
                HashMap::from([(
                    AP.into(),
                    HashMap::from([
                        ("Ssid".into(), b"Saved".to_vec().to_variant()),
                        ("Strength".into(), 40u8.to_variant()),
                        ("Flags".into(), 1u32.to_variant()),
                    ]),
                )]),
            ),
            (
                path("/ap2"),
                HashMap::from([(
                    AP.into(),
                    HashMap::from([
                        ("Ssid".into(), b"Saved".to_vec().to_variant()),
                        ("Strength".into(), 90u8.to_variant()),
                        ("Flags".into(), 1u32.to_variant()),
                    ]),
                )]),
            ),
            (
                path("/ap3"),
                HashMap::from([(
                    AP.into(),
                    HashMap::from([
                        ("Ssid".into(), b"Guest".to_vec().to_variant()),
                        ("Strength".into(), 75u8.to_variant()),
                    ]),
                )]),
            ),
        ])
    }

    #[test]
    fn discovery_prefers_connected_ap_and_preserves_open_network() {
        let networks = access_points(&fixture());
        assert_eq!(networks.len(), 2);
        assert_eq!(networks[0].path, path("/ap1"));
        assert!(networks[0].active && networks[0].secured);
        assert_eq!(networks[1].ssid, b"Guest");
        assert!(!networks[1].secured);
    }

    #[test]
    fn profile_security_and_password_validation() {
        assert_eq!(security(1, 0, 0x100), Security::Psk);
        assert_eq!(security(1, 0, 0x400), Security::Sae);
        assert_eq!(security(1, 0, 0x800), Security::Owe);
        assert_eq!(security(1, 0, 0x200), Security::Unsupported);
        assert_eq!(security(1, 0, 0), Security::Unsupported);
        let mut point = access_points(&fixture()).remove(1);
        assert!(
            !settings(&point, "", false)
                .unwrap()
                .contains_key("802-11-wireless-security")
        );
        point.security = Security::Psk;
        assert!(settings(&point, "short", true).is_err());
        assert!(settings(&point, &"g".repeat(64), true).is_err());
        assert!(settings(&point, &"a".repeat(64), true).is_ok());
        let profile = settings(&point, "test-password", true).unwrap();
        assert_eq!(
            profile["802-11-wireless"]["ssid"].get::<Vec<u8>>().unwrap(),
            point.ssid
        );
        assert_eq!(
            profile["802-11-wireless-security"]["key-mgmt"].str(),
            Some("wpa-psk")
        );
        point.security = Security::Sae;
        assert!(settings(&point, "x", true).is_ok());
        assert!(settings(&point, "", true).is_err());
    }

    #[test]
    #[ignore = "requires a private test D-Bus and GTK display"]
    fn live_discovery_activation_and_changes() {
        assert_eq!(
            std::env::var("WM_NETWORK_TEST_PRIVATE_BUS").as_deref(),
            Ok("1")
        );
        gtk::init().unwrap();
        let context = glib::MainContext::default();
        let _guard = context.acquire().unwrap();
        let connection = gio::bus_get_sync(gio::BusType::Session, gio::Cancellable::NONE).unwrap();
        connection
            .call_sync(
                Some("org.freedesktop.DBus"),
                "/org/freedesktop/DBus",
                "org.freedesktop.DBus",
                "RequestName",
                Some(&(NM, 4u32).to_variant()),
                None,
                gio::DBusCallFlags::NONE,
                1000,
                gio::Cancellable::NONE,
            )
            .unwrap();
        let objects = Rc::new(RefCell::new(fixture()));
        let response = objects.clone();
        let snapshots = Rc::new(Cell::new(0));
        let snapshot_calls = snapshots.clone();
        let xml = gio::DBusNodeInfo::for_xml("<node><interface name='org.freedesktop.DBus.ObjectManager'><method name='GetManagedObjects'><arg type='a{oa{sa{sv}}}' direction='out'/></method></interface></node>").unwrap();
        let object_registration = connection
            .register_object("/org/freedesktop", &xml.interfaces()[0])
            .method_call(move |_, _, _, _, _, _, invocation| {
                snapshot_calls.set(snapshot_calls.get() + 1);
                invocation.return_value(Some(&(response.borrow().clone(),).to_variant()));
            })
            .build()
            .unwrap();
        let calls = Rc::new(Cell::new(0));
        let called = calls.clone();
        let additions = Rc::new(Cell::new(0));
        let added = additions.clone();
        let xml = gio::DBusNodeInfo::for_xml("<node><interface name='org.freedesktop.NetworkManager'><method name='ActivateConnection'><arg type='o' direction='in'/><arg type='o' direction='in'/><arg type='o' direction='in'/><arg type='o' direction='out'/></method><method name='AddAndActivateConnection2'><arg type='a{sa{sv}}' direction='in'/><arg type='o' direction='in'/><arg type='o' direction='in'/><arg type='a{sv}' direction='in'/><arg type='o' direction='out'/><arg type='o' direction='out'/><arg type='a{sv}' direction='out'/></method></interface></node>").unwrap();
        let registration = connection
            .register_object("/org/freedesktop/NetworkManager", &xml.interfaces()[0])
            .method_call(move |_, _, _, _, method, parameters, invocation| {
                if method == "AddAndActivateConnection2" {
                    let (profile, device, ap, options) = parameters
                        .get::<(Settings, ObjectPath, ObjectPath, Properties)>()
                        .unwrap();
                    assert_eq!((device, ap), (path("/device"), path("/ap3")));
                    assert_eq!(
                        profile["802-11-wireless"]["ssid"].get::<Vec<u8>>().unwrap(),
                        b"Guest"
                    );
                    if added.get() == 0 {
                        assert!(!profile.contains_key("802-11-wireless-security"));
                        assert_eq!(options["persist"].str(), Some("volatile"));
                    } else {
                        assert_eq!(
                            profile["802-11-wireless-security"]["psk"].str(),
                            Some("test-password")
                        );
                        assert_eq!(options["persist"].str(), Some("disk"));
                    }
                    added.set(added.get() + 1);
                    if added.get() == 2 {
                        invocation.return_dbus_error(
                            "org.freedesktop.NetworkManager.PermissionDenied",
                            "test denial",
                        );
                        return;
                    }
                    invocation.return_value(Some(
                        &(path("/profile"), path("/active"), Properties::new()).to_variant(),
                    ));
                    return;
                }
                assert_eq!(method, "ActivateConnection");
                assert_eq!(
                    parameters.get::<(ObjectPath, ObjectPath, ObjectPath)>(),
                    Some((path("/"), path("/device"), path("/ap3")))
                );
                called.set(called.get() + 1);
                invocation.return_dbus_error(
                    "org.freedesktop.NetworkManager.UnknownConnection",
                    "no saved connection",
                );
            })
            .build()
            .unwrap();
        let manager = gio::DBusProxy::for_bus_sync(
            gio::BusType::Session,
            gio::DBusProxyFlags::DO_NOT_LOAD_PROPERTIES,
            None,
            NM,
            "/org/freedesktop/NetworkManager",
            NM,
            gio::Cancellable::NONE,
        )
        .unwrap();
        manager.set_cached_property("NetworkingEnabled", Some(&true.to_variant()));
        manager.set_cached_property("WirelessEnabled", Some(&true.to_variant()));
        let (root, panel) = panel_inner(&manager);
        let active_state = Rc::new(Cell::new(1u32));
        let read_active_state = active_state.clone();
        let active_info = gio::DBusNodeInfo::for_xml("<node><interface name='org.freedesktop.NetworkManager.Connection.Active'><property name='State' type='u' access='read'/></interface></node>").unwrap();
        let active_registration = connection
            .register_object("/active", &active_info.interfaces()[0])
            .property(move |_, _, _, _, _| read_active_state.get().to_variant())
            .build()
            .unwrap();
        let scan_calls = Rc::new(Cell::new(0));
        let scanned = scan_calls.clone();
        let scan_info = gio::DBusNodeInfo::for_xml("<node><interface name='org.freedesktop.NetworkManager.Device.Wireless'><method name='RequestScan'><arg type='a{sv}' direction='in'/></method></interface></node>").unwrap();
        let scan_registration = connection
            .register_object("/device", &scan_info.interfaces()[0])
            .method_call(move |connection, _, _, _, method, parameters, invocation| {
                assert_eq!(method, "RequestScan");
                assert_eq!(
                    parameters
                        .get::<(HashMap<String, glib::Variant>,)>()
                        .unwrap()
                        .0
                        .len(),
                    0
                );
                scanned.set(scanned.get() + 1);
                if scanned.get() == 1 {
                    invocation.return_value(Some(&().to_variant()));
                    let changed = HashMap::from([("LastScan", 1234i64.to_variant())]);
                    connection
                        .emit_signal(
                            None,
                            "/device",
                            "org.freedesktop.DBus.Properties",
                            "PropertiesChanged",
                            Some(&(WIRELESS, changed, Vec::<String>::new()).to_variant()),
                        )
                        .unwrap();
                } else {
                    invocation.return_dbus_error(
                        "org.freedesktop.NetworkManager.Device.NotAllowed",
                        "scan rate limited",
                    );
                }
            })
            .build()
            .unwrap();
        let input_test = std::env::var_os("WM_NETWORK_TEST_INPUT").is_some();
        let window = gtk::Window::new();
        let menu = gtk::MenuButton::new();
        let popover = gtk::Popover::new();
        if input_test {
            crate::css(&wm_core::Config::load().unwrap());
            use gtk4_layer_shell::{Edge, Layer, LayerShell};
            window.init_layer_shell();
            window.set_layer(Layer::Top);
            window.set_namespace(Some("wm-network-input-test"));
            window.set_anchor(Edge::Top, true);
            window.set_anchor(Edge::Left, true);
            window.set_default_size(360, 40);
            menu.set_label("Test network");
            popover.set_child(Some(&root));
            menu.set_popover(Some(&popover));
            crate::network::popover_keyboard_focus(&menu, &popover);
            window.set_child(Some(&menu));
        } else {
            window.set_child(Some(&root));
        }
        window.present();
        let wait = |predicate: &dyn Fn() -> bool| {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(4);
            while !predicate() {
                while context.pending() {
                    context.iteration(false);
                }
                assert!(
                    std::time::Instant::now() < deadline,
                    "Wi-Fi panel did not settle"
                );
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
        };
        if input_test {
            wait(&|| window.is_mapped() && menu.width() > 0);
            let deadline = std::time::Instant::now() + std::time::Duration::from_millis(200);
            while std::time::Instant::now() < deadline {
                while context.pending() {
                    context.iteration(false);
                }
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            let status = std::process::Command::new("xdotool")
                .env("DISPLAY", std::env::var("WM_TEST_HOST_DISPLAY").unwrap())
                .args([
                    "mousemove",
                    "--window",
                    &std::env::var("WM_TEST_HOST_WINDOW").unwrap(),
                    "100",
                    "20",
                    "click",
                    "1",
                ])
                .status()
                .unwrap();
            assert!(status.success());
        }
        wait(&|| panel.list.first_child().is_some());
        panel.scan_button.emit_clicked();
        panel.scan(); // Direct re-entry must also be suppressed while pending.
        wait(&|| scan_calls.get() == 1 && !panel.scanning.get());
        assert!(panel.scan_button.is_sensitive());
        panel.scan_button.emit_clicked();
        wait(&|| scan_calls.get() == 2 && !panel.scanning.get());
        assert!(panel.scan_button.is_sensitive());
        assert!(
            panel
                .message
                .label()
                .contains("Could not request Wi-Fi scan")
        );
        let first = panel.list.first_child().unwrap();
        assert!(!first.is_sensitive());
        let guest = first
            .next_sibling()
            .unwrap()
            .downcast::<gtk::Button>()
            .unwrap();
        guest.emit_clicked();
        wait(&|| calls.get() == 1 && !panel.connecting.get());
        assert!(
            panel
                .message
                .label()
                .contains("Could not use a saved connection")
        );
        assert!(panel.list.is_sensitive() && guest.is_sensitive());
        let password = panel
            .form
            .first_child()
            .unwrap()
            .next_sibling()
            .unwrap()
            .downcast::<gtk::PasswordEntry>()
            .unwrap();
        assert!(!password.is_visible());
        let remember = password
            .next_sibling()
            .unwrap()
            .downcast::<gtk::CheckButton>()
            .unwrap();
        remember.set_active(false);
        panel
            .form
            .last_child()
            .unwrap()
            .downcast::<gtk::Button>()
            .unwrap()
            .emit_clicked();
        wait(&|| additions.get() == 1 && !panel.connecting.get());
        assert_eq!(panel.progress.label(), "Connecting…");
        active_state.set(4);
        connection
            .emit_signal(
                None,
                "/active",
                ACTIVE,
                "StateChanged",
                Some(&(4u32, 10u32).to_variant()),
            )
            .unwrap();
        wait(&|| panel.progress.label().contains("Authentication failed"));
        assert!(panel.connection_path.borrow().is_none());
        active_state.set(1);
        assert!(panel.form.first_child().is_none());
        let mut personal = access_points(&fixture()).remove(1);
        personal.security = Security::Psk;
        panel.new_connection(personal);
        let password = panel
            .form
            .first_child()
            .unwrap()
            .next_sibling()
            .unwrap()
            .downcast::<gtk::PasswordEntry>()
            .unwrap();
        assert!(password.is_visible());
        if input_test {
            use gtk4_layer_shell::{KeyboardMode, LayerShell};
            wait(&|| password.is_mapped() && password.width() > 0);
            assert_eq!(window.keyboard_mode(), KeyboardMode::OnDemand);
            let bounds = password
                .compute_bounds(&window)
                .expect("password bounds in bar coordinates");
            let x = (bounds.x() + bounds.width() / 2.0).round() as i32;
            let y = (bounds.y() + bounds.height() / 2.0).round() as i32;
            let host_window = std::env::var("WM_TEST_HOST_WINDOW").unwrap();
            let status = std::process::Command::new("xdotool")
                .env("DISPLAY", std::env::var("WM_TEST_HOST_DISPLAY").unwrap())
                .args([
                    "mousemove",
                    "--window",
                    &host_window,
                    &x.to_string(),
                    &y.to_string(),
                    "click",
                    "1",
                    "type",
                    "--clearmodifiers",
                    "--delay",
                    "20",
                    "test-password",
                ])
                .status()
                .unwrap();
            assert!(status.success());
            wait(&|| password.text() == "test-password");
            if let Some(path) = std::env::var_os("WM_NETWORK_TEST_SCREENSHOT") {
                let status = std::process::Command::new("import")
                    .env("DISPLAY", std::env::var("WM_TEST_HOST_DISPLAY").unwrap())
                    .args(["-window", &host_window])
                    .arg(path)
                    .status()
                    .unwrap();
                assert!(status.success());
            }
        } else {
            password.set_text("test-password");
        }
        panel
            .form
            .last_child()
            .unwrap()
            .downcast::<gtk::Button>()
            .unwrap()
            .emit_clicked();
        assert!(
            password.text().is_empty(),
            "submitted password must leave the entry immediately"
        );
        wait(&|| additions.get() == 2 && !panel.connecting.get());
        assert!(
            panel
                .message
                .label()
                .contains("Could not create the connection")
        );
        assert!(password.text().is_empty());
        let retry = panel
            .form
            .last_child()
            .unwrap()
            .downcast::<gtk::Button>()
            .unwrap();
        assert!(retry.is_sensitive());
        password.set_text("test-password");
        retry.emit_clicked();
        wait(&|| additions.get() == 3 && !panel.connecting.get());
        active_state.set(2);
        connection
            .emit_signal(
                None,
                "/active",
                ACTIVE,
                "StateChanged",
                Some(&(2u32, 1u32).to_variant()),
            )
            .unwrap();
        wait(&|| panel.progress.label() == "Connected to Wi-Fi");
        assert_eq!(*panel.connection_path.borrow(), Some(path("/active")));
        assert!(panel.form.first_child().is_none());
        objects.borrow_mut().remove(&path("/ap3"));
        let before = snapshots.get();
        for _ in 0..20 {
            connection
                .emit_signal(
                    None,
                    "/device",
                    WIRELESS,
                    "AccessPointRemoved",
                    Some(&(path("/ap3"),).to_variant()),
                )
                .unwrap();
        }
        wait(&|| {
            panel
                .list
                .first_child()
                .is_some_and(|first| first.next_sibling().is_none())
        });
        assert_eq!(
            snapshots.get(),
            before + 1,
            "signal burst should share one snapshot"
        );
        assert_eq!(
            panel.progress.label(),
            "Connected to Wi-Fi",
            "list refresh must preserve connection result"
        );
        let mut personal = access_points(&fixture()).remove(1);
        personal.security = Security::Psk;
        panel.new_connection(personal);
        let password = panel
            .form
            .first_child()
            .unwrap()
            .next_sibling()
            .unwrap()
            .downcast::<gtk::PasswordEntry>()
            .unwrap();
        password.set_text("test-password");
        window.set_visible(false);
        if input_test {
            use gtk4_layer_shell::{KeyboardMode, LayerShell};
            assert_eq!(window.keyboard_mode(), KeyboardMode::None);
        }
        assert!(
            password.text().is_empty(),
            "closing the panel must clear password text"
        );
        let before = snapshots.get();
        connection
            .emit_signal(
                None,
                "/device",
                WIRELESS,
                "AccessPointAdded",
                Some(&(path("/ap3"),).to_variant()),
            )
            .unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(200);
        while std::time::Instant::now() < deadline {
            while context.pending() {
                context.iteration(false);
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert_eq!(
            snapshots.get(),
            before,
            "hidden panel must not load snapshots"
        );
        connection
            .emit_signal(
                None,
                "/active",
                ACTIVE,
                "StateChanged",
                Some(&(4u32, 2u32).to_variant()),
            )
            .unwrap();
        wait(&|| panel.progress.label() == "Disconnected by request");
        assert!(panel.connection_path.borrow().is_none());
        assert_eq!(
            snapshots.get(),
            before,
            "connection signals do not require a snapshot"
        );
        window.close();
        connection.unregister_object(scan_registration).unwrap();
        connection.unregister_object(registration).unwrap();
        connection.unregister_object(object_registration).unwrap();
        connection.unregister_object(active_registration).unwrap();
        if input_test {
            std::fs::write(std::env::var("WM_NETWORK_TEST_RECEIPT").unwrap(), "ok").unwrap();
        }
    }
}
