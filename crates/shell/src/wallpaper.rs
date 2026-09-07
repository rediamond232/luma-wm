use super::*;
use gstreamer::{self as gst, prelude::*};
struct Background {
    window: gtk::ApplicationWindow,
    name: String,
    player: Option<Video>,
}
struct Video {
    player: gst::Element,
    failed: Rc<std::cell::Cell<bool>>,
    requested: std::cell::Cell<gst::State>,
    picture: gtk::Picture,
    _watch: gst::bus::BusWatchGuard,
}
impl Video {
    fn request(&self, target: gst::State) {
        if self.failed.get() || self.requested.get() == target {
            return;
        }
        self.requested.set(target);
        if let Err(error) = self.player.set_state(target) {
            self.failed.set(true);
            let _ = self.player.set_state(gst::State::Null);
            self.picture.set_visible(false);
            eprintln!("wallpaper state transition failed: {error}");
        }
    }
}
impl Drop for Video {
    fn drop(&mut self) {
        let _ = self.player.set_state(gst::State::Null);
    }
}
impl Drop for Background {
    fn drop(&mut self) {
        self.window.close();
    }
}
pub fn start(app: &gtk::Application, c: &Config) {
    let config = Rc::new(RefCell::new(c.clone()));
    if let Err(e) = gst::init() {
        eprintln!("GStreamer initialization: {e}");
        return;
    }
    if let Err(e) = gstgtk4::plugin_register_static() {
        eprintln!("GTK video sink: {e}");
        return;
    }
    let backgrounds = Rc::new(RefCell::new(Vec::<Background>::new()));
    let rebuild = {
        let backgrounds = backgrounds.clone();
        let app = app.clone();
        move |c: &Config| {
            backgrounds.borrow_mut().clear();
            for monitor in monitors() {
                let name = monitor
                    .connector()
                    .map(|s| s.to_string())
                    .unwrap_or_default();
                let path = c.wallpaper.outputs.get(&name).unwrap_or(&c.wallpaper.path);
                let window = layer_window(&app, Some(&monitor), Layer::Background, "wm-wallpaper");
                for edge in [Edge::Top, Edge::Bottom, Edge::Left, Edge::Right] {
                    window.set_anchor(edge, true)
                }
                window.set_exclusive_zone(-1);
                let overlay = gtk::Overlay::new();
                let bg = gtk::DrawingArea::new();
                let color = wm_core::color(&c.theme.background).unwrap();
                bg.set_draw_func(move |_, cr, _, _| {
                    cr.set_source_rgb(color[0] as f64, color[1] as f64, color[2] as f64);
                    let _ = cr.paint();
                });
                overlay.set_child(Some(&bg));
                let mut player = None;
                if !path.is_empty() {
                    if c.wallpaper.kind == "video" {
                        match video(path, c.wallpaper.fps, &overlay, c.wallpaper.fit == "fill") {
                            Ok(p) => player = Some(p),
                            Err(e) => eprintln!("video wallpaper {path}: {e}"),
                        }
                    } else {
                        let picture = gtk::Picture::for_filename(path);
                        picture.set_can_shrink(true);
                        picture.set_content_fit(if c.wallpaper.fit == "fill" {
                            gtk::ContentFit::Cover
                        } else {
                            gtk::ContentFit::Contain
                        });
                        overlay.add_overlay(&picture);
                    }
                }
                window.set_child(Some(&overlay));
                window.present();
                backgrounds.borrow_mut().push(Background {
                    window,
                    name,
                    player,
                });
            }
        }
    };
    let rebuild = Rc::new(rebuild);
    rebuild(c);
    let snapshot = Rc::new(RefCell::new(Snapshot::default()));
    let battery = Rc::new(RefCell::new(on_battery()));
    let update = {
        let backgrounds = backgrounds.clone();
        let snapshot = snapshot.clone();
        let battery = battery.clone();
        let config = config.clone();
        move || {
            let paused = config.borrow().wallpaper.pause_on_battery && *battery.borrow();
            for bg in backgrounds.borrow().iter() {
                let visible = snapshot
                    .borrow()
                    .outputs
                    .iter()
                    .find(|o| o.name == bg.name)
                    .is_some_and(|o| o.wallpaper_visible && o.active);
                if let Some(p) = &bg.player {
                    let target = if visible && !paused {
                        gst::State::Playing
                    } else {
                        gst::State::Paused
                    };
                    p.request(target);
                }
            }
        }
    };
    let update = Rc::new(update);
    if let Some(d) = gdk::Display::default() {
        let rebuild = rebuild.clone();
        let config = config.clone();
        let update = update.clone();
        d.monitors().connect_items_changed(move |_, _, _, _| {
            rebuild(&config.borrow());
            update();
        });
    }
    let rx = subscribe();
    let snap = snapshot.clone();
    let u = update.clone();
    glib::MainContext::default().spawn_local(async move {
        while let Ok(s) = rx.recv().await {
            *snap.borrow_mut() = s;
            u();
        }
    });
    let fallback = Rc::new(RefCell::new(None::<glib::SourceId>));
    let fallback_shutdown = fallback.clone();
    app.connect_shutdown(move |_| {
        if let Some(timer) = fallback_shutdown.borrow_mut().take() {
            timer.remove();
        }
    });
    let u = update.clone();
    crate::battery::watch_on_battery(app, move |power| {
        if let Some(timer) = fallback.borrow_mut().take() {
            timer.remove();
        }
        *battery.borrow_mut() = power.unwrap_or_else(on_battery);
        u();
        if power.is_none() {
            let battery = battery.clone();
            let u = u.clone();
            *fallback.borrow_mut() = Some(glib::timeout_add_seconds_local(30, move || {
                *battery.borrow_mut() = on_battery();
                u();
                glib::ControlFlow::Continue
            }));
        }
    });
    let target = wm_core::config_path();
    if let Some(parent) = target.parent().filter(|p| p.exists()) {
        use notify::Watcher;
        let (tx, rx) = async_channel::bounded(1);
        let path = target.clone();
        if let Ok(mut watcher) =
            notify::recommended_watcher(move |e: Result<notify::Event, notify::Error>| {
                if let Ok(e) = e {
                    if !matches!(e.kind, notify::EventKind::Access(_)) && e.paths.contains(&path) {
                        let _ = tx.try_send(());
                    }
                }
            })
        {
            if watcher
                .watch(parent, notify::RecursiveMode::NonRecursive)
                .is_ok()
            {
                glib::MainContext::default().spawn_local(async move {
                    let _watcher = watcher;
                    while rx.recv().await.is_ok() {
                        if let Ok(c) = Config::load() {
                            css(&c);
                            if !same_content(&config.borrow(), &c) {
                                rebuild(&c);
                            }
                            *config.borrow_mut() = c;
                            update();
                        }
                    }
                });
            }
        }
    }
}
fn same_content(a: &Config, b: &Config) -> bool {
    a.theme.background == b.theme.background
        && a.wallpaper.kind == b.wallpaper.kind
        && a.wallpaper.path == b.wallpaper.path
        && a.wallpaper.outputs == b.wallpaper.outputs
        && a.wallpaper.fit == b.wallpaper.fit
        && a.wallpaper.fps == b.wallpaper.fps
}
fn on_battery() -> bool {
    std::fs::read_dir("/sys/class/power_supply")
        .into_iter()
        .flatten()
        .flatten()
        .any(|p| {
            std::fs::read_to_string(p.path().join("status"))
                .is_ok_and(|s| s.trim() == "Discharging")
        })
}
fn video(
    path: &str,
    fps: u32,
    overlay: &gtk::Overlay,
    cover: bool,
) -> Result<Video, Box<dyn std::error::Error>> {
    let sink = gst::ElementFactory::make("gtk4paintablesink").build()?;
    let paintable = sink.property::<gdk::Paintable>("paintable");
    let picture = gtk::Picture::for_paintable(&paintable);
    picture.set_can_shrink(true);
    picture.set_content_fit(if cover {
        gtk::ContentFit::Cover
    } else {
        gtk::ContentFit::Contain
    });
    overlay.add_overlay(&picture);
    let uri = glib::filename_to_uri(std::fs::canonicalize(path)?, None)?;
    let audio = gst::ElementFactory::make("fakesink").build()?;
    let filter = gst::parse::bin_from_description(
        &format!("videorate drop-only=true max-rate={fps}"),
        true,
    )?;
    let player = gst::ElementFactory::make("playbin")
        .property("uri", uri.as_str())
        .property("video-sink", &sink)
        .property("audio-sink", &audio)
        .property("video-filter", &filter)
        .property("mute", true)
        .build()?;
    let weak = player.downgrade();
    let failed = Rc::new(std::cell::Cell::new(false));
    let failure = failed.clone();
    let debug = std::env::var_os("WM_WALLPAPER_DEBUG").is_some();
    let display = picture.clone();
    let guard = player
        .bus()
        .ok_or("video bus unavailable")?
        .add_watch_local(move |_, msg| {
            if let Some(p) = weak.upgrade() {
                match msg.view() {
                    gst::MessageView::Eos(_) => {
                        if failure.get() {
                            return glib::ControlFlow::Continue;
                        }
                        if let Err(error) = p.seek_simple(
                            gst::SeekFlags::FLUSH | gst::SeekFlags::KEY_UNIT,
                            gst::ClockTime::ZERO,
                        ) {
                            failure.set(true);
                            let _ = p.set_state(gst::State::Null);
                            picture.set_visible(false);
                            eprintln!("wallpaper loop failed: {error}");
                        } else if debug {
                            eprintln!("wallpaper loop: restarted");
                        }
                    }
                    gst::MessageView::Error(e) => {
                        if !failure.replace(true) {
                            eprintln!("wallpaper playback: {}", e.error());
                        }
                        let _ = p.set_state(gst::State::Null);
                        picture.set_visible(false);
                    }
                    gst::MessageView::StateChanged(state)
                        if debug
                            && msg
                                .src()
                                .is_some_and(|src| src == p.upcast_ref::<gst::Object>()) =>
                    {
                        eprintln!("wallpaper pipeline: {:?}", state.current());
                    }
                    _ => {}
                }
            }
            glib::ControlFlow::Continue
        })?;
    if let Err(error) = player.set_state(gst::State::Paused) {
        let _ = player.set_state(gst::State::Null);
        display.set_visible(false);
        return Err(error.into());
    }
    Ok(Video {
        player,
        failed,
        requested: std::cell::Cell::new(gst::State::Paused),
        picture: display,
        _watch: guard,
    })
}
