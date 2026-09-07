//! Visible-only MPRIS position display. Playback is extrapolated, never polled.
use glib::variant::ObjectPath;
use gtk::{gio, glib, prelude::*};
use std::{
    cell::{Cell, RefCell},
    collections::BTreeMap,
    rc::Rc,
    time::{Duration, Instant},
};

const IFACE: &str = "org.mpris.MediaPlayer2.Player";
struct Position {
    root: glib::WeakRef<gtk::Box>,
    scale: gtk::Scale,
    label: gtk::Label,
    proxy: gio::DBusProxy,
    track: RefCell<Option<(ObjectPath, i64)>>,
    base: Cell<f64>,
    at: Cell<Instant>,
    rate: Cell<f64>,
    revision: Cell<u64>,
    pending: Cell<bool>,
    reading: Cell<bool>,
    reread: Cell<bool>,
    timer: RefCell<Option<glib::SourceId>>,
}
impl Drop for Position {
    fn drop(&mut self) {
        if let Some(timer) = self.timer.get_mut().take() {
            timer.remove();
        }
    }
}
fn flag(proxy: &gio::DBusProxy, key: &str) -> bool {
    proxy
        .cached_property(key)
        .and_then(|v| v.get::<bool>())
        .unwrap_or(false)
}
fn time(value: f64) -> String {
    let seconds = value.max(0.0) as u64 / 1_000_000;
    format!("{}:{:02}", seconds / 60, seconds % 60)
}
impl Position {
    fn current(&self) -> f64 {
        (self.base.get() + self.at.get().elapsed().as_secs_f64() * self.rate.get() * 1_000_000.0)
            .clamp(
                0.0,
                self.track
                    .borrow()
                    .as_ref()
                    .map_or(0.0, |(_, len)| *len as f64),
            )
    }
    fn anchor(&self, value: f64) {
        self.base.set(value.max(0.0));
        self.at.set(Instant::now());
        self.revision.set(self.revision.get().wrapping_add(1));
    }
    fn mapped(&self) -> bool {
        self.root.upgrade().is_some_and(|root| root.is_mapped())
    }
    fn render(&self) {
        let length = self.track.borrow().as_ref().map(|(_, len)| *len);
        self.scale.set_sensitive(
            length.is_some()
                && !self.pending.get()
                && self.proxy.name_owner().is_some()
                && flag(&self.proxy, "CanControl")
                && flag(&self.proxy, "CanSeek"),
        );
        self.scale
            .set_range(0.0, length.unwrap_or(1) as f64 / 1_000_000.0);
        self.scale.set_value(self.current() / 1_000_000.0);
        self.label.set_label(&length.map_or_else(
            || "Position unavailable".into(),
            |len| format!("{} / {}", time(self.current()), time(len as f64)),
        ));
    }
    fn refresh(&self) {
        let value = self.current();
        let metadata = self
            .proxy
            .cached_property("Metadata")
            .and_then(|v| v.get::<BTreeMap<String, glib::Variant>>())
            .unwrap_or_default();
        let track = metadata
            .get("mpris:trackid")
            .and_then(|v| v.get::<ObjectPath>())
            .filter(|id| id.as_str() != "/org/mpris/MediaPlayer2/TrackList/NoTrack")
            .zip(
                metadata
                    .get("mpris:length")
                    .and_then(|v| v.get::<i64>())
                    .filter(|len| *len > 0),
            );
        let changed = *self.track.borrow() != track;
        *self.track.borrow_mut() = track;
        self.anchor(if changed { 0.0 } else { value });
        let playing = self
            .proxy
            .cached_property("PlaybackStatus")
            .and_then(|v| v.get::<String>())
            .as_deref()
            == Some("Playing");
        self.rate.set(if playing {
            self.proxy
                .cached_property("Rate")
                .and_then(|v| v.get::<f64>())
                .filter(|v| v.is_finite())
                .unwrap_or(1.0)
        } else {
            0.0
        });
        self.render();
    }
    fn read(self: &Rc<Self>) {
        if !self.mapped() || self.proxy.name_owner().is_none() {
            return;
        }
        if self.reading.replace(true) {
            self.reread.set(true);
            return;
        }
        let revision = self.revision.get();
        let owner = self.proxy.name_owner();
        let weak = Rc::downgrade(self);
        self.proxy.call(
            "org.freedesktop.DBus.Properties.Get",
            Some(&(IFACE, "Position").to_variant()),
            gio::DBusCallFlags::NONE,
            2000,
            gio::Cancellable::NONE,
            move |result| {
                let Some(state) = weak.upgrade() else { return };
                state.reading.set(false);
                if state.revision.get() == revision && state.proxy.name_owner() == owner {
                    if let Ok(result) = result {
                        if let Some((value,)) = result.get::<(glib::Variant,)>() {
                            if let Some(position) = value.get::<i64>() {
                                state.anchor(position as f64);
                                state.render();
                            }
                        }
                    }
                }
                if state.reread.replace(false) {
                    state.read();
                }
            },
        );
    }
    fn seek(self: &Rc<Self>, seconds: f64) {
        if !seconds.is_finite() || !self.scale.is_sensitive() {
            return;
        }
        let Some((track, length)) = self.track.borrow().clone() else {
            return;
        };
        if self.pending.replace(true) {
            return;
        }
        self.revision.set(self.revision.get().wrapping_add(1));
        self.scale.set_tooltip_text(Some("Track position"));
        let value = (seconds * 1_000_000.0).clamp(0.0, length as f64) as i64;
        let owner = self.proxy.name_owner();
        let weak = Rc::downgrade(self);
        self.render();
        self.proxy.call(
            "SetPosition",
            Some(&(track, value).to_variant()),
            gio::DBusCallFlags::NONE,
            2000,
            gio::Cancellable::NONE,
            move |result| {
                let Some(state) = weak.upgrade() else { return };
                state.pending.set(false);
                if state.proxy.name_owner() == owner {
                    if let Err(error) = result {
                        state
                            .scale
                            .set_tooltip_text(Some(&format!("Seeking failed: {error}")));
                    }
                    state.read();
                }
                state.render();
            },
        );
    }
}
pub fn widget(proxy: &gio::DBusProxy) -> gtk::Box {
    let root = gtk::Box::new(gtk::Orientation::Vertical, 4);
    let scale = gtk::Scale::with_range(gtk::Orientation::Horizontal, 0.0, 1.0, 1.0);
    scale.set_draw_value(false);
    scale.set_width_request(240);
    scale.set_tooltip_text(Some("Track position"));
    scale.update_property(&[gtk::accessible::Property::Label("Track position")]);
    let label = gtk::Label::new(None);
    root.append(&scale);
    root.append(&label);
    let state = Rc::new(Position {
        root: root.downgrade(),
        scale: scale.clone(),
        label,
        proxy: proxy.clone(),
        track: RefCell::new(None),
        base: Cell::new(0.0),
        at: Cell::new(Instant::now()),
        rate: Cell::new(0.0),
        revision: Cell::new(0),
        pending: Cell::new(false),
        reading: Cell::new(false),
        reread: Cell::new(false),
        timer: RefCell::new(None),
    });
    state.refresh();
    let weak = Rc::downgrade(&state);
    scale.connect_change_value(move |_, _, value| {
        if let Some(state) = weak.upgrade() {
            state.seek(value);
        }
        glib::Propagation::Stop
    });
    let weak = Rc::downgrade(&state);
    proxy.connect_local("g-properties-changed", false, move |_| {
        if let Some(state) = weak.upgrade() {
            state.refresh();
            state.read();
        }
        None
    });
    let weak = Rc::downgrade(&state);
    proxy.connect_local("g-signal", false, move |values| {
        if values[2].get::<String>().as_deref() == Ok("Seeked") {
            if let (Some(state), Ok(args)) = (weak.upgrade(), values[3].get::<glib::Variant>()) {
                if let Some((position,)) = args.get::<(i64,)>() {
                    state.anchor(position as f64);
                    state.render();
                }
            }
        }
        None
    });
    let weak = Rc::downgrade(&state);
    root.connect_map(move |_| {
        if let Some(state) = weak.upgrade() {
            state.read();
            if state.timer.borrow().is_none() {
                let weak = Rc::downgrade(&state);
                *state.timer.borrow_mut() = Some(glib::timeout_add_local(
                    Duration::from_millis(250),
                    move || {
                        if let Some(state) = weak.upgrade() {
                            state.render();
                            glib::ControlFlow::Continue
                        } else {
                            glib::ControlFlow::Break
                        }
                    },
                ));
            }
        }
    });
    let weak = Rc::downgrade(&state);
    root.connect_unmap(move |_| {
        if let Some(state) = weak.upgrade() {
            if let Some(timer) = state.timer.borrow_mut().take() {
                timer.remove();
            }
            state.revision.set(state.revision.get().wrapping_add(1));
        }
    });
    root.connect_destroy(move |_| {
        let _ = &state;
    });
    root
}
