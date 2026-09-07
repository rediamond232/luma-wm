use super::*;
use std::collections::{BTreeMap, VecDeque};
const XML: &str = r#"<node><interface name="org.freedesktop.Notifications">
<method name="GetCapabilities"><arg direction="out" type="as"/></method>
<method name="GetServerInformation"><arg direction="out" type="s"/><arg direction="out" type="s"/><arg direction="out" type="s"/><arg direction="out" type="s"/></method>
<method name="Notify"><arg direction="in" type="s"/><arg direction="in" type="u"/><arg direction="in" type="s"/><arg direction="in" type="s"/><arg direction="in" type="s"/><arg direction="in" type="as"/><arg direction="in" type="a{sv}"/><arg direction="in" type="i"/><arg direction="out" type="u"/></method>
<method name="CloseNotification"><arg direction="in" type="u"/></method>
<signal name="NotificationClosed"><arg type="u"/><arg type="u"/></signal><signal name="ActionInvoked"><arg type="u"/><arg type="s"/></signal>
</interface></node>"#;
#[derive(Default)]
pub struct Center {
    history: Rc<RefCell<VecDeque<(String, String)>>>,
    dnd: std::cell::Cell<bool>,
    window: RefCell<Option<glib::WeakRef<gtk::ApplicationWindow>>>,
    list: RefCell<Option<glib::WeakRef<gtk::Box>>>,
}
struct Popup {
    widget: gtk::Box,
    timer: Option<glib::SourceId>,
}
struct Popups {
    window: gtk::ApplicationWindow,
    stack: gtk::Box,
    items: BTreeMap<u32, Popup>,
}
impl Popups {
    fn new(app: &gtk::Application) -> Self {
        let window = layer_window(app, None, Layer::Overlay, "wm-notifications");
        window.set_anchor(Edge::Top, true);
        window.set_anchor(Edge::Right, true);
        window.set_margin(Edge::Top, 48);
        window.set_margin(Edge::Right, 12);
        window.set_default_size(350, -1);
        let stack = gtk::Box::new(gtk::Orientation::Vertical, 8);
        let height = monitors()
            .first()
            .map(|m| (m.geometry().height() - 120).clamp(100, 480))
            .unwrap_or(480);
        let scroll = gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::Never)
            .max_content_height(height)
            .propagate_natural_height(true)
            .child(&stack)
            .build();
        window.set_child(Some(&scroll));
        Self {
            window,
            stack,
            items: BTreeMap::new(),
        }
    }
    fn remove(&mut self, id: u32) -> bool {
        let Some(popup) = self.items.remove(&id) else {
            return false;
        };
        if let Some(timer) = popup.timer {
            timer.remove();
        }
        self.stack.remove(&popup.widget);
        if self.items.is_empty() {
            self.window.set_visible(false);
        }
        true
    }
}
fn close_popup(
    popups: &Rc<RefCell<Popups>>,
    connection: &gio::DBusConnection,
    id: u32,
    reason: u32,
) {
    if popups.borrow_mut().remove(id) {
        emit(connection, "NotificationClosed", (id, reason).to_variant());
    }
}
impl Center {
    fn refresh(&self) {
        let Some(list) = self.list.borrow().as_ref().and_then(|l| l.upgrade()) else {
            return;
        };
        while let Some(child) = list.first_child() {
            list.remove(&child);
        }
        if self.history.borrow().is_empty() {
            list.append(&gtk::Label::new(Some("No notifications yet")));
        }
        for (summary, body) in self.history.borrow().iter().rev() {
            let item = gtk::Box::new(gtk::Orientation::Vertical, 4);
            item.add_css_class("notification");
            for (text, bold) in [(summary, true), (body, false)] {
                let label = gtk::Label::new(Some(text));
                label.set_wrap(true);
                label.set_xalign(0.);
                label.set_max_width_chars(48);
                if bold {
                    label.add_css_class("title");
                }
                item.append(&label);
            }
            list.append(&item);
        }
    }
    pub fn show(self: &Rc<Self>, app: &gtk::Application) {
        if let Some(window) = self.window.borrow().as_ref().and_then(|w| w.upgrade()) {
            window.present();
            return;
        }
        let window = layer_window(app, None, Layer::Overlay, "wm-notification-center");
        window.set_keyboard_mode(KeyboardMode::Exclusive);
        window.set_default_size(480, 520);
        let root = gtk::Box::new(gtk::Orientation::Vertical, 12);
        root.add_css_class("launcher");
        let controls = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        let dnd = gtk::CheckButton::with_label("Do Not Disturb");
        dnd.set_active(self.dnd.get());
        let center = self.clone();
        dnd.connect_toggled(move |b| center.dnd.set(b.is_active()));
        controls.append(&dnd);
        let clear = gtk::Button::with_label("Clear history");
        let center = self.clone();
        clear.connect_clicked(move |_| {
            center.history.borrow_mut().clear();
            center.refresh();
        });
        controls.append(&clear);
        let close = gtk::Button::with_label("Close");
        let weak = window.downgrade();
        close.connect_clicked(move |_| {
            if let Some(w) = weak.upgrade() {
                w.close();
            }
        });
        controls.append(&close);
        root.append(&controls);
        let list = gtk::Box::new(gtk::Orientation::Vertical, 8);
        let scroll = gtk::ScrolledWindow::builder()
            .vexpand(true)
            .child(&list)
            .build();
        root.append(&scroll);
        window.set_child(Some(&root));
        *self.window.borrow_mut() = Some(window.downgrade());
        *self.list.borrow_mut() = Some(list.downgrade());
        self.refresh();
        let keys = gtk::EventControllerKey::new();
        keys.set_propagation_phase(gtk::PropagationPhase::Capture);
        let weak = window.downgrade();
        keys.connect_key_pressed(move |_, key, _, _| {
            if key == gdk::Key::Escape {
                if let Some(w) = weak.upgrade() {
                    w.close();
                }
                glib::Propagation::Stop
            } else {
                glib::Propagation::Proceed
            }
        });
        window.add_controller(keys);
        window.present();
    }
}
pub fn start(app: &gtk::Application) -> Rc<Center> {
    let center = Rc::new(Center::default());
    let server = center.clone();
    let app = app.clone();
    gio::bus_own_name(
        gio::BusType::Session,
        "org.freedesktop.Notifications",
        gio::BusNameOwnerFlags::NONE,
        move |connection, _| {
            let info = gio::DBusNodeInfo::for_xml(XML)
                .unwrap()
                .lookup_interface("org.freedesktop.Notifications")
                .unwrap();
            let id = Rc::new(RefCell::new(0u32));
            let windows = Rc::new(RefCell::new(Popups::new(&app)));
            let history = server.history.clone();
            let center = server.clone();
            let _ = connection
                .register_object("/org/freedesktop/Notifications", &info)
                .method_call(
                    move |conn, _, _, _, method, params, invocation| match method {
                        "GetCapabilities" => {
                            invocation.return_value(Some(&(vec!["body", "actions"],).to_variant()))
                        }
                        "GetServerInformation" => invocation.return_value(Some(
                            &("wm-shell", "customwm", "0.1.0", "1.2").to_variant(),
                        )),
                        "CloseNotification" => {
                            let n = params.child_get::<u32>(0);
                            close_popup(&windows, &conn, n, 3);
                            invocation.return_value(Some(&().to_variant()));
                        }
                        "Notify" => {
                            let replace = params.child_get::<u32>(1);
                            let summary: String =
                                params.child_get::<String>(3).chars().take(256).collect();
                            let body: String =
                                params.child_get::<String>(4).chars().take(8192).collect();
                            let actions = params.child_get::<Vec<String>>(5);
                            let timeout = params.child_get::<i32>(7);
                            let n = if replace != 0 && windows.borrow().items.contains_key(&replace)
                            {
                                replace
                            } else {
                                let mut counter = id.borrow_mut();
                                loop {
                                    *counter = counter.wrapping_add(1).max(1);
                                    if !windows.borrow().items.contains_key(&counter) {
                                        break *counter;
                                    }
                                }
                            };
                            windows.borrow_mut().remove(n);
                            {
                                let mut h = history.borrow_mut();
                                h.push_back((summary.clone(), body.clone()));
                                while h.len() > 100 {
                                    h.pop_front();
                                }
                            }
                            center.refresh();
                            if center.dnd.get() {
                                invocation.return_value(Some(&(n,).to_variant()));
                                emit(&conn, "NotificationClosed", (n, 1u32).to_variant());
                                return;
                            }
                            let oldest = {
                                let popups = windows.borrow();
                                (popups.items.len() >= 3)
                                    .then(|| *popups.items.keys().next().unwrap())
                            };
                            if let Some(oldest) = oldest {
                                close_popup(&windows, &conn, oldest, 1);
                            }
                            let root = gtk::Box::new(gtk::Orientation::Vertical, 6);
                            root.add_css_class("notification");
                            let title = gtk::Label::new(Some(&summary));
                            title.add_css_class("title");
                            title.set_xalign(0.);
                            title.set_wrap(true);
                            title.set_lines(2);
                            title.set_max_width_chars(40);
                            title.set_ellipsize(gtk::pango::EllipsizeMode::End);
                            let text = gtk::Label::new(Some(&body));
                            text.set_wrap(true);
                            text.set_xalign(0.);
                            text.set_max_width_chars(45);
                            text.set_lines(3);
                            text.set_ellipsize(gtk::pango::EllipsizeMode::End);
                            root.append(&title);
                            root.append(&text);
                            let row = gtk::FlowBox::new();
                            row.set_selection_mode(gtk::SelectionMode::None);
                            row.set_max_children_per_line(3);
                            for pair in actions
                                .chunks_exact(2)
                                .filter(|p| p[0].len() <= 1024)
                                .take(4)
                            {
                                let label: String = pair[1].chars().take(40).collect();
                                let b = gtk::Button::with_label(&label);
                                let key = pair[0].clone();
                                let conn = conn.clone();
                                let windows = windows.clone();
                                b.connect_clicked(move |_| {
                                    emit(&conn, "ActionInvoked", (n, key.as_str()).to_variant());
                                    close_popup(&windows, &conn, n, 2);
                                });
                                row.insert(&b, -1);
                            }
                            let close = gtk::Button::with_label("Dismiss");
                            let ws = windows.clone();
                            let cn = conn.clone();
                            close.connect_clicked(move |_| {
                                close_popup(&ws, &cn, n, 2);
                            });
                            row.insert(&close, -1);
                            root.append(&row);
                            {
                                let mut popups = windows.borrow_mut();
                                popups.stack.append(&root);
                                popups.items.insert(
                                    n,
                                    Popup {
                                        widget: root,
                                        timer: None,
                                    },
                                );
                                popups.window.present();
                            }
                            if timeout != 0 {
                                let ws = windows.clone();
                                let cn = conn.clone();
                                let timer = glib::timeout_add_local_once(
                                    std::time::Duration::from_millis(if timeout < 0 {
                                        5000
                                    } else {
                                        timeout as u64
                                    }),
                                    move || {
                                        if let Some(popup) = ws.borrow_mut().items.get_mut(&n) {
                                            popup.timer.take();
                                        }
                                        close_popup(&ws, &cn, n, 1);
                                    },
                                );
                                windows.borrow_mut().items.get_mut(&n).unwrap().timer = Some(timer);
                            }
                            invocation.return_value(Some(&(n,).to_variant()));
                        }
                        _ => invocation.return_dbus_error(
                            "org.freedesktop.DBus.Error.UnknownMethod",
                            "Unknown notification method",
                        ),
                    },
                )
                .build();
        },
        |_, _| {},
        |_, _| eprintln!("Notification service unavailable: another session may own it"),
    );
    center
}
fn emit(c: &gio::DBusConnection, name: &str, value: glib::Variant) {
    let _ = c.emit_signal(
        None,
        "/org/freedesktop/Notifications",
        "org.freedesktop.Notifications",
        name,
        Some(&value),
    );
}
