//! MPRIS discovery and controls, updated through D-Bus signals.
use gtk::{gio, glib, prelude::*};
use std::{
    cell::{Cell, RefCell},
    collections::BTreeMap,
    rc::Rc,
};
const PREFIX: &str = "org.mpris.MediaPlayer2.";
const PATH: &str = "/org/mpris/MediaPlayer2";
const INTERFACE: &str = "org.mpris.MediaPlayer2.Player";

struct Player {
    generation: u64,
    row: Option<(gtk::Box, gio::DBusProxy)>,
}
struct Media {
    root: glib::WeakRef<gtk::Box>,
    players: RefCell<BTreeMap<String, Player>>,
    generation: Cell<u64>,
    bus: RefCell<Option<gio::DBusProxy>>,
    selected: RefCell<Option<String>>,
    selector: gtk::MenuButton,
    selector_names: RefCell<Vec<String>>,
}
fn boolean(proxy: &gio::DBusProxy, name: &str) -> bool {
    proxy
        .cached_property(name)
        .and_then(|v| v.get::<bool>())
        .unwrap_or(false)
}
fn playing(proxy: &gio::DBusProxy) -> bool {
    proxy
        .cached_property("PlaybackStatus")
        .and_then(|v| v.get::<String>())
        .as_deref()
        == Some("Playing")
}
fn title(proxy: &gio::DBusProxy) -> String {
    let metadata = proxy
        .cached_property("Metadata")
        .and_then(|v| v.get::<BTreeMap<String, glib::Variant>>())
        .unwrap_or_default();
    let title = metadata
        .get("xesam:title")
        .and_then(|v| v.get::<String>())
        .unwrap_or_else(|| "Media".into());
    let artist = metadata
        .get("xesam:artist")
        .and_then(|v| v.get::<Vec<String>>())
        .and_then(|v| v.into_iter().next());
    match artist {
        Some(artist) if !artist.is_empty() => format!("{artist} · {title}"),
        _ => title,
    }
    .chars()
    .take(256)
    .collect()
}
impl Media {
    fn refresh(self: &Rc<Self>) {
        let players = self.players.borrow();
        let names: Vec<_> = players
            .iter()
            .filter(|(_, player)| player.row.is_some())
            .map(|(name, _)| name.clone())
            .collect();
        if self
            .selected
            .borrow()
            .as_ref()
            .is_some_and(|name| !names.contains(name))
        {
            self.selected.borrow_mut().take();
        }
        let manual = self.selected.borrow();
        let selected = manual.as_ref().or_else(|| {
            players
                .iter()
                .filter_map(|(name, p)| p.row.as_ref().map(|(_, proxy)| (name, proxy)))
                .find(|(_, proxy)| playing(proxy))
                .map(|(name, _)| name)
                .or_else(|| {
                    players
                        .iter()
                        .find(|(_, p)| p.row.is_some())
                        .map(|(name, _)| name)
                })
        });
        for (name, player) in players.iter() {
            if let Some((row, _)) = &player.row {
                row.set_visible(selected == Some(name));
            }
        }
        let selected_label: String = manual
            .as_deref()
            .and_then(|name| name.strip_prefix(PREFIX))
            .unwrap_or("Auto")
            .chars()
            .take(24)
            .collect();
        self.selector.set_label(&selected_label);
        self.selector.set_tooltip_text(Some("Choose media player"));
        if let Some(root) = self.root.upgrade() {
            if names.len() > 1 && self.selector.parent().is_none() {
                root.append(&self.selector);
            }
            if names.len() <= 1 && self.selector.parent().is_some() {
                root.remove(&self.selector);
            }
        }
        if *self.selector_names.borrow() != names {
            *self.selector_names.borrow_mut() = names.clone();
            let popover = gtk::Popover::new();
            let list = gtk::Box::new(gtk::Orientation::Vertical, 2);
            for choice in std::iter::once(None).chain(names.into_iter().map(Some)) {
                let label = choice
                    .as_deref()
                    .and_then(|name| name.strip_prefix(PREFIX))
                    .unwrap_or("Automatic");
                let short: String = label.chars().take(64).collect();
                let button = gtk::Button::with_label(&short);
                button.set_tooltip_text(Some(label));
                let weak = Rc::downgrade(self);
                button.connect_clicked(move |_| {
                    if let Some(media) = weak.upgrade() {
                        *media.selected.borrow_mut() = choice.clone();
                        media.selector.popdown();
                        media.refresh();
                    }
                });
                list.append(&button);
            }
            let scroll = gtk::ScrolledWindow::new();
            scroll.set_policy(gtk::PolicyType::Never, gtk::PolicyType::Automatic);
            scroll.set_max_content_height(320);
            scroll.set_propagate_natural_height(true);
            scroll.set_min_content_width(180);
            scroll.set_child(Some(&list));
            popover.set_child(Some(&scroll));
            self.selector.set_popover(Some(&popover));
        }
    }
    fn changed(self: &Rc<Self>, name: String, owner: String) {
        if !name.starts_with(PREFIX) {
            return;
        }
        let Some(root) = self.root.upgrade() else {
            return;
        };
        if let Some(old) = self.players.borrow_mut().remove(&name) {
            if let Some((row, _)) = old.row {
                root.remove(&row);
            }
        }
        self.refresh();
        if owner.is_empty() || self.players.borrow().len() >= 32 {
            return;
        }
        let generation = self.generation.get().wrapping_add(1);
        self.generation.set(generation);
        self.players.borrow_mut().insert(
            name.clone(),
            Player {
                generation,
                row: None,
            },
        );
        let weak = Rc::downgrade(self);
        gio::DBusProxy::for_bus(
            gio::BusType::Session,
            gio::DBusProxyFlags::DO_NOT_AUTO_START
                | gio::DBusProxyFlags::GET_INVALIDATED_PROPERTIES,
            None,
            &name.clone(),
            PATH,
            INTERFACE,
            gio::Cancellable::NONE,
            move |result| {
                let Some(media) = weak.upgrade() else { return };
                if media
                    .players
                    .borrow()
                    .get(&name)
                    .is_none_or(|p| p.generation != generation)
                {
                    return;
                }
                let Ok(proxy) = result else {
                    media.players.borrow_mut().remove(&name);
                    return;
                };
                if proxy.name_owner().is_none() {
                    media.players.borrow_mut().remove(&name);
                    return;
                }
                let Some(root) = media.root.upgrade() else {
                    return;
                };
                let row = player_row(&proxy, &name);
                root.append(&row);
                media.players.borrow_mut().get_mut(&name).unwrap().row = Some((row, proxy.clone()));
                let weak = Rc::downgrade(&media);
                proxy.connect_local("g-properties-changed", false, move |_| {
                    if let Some(media) = weak.upgrade() {
                        media.refresh();
                    }
                    None
                });
                media.refresh();
            },
        );
    }
}

fn extra_controls(proxy: &gio::DBusProxy) -> gtk::MenuButton {
    let menu = gtk::MenuButton::builder().label("⋯").build();
    menu.set_tooltip_text(Some("More media controls"));
    let content = gtk::Box::new(gtk::Orientation::Vertical, 6);
    content.set_margin_top(12);
    content.set_margin_bottom(12);
    content.set_margin_start(12);
    content.set_margin_end(12);
    let error = gtk::Label::new(None);
    error.set_wrap(true);
    error.set_max_width_chars(36);
    let mut updates = Vec::new();
    for action in ["Back 10 seconds", "Forward 10 seconds", "Shuffle", "Repeat"] {
        let button = gtk::Button::with_label(action);
        content.append(&button);
        let pending = Rc::new(Cell::new(false));
        let weak_button = button.downgrade();
        let busy = pending.clone();
        let update = Rc::new(move |proxy: &gio::DBusProxy| {
            let Some(button) = weak_button.upgrade() else {
                return;
            };
            let supported = match action {
                "Shuffle" => proxy
                    .cached_property("Shuffle")
                    .and_then(|v| v.get::<bool>())
                    .is_some(),
                "Repeat" => proxy
                    .cached_property("LoopStatus")
                    .and_then(|v| v.get::<String>())
                    .is_some(),
                _ => boolean(proxy, "CanSeek"),
            };
            button.set_sensitive(
                proxy.name_owner().is_some()
                    && boolean(proxy, "CanControl")
                    && supported
                    && !busy.get(),
            );
            if action == "Shuffle" {
                button.set_label(if boolean(proxy, "Shuffle") {
                    "Shuffle: on"
                } else {
                    "Shuffle: off"
                });
            } else if action == "Repeat" {
                let mode = proxy
                    .cached_property("LoopStatus")
                    .and_then(|v| v.get::<String>())
                    .unwrap_or_default();
                button.set_label(match mode.as_str() {
                    "Track" => "Repeat: track",
                    "Playlist" => "Repeat: playlist",
                    _ => "Repeat: off",
                });
            }
        });
        update(proxy);
        updates.push(update.clone());
        let proxy = proxy.clone();
        let weak_error = error.downgrade();
        button.connect_clicked(move |button| {
            if !button.is_sensitive() || pending.replace(true) {
                return;
            }
            let (method, parameters) = match action {
                "Shuffle" => (
                    "org.freedesktop.DBus.Properties.Set",
                    (
                        INTERFACE,
                        "Shuffle",
                        (!boolean(&proxy, "Shuffle")).to_variant(),
                    )
                        .to_variant(),
                ),
                "Repeat" => {
                    let mode = proxy
                        .cached_property("LoopStatus")
                        .and_then(|v| v.get::<String>())
                        .unwrap_or_default();
                    let next = match mode.as_str() {
                        "None" => "Track",
                        "Track" => "Playlist",
                        _ => "None",
                    };
                    (
                        "org.freedesktop.DBus.Properties.Set",
                        (INTERFACE, "LoopStatus", next.to_variant()).to_variant(),
                    )
                }
                _ => (
                    "Seek",
                    (if action == "Back 10 seconds" {
                        -10_000_000i64
                    } else {
                        10_000_000i64
                    },)
                        .to_variant(),
                ),
            };
            if let Some(error) = weak_error.upgrade() {
                error.set_label("");
            }
            update(&proxy);
            let pending = pending.clone();
            let update = update.clone();
            let retained = proxy.clone();
            let weak_error = weak_error.clone();
            let owner = proxy.name_owner();
            proxy.call(
                method,
                Some(&parameters),
                gio::DBusCallFlags::NONE,
                2000,
                gio::Cancellable::NONE,
                move |result| {
                    pending.set(false);
                    if retained.name_owner() == owner {
                        if let (Err(reason), Some(error)) = (result, weak_error.upgrade()) {
                            error.set_label(&format!("Media command failed: {reason}"));
                        }
                    }
                    update(&retained);
                },
            );
        });
    }
    content.append(&error);
    content.append(&crate::media_position::widget(proxy));
    content.append(&crate::media_art::widget(proxy));
    let popover = gtk::Popover::new();
    popover.set_child(Some(&content));
    menu.set_popover(Some(&popover));
    crate::network::popover_keyboard_focus(&menu, &popover);
    proxy.connect_local("g-properties-changed", false, move |values| {
        if let Ok(proxy) = values[0].get::<gio::DBusProxy>() {
            for update in &updates {
                update(&proxy);
            }
        }
        None
    });
    menu
}

fn player_row(proxy: &gio::DBusProxy, name: &str) -> gtk::Box {
    let row = gtk::Box::new(gtk::Orientation::Horizontal, 2);
    row.set_tooltip_text(Some(name.strip_prefix(PREFIX).unwrap_or(name)));
    let label = gtk::Label::new(Some(&title(proxy)));
    label.set_ellipsize(gtk::pango::EllipsizeMode::End);
    label.set_max_width_chars(24);
    row.append(&label);
    let previous = gtk::Button::with_label("⏮");
    let play = gtk::Button::with_label("▶");
    let next = gtk::Button::with_label("⏭");
    for (button, method, tooltip) in [
        (&previous, "Previous", "Previous track"),
        (&play, "PlayPause", "Play or pause"),
        (&next, "Next", "Next track"),
    ] {
        button.set_tooltip_text(Some(tooltip));
        let proxy = proxy.clone();
        button.connect_clicked(move |button| {
            let allowed = boolean(&proxy, "CanControl")
                && match method {
                    "Previous" => boolean(&proxy, "CanGoPrevious"),
                    "Next" => boolean(&proxy, "CanGoNext"),
                    _ => boolean(
                        &proxy,
                        if playing(&proxy) {
                            "CanPause"
                        } else {
                            "CanPlay"
                        },
                    ),
                };
            if !allowed {
                return;
            }
            let weak = button.downgrade();
            proxy.call(
                method,
                None,
                gio::DBusCallFlags::NONE,
                2000,
                gio::Cancellable::NONE,
                move |result| {
                    if let (Err(error), Some(button)) = (result, weak.upgrade()) {
                        button.set_tooltip_text(Some(&format!("Media command failed: {error}")));
                    }
                },
            );
        });
        row.append(button);
    }
    row.append(&extra_controls(proxy));
    let update = Rc::new({
        let label = label.downgrade();
        let previous = previous.downgrade();
        let play = play.downgrade();
        let next = next.downgrade();
        move |proxy: &gio::DBusProxy| {
            if let Some(label) = label.upgrade() {
                label.set_text(&title(proxy));
            }
            let control = boolean(proxy, "CanControl");
            if let Some(button) = previous.upgrade() {
                button.set_sensitive(control && boolean(proxy, "CanGoPrevious"));
            }
            if let Some(button) = next.upgrade() {
                button.set_sensitive(control && boolean(proxy, "CanGoNext"));
            }
            if let Some(button) = play.upgrade() {
                button.set_label(if playing(proxy) { "⏸" } else { "▶" });
                button.set_sensitive(
                    control
                        && boolean(
                            proxy,
                            if playing(proxy) {
                                "CanPause"
                            } else {
                                "CanPlay"
                            },
                        ),
                );
            }
        }
    });
    update(proxy);
    proxy.connect_local("g-properties-changed", false, move |values| {
        if let Ok(proxy) = values[0].get::<gio::DBusProxy>() {
            update(&proxy);
        }
        None
    });
    row
}

pub fn widget() -> gtk::Box {
    let root = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    let media = Rc::new(Media {
        root: root.downgrade(),
        players: RefCell::new(BTreeMap::new()),
        generation: Cell::new(0),
        bus: RefCell::new(None),
        selected: RefCell::new(None),
        selector: gtk::MenuButton::new(),
        selector_names: RefCell::new(Vec::new()),
    });
    let weak = Rc::downgrade(&media);
    gio::DBusProxy::for_bus(
        gio::BusType::Session,
        gio::DBusProxyFlags::DO_NOT_AUTO_START,
        None,
        "org.freedesktop.DBus",
        "/org/freedesktop/DBus",
        "org.freedesktop.DBus",
        gio::Cancellable::NONE,
        move |result| {
            let (Some(media), Ok(bus)) = (weak.upgrade(), result) else {
                return;
            };
            let weak = Rc::downgrade(&media);
            bus.connect_local("g-signal", false, move |values| {
                if values[2].get::<String>().as_deref() == Ok("NameOwnerChanged") {
                    if let (Some(media), Ok(args)) =
                        (weak.upgrade(), values[3].get::<glib::Variant>())
                    {
                        if let Some((name, _, owner)) = args.get::<(String, String, String)>() {
                            media.changed(name, owner);
                        }
                    }
                }
                None
            });
            let weak = Rc::downgrade(&media);
            bus.call(
                "ListNames",
                None,
                gio::DBusCallFlags::NONE,
                2000,
                gio::Cancellable::NONE,
                move |result| {
                    if let (Some(media), Ok(result)) = (weak.upgrade(), result) {
                        if let Some((names,)) = result.get::<(Vec<String>,)>() {
                            for name in names {
                                if !media.players.borrow().contains_key(&name) {
                                    media.changed(name, "discover".into());
                                }
                            }
                        }
                    }
                },
            );
            *media.bus.borrow_mut() = Some(bus);
        },
    );
    root.connect_destroy(move |_| {
        let _ = &media;
    });
    root
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    #[ignore = "requires a private test D-Bus and GTK display"]
    fn live_player_discovery_controls_and_removal() {
        assert_eq!(
            std::env::var("WM_NETWORK_TEST_PRIVATE_BUS").as_deref(),
            Ok("1")
        );
        gtk::init().unwrap();
        let context = glib::MainContext::default();
        let _guard = context.acquire().unwrap();
        let connection = gio::bus_get_sync(gio::BusType::Session, gio::Cancellable::NONE).unwrap();
        let name = "org.mpris.MediaPlayer2.wmtest";
        let info = gio::DBusNodeInfo::for_xml("<node><interface name='org.mpris.MediaPlayer2.Player'><method name='SetPosition'><arg type='o' direction='in'/><arg type='x' direction='in'/></method><property name='Position' type='x' access='read'/><property name='Rate' type='d' access='read'/><method name='Seek'><arg type='x' direction='in'/></method><property name='CanSeek' type='b' access='read'/><property name='Shuffle' type='b' access='readwrite'/><property name='LoopStatus' type='s' access='readwrite'/><method name='Next'/><method name='Previous'/><method name='PlayPause'/><property name='PlaybackStatus' type='s' access='read'/><property name='Metadata' type='a{sv}' access='read'/><property name='CanControl' type='b' access='read'/><property name='CanPlay' type='b' access='read'/><property name='CanPause' type='b' access='read'/><property name='CanGoNext' type='b' access='read'/><property name='CanGoPrevious' type='b' access='read'/></interface></node>").unwrap();
        let calls = Rc::new(RefCell::new(Vec::<String>::new()));
        let received = calls.clone();
        let writes = Rc::new(RefCell::new(Vec::new()));
        let saved_writes = writes.clone();
        let seek_offsets = Rc::new(RefCell::new(Vec::new()));
        let offsets = seek_offsets.clone();
        let deny = Rc::new(Cell::new(false));
        let deny_call = deny.clone();
        let position = Rc::new(Cell::new(30_000_000i64));
        let read_position = position.clone();
        let position_reads = Rc::new(Cell::new(0));
        let reads = position_reads.clone();
        let set_positions = Rc::new(RefCell::new(Vec::new()));
        let positions = set_positions.clone();
        let artwork_path =
            std::env::temp_dir().join(format!("wm-media-art-{}.ppm", std::process::id()));
        let mut artwork = b"P6\n400 200\n255\n".to_vec();
        artwork.extend([192u8, 64, 32].repeat(400 * 200));
        std::fs::write(&artwork_path, artwork).unwrap();
        let artwork_url = gio::File::for_path(&artwork_path).uri().to_string();
        let registration = connection
            .register_object(PATH, &info.interfaces()[0])
            .property(move |_, _, _, _, property| match property {
                "PlaybackStatus" => "Playing".to_variant(),
                "Position" => {
                    reads.set(reads.get() + 1);
                    read_position.get().to_variant()
                }
                "Rate" => 1.0f64.to_variant(),
                "Shuffle" => false.to_variant(),
                "LoopStatus" => "None".to_variant(),
                "Metadata" => BTreeMap::from([
                    ("xesam:title", "Test track".to_variant()),
                    (
                        "mpris:trackid",
                        glib::variant::ObjectPath::try_from("/track/one")
                            .unwrap()
                            .to_variant(),
                    ),
                    ("mpris:length", 180_000_000i64.to_variant()),
                    ("mpris:artUrl", artwork_url.to_variant()),
                ])
                .to_variant(),
                _ => true.to_variant(),
            })
            .set_property(move |connection, _, _, _, property, value| {
                saved_writes
                    .borrow_mut()
                    .push((property.to_string(), value.clone()));
                connection
                    .emit_signal(
                        None,
                        PATH,
                        "org.freedesktop.DBus.Properties",
                        "PropertiesChanged",
                        Some(
                            &(
                                INTERFACE,
                                BTreeMap::from([(property, value)]),
                                Vec::<String>::new(),
                            )
                                .to_variant(),
                        ),
                    )
                    .unwrap();
                true
            })
            .method_call(move |connection, _, _, _, method, parameters, invocation| {
                if method == "SetPosition" {
                    let request = parameters
                        .get::<(glib::variant::ObjectPath, i64)>()
                        .unwrap();
                    position.set(request.1);
                    positions.borrow_mut().push(request);
                    connection
                        .emit_signal(
                            None,
                            PATH,
                            INTERFACE,
                            "Seeked",
                            Some(&(position.get(),).to_variant()),
                        )
                        .unwrap();
                } else if method == "Seek" {
                    offsets
                        .borrow_mut()
                        .push(parameters.get::<(i64,)>().unwrap().0);
                    if deny_call.get() {
                        invocation.return_dbus_error(
                            "org.mpris.MediaPlayer2.Error.Failed",
                            "test seek failure",
                        );
                        return;
                    }
                } else {
                    received.borrow_mut().push(method.into());
                }
                invocation.return_value(Some(&().to_variant()));
            })
            .build()
            .unwrap();
        let root = widget();
        let name_call = |method, params: glib::Variant| {
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
                .unwrap()
        };
        assert_eq!(
            name_call("RequestName", (name, 4u32).to_variant()).get::<(u32,)>(),
            Some((1,))
        );
        let wait = |condition: &dyn Fn() -> bool| {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
            while !condition() {
                while context.pending() {
                    context.iteration(false);
                }
                assert!(
                    std::time::Instant::now() < deadline,
                    "media state did not update"
                );
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
        };
        wait(&|| root.first_child().is_some());
        let row = root.first_child().unwrap().downcast::<gtk::Box>().unwrap();
        let label = row.first_child().unwrap().downcast::<gtk::Label>().unwrap();
        assert_eq!(label.text(), "Test track");
        let previous = label
            .next_sibling()
            .unwrap()
            .downcast::<gtk::Button>()
            .unwrap();
        let play = previous
            .next_sibling()
            .unwrap()
            .downcast::<gtk::Button>()
            .unwrap();
        let next = play
            .next_sibling()
            .unwrap()
            .downcast::<gtk::Button>()
            .unwrap();
        assert_eq!(play.label().as_deref(), Some("⏸"));
        previous.emit_clicked();
        play.emit_clicked();
        next.emit_clicked();
        wait(&|| calls.borrow().len() == 3);
        assert_eq!(&*calls.borrow(), &["Previous", "PlayPause", "Next"]);
        let extra = next
            .next_sibling()
            .unwrap()
            .downcast::<gtk::MenuButton>()
            .unwrap();
        let controls = extra
            .popover()
            .unwrap()
            .child()
            .unwrap()
            .downcast::<gtk::Box>()
            .unwrap();
        let back = controls
            .first_child()
            .unwrap()
            .downcast::<gtk::Button>()
            .unwrap();
        let forward = back
            .next_sibling()
            .unwrap()
            .downcast::<gtk::Button>()
            .unwrap();
        let shuffle = forward
            .next_sibling()
            .unwrap()
            .downcast::<gtk::Button>()
            .unwrap();
        let repeat = shuffle
            .next_sibling()
            .unwrap()
            .downcast::<gtk::Button>()
            .unwrap();
        let error = repeat
            .next_sibling()
            .unwrap()
            .downcast::<gtk::Label>()
            .unwrap();
        assert!(writes.borrow().is_empty());
        back.emit_clicked();
        back.emit_clicked(); // pending requests suppress duplicate clicks
        forward.emit_clicked();
        wait(&|| seek_offsets.borrow().len() == 2 && back.is_sensitive() && forward.is_sensitive());
        assert_eq!(&*seek_offsets.borrow(), &[-10_000_000i64, 10_000_000]);
        shuffle.emit_clicked();
        wait(&|| shuffle.label().as_deref() == Some("Shuffle: on") && shuffle.is_sensitive());
        for expected in ["Repeat: track", "Repeat: playlist", "Repeat: off"] {
            repeat.emit_clicked();
            wait(&|| repeat.label().as_deref() == Some(expected) && repeat.is_sensitive());
        }
        assert_eq!(writes.borrow()[0], ("Shuffle".into(), true.to_variant()));
        for (index, mode) in ["Track", "Playlist", "None"].into_iter().enumerate() {
            assert_eq!(
                writes.borrow()[index + 1],
                ("LoopStatus".into(), mode.to_variant())
            );
        }
        deny.set(true);
        back.emit_clicked();
        wait(&|| !error.label().is_empty() && back.is_sensitive());
        deny.set(false);
        back.emit_clicked();
        wait(&|| seek_offsets.borrow().len() == 4 && back.is_sensitive());
        assert!(error.label().is_empty());
        let position_box = error
            .next_sibling()
            .unwrap()
            .downcast::<gtk::Box>()
            .unwrap();
        let slider = position_box
            .first_child()
            .unwrap()
            .downcast::<gtk::Scale>()
            .unwrap();
        let position_label = slider
            .next_sibling()
            .unwrap()
            .downcast::<gtk::Label>()
            .unwrap();
        let input_test = std::env::var_os("WM_MEDIA_TEST_INPUT").is_some();
        let window = gtk::Window::new();
        if input_test {
            crate::css(&wm_core::Config::load().unwrap());
            root.add_css_class("panel");
            use gtk4_layer_shell::{Edge, Layer, LayerShell};
            window.init_layer_shell();
            window.set_layer(Layer::Top);
            window.set_namespace(Some("wm-media-input-test"));
            window.set_anchor(Edge::Top, true);
            window.set_anchor(Edge::Left, true);
            window.set_default_size(400, 40);
            window.set_child(Some(&root));
        } else {
            controls.remove(&position_box);
            let fixture = gtk::Box::new(gtk::Orientation::Vertical, 4);
            fixture.append(&root);
            fixture.append(&position_box);
            window.set_child(Some(&fixture));
        }
        let click = |widget: &gtk::Widget| {
            let bounds = widget.compute_bounds(&window).unwrap();
            assert!(
                std::process::Command::new("xdotool")
                    .env("DISPLAY", std::env::var("WM_TEST_HOST_DISPLAY").unwrap())
                    .args([
                        "mousemove",
                        "--window",
                        &std::env::var("WM_TEST_HOST_WINDOW").unwrap(),
                        &((bounds.x() + bounds.width() / 2.0) as i32).to_string(),
                        &((bounds.y() + bounds.height() / 2.0) as i32).to_string(),
                        "click",
                        "1"
                    ])
                    .status()
                    .unwrap()
                    .success()
            );
        };
        window.present();
        wait(&|| root.is_mapped() && extra.width() > 0);
        if input_test {
            let until = std::time::Instant::now() + std::time::Duration::from_millis(200);
            wait(&|| std::time::Instant::now() >= until);
            click(extra.upcast_ref());
        }
        wait(&|| position_box.is_mapped() && slider.value() >= 30.0);
        if input_test {
            let artwork = position_box
                .next_sibling()
                .unwrap()
                .downcast::<gtk::Box>()
                .unwrap();
            let picture = artwork
                .first_child()
                .unwrap()
                .downcast::<gtk::Picture>()
                .unwrap();
            wait(&|| picture.paintable().is_some());
            let texture = picture
                .paintable()
                .unwrap()
                .downcast::<gtk::gdk::Texture>()
                .unwrap();
            assert_eq!((texture.width(), texture.height()), (192, 96));
            let mut pixels = vec![0u8; 192 * 96 * 4];
            texture.download(&mut pixels, 192 * 4);
            assert_eq!(&pixels[0..4], &[32, 64, 192, 255]);
        }
        assert_eq!(slider.adjustment().upper(), 180.0);
        assert!(position_label.label().contains("/ 3:00"));
        let reads_before = position_reads.get();
        let before = slider.value();
        wait(&|| slider.value() > before + 0.3);
        assert_eq!(
            position_reads.get(),
            reads_before,
            "playback advances without polling"
        );
        if input_test {
            use gtk4_layer_shell::{KeyboardMode, LayerShell};
            assert_eq!(window.keyboard_mode(), KeyboardMode::OnDemand);
            click(slider.upcast_ref());
            wait(&|| !set_positions.borrow().is_empty() && slider.is_sensitive());
            set_positions.borrow_mut().clear();
        }
        slider.emit_by_name::<bool>("change-value", &[&gtk::ScrollType::Jump, &90.0f64]);
        wait(&|| {
            set_positions.borrow().len() == 1 && slider.is_sensitive() && slider.value() >= 90.0
        });
        assert_eq!(
            set_positions.borrow()[0],
            (
                glib::variant::ObjectPath::try_from("/track/one").unwrap(),
                90_000_000
            )
        );
        connection
            .emit_signal(
                None,
                PATH,
                INTERFACE,
                "Seeked",
                Some(&(12_000_000i64,).to_variant()),
            )
            .unwrap();
        wait(&|| slider.value() >= 12.0 && slider.value() < 13.0);
        connection
            .emit_signal(
                None,
                PATH,
                "org.freedesktop.DBus.Properties",
                "PropertiesChanged",
                Some(
                    &(
                        INTERFACE,
                        BTreeMap::from([("PlaybackStatus", "Paused".to_variant())]),
                        Vec::<String>::new(),
                    )
                        .to_variant(),
                ),
            )
            .unwrap();
        wait(&|| play.label().as_deref() == Some("▶"));
        wait(&|| position_box.is_mapped() && slider.value() == 90.0);
        let paused_reads = position_reads.get();
        let until = std::time::Instant::now() + std::time::Duration::from_millis(600);
        wait(&|| std::time::Instant::now() >= until);
        assert_eq!(slider.value(), 90.0, "paused playback stays still");
        assert_eq!(position_reads.get(), paused_reads);
        if input_test {
            if let Ok(path) = std::env::var("WM_MEDIA_TEST_SCREENSHOT") {
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
        }
        if let Ok(server) = std::env::var("WM_MEDIA_ART_SERVER") {
            let markers = std::path::PathBuf::from(std::env::var("WM_MEDIA_ART_MARKERS").unwrap());
            let art_box = position_box
                .next_sibling()
                .unwrap()
                .downcast::<gtk::Box>()
                .unwrap();
            let picture = art_box
                .first_child()
                .unwrap()
                .downcast::<gtk::Picture>()
                .unwrap();
            let change_art = |url: String| {
                let metadata = BTreeMap::from([
                    ("xesam:title", "Test track".to_variant()),
                    (
                        "mpris:trackid",
                        glib::variant::ObjectPath::try_from("/track/one")
                            .unwrap()
                            .to_variant(),
                    ),
                    ("mpris:length", 180_000_000i64.to_variant()),
                    ("mpris:artUrl", url.to_variant()),
                ]);
                connection
                    .emit_signal(
                        None,
                        PATH,
                        "org.freedesktop.DBus.Properties",
                        "PropertiesChanged",
                        Some(
                            &(
                                INTERFACE,
                                BTreeMap::from([("Metadata", metadata.to_variant())]),
                                Vec::<String>::new(),
                            )
                                .to_variant(),
                        ),
                    )
                    .unwrap();
            };
            change_art(format!("{server}/old"));
            wait(&|| markers.join("old.started").exists() && picture.paintable().is_none());
            change_art(gio::File::for_path(&artwork_path).uri().to_string());
            wait(&|| picture.paintable().is_some());
            let newest = picture.paintable().unwrap();
            assert_eq!(newest.intrinsic_width(), 192);
            std::fs::write(markers.join("old.release"), "ok").unwrap();
            wait(&|| markers.join("old.done").exists());
            let until = std::time::Instant::now() + std::time::Duration::from_millis(200);
            wait(&|| std::time::Instant::now() >= until);
            assert_eq!(
                picture.paintable().as_ref(),
                Some(&newest),
                "old HTTP body must not replace the new thumbnail"
            );
            change_art(format!("{server}/closed"));
            wait(&|| markers.join("closed.started").exists() && picture.paintable().is_none());
            extra.popdown();
            wait(&|| !position_box.is_mapped());
            std::fs::write(markers.join("closed.release"), "ok").unwrap();
            wait(&|| markers.join("closed.done").exists());
            let until = std::time::Instant::now() + std::time::Duration::from_millis(200);
            wait(&|| std::time::Instant::now() >= until);
            assert!(
                picture.paintable().is_none(),
                "closed menu must discard the pending thumbnail"
            );
        }
        if input_test {
            extra.popdown();
        } else {
            position_box.set_visible(false);
        }
        wait(&|| !position_box.is_mapped());
        let hidden_value = slider.value();
        let hidden_reads = position_reads.get();
        let until = std::time::Instant::now() + std::time::Duration::from_millis(600);
        wait(&|| std::time::Instant::now() >= until);
        assert_eq!(
            slider.value(),
            hidden_value,
            "closed menu stops its update timer"
        );
        assert_eq!(position_reads.get(), hidden_reads);
        let changed = BTreeMap::from([
            ("CanControl", false.to_variant()),
            ("PlaybackStatus", "Paused".to_variant()),
            (
                "Metadata",
                BTreeMap::from([("xesam:title", "New track".to_variant())]).to_variant(),
            ),
        ]);
        connection
            .emit_signal(
                None,
                PATH,
                "org.freedesktop.DBus.Properties",
                "PropertiesChanged",
                Some(&(INTERFACE, changed, Vec::<String>::new()).to_variant()),
            )
            .unwrap();
        wait(&|| !next.is_sensitive() && label.text() == "New track");
        assert_eq!(play.label().as_deref(), Some("▶"));
        assert!(!play.is_sensitive());
        assert!(
            !slider.is_sensitive(),
            "missing track metadata disables seeking"
        );
        assert!(!back.is_sensitive() && !forward.is_sensitive());
        assert!(!shuffle.is_sensitive() && !repeat.is_sensitive());
        back.emit_clicked();
        shuffle.emit_clicked();
        assert_eq!(seek_offsets.borrow().len(), 4);
        assert_eq!(writes.borrow().len(), 4);
        next.emit_clicked();
        assert_eq!(calls.borrow().len(), 3);
        name_call("ReleaseName", (name,).to_variant());
        wait(&|| root.first_child().is_none());
        name_call("RequestName", (name, 4u32).to_variant());
        wait(&|| root.first_child().is_some());
        let second_name = "org.mpris.MediaPlayer2.zztest";
        name_call("RequestName", (second_name, 4u32).to_variant());
        wait(&|| {
            root.last_child()
                .is_some_and(|child| child.is::<gtk::MenuButton>())
        });
        let selector = root
            .last_child()
            .unwrap()
            .downcast::<gtk::MenuButton>()
            .unwrap();
        let list = selector
            .popover()
            .unwrap()
            .child()
            .unwrap()
            .downcast::<gtk::ScrolledWindow>()
            .unwrap()
            .child()
            .unwrap()
            .downcast::<gtk::Viewport>()
            .unwrap()
            .child()
            .unwrap()
            .downcast::<gtk::Box>()
            .unwrap();
        let choice = list
            .last_child()
            .unwrap()
            .downcast::<gtk::Button>()
            .unwrap();
        assert_eq!(choice.label().as_deref(), Some("zztest"));
        choice.emit_clicked();
        assert_eq!(selector.label().as_deref(), Some("zztest"));
        assert!(!root.first_child().unwrap().is_visible());
        assert!(
            root.first_child()
                .unwrap()
                .next_sibling()
                .unwrap()
                .is_visible()
        );
        name_call("ReleaseName", (second_name,).to_variant());
        wait(&|| selector.parent().is_none());
        assert!(root.first_child().unwrap().is_visible());
        assert_eq!(selector.label().as_deref(), Some("Auto"));
        if input_test {
            use gtk4_layer_shell::{KeyboardMode, LayerShell};
            assert_eq!(window.keyboard_mode(), KeyboardMode::None);
            std::fs::write(std::env::var("WM_MEDIA_TEST_RECEIPT").unwrap(), "ok").unwrap();
        }
        window.close();
        std::fs::remove_file(artwork_path).unwrap();
        connection.unregister_object(registration).unwrap();
    }
}
