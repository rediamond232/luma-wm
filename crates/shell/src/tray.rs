//! StatusNotifier host and bar icons.
use gtk::{gdk, gio, glib, prelude::*};
use std::{
    cell::{Cell, RefCell},
    collections::BTreeMap,
    rc::Rc,
    sync::atomic::{AtomicU64, Ordering},
};
const WATCHER: &str = "org.kde.StatusNotifierWatcher";
const ITEM: &str = "org.kde.StatusNotifierItem";
static NEXT_HOST: AtomicU64 = AtomicU64::new(0);

fn bounded_text(value: &str, limit: usize) -> String {
    value
        .chars()
        .take(limit)
        .map(|c| if c.is_control() && c != '\n' { ' ' } else { c })
        .collect()
}
fn tooltip_description(value: &str) -> String {
    let value = bounded_text(value, 8192);
    let wrapped = format!("<tooltip>{value}</tooltip>");
    let mut result = String::new();
    for event in xml::reader::EventReader::from_str(&wrapped) {
        use xml::reader::XmlEvent;
        match event {
            Ok(XmlEvent::Characters(text) | XmlEvent::CData(text) | XmlEvent::Whitespace(text)) => {
                result.push_str(&text)
            }
            Ok(XmlEvent::StartElement {
                name, attributes, ..
            }) if name.local_name == "img" => {
                if let Some(alt) = attributes.iter().find(|a| a.name.local_name == "alt") {
                    result.push_str(&alt.value);
                }
            }
            Ok(XmlEvent::StartElement { name, .. }) if name.local_name == "br" => result.push('\n'),
            Err(_) => {
                result = value;
                break;
            }
            _ => {}
        }
    }
    bounded_text(&result.lines().take(8).collect::<Vec<_>>().join("\n"), 2048)
}
fn tooltip(value: Option<&glib::Variant>, fallback: &str) -> String {
    let Some(value) = value.filter(|v| v.type_().as_str() == "(sa(iiay)ss)") else {
        return fallback.to_string();
    };
    // Inspect text children directly; do not copy unused tooltip pixmaps.
    let title = value.child_value(2);
    let title = bounded_text(title.str().unwrap_or_default(), 256);
    let description = value.child_value(3);
    let description = tooltip_description(description.str().unwrap_or_default());
    let title = if title.trim().is_empty() {
        fallback
    } else {
        &title
    };
    if description.trim().is_empty() {
        title.to_string()
    } else if title.is_empty() {
        description
    } else {
        format!("{title}\n{description}")
    }
}

fn text(proxy: &gio::DBusProxy, key: &str) -> String {
    proxy
        .cached_property(key)
        .and_then(|v| v.get::<String>())
        .unwrap_or_default()
        .chars()
        .take(256)
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect()
}
#[derive(Default)]
pub(crate) struct IconThemeCache {
    paths: Vec<String>,
    pub(crate) theme: Option<gtk::IconTheme>,
}
impl IconThemeCache {
    fn update(&mut self, proxy: &gio::DBusProxy) {
        self.update_paths(
            proxy
                .cached_property("IconThemePath")
                .and_then(|v| v.get::<String>())
                .into_iter()
                .collect(),
        );
    }
    pub(crate) fn update_paths(&mut self, paths: Vec<String>) {
        let paths: Vec<_> = paths
            .into_iter()
            .take(16)
            .filter(|path| {
                path.len() <= 4096
                    && !path.chars().any(char::is_control)
                    && std::path::Path::new(path).is_absolute()
            })
            .collect();
        if self.paths == paths {
            return;
        }
        self.paths = paths;
        self.theme = if self.paths.is_empty() {
            None
        } else {
            let theme = gtk::IconTheme::new();
            let name = gdk::Display::default()
                .map(|display| gtk::IconTheme::for_display(&display).theme_name());
            theme.set_theme_name(name.as_deref().or(Some("hicolor")));
            theme.set_search_path(
                &self
                    .paths
                    .iter()
                    .map(std::path::Path::new)
                    .collect::<Vec<_>>(),
            );
            Some(theme)
        };
    }
}
fn icon(
    proxy: &gio::DBusProxy,
    image: &gtk::Image,
    prefix: &str,
    custom: Option<&gtk::IconTheme>,
    cache: &mut PixmapCache,
) -> bool {
    let name = text(proxy, &format!("{prefix}IconName"));
    if !name.is_empty() {
        if let Some(theme) = custom.filter(|theme| theme.has_icon(&name)) {
            image.set_paintable(Some(&theme.lookup_icon(
                &name,
                &[],
                image.pixel_size().max(1),
                image.scale_factor(),
                image.direction(),
                gtk::IconLookupFlags::empty(),
            )));
            return true;
        }
        if let Some(display) = gdk::Display::default() {
            if gtk::IconTheme::for_display(&display).has_icon(&name) {
                image.set_icon_name(Some(&name));
                return true;
            }
        }
    }
    cache.show(proxy.cached_property(&format!("{prefix}IconPixmap")), image)
}
#[derive(Default)]
struct PixmapCache {
    input: Option<glib::Variant>,
    size: i32,
    texture: Option<gdk::MemoryTexture>,
}
impl PixmapCache {
    fn show(&mut self, input: Option<glib::Variant>, image: &gtk::Image) -> bool {
        let size = image
            .pixel_size()
            .max(1)
            .saturating_mul(image.scale_factor().max(1))
            .min(256);
        if self.input != input || self.size != size {
            self.texture = input.as_ref().and_then(|value| pixmap_texture(value, size));
            self.input = input;
            self.size = size;
        }
        if let Some(texture) = &self.texture {
            if image.paintable().as_ref() != Some(texture.upcast_ref()) {
                image.set_paintable(Some(texture));
            }
            true
        } else {
            image.clear();
            false
        }
    }
}
fn pixmap_texture(value: &glib::Variant, size: i32) -> Option<gdk::MemoryTexture> {
    if value.type_().as_str() != "a(iiay)" {
        return None;
    }
    // Inspect dimensions/byte counts before copying. Only the selected valid
    // candidate is converted, and entries beyond the first sixteen are ignored.
    let best = (0..value.n_children().min(16))
        .filter_map(|index| {
            let item = value.child_value(index);
            let w = item.child_value(0).get::<i32>()?;
            let h = item.child_value(1).get::<i32>()?;
            let bytes = item.child_value(2);
            if !(1..=256).contains(&w)
                || !(1..=256).contains(&h)
                || bytes.size() != w as usize * h as usize * 4
            {
                return None;
            }
            Some((w, h, bytes))
        })
        .min_by_key(|(w, h, _)| {
            (
                if *w >= size && *h >= size { 0 } else { 1 },
                (*w - size).abs() + (*h - size).abs(),
            )
        })?;
    let (w, h, bytes) = best;
    let mut bytes = bytes.fixed_array::<u8>().ok()?.to_vec();
    for pixel in bytes.chunks_exact_mut(4) {
        pixel.rotate_left(1);
    }
    Some(gdk::MemoryTexture::new(
        w,
        h,
        gdk::MemoryFormat::R8g8b8a8,
        &glib::Bytes::from_owned(bytes),
        w as usize * 4,
    ))
}

fn item_button(proxy: &gio::DBusProxy) -> gtk::Button {
    let image = gtk::Image::new();
    image.set_pixel_size(18);
    let overlay = gtk::Overlay::new();
    overlay.set_halign(gtk::Align::Center);
    overlay.set_valign(gtk::Align::Center);
    overlay.set_child(Some(&image));
    let badge = gtk::Image::new();
    badge.set_pixel_size(9);
    badge.set_halign(gtk::Align::End);
    badge.set_valign(gtk::Align::End);
    badge.set_can_target(false);
    overlay.add_overlay(&badge);
    let button = gtk::Button::builder().child(&overlay).build();
    let weak_button = button.downgrade();
    let weak_image = image.downgrade();
    let weak_badge = badge.downgrade();
    let theme = RefCell::new(IconThemeCache::default());
    let caches = RefCell::new([
        PixmapCache::default(),
        PixmapCache::default(),
        PixmapCache::default(),
    ]);
    let update = Rc::new(move |proxy: &gio::DBusProxy| {
        if let (Some(button), Some(image)) = (weak_button.upgrade(), weak_image.upgrade()) {
            let status = text(proxy, "Status");
            button.set_visible(status != "Passive" && proxy.name_owner().is_some());
            let title = text(proxy, "Title");
            let tooltip = tooltip(proxy.cached_property("ToolTip").as_ref(), &title);
            button.set_tooltip_text(Some(&tooltip));
            button.update_property(&[gtk::accessible::Property::Label(&title)]);
            let mut theme = theme.borrow_mut();
            theme.update(proxy);
            let custom = theme.theme.as_ref();
            let mut caches = caches.borrow_mut();
            let attention = status == "NeedsAttention"
                && icon(proxy, &image, "Attention", custom, &mut caches[1]);
            if !attention && !icon(proxy, &image, "", custom, &mut caches[0]) {
                image.set_icon_name(Some("application-x-executable-symbolic"));
            }
            if let Some(badge) = weak_badge.upgrade() {
                badge.set_visible(icon(proxy, &badge, "Overlay", custom, &mut caches[2]));
            }
        }
    });
    update(proxy);
    for image in [&image, &badge] {
        let update = update.clone();
        let weak_proxy = proxy.downgrade();
        image.connect_scale_factor_notify(move |_| {
            if let Some(proxy) = weak_proxy.upgrade() {
                update(&proxy);
            }
        });
    }
    let properties = update.clone();
    proxy.connect_local("g-properties-changed", false, move |values| {
        if let Ok(proxy) = values[0].get::<gio::DBusProxy>() {
            properties(&proxy);
        }
        None
    });
    // SNI implementations often emit NewIcon/NewStatus instead of PropertiesChanged.
    let busy = Rc::new(Cell::new(false));
    let again = Rc::new(Cell::new(false));
    let proxy_signal = proxy.clone();
    let weak_proxy = proxy_signal.downgrade();
    proxy.connect_local("g-signal", false, move |_| {
        if let Some(proxy) = weak_proxy.upgrade() {
            refresh_item(&proxy, update.clone(), busy.clone(), again.clone());
        }
        None
    });
    let action_proxy = proxy.clone();
    let menu = crate::tray_menu::Menu::new(&button, proxy);
    let primary_menu = menu.clone();
    button.connect_clicked(move |button| {
        let menu = action_proxy
            .cached_property("ItemIsMenu")
            .and_then(|v| v.get::<bool>())
            .unwrap_or(false);
        if menu && primary_menu.show() {
            return;
        }
        action(
            &action_proxy,
            button,
            if menu { "ContextMenu" } else { "Activate" },
        );
    });
    let click = gtk::GestureClick::new();
    click.set_button(0);
    let weak = button.downgrade();
    let click_proxy = proxy.clone();
    click.connect_pressed(move |gesture, _, _, _| {
        let method = match gesture.current_button() {
            2 => "SecondaryActivate",
            3 => "ContextMenu",
            _ => return,
        };
        if let Some(button) = weak.upgrade() {
            if method == "ContextMenu" && menu.show() {
                gesture.set_state(gtk::EventSequenceState::Claimed);
                return;
            }
            action(&click_proxy, &button, method);
        }
        gesture.set_state(gtk::EventSequenceState::Claimed);
    });
    button.add_controller(click);
    // Discrete mode accumulates smooth input into steps in GTK, avoiding a
    // D-Bus request for every tiny touchpad movement. Match GTK/Waybar signs:
    // positive means down/right, negative means up/left.
    let scroll = gtk::EventControllerScroll::new(
        gtk::EventControllerScrollFlags::BOTH_AXES | gtk::EventControllerScrollFlags::DISCRETE,
    );
    let weak = button.downgrade();
    let scroll_proxy = proxy.clone();
    scroll.connect_scroll(move |_, dx, dy| {
        if let Some(button) = weak.upgrade() {
            for (amount, orientation) in [(dx, "horizontal"), (dy, "vertical")] {
                if amount.is_finite() && amount != 0.0 {
                    let delta = amount as i32;
                    if delta != 0 {
                        call_action(
                            &scroll_proxy,
                            &button,
                            "Scroll",
                            &(delta, orientation).to_variant(),
                        );
                    }
                }
            }
        }
        glib::Propagation::Stop
    });
    button.add_controller(scroll);
    button
}

#[cfg(test)]
pub(crate) fn verify_icon_cache() {
    let image = gtk::Image::new();
    image.set_pixel_size(18);
    let mut cache = PixmapCache::default();
    let pixels = vec![
        (18i32, 18i32, vec![255u8, 32, 180, 80].repeat(18 * 18)),
        (9, 9, vec![255u8, 210, 40, 90].repeat(9 * 9)),
    ];
    assert!(cache.show(Some(pixels.to_variant()), &image));
    let original = image.paintable().unwrap();
    for _ in 0..100 {
        assert!(cache.show(Some(pixels.to_variant()), &image));
        assert_eq!(
            image.paintable().as_ref(),
            Some(&original),
            "unchanged updates must reuse the same texture"
        );
    }
    image.set_pixel_size(9);
    assert!(cache.show(Some(pixels.to_variant()), &image));
    assert_eq!(cache.texture.as_ref().unwrap().width(), 9);
    assert_ne!(image.paintable().as_ref(), Some(&original));
    let changed = vec![(9i32, 9i32, vec![255u8, 50, 60, 70].repeat(81))].to_variant();
    let previous = image.paintable().unwrap();
    assert!(cache.show(Some(changed), &image));
    assert_ne!(image.paintable().as_ref(), Some(&previous));
    assert!(!cache.show(None, &image));
    assert!(image.paintable().is_none());
    for invalid in [
        false.to_variant(),
        vec![(257i32, 1i32, vec![0u8; 1028])].to_variant(),
        vec![(18i32, 18i32, vec![0u8; 4])].to_variant(),
    ] {
        assert!(pixmap_texture(&invalid, 18).is_none());
    }
    let mut candidates = vec![(0i32, 0i32, Vec::<u8>::new()); 16];
    candidates.push(pixels[0].clone());
    assert!(
        pixmap_texture(&candidates.to_variant(), 18).is_none(),
        "candidate limit must be respected"
    );
}
type Update = Rc<dyn Fn(&gio::DBusProxy)>;
fn refresh_item(
    proxy: &gio::DBusProxy,
    update: Update,
    busy: Rc<Cell<bool>>,
    again: Rc<Cell<bool>>,
) {
    if busy.replace(true) {
        again.set(true);
        return;
    }
    let retained = proxy.clone();
    let owner = proxy.name_owner();
    proxy.call(
        "org.freedesktop.DBus.Properties.GetAll",
        Some(&(ITEM,).to_variant()),
        gio::DBusCallFlags::NONE,
        2000,
        gio::Cancellable::NONE,
        move |result| {
            busy.set(false);
            if retained.name_owner() == owner {
                if let Ok(result) = result {
                    if let Some((properties,)) = result.get::<(BTreeMap<String, glib::Variant>,)>()
                    {
                        for (key, value) in properties {
                            retained.set_cached_property(&key, Some(&value));
                        }
                        update(&retained);
                    }
                }
            }
            if again.replace(false) {
                refresh_item(&retained, update, busy, again);
            }
        },
    );
}
fn action(proxy: &gio::DBusProxy, button: &gtk::Button, method: &str) {
    call_action(proxy, button, method, &(0i32, 0i32).to_variant());
}
fn call_action(
    proxy: &gio::DBusProxy,
    button: &gtk::Button,
    method: &str,
    parameters: &glib::Variant,
) {
    if proxy.name_owner().is_none() {
        return;
    }
    let weak = button.downgrade();
    proxy.call(
        method,
        Some(parameters),
        gio::DBusCallFlags::NONE,
        2000,
        gio::Cancellable::NONE,
        move |result| {
            if let (Err(error), Some(button)) = (result, weak.upgrade()) {
                button.set_tooltip_text(Some(&format!("Tray action failed: {error}")));
            }
        },
    );
}
struct Host {
    root: glib::WeakRef<gtk::Box>,
    watcher: gio::DBusProxy,
    items: RefCell<BTreeMap<String, (u64, Option<gtk::Button>)>>,
    generation: Cell<u64>,
    busy: Cell<bool>,
    again: Cell<bool>,
    name: String,
}
impl Host {
    fn refresh(self: &Rc<Self>) {
        if self.busy.replace(true) {
            self.again.set(true);
            return;
        }
        let weak = Rc::downgrade(self);
        let owner = self.watcher.name_owner();
        self.watcher.call(
            "org.freedesktop.DBus.Properties.Get",
            Some(&(WATCHER, "RegisteredStatusNotifierItems").to_variant()),
            gio::DBusCallFlags::NONE,
            2000,
            gio::Cancellable::NONE,
            move |result| {
                let Some(host) = weak.upgrade() else { return };
                host.busy.set(false);
                if host.watcher.name_owner() == owner {
                    if let Ok(result) = result {
                        if let Some((value,)) = result.get::<(glib::Variant,)>() {
                            if let Some(names) = value.get::<Vec<String>>() {
                                host.sync(names);
                            }
                        }
                    }
                }
                if host.again.replace(false) {
                    host.refresh();
                }
            },
        );
    }
    fn sync(self: &Rc<Self>, names: Vec<String>) {
        let Some(root) = self.root.upgrade() else {
            return;
        };
        let names: Vec<_> = names.into_iter().take(64).collect();
        self.items.borrow_mut().retain(|name, (_, button)| {
            if names.contains(name) {
                true
            } else {
                if let Some(button) = button {
                    root.remove(button);
                }
                false
            }
        });
        for name in names {
            if self.items.borrow().contains_key(&name) {
                continue;
            }
            let (service, path) = name.split_once('/').map_or_else(
                || (name.as_str(), "/StatusNotifierItem".to_string()),
                |(service, suffix)| (service, format!("/{suffix}")),
            );
            if !gio::dbus_is_name(service)
                || glib::variant::ObjectPath::try_from(path.as_str()).is_err()
            {
                continue;
            }
            let generation = self.generation.get().wrapping_add(1);
            self.generation.set(generation);
            self.items
                .borrow_mut()
                .insert(name.clone(), (generation, None));
            let weak = Rc::downgrade(self);
            let id = name.clone();
            gio::DBusProxy::for_bus(
                gio::BusType::Session,
                gio::DBusProxyFlags::DO_NOT_AUTO_START
                    | gio::DBusProxyFlags::GET_INVALIDATED_PROPERTIES,
                None,
                service,
                &path,
                ITEM,
                gio::Cancellable::NONE,
                move |result| {
                    let Some(host) = weak.upgrade() else { return };
                    if host
                        .items
                        .borrow()
                        .get(&id)
                        .is_none_or(|(current, _)| *current != generation)
                    {
                        return;
                    }
                    if let (Ok(proxy), Some(root)) = (result, host.root.upgrade()) {
                        let button = item_button(&proxy);
                        root.append(&button);
                        host.items
                            .borrow_mut()
                            .insert(id, (generation, Some(button)));
                    }
                },
            );
        }
    }
    fn register(self: &Rc<Self>) {
        if self.watcher.name_owner().is_none() {
            self.sync(vec![]);
            return;
        }
        let weak = Rc::downgrade(self);
        self.watcher.call(
            "RegisterStatusNotifierHost",
            Some(&(self.name.clone(),).to_variant()),
            gio::DBusCallFlags::NONE,
            2000,
            gio::Cancellable::NONE,
            move |_| {
                if let Some(host) = weak.upgrade() {
                    host.refresh();
                }
            },
        );
    }
}
pub fn widget() -> gtk::Box {
    let root = gtk::Box::new(gtk::Orientation::Horizontal, 2);
    let weak = root.downgrade();
    gio::DBusProxy::for_bus(
        gio::BusType::Session,
        gio::DBusProxyFlags::DO_NOT_AUTO_START | gio::DBusProxyFlags::GET_INVALIDATED_PROPERTIES,
        None,
        WATCHER,
        "/StatusNotifierWatcher",
        WATCHER,
        gio::Cancellable::NONE,
        move |result| {
            let (Some(root), Ok(watcher)) = (weak.upgrade(), result) else {
                return;
            };
            let name = format!(
                "org.kde.StatusNotifierHost.luma_{}_{}",
                std::process::id(),
                NEXT_HOST.fetch_add(1, Ordering::Relaxed)
            );
            let host = Rc::new(Host {
                root: root.downgrade(),
                watcher: watcher.clone(),
                items: RefCell::new(BTreeMap::new()),
                generation: Cell::new(0),
                busy: Cell::new(false),
                again: Cell::new(false),
                name: name.clone(),
            });
            let weak = Rc::downgrade(&host);
            watcher.connect_local("g-signal", false, move |_| {
                if let Some(host) = weak.upgrade() {
                    host.refresh();
                }
                None
            });
            let weak = Rc::downgrade(&host);
            watcher.connect_notify_local(Some("g-name-owner"), move |_, _| {
                if let Some(host) = weak.upgrade() {
                    host.register();
                }
            });
            let weak = Rc::downgrade(&host);
            let owner = gio::bus_own_name_on_connection(
                &watcher.connection(),
                &name,
                gio::BusNameOwnerFlags::NONE,
                move |_, _| {
                    if let Some(host) = weak.upgrade() {
                        host.register();
                    }
                },
                |_, _| {},
            );
            let owner = RefCell::new(Some(owner));
            root.connect_destroy(move |_| {
                if let Some(owner) = owner.borrow_mut().take() {
                    gio::bus_unown_name(owner);
                }
                let _ = &host;
            });
        },
    );
    root
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn structured_tooltips_preserve_text_and_bound_content() {
        let value = ("",Vec::<(i32,i32,Vec<u8>)>::new(), "Player", "<b>Playing</b> &amp; ready <a href='https://example.invalid'>Details</a><br/><img src='/missing' alt='Cover'/>").to_variant();
        assert_eq!(
            tooltip(Some(&value), "Fallback"),
            "Player\nPlaying & ready Details\nCover"
        );
        assert_eq!(tooltip(None, "Fallback"), "Fallback");
        assert_eq!(tooltip(Some(&true.to_variant()), "Fallback"), "Fallback");
        let empty = ("", Vec::<(i32, i32, Vec<u8>)>::new(), "", "Description").to_variant();
        assert_eq!(tooltip(Some(&empty), "Fallback"), "Fallback\nDescription");
        assert_eq!(tooltip_description("<b>broken"), "<b>broken");
        assert_eq!(tooltip_description("<unknown>Text</unknown>"), "Text");
        assert_eq!(tooltip_description(&"é".repeat(9000)).chars().count(), 2048);
        assert_eq!(
            tooltip_description(&"line\n".repeat(100)).lines().count(),
            8
        );
        assert_eq!(tooltip_description("A\0B"), "A B");
    }
}
