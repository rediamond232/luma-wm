//! On-demand com.canonical.dbusmenu popover. Only the visible menu page is read.
use gtk::{gdk, gdk_pixbuf, gio, glib, prelude::*};
use std::{
    cell::{Cell, RefCell},
    collections::BTreeMap,
    rc::Rc,
};

const INTERFACE: &str = "com.canonical.dbusmenu";
type Layout = (i32, BTreeMap<String, glib::Variant>, Vec<glib::Variant>);
const PROPERTIES: &[&str] = &[
    "label",
    "type",
    "enabled",
    "visible",
    "toggle-type",
    "toggle-state",
    "children-display",
    "icon-name",
    "icon-data",
    "shortcut",
];
type Pixels = (i32, i32, usize, Vec<u8>);

pub(crate) struct Menu {
    button: glib::WeakRef<gtk::Button>,
    item: gio::DBusProxy,
    popover: gtk::Popover,
    content: gtk::Box,
    proxy: RefCell<Option<gio::DBusProxy>>,
    theme: RefCell<crate::tray::IconThemeCache>,
    property_signal: RefCell<Option<gio::SignalSubscription>>,
    parents: RefCell<Vec<i32>>,
    generation: Cell<u64>,
    busy: Cell<bool>,
    again: Cell<bool>,
}
impl Menu {
    pub(crate) fn new(button: &gtk::Button, item: &gio::DBusProxy) -> Rc<Self> {
        let content = gtk::Box::new(gtk::Orientation::Vertical, 2);
        content.set_size_request(220, -1);
        let scrolled = gtk::ScrolledWindow::builder()
            .child(&content)
            .hscrollbar_policy(gtk::PolicyType::Never)
            .propagate_natural_height(true)
            .max_content_height(480)
            .min_content_width(220)
            .build();
        let popover = gtk::Popover::builder().child(&scrolled).build();
        popover.set_parent(button);
        crate::network::popover_keyboard_focus(button, &popover);
        let menu = Rc::new(Self {
            button: button.downgrade(),
            item: item.clone(),
            popover,
            content,
            proxy: RefCell::new(None),
            theme: RefCell::new(crate::tray::IconThemeCache::default()),
            property_signal: RefCell::new(None),
            parents: RefCell::new(vec![0]),
            generation: Cell::new(0),
            busy: Cell::new(false),
            again: Cell::new(false),
        });
        let weak = Rc::downgrade(&menu);
        menu.popover.connect_closed(move |_| {
            if let Some(menu) = weak.upgrade() {
                menu.generation.set(menu.generation.get().wrapping_add(1));
                menu.proxy.borrow_mut().take();
                menu.property_signal.borrow_mut().take();
                menu.busy.set(false);
                menu.again.set(false);
            }
        });
        let weak = Rc::downgrade(&menu);
        item.connect_notify_local(Some("g-name-owner"), move |_, _| {
            if let Some(menu) = weak.upgrade() {
                menu.popover.popdown();
            }
        });
        let retained = menu.clone();
        button.connect_destroy(move |_| {
            retained.popover.popdown();
            retained.popover.unparent();
        });
        menu
    }
    /// False means the item has no usable exported menu; use SNI ContextMenu.
    pub(crate) fn show(self: &Rc<Self>) -> bool {
        let Some(path) = self
            .item
            .cached_property("Menu")
            .and_then(|v| v.get::<glib::variant::ObjectPath>())
        else {
            return false;
        };
        if path.as_str() == "/" {
            return false;
        }
        let Some(owner) = self.item.name_owner() else {
            return false;
        };
        if self.popover.is_visible() {
            return true;
        }
        self.parents.replace(vec![0]);
        self.message("Loading…");
        self.popover.popup();
        let generation = self.generation.get();
        let weak = Rc::downgrade(self);
        gio::DBusProxy::new(
            &self.item.connection(),
            gio::DBusProxyFlags::DO_NOT_AUTO_START | gio::DBusProxyFlags::DO_NOT_LOAD_PROPERTIES,
            None,
            Some(&owner),
            path.as_str(),
            INTERFACE,
            gio::Cancellable::NONE,
            move |result| {
                let Some(menu) = weak.upgrade().filter(|m| m.generation.get() == generation) else {
                    return;
                };
                match result {
                    Ok(proxy) => {
                        let weak = Rc::downgrade(&menu);
                        proxy.connect_local("g-signal", false, move |values| {
                            let name = values[2].get::<String>().unwrap_or_default();
                            if matches!(name.as_str(), "LayoutUpdated" | "ItemsPropertiesUpdated") {
                                if let Some(menu) = weak.upgrade() {
                                    menu.load(false);
                                }
                            }
                            None
                        });
                        let weak = Rc::downgrade(&menu);
                        let id = proxy.connection().subscribe_to_signal(
                            proxy.name().as_deref(),
                            Some("org.freedesktop.DBus.Properties"),
                            Some("PropertiesChanged"),
                            Some(proxy.object_path().as_str()),
                            Some(INTERFACE),
                            gio::DBusSignalFlags::NONE,
                            move |signal| {
                                let args = signal.parameters;
                                if args.size() > 65536 {
                                    return;
                                }
                                if let Some((_, changed, invalidated)) = args.get::<(
                                    String,
                                    BTreeMap<String, glib::Variant>,
                                    Vec<String>,
                                )>(
                                ) {
                                    if changed.contains_key("IconThemePath")
                                        || invalidated.iter().any(|s| s == "IconThemePath")
                                    {
                                        if let Some(menu) = weak.upgrade() {
                                            menu.load(false);
                                        }
                                    }
                                }
                            },
                        );
                        menu.property_signal.replace(Some(id));
                        menu.proxy.replace(Some(proxy));
                        menu.load(true);
                    }
                    Err(_) => menu.message("Menu unavailable"),
                }
            },
        );
        true
    }
    fn clear(&self) {
        while let Some(child) = self.content.first_child() {
            self.content.remove(&child);
        }
    }
    fn message(&self, message: &str) {
        self.clear();
        self.content.append(&gtk::Label::new(Some(message)));
    }
    fn load(self: &Rc<Self>, announce: bool) {
        if !self.popover.is_visible() {
            return;
        }
        if self.busy.replace(true) {
            self.again.set(true);
            return;
        }
        let Some(proxy) = self.proxy.borrow().clone() else {
            self.busy.set(false);
            return;
        };
        let parent = *self.parents.borrow().last().unwrap_or(&0);
        let generation = self.generation.get();
        let weak = Rc::downgrade(self);
        let mut child = self.content.first_child();
        let mut focused = None;
        while let Some(widget) = child {
            if widget.has_focus() {
                focused = Some(widget.widget_name());
            }
            child = widget.next_sibling();
        }
        self.content.set_sensitive(false);
        glib::MainContext::default().spawn_local(async move {
            if announce {
                // Some implementations omit this optional preparation method.
                let _ = proxy
                    .call_future(
                        "AboutToShow",
                        Some(&(parent,).to_variant()),
                        gio::DBusCallFlags::NONE,
                        2000,
                    )
                    .await;
            }
            let Some(menu) = weak.upgrade().filter(|m| m.generation.get() == generation) else {
                return;
            };
            drop(menu);
            let theme_paths = proxy
                .call_future(
                    "org.freedesktop.DBus.Properties.Get",
                    Some(&(INTERFACE, "IconThemePath").to_variant()),
                    gio::DBusCallFlags::NONE,
                    2000,
                )
                .await
                .ok()
                .filter(|v| v.size() <= 65536)
                .and_then(|v| v.get::<(glib::Variant,)>())
                .and_then(|(v,)| v.get::<Vec<String>>())
                .unwrap_or_default();
            if weak
                .upgrade()
                .filter(|m| m.generation.get() == generation)
                .is_none()
            {
                return;
            }
            let result = proxy
                .call_future(
                    "GetLayout",
                    Some(&(parent, 1i32, PROPERTIES).to_variant()),
                    gio::DBusCallFlags::NONE,
                    2000,
                )
                .await;
            let Some(menu) = weak.upgrade().filter(|m| m.generation.get() == generation) else {
                return;
            };
            drop(menu);
            let rows = result.ok().and_then(|v| parse_layout(&v, parent));
            // One worker per page keeps image decoding out of the input loop.
            let icons = if let Some(rows) = rows.as_ref() {
                let data = rows
                    .iter()
                    .map(|(_, properties, _)| {
                        properties
                            .get("icon-data")
                            .and_then(|v| v.get::<Vec<u8>>())
                            .unwrap_or_default()
                    })
                    .collect::<Vec<_>>();
                if data.iter().any(|bytes| !bytes.is_empty()) {
                    gio::spawn_blocking(move || {
                        data.into_iter().map(decode_icon).collect::<Vec<_>>()
                    })
                    .await
                    .ok()
                } else {
                    None
                }
            } else {
                None
            };
            let Some(menu) = weak.upgrade().filter(|m| m.generation.get() == generation) else {
                return;
            };
            let weak = Rc::downgrade(&menu);
            let refresh: Rc<dyn Fn()> = Rc::new(move || {
                if let Some(menu) = weak.upgrade() {
                    menu.load(false);
                }
            });
            let mut theme = menu.theme.borrow_mut();
            theme.update_paths(theme_paths);
            theme.watch(refresh);
            drop(theme);
            menu.busy.set(false);
            menu.content.set_sensitive(true);
            match rows {
                Some(rows) => menu.render(rows, icons.unwrap_or_default(), focused),
                None => menu.message("Menu unavailable"),
            }
            if menu.again.replace(false) {
                menu.load(false);
            }
        });
    }
    fn render(
        self: &Rc<Self>,
        rows: Vec<Layout>,
        icons: Vec<Option<Pixels>>,
        focused: Option<glib::GString>,
    ) {
        self.clear();
        if self.parents.borrow().len() > 1 {
            let back = gtk::Button::with_label("‹ Back");
            back.set_widget_name("tray-menu-back");
            let weak = Rc::downgrade(self);
            back.connect_clicked(move |_| {
                if let Some(menu) = weak.upgrade() {
                    menu.parents.borrow_mut().pop();
                    menu.load(true);
                }
            });
            self.content.append(&back);
        }
        let mut icons = icons.into_iter();
        for (id, properties, _) in rows {
            let pixels = icons.next().flatten();
            let flag = |key: &str| {
                properties
                    .get(key)
                    .and_then(|v| v.get::<bool>())
                    .unwrap_or(true)
            };
            if !flag("visible") {
                continue;
            }
            if string(&properties, "type") == "separator" {
                self.content
                    .append(&gtk::Separator::new(gtk::Orientation::Horizontal));
                continue;
            }
            let submenu = string(&properties, "children-display") == "submenu";
            let label = string(&properties, "label");
            let row = gtk::Button::new();
            row.set_widget_name(&format!("tray-menu-{id}"));
            let line = gtk::Box::new(gtk::Orientation::Horizontal, 8);
            let name = string(&properties, "icon-name");
            let custom = self
                .theme
                .borrow()
                .theme
                .clone()
                .filter(|theme| !name.is_empty() && theme.has_icon(&name));
            let themed = gdk::Display::default().is_some_and(|display| {
                !name.is_empty() && gtk::IconTheme::for_display(&display).has_icon(&name)
            });
            if custom.is_some() || themed || pixels.is_some() {
                let image = gtk::Image::new();
                image.set_pixel_size(16);
                if let Some(theme) = custom {
                    image.set_paintable(Some(&theme.lookup_icon(
                        &name,
                        &[],
                        16,
                        self.content.scale_factor(),
                        image.direction(),
                        gtk::IconLookupFlags::empty(),
                    )));
                } else if themed {
                    image.set_icon_name(Some(&name));
                } else if let Some((width, height, stride, bytes)) = pixels {
                    image.set_paintable(Some(&gdk::MemoryTexture::new(
                        width,
                        height,
                        gdk::MemoryFormat::R8g8b8a8,
                        &glib::Bytes::from_owned(bytes),
                        stride,
                    )));
                }
                line.append(&image);
            }
            let toggle = string(&properties, "toggle-type");
            if matches!(toggle.as_str(), "checkmark" | "radio") {
                let state = properties
                    .get("toggle-state")
                    .and_then(|v| v.get::<i32>())
                    .unwrap_or(-1);
                let mark = gtk::CheckButton::new();
                if toggle == "radio" {
                    mark.add_css_class("radio");
                }
                mark.set_active(state == 1);
                mark.set_inconsistent(state != 0 && state != 1);
                mark.set_can_target(false);
                mark.set_focusable(false);
                line.append(&mark);
            }
            let text = gtk::Label::new(Some(&label));
            text.set_use_underline(true);
            text.set_mnemonic_widget(Some(&row));
            text.set_xalign(0.0);
            text.set_hexpand(true);
            text.set_max_width_chars(48);
            text.set_ellipsize(gtk::pango::EllipsizeMode::End);
            line.append(&text);
            let shortcut = shortcut(&properties);
            if !shortcut.is_empty() {
                let hint = gtk::Label::new(Some(&shortcut));
                hint.add_css_class("dim-label");
                hint.set_xalign(1.0);
                hint.set_max_width_chars(24);
                hint.set_ellipsize(gtk::pango::EllipsizeMode::End);
                line.append(&hint);
            }
            if submenu {
                line.append(&gtk::Label::new(Some("›")));
            }
            row.set_child(Some(&line));
            row.set_sensitive(flag("enabled") && (!submenu || self.parents.borrow().len() < 16));
            let weak = Rc::downgrade(self);
            row.connect_clicked(move |_| {
                if let Some(menu) = weak.upgrade() {
                    if submenu {
                        if !menu.parents.borrow().contains(&id) {
                            menu.parents.borrow_mut().push(id);
                            menu.load(true);
                        }
                    } else {
                        menu.activate(id);
                    }
                }
            });
            self.content.append(&row);
        }
        if self.content.first_child().is_none() {
            self.message("No actions");
        }
        let mut child = self.content.first_child();
        while let Some(widget) = child {
            if focused.as_ref() == Some(&widget.widget_name()) && widget.grab_focus() {
                return;
            }
            child = widget.next_sibling();
        }
        self.content.child_focus(gtk::DirectionType::TabForward);
    }
    fn activate(&self, id: i32) {
        if let Some(proxy) = self.proxy.borrow().as_ref() {
            let weak = self.button.clone();
            let timestamp = (glib::monotonic_time() / 1000) as u32;
            proxy.call(
                "Event",
                Some(&(id, "clicked", 0i32.to_variant(), timestamp).to_variant()),
                gio::DBusCallFlags::NONE,
                2000,
                gio::Cancellable::NONE,
                move |result| {
                    if let (Err(error), Some(button)) = (result, weak.upgrade()) {
                        button.set_tooltip_text(Some(&format!("Menu action failed: {error}")));
                    }
                },
            );
        }
        self.popover.popdown();
    }
}
fn decode_icon(bytes: Vec<u8>) -> Option<Pixels> {
    if bytes.len() < 33
        || bytes.len() > 256 * 1024
        || &bytes[..8] != b"\x89PNG\r\n\x1a\n"
        || bytes[8..12] != 13u32.to_be_bytes()
        || &bytes[12..16] != b"IHDR"
    {
        return None;
    }
    let width = u32::from_be_bytes(bytes[16..20].try_into().ok()?);
    let height = u32::from_be_bytes(bytes[20..24].try_into().ok()?);
    if !(1..=256).contains(&width) || !(1..=256).contains(&height) {
        return None;
    }
    let loader = gdk_pixbuf::PixbufLoader::with_type("png").ok()?;
    let write = loader.write(&bytes);
    let close = loader.close();
    write.ok()?;
    close.ok()?;
    let pixbuf = loader.pixbuf()?.add_alpha(false, 0, 0, 0).ok()?;
    if pixbuf.width() > 256 || pixbuf.height() > 256 {
        return None;
    }
    Some((
        pixbuf.width(),
        pixbuf.height(),
        pixbuf.rowstride() as usize,
        pixbuf.read_pixel_bytes().to_vec(),
    ))
}
fn shortcut(properties: &BTreeMap<String, glib::Variant>) -> String {
    let Some(chords) = properties
        .get("shortcut")
        .and_then(|v| v.get::<Vec<Vec<String>>>())
    else {
        return String::new();
    };
    if chords.len() > 4 {
        return String::new();
    }
    let mut result = Vec::new();
    for chord in chords {
        let Some((key, modifiers)) = chord.split_last() else {
            return String::new();
        };
        if key.is_empty()
            || key.chars().count() > 32
            || key.chars().any(char::is_control)
            || modifiers.len() > 4
            || modifiers
                .iter()
                .any(|m| !matches!(m.as_str(), "Control" | "Alt" | "Shift" | "Super"))
        {
            return String::new();
        }
        let mut keys = modifiers
            .iter()
            .map(|m| if m == "Control" { "Ctrl" } else { m.as_str() })
            .collect::<Vec<_>>();
        keys.push(key);
        result.push(keys.join("+"));
    }
    result.join(", ")
}
fn string(properties: &BTreeMap<String, glib::Variant>, key: &str) -> String {
    properties
        .get(key)
        .and_then(|v| v.str())
        .unwrap_or("")
        .chars()
        .take(512)
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect()
}
fn parse_layout(value: &glib::Variant, parent: i32) -> Option<Vec<Layout>> {
    if value.size() > 1024 * 1024 {
        return None;
    }
    let (_, (id, _, children)) = value.get::<(u32, Layout)>()?;
    if id != parent || children.len() > 128 {
        return None;
    }
    let mut ids = std::collections::BTreeSet::new();
    children
        .into_iter()
        .map(|v| {
            let row = v.get::<Layout>()?;
            if row.0 == parent || !ids.insert(row.0) {
                return None;
            }
            Some(row)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use gtk4_layer_shell::{Edge, KeyboardMode, Layer, LayerShell};

    fn png(width: i32, height: i32) -> Vec<u8> {
        let pixels =
            gdk_pixbuf::Pixbuf::new(gdk_pixbuf::Colorspace::Rgb, true, 8, width, height).unwrap();
        pixels.fill(0x20b450ff);
        pixels.save_to_bufferv("png", &[]).unwrap()
    }
    #[test]
    fn menu_icons_and_shortcuts_are_bounded() {
        let (width, height, stride, bytes) = decode_icon(png(16, 16)).unwrap();
        assert_eq!((width, height), (16, 16));
        assert!(stride >= 64);
        assert_eq!(&bytes[..4], &[32, 180, 80, 255]);
        assert!(decode_icon(png(257, 1)).is_none());
        assert!(decode_icon(vec![0; 256 * 1024 + 1]).is_none());
        let mut broken = png(16, 16);
        broken.truncate(33);
        assert!(decode_icon(broken).is_none());
        let properties = |chords: Vec<Vec<&str>>| {
            BTreeMap::from([("shortcut".to_string(), chords.to_variant())])
        };
        assert_eq!(shortcut(&properties(vec![vec!["Control", "S"]])), "Ctrl+S");
        assert_eq!(
            shortcut(&properties(vec![vec!["Control", "Q"], vec!["Alt", "X"]])),
            "Ctrl+Q, Alt+X"
        );
        assert!(shortcut(&properties(vec![vec!["Unknown", "S"]])).is_empty());
        assert!(shortcut(&properties(vec![vec!["Control", "\n"]])).is_empty());
        assert!(shortcut(&properties(vec![vec!["A"]; 5])).is_empty());
    }

    fn row(id: i32, properties: &[(&str, glib::Variant)]) -> glib::Variant {
        (
            id,
            properties
                .iter()
                .map(|(k, v)| (k.to_string(), v.clone()))
                .collect::<BTreeMap<_, _>>(),
            Vec::<glib::Variant>::new(),
        )
            .to_variant()
    }
    fn layout(parent: i32, rows: Vec<glib::Variant>) -> glib::Variant {
        (
            1u32,
            (parent, BTreeMap::<String, glib::Variant>::new(), rows),
        )
            .to_variant()
    }
    #[test]
    fn layout_validation() {
        let child = row(1, &[("label", "_Open".to_variant())]);
        assert_eq!(
            parse_layout(&layout(0, vec![child.clone()]), 0)
                .unwrap()
                .len(),
            1
        );
        assert!(parse_layout(&layout(0, vec![child.clone(), child.clone()]), 0).is_none());
        assert!(parse_layout(&layout(1, vec![child.clone()]), 1).is_none());
        assert!(parse_layout(&layout(0, vec![child; 129]), 0).is_none());
        assert!(parse_layout(&layout(1, vec![]), 0).is_none());
        assert!(parse_layout(&layout(0, vec![true.to_variant()]), 0).is_none());
        assert!(parse_layout(&false.to_variant(), 0).is_none());
    }

    #[test]
    #[ignore = "requires a private nested compositor, D-Bus and GTK display"]
    fn live_menu_navigation_updates_and_events() {
        assert_eq!(std::env::var("WM_TRAY_MENU_TEST").as_deref(), Ok("1"));
        gtk::init().unwrap();
        crate::tray::verify_icon_cache();
        crate::css(&wm_core::Config::load().unwrap());
        let context = glib::MainContext::default();
        let _guard = context.acquire().unwrap();
        let bus = gio::bus_get_sync(gio::BusType::Session, gio::Cancellable::NONE).unwrap();
        let info = gio::DBusNodeInfo::for_xml("<node><interface name='com.canonical.dbusmenu'><property name='IconThemePath' type='as' access='read'/><method name='AboutToShow'><arg type='i' direction='in'/><arg type='b' direction='out'/></method><method name='GetLayout'><arg type='i' direction='in'/><arg type='i' direction='in'/><arg type='as' direction='in'/><arg type='u' direction='out'/><arg type='(ia{sv}av)' direction='out'/></method><method name='Event'><arg type='i' direction='in'/><arg type='s' direction='in'/><arg type='v' direction='in'/><arg type='u' direction='in'/></method><signal name='LayoutUpdated'><arg type='u'/><arg type='i'/></signal></interface></node>").unwrap();
        let theme_paths = Rc::new(RefCell::new(Vec::<String>::new()));
        let property_paths = theme_paths.clone();
        let reads = Rc::new(Cell::new(0));
        let read_count = reads.clone();
        let delay = Rc::new(Cell::new(0u64));
        let delay_reply = delay.clone();
        let changed = Rc::new(Cell::new(false));
        let changed_reply = changed.clone();
        let events = Rc::new(RefCell::new(Vec::new()));
        let event_reply = events.clone();
        let announcements = Rc::new(RefCell::new(Vec::new()));
        let announced = announcements.clone();
        let icon_data = png(16, 16);
        let registration = bus
            .register_object("/Menu", &info.interfaces()[0])
            .method_call(move |_, _, _, _, method, args, invocation| match method {
                "AboutToShow" => {
                    announced.borrow_mut().push(args.get::<(i32,)>().unwrap().0);
                    invocation.return_value(Some(&(false,).to_variant()));
                }
                "GetLayout" => {
                    let (parent, depth, properties) =
                        args.get::<(i32, i32, Vec<String>)>().unwrap();
                    assert_eq!(depth, 1);
                    assert!(properties.contains(&"enabled".to_string()));
                    read_count.set(read_count.get() + 1);
                    let rows = if parent == 0 {
                        vec![
                            row(
                                1,
                                &[
                                    ("label", "_More".to_variant()),
                                    ("children-display", "submenu".to_variant()),
                                    ("icon-name", "document-open-symbolic".to_variant()),
                                ],
                            ),
                            row(
                                2,
                                &[
                                    ("label", "Disabled".to_variant()),
                                    ("enabled", false.to_variant()),
                                    ("icon-name", "luma-nonexistent-menu-test-icon".to_variant()),
                                    ("icon-data", icon_data.to_variant()),
                                ],
                            ),
                            row(
                                3,
                                &[
                                    ("label", "Hidden".to_variant()),
                                    ("visible", false.to_variant()),
                                ],
                            ),
                            row(
                                4,
                                &[
                                    ("label", "Checked".to_variant()),
                                    ("toggle-type", "checkmark".to_variant()),
                                    ("toggle-state", 1i32.to_variant()),
                                    ("shortcut", vec![vec!["Control", "S"]].to_variant()),
                                ],
                            ),
                        ]
                    } else {
                        assert_eq!(parent, 1);
                        vec![row(
                            5,
                            &[(
                                "label",
                                if changed_reply.get() {
                                    "Updated"
                                } else {
                                    "Launch"
                                }
                                .to_variant(),
                            )],
                        )]
                    };
                    let reply = layout(parent, rows);
                    let delay = delay_reply.get();
                    if delay == 0 {
                        invocation.return_value(Some(&reply));
                    } else {
                        glib::timeout_add_local_once(
                            std::time::Duration::from_millis(delay),
                            move || {
                                invocation.return_value(Some(&reply));
                            },
                        );
                    }
                }
                "Event" => {
                    event_reply
                        .borrow_mut()
                        .push(args.get::<(i32, String, glib::Variant, u32)>().unwrap());
                    invocation.return_value(Some(&().to_variant()));
                }
                _ => unreachable!(),
            })
            .property(move |_, _, _, _, property| {
                assert_eq!(property, "IconThemePath");
                property_paths.borrow().to_variant()
            })
            .build()
            .unwrap();
        let item = context
            .block_on(gio::DBusProxy::new_future(
                &bus,
                gio::DBusProxyFlags::DO_NOT_LOAD_PROPERTIES,
                None,
                bus.unique_name().as_deref(),
                "/Item",
                "org.kde.StatusNotifierItem",
            ))
            .unwrap();
        item.set_cached_property(
            "Menu",
            Some(
                &glib::variant::ObjectPath::try_from("/Menu")
                    .unwrap()
                    .to_variant(),
            ),
        );
        let button = gtk::Button::with_label("Tray menu");
        let window = gtk::Window::new();
        window.init_layer_shell();
        window.set_layer(Layer::Top);
        window.set_anchor(Edge::Top, true);
        window.set_anchor(Edge::Left, true);
        window.set_anchor(Edge::Right, true);
        window.set_keyboard_mode(KeyboardMode::None);
        window.set_default_size(260, 40);
        window.set_child(Some(&button));
        button.set_halign(gtk::Align::End);
        let menu = Menu::new(&button, &item);
        let open = menu.clone();
        button.connect_clicked(move |_| {
            open.show();
        });
        window.present();
        let wait_sequence = Cell::new(0u32);
        let wait = |condition: &dyn Fn() -> bool| {
            let sequence = wait_sequence.get().wrapping_add(1);
            wait_sequence.set(sequence);
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
            while !condition() {
                while context.pending() {
                    context.iteration(false);
                }
                if std::time::Instant::now() >= deadline {
                    let point = button.compute_point(&window, &gtk::graphene::Point::new(0.0, 0.0));
                    let pointer = std::process::Command::new("xdotool")
                        .env("DISPLAY", std::env::var("WM_TEST_HOST_DISPLAY").unwrap())
                        .arg("getmouselocation")
                        .arg("--shell")
                        .output()
                        .ok()
                        .map(|output| String::from_utf8_lossy(&output.stdout).into_owned());
                    eprintln!(
                        "tray-menu wait {sequence}: window={}x{}, button={}x{} at {point:?}, mapped={}, busy={}, reads={}, host pointer={pointer:?}",
                        window.width(),
                        window.height(),
                        button.width(),
                        button.height(),
                        button.is_mapped(),
                        menu.busy.get(),
                        reads.get(),
                    );
                }
                assert!(
                    std::time::Instant::now() < deadline,
                    "menu state did not settle at wait {sequence}"
                );
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
        };
        let pump = || {
            context.block_on(glib::timeout_future(std::time::Duration::from_millis(200)));
        };
        wait(&|| button.is_mapped());
        // Mapping can precede the layer-shell configure that stretches this
        // left/right-anchored window from its 260 px request to the output.
        // Pointer coordinates must use the committed output-width allocation.
        wait(&|| window.width() > 300);
        pump();
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
        input(&[
            "mousemove",
            "--window",
            &std::env::var("WM_TEST_HOST_WINDOW").unwrap(),
            &(window.width() - 30).to_string(),
            "20",
            "click",
            "1",
        ]);
        wait(&|| !menu.busy.get() && reads.get() == 1);
        assert_eq!(window.keyboard_mode(), KeyboardMode::OnDemand);
        let first = menu
            .content
            .first_child()
            .unwrap()
            .downcast::<gtk::Button>()
            .unwrap();
        let disabled = first.next_sibling().unwrap();
        assert!(!disabled.is_sensitive());
        let checked = disabled.next_sibling().unwrap();
        assert!(checked.next_sibling().is_none(), "hidden row was rendered");
        let icon = first
            .child()
            .unwrap()
            .first_child()
            .unwrap()
            .downcast::<gtk::Image>()
            .unwrap();
        assert_eq!(icon.icon_name().as_deref(), Some("document-open-symbolic"));
        let fallback = disabled
            .first_child()
            .unwrap()
            .first_child()
            .unwrap()
            .downcast::<gtk::Image>()
            .unwrap();
        let texture = fallback
            .paintable()
            .unwrap()
            .downcast::<gdk::Texture>()
            .unwrap();
        assert_eq!((texture.width(), texture.height()), (16, 16));
        let hint = checked
            .first_child()
            .unwrap()
            .last_child()
            .unwrap()
            .downcast::<gtk::Label>()
            .unwrap();
        assert_eq!(hint.text(), "Ctrl+S");
        pump();
        if let Ok(path) = std::env::var("WM_TRAY_MENU_SCREENSHOT") {
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
        assert!(
            menu.content.width() >= 220,
            "menu labels must have usable width"
        );
        let surface = menu.popover.native().unwrap().surface().unwrap();
        let popup = surface.clone().downcast::<gdk::Popup>().unwrap();
        assert!(
            popup.position_x() >= 0 && popup.position_x() + surface.width() <= window.width(),
            "bar popup must fit output: x={}, width={}, output={}",
            popup.position_x(),
            surface.width(),
            window.width()
        );
        first.grab_focus();
        pump();
        input(&["key", "Return"]);
        wait(&|| reads.get() == 2 && !menu.busy.get());
        assert_eq!(&*announcements.borrow(), &[0, 1]);
        menu.content.last_child().unwrap().grab_focus();
        changed.set(true);
        bus.emit_signal(
            None,
            "/Menu",
            INTERFACE,
            "LayoutUpdated",
            Some(&(2u32, 1i32).to_variant()),
        )
        .unwrap();
        wait(&|| reads.get() == 3 && !menu.busy.get());
        let leaf = menu
            .content
            .last_child()
            .unwrap()
            .downcast::<gtk::Button>()
            .unwrap();
        assert!(
            leaf.has_focus(),
            "property updates must preserve the selected action"
        );
        let label = leaf
            .child()
            .unwrap()
            .first_child()
            .unwrap()
            .downcast::<gtk::Label>()
            .unwrap();
        assert_eq!(label.text(), "Updated");
        leaf.grab_focus();
        pump();
        input(&["key", "Return"]);
        wait(&|| !events.borrow().is_empty() && !menu.popover.is_visible());
        assert_eq!(events.borrow()[0].0, 5);
        assert_eq!(events.borrow()[0].1, "clicked");
        assert_eq!(events.borrow()[0].2.get::<i32>(), Some(0));
        assert_eq!(window.keyboard_mode(), KeyboardMode::None);
        let previous = reads.get();
        bus.emit_signal(
            None,
            "/Menu",
            INTERFACE,
            "LayoutUpdated",
            Some(&(3u32, 0i32).to_variant()),
        )
        .unwrap();
        pump();
        assert_eq!(reads.get(), previous, "hidden menus must not fetch updates");
        assert!(menu.show());
        wait(&|| reads.get() > previous && !menu.busy.get());
        pump();
        input(&["key", "Escape"]);
        wait(&|| !menu.popover.is_visible());
        assert_eq!(window.keyboard_mode(), KeyboardMode::None);
        delay.set(350);
        let previous = reads.get();
        assert!(menu.show());
        wait(&|| reads.get() > previous);
        assert!(menu.busy.get());
        menu.popover.popdown();
        delay.set(0);
        let previous = reads.get();
        assert!(menu.show());
        wait(&|| reads.get() > previous && !menu.busy.get());
        let first = menu.content.first_child().unwrap();
        context.block_on(glib::timeout_future(std::time::Duration::from_millis(500)));
        assert_eq!(
            menu.content.first_child().unwrap(),
            first,
            "a closed menu reply must not replace a reopened menu"
        );
        menu.popover.popdown();
        button.set_halign(gtk::Align::Start);
        pump();
        let previous = reads.get();
        assert!(menu.show());
        wait(&|| reads.get() > previous && !menu.busy.get());
        pump();
        let surface = menu.popover.native().unwrap().surface().unwrap();
        let popup = surface.clone().downcast::<gdk::Popup>().unwrap();
        assert!(
            popup.position_x() >= 0 && popup.position_x() + surface.width() <= window.width(),
            "left-edge bar popup must fit output"
        );
        let directory =
            std::env::temp_dir().join(format!("luma-menu-icons-{}", std::process::id()));
        std::fs::create_dir_all(directory.join("a")).unwrap();
        std::fs::create_dir_all(directory.join("b")).unwrap();
        for folder in ["a", "b"] {
            std::fs::write(
                directory.join(folder).join("document-open-symbolic.png"),
                png(16, 16),
            )
            .unwrap();
        }
        let global = gtk::IconTheme::for_display(&gdk::Display::default().unwrap());
        let original_paths = global.search_path();
        let set_paths = |paths: Vec<String>| {
            theme_paths.replace(paths.clone());
            bus.emit_signal(
                None,
                "/Menu",
                "org.freedesktop.DBus.Properties",
                "PropertiesChanged",
                Some(
                    &(
                        INTERFACE,
                        BTreeMap::from([("IconThemePath", paths.to_variant())]),
                        Vec::<String>::new(),
                    )
                        .to_variant(),
                ),
            )
            .unwrap();
        };
        let first_icon = || {
            menu.content
                .first_child()
                .unwrap()
                .first_child()
                .unwrap()
                .first_child()
                .unwrap()
                .downcast::<gtk::Image>()
                .unwrap()
        };
        for folder in ["a", "b"] {
            let previous = reads.get();
            set_paths(vec![directory.join(folder).to_string_lossy().into_owned()]);
            wait(&|| reads.get() > previous && !menu.busy.get());
            let icon = first_icon()
                .paintable()
                .unwrap()
                .downcast::<gtk::IconPaintable>()
                .unwrap();
            assert_eq!(
                icon.file().unwrap().path().unwrap(),
                directory.join(folder).join("document-open-symbolic.png")
            );
        }
        for paths in [vec!["relative/path".into()], vec![]] {
            let previous = reads.get();
            set_paths(paths);
            wait(&|| reads.get() > previous && !menu.busy.get());
            assert_eq!(
                first_icon().icon_name().as_deref(),
                Some("document-open-symbolic")
            );
        }
        assert_eq!(
            global.search_path(),
            original_paths,
            "menu paths must not mutate the global theme"
        );
        menu.popover.popdown();
        pump();
        assert!(menu.property_signal.borrow().is_none());
        let previous = reads.get();
        set_paths(vec![directory.join("a").to_string_lossy().into_owned()]);
        pump();
        assert_eq!(
            reads.get(),
            previous,
            "closed menus must not reload on theme changes"
        );
        std::fs::remove_dir_all(directory).unwrap();
        window.close();
        bus.unregister_object(registration).unwrap();
        std::fs::write(std::env::var("WM_TRAY_MENU_RECEIPT").unwrap(), "ok").unwrap();
    }
}
