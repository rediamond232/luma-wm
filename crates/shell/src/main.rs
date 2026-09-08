use gtk::{gdk, gio, glib, prelude::*};
use gtk4_layer_shell::{Edge, KeyboardMode, Layer, LayerShell};
use std::{
    cell::RefCell,
    io::{BufRead, Write},
    rc::Rc,
};
use wm_core::{Config, Snapshot};
mod audio;
mod battery;
mod bluetooth;
mod bluetooth_pairing;
mod media;
mod media_art;
mod media_position;
mod network;
mod notifications;
mod tray;
mod tray_menu;
mod tray_watcher;
mod wallpaper;
mod wifi;
fn command(cmd: String) {
    std::thread::spawn(move || {
        if let Err(e) = wm_core::connect_command(&cmd) {
            eprintln!("wm-shell: {e}")
        }
    });
}
fn subscribe() -> async_channel::Receiver<Snapshot> {
    let (tx, rx) = async_channel::bounded(8);
    std::thread::spawn(move || {
        let run = || -> Result<(), Box<dyn std::error::Error>> {
            let mut s = std::os::unix::net::UnixStream::connect(wm_core::socket_path()?)?;
            s.write_all(b"{\"version\":1,\"command\":\"subscribe\"}\n")?;
            for l in std::io::BufReader::new(s).lines() {
                let response: wm_core::Response = serde_json::from_str(&l?)?;
                if tx.send_blocking(response.state).is_err() {
                    break;
                }
            }
            Ok(())
        };
        if let Err(e) = run() {
            eprintln!("shell subscription: {e}")
        }
    });
    rx
}
thread_local! {
    static STYLE: gtk::CssProvider = {
        let provider = gtk::CssProvider::new();
        if let Some(display) = gdk::Display::default() {
            gtk::style_context_add_provider_for_display(
                // The desktop's explicit palette must win over a host GTK
                // stylesheet. This provider affects only this shell process.
                &display, &provider, gtk::STYLE_PROVIDER_PRIORITY_USER + 1,
            );
        }
        provider
    };
}
fn css(config: &Config) {
    let t = &config.theme;
    let css = format!(
        r#"
window {{ background: transparent; color: {fg}; font-family: "{font}"; font-size: {size}px; }}
.panel, .launcher, .notification {{ background: alpha({bg}, {opacity}); border: {border}px solid alpha({accent}, .35); border-radius: {radius}px; padding: 6px 12px; }}
button {{ background: transparent; color: {muted}; border: 0; box-shadow: none; border-radius: 7px; padding: 4px 10px; min-height: 18px; }}
button label, button image, button arrow {{ color: {muted}; }}
tooltip {{ background: {bg}; color: {fg}; border: 1px solid alpha({accent}, .35); border-radius: 8px; padding: 6px 8px; }}
tooltip label {{ color: {fg}; }}
popover button, popover button label, popover button image {{ color: {fg}; }}
button:checked label, button:checked image, button:checked arrow, button.active label {{ color: {accent}; }}
button:hover, row:selected {{ background: alpha({accent}, .15); color: {fg}; }}
button.active, button:checked {{ background: alpha({accent}, .22); color: {accent}; }}
button:disabled, button:disabled label, button:disabled image, button:disabled arrow {{ color: alpha({muted}, .55); }}
button:focus, entry:focus-within {{ outline: 2px solid alpha({accent}, .7); outline-offset: -2px; }}
popover {{ background: transparent; color: {fg}; font-family: "{font}"; font-size: {size}px; }}
popover > contents, popover > arrow {{ background: {bg}; border: {border}px solid alpha({accent}, .35); }}
popover > contents {{ border-radius: {radius}px; box-shadow: none; padding: 0; }}
scale trough {{ background: alpha({muted}, .25); border: 0; border-radius: 3px; min-height: 4px; }}
scale highlight {{ background: {accent}; border: 0; border-radius: 3px; }}
scale slider {{ background: {accent}; border: 0; box-shadow: none; min-width: 12px; min-height: 12px; }}
scale:disabled highlight, scale:disabled slider {{ background: {muted}; }}
checkbutton check {{ background: alpha({muted}, .2); color: {fg}; border: 1px solid {muted}; }}
checkbutton check:checked {{ background: {accent}; color: {bg}; border-color: {accent}; }}
switch {{ background: alpha({muted}, .25); border: 0; }}
switch:checked {{ background: {accent}; }}
switch slider {{ background: {fg}; border: 0; box-shadow: none; }}
entry {{ background: alpha({bg}, .9); color: {fg}; border: 0; box-shadow: none; padding: 12px; }}
list, row {{ background: transparent; color: {fg}; }}
row {{ padding: 8px; border-radius: 8px; }}
.title {{ font-weight: bold; }}
.muted {{ color: {muted}; }}
"#,
        fg = t.foreground,
        bg = t.background,
        accent = t.accent,
        muted = t.muted,
        font = t.font.replace(['"', '\\'], ""),
        size = t.font_size,
        opacity = t.opacity,
        border = t.border,
        radius = t.radius
    );
    STYLE.with(|provider| provider.load_from_string(&css));
}
fn layer_window(
    app: &gtk::Application,
    monitor: Option<&gdk::Monitor>,
    layer: Layer,
    namespace: &str,
) -> gtk::ApplicationWindow {
    let w = gtk::ApplicationWindow::builder()
        .application(app)
        .title(namespace)
        .build();
    w.init_layer_shell();
    w.set_layer(layer);
    w.set_namespace(Some(namespace));
    if let Some(m) = monitor {
        w.set_monitor(Some(m))
    }
    w
}
fn monitors() -> Vec<gdk::Monitor> {
    let Some(d) = gdk::Display::default() else {
        return vec![];
    };
    let m = d.monitors();
    (0..m.n_items())
        .filter_map(|i| m.item(i)?.downcast().ok())
        .collect()
}
fn main() {
    gdk::set_allowed_backends("wayland");
    let args: Vec<String> = std::env::args().collect();
    let mode = if args.iter().any(|s| s == "--wallpaper") {
        "wallpaper"
    } else if args.iter().any(|s| s == "--launcher") {
        "launcher"
    } else {
        "shell"
    };
    let mut application_id = format!("org.customwm.{mode}");
    if mode == "launcher" {
        use std::hash::{Hash, Hasher};
        let mut hash = std::collections::hash_map::DefaultHasher::new();
        wm_core::socket_path().ok().hash(&mut hash);
        application_id.push_str(&format!(".s{:x}", hash.finish()));
    }
    let app = gtk::Application::builder()
        .application_id(application_id)
        .flags(if mode == "launcher" {
            gio::ApplicationFlags::FLAGS_NONE
        } else {
            gio::ApplicationFlags::NON_UNIQUE
        })
        .build();
    app.connect_activate(move |app| {
        if mode == "launcher" {
            if let Some(window) = app.windows().first() {
                window.present();
                return;
            }
        }
        let c = Config::load().unwrap_or_else(|e| {
            eprintln!("config: {e}");
            Config::default()
        });
        css(&c);
        match mode {
            "launcher" => launcher(app, &c),
            "wallpaper" => wallpaper::start(app, &c),
            _ => shell(app, &c),
        }
    });
    app.run_with_args(&["wm-shell"]);
}
fn shell(app: &gtk::Application, c: &Config) {
    tray_watcher::start(app);
    let notification_center = notifications::start(app);
    let config = Rc::new(RefCell::new(c.clone()));
    let timers = Rc::new(RefCell::new(Vec::<glib::SourceId>::new()));
    let bars = Rc::new(RefCell::new(Vec::<(
        gtk::ApplicationWindow,
        String,
        gtk::Label,
        Vec<gtk::Button>,
    )>::new()));
    let build = {
        let bars = bars.clone();
        let app = app.clone();
        let config = config.clone();
        let timers = timers.clone();
        let notification_center = notification_center.clone();
        move || {
            let c = config.borrow();
            for timer in timers.borrow_mut().drain(..) {
                timer.remove();
            }
            for (w, _, _, _) in bars.borrow_mut().drain(..) {
                w.close()
            }
            for monitor in monitors() {
                let name = monitor
                    .connector()
                    .map(|s| s.to_string())
                    .unwrap_or_default();
                let w = layer_window(&app, Some(&monitor), Layer::Top, "wm-bar");
                w.set_anchor(Edge::Left, true);
                w.set_anchor(Edge::Right, true);
                w.set_anchor(
                    if c.shell.position == "bottom" {
                        Edge::Bottom
                    } else {
                        Edge::Top
                    },
                    true,
                );
                w.auto_exclusive_zone_enable();
                w.set_height_request(c.shell.height);
                let root = gtk::CenterBox::new();
                root.add_css_class("panel");
                let left = gtk::Box::new(gtk::Orientation::Horizontal, 3);
                let right = gtk::Box::new(gtk::Orientation::Horizontal, 12);
                let mut buttons = vec![];
                if c.shell.modules.iter().any(|m| m == "workspaces") {
                    for n in 1..=c.layout.workspaces {
                        let b = gtk::Button::with_label(&n.to_string());
                        let output = name.clone();
                        b.connect_clicked(move |_| command(format!("workspace {n} {output}")));
                        left.append(&b);
                        buttons.push(b)
                    }
                }
                let title = gtk::Label::new(None);
                title.set_ellipsize(gtk::pango::EllipsizeMode::End);
                title.set_max_width_chars(55);
                if c.shell.modules.iter().any(|m| m == "title") {
                    root.set_center_widget(Some(&title));
                }
                if c.shell.modules.iter().any(|m| m == "audio") {
                    right.append(&audio::widget());
                }
                if c.shell.modules.iter().any(|m| m == "network") {
                    right.append(&network::button());
                }
                if c.shell.modules.iter().any(|m| m == "bluetooth") {
                    right.append(&bluetooth::button());
                }
                if c.shell.modules.iter().any(|m| m == "media") {
                    right.append(&media::widget());
                }
                if c.shell.modules.iter().any(|m| m == "tray") {
                    let geometry = monitor.geometry();
                    right.append(&tray::widget((
                        geometry.x(),
                        geometry.y(),
                        geometry.height(),
                        c.shell.position == "bottom",
                    )));
                }
                if c.shell.modules.iter().any(|m| m == "battery") {
                    right.append(&battery::widget());
                }
                if c.shell.modules.iter().any(|m| m == "clock") {
                    let l = gtk::Label::new(None);
                    right.append(&l);
                    let update = move || {
                        if let Ok(now) = glib::DateTime::now_local() {
                            if let Ok(s) = now.format("%a  %H:%M") {
                                l.set_text(&s)
                            }
                        }
                    };
                    update();
                    timers
                        .borrow_mut()
                        .push(glib::timeout_add_seconds_local(30, move || {
                            update();
                            glib::ControlFlow::Continue
                        }));
                }
                if c.shell.modules.iter().any(|m| m == "notifications") {
                    let button = gtk::Button::with_label("Notifications");
                    let center = notification_center.clone();
                    let application = app.clone();
                    button.connect_clicked(move |_| center.show(&application));
                    right.append(&button);
                }
                let launch = gtk::Button::with_label("⌕");
                launch.connect_clicked(|_| command("launcher".into()));
                left.prepend(&launch);
                root.set_start_widget(Some(&left));
                root.set_end_widget(Some(&right));
                w.set_child(Some(&root));
                w.present();
                bars.borrow_mut().push((w, name, title, buttons));
            }
        }
    };
    let build = Rc::new(build);
    build();
    if let Some(d) = gdk::Display::default() {
        let model = d.monitors();
        let monitor_handlers = Rc::new(RefCell::new(
            Vec::<(gdk::Monitor, glib::SignalHandlerId)>::new(),
        ));
        let reconnect = Rc::new({
            let model = model.clone();
            let handlers = monitor_handlers.clone();
            let build = build.clone();
            move || {
                for (monitor, handler) in handlers.borrow_mut().drain(..) {
                    monitor.disconnect(handler);
                }
                for index in 0..model.n_items() {
                    let Some(monitor) = model
                        .item(index)
                        .and_then(|item| item.downcast::<gdk::Monitor>().ok())
                    else {
                        continue;
                    };
                    let rebuild = build.clone();
                    let handler = monitor.connect_notify_local(None, move |_, property| {
                        if matches!(property.name(), "geometry" | "scale-factor") {
                            rebuild();
                        }
                    });
                    handlers.borrow_mut().push((monitor, handler));
                }
            }
        });
        reconnect();
        model.connect_items_changed({
            let reconnect = reconnect.clone();
            let build = build.clone();
            move |_, _, _, _| {
                reconnect();
                build();
            }
        });
    }
    let rx = subscribe();
    glib::MainContext::default().spawn_local(async move {
        while let Ok(s) = rx.recv().await {
            for (_, name, title, buttons) in bars.borrow().iter() {
                if let Some(o) = s.outputs.iter().find(|o| &o.name == name) {
                    for (i, b) in buttons.iter().enumerate() {
                        if i + 1 == o.workspace as usize {
                            b.add_css_class("active")
                        } else {
                            b.remove_css_class("active")
                        }
                    }
                    title.set_text(
                        &s.windows
                            .iter()
                            .find(|w| Some(w.id) == s.focused && &w.output == name)
                            .map(|w| w.title.clone())
                            .unwrap_or_default(),
                    );
                }
            }
        }
    });
    // Rebuild geometry and modules together with the stylesheet.
    let path = wm_core::config_path();
    if let Some(parent) = path.parent().filter(|p| p.exists()) {
        use notify::Watcher;
        let (tx, rx) = async_channel::bounded(1);
        let target = path.clone();
        if let Ok(mut watcher) =
            notify::recommended_watcher(move |e: Result<notify::Event, notify::Error>| {
                if let Ok(e) = e {
                    if !matches!(e.kind, notify::EventKind::Access(_)) && e.paths.contains(&target)
                    {
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
                        // The compositor and shell watch the same file. Let the compositor
                        // publish output scale/transform changes before recreating layer
                        // surfaces, and collapse editor replace/write event bursts.
                        glib::timeout_future(std::time::Duration::from_millis(75)).await;
                        while rx.try_recv().is_ok() {}
                        if let Ok(c) = Config::load() {
                            css(&c);
                            *config.borrow_mut() = c;
                            build();
                        }
                    }
                });
            }
        }
    }
}
#[derive(Clone)]
enum Entry {
    App(gio::AppInfo),
    Window(u64, String, String),
    Command(String),
    Power(&'static str, &'static str),
}
fn launcher(app: &gtk::Application, c: &Config) {
    let w = layer_window(app, None, Layer::Overlay, "wm-launcher");
    w.set_keyboard_mode(KeyboardMode::Exclusive);
    w.set_default_size(640, 480);
    let root = gtk::Box::new(gtk::Orientation::Vertical, 8);
    root.add_css_class("launcher");
    let search = gtk::SearchEntry::new();
    search.set_placeholder_text(Some("Search apps · @ windows · > command · : power"));
    let list = gtk::ListBox::new();
    list.set_selection_mode(gtk::SelectionMode::Single);
    let scroll = gtk::ScrolledWindow::builder()
        .min_content_height(360)
        .max_content_height(550)
        .child(&list)
        .build();
    root.append(&search);
    root.append(&scroll);
    w.set_child(Some(&root));
    let apps: Vec<_> = gio::AppInfo::all()
        .into_iter()
        .filter(|a| a.should_show())
        .collect();
    let state = Rc::new(RefCell::new(Snapshot::default()));
    let entries = Rc::new(RefCell::new(Vec::new()));
    let refill = {
        let list = list.clone();
        let entries = entries.clone();
        let state = state.clone();
        move |text: &str| {
            while let Some(child) = list.first_child() {
                list.remove(&child)
            }
            let q = text.to_lowercase();
            let items: Vec<Entry> = if let Some(q) = q.strip_prefix('@') {
                state
                    .borrow()
                    .windows
                    .iter()
                    .filter(|w| {
                        w.title.to_lowercase().contains(q.trim())
                            || w.app_id.to_lowercase().contains(q.trim())
                    })
                    .map(|w| Entry::Window(w.id, w.title.clone(), w.app_id.clone()))
                    .collect()
            } else if let Some(cmd) = text.strip_prefix('>') {
                vec![Entry::Command(cmd.trim().into())]
            } else if q.starts_with(':') {
                vec![
                    Entry::Power("Lock", "lock"),
                    Entry::Power("Log out", "quit"),
                    Entry::Power("Suspend", "suspend"),
                    Entry::Power("Reboot", "reboot"),
                    Entry::Power("Power off", "poweroff"),
                ]
            } else {
                let mut a: Vec<_> = apps
                    .iter()
                    .filter(|a| a.display_name().to_lowercase().contains(&q))
                    .cloned()
                    .collect();
                a.sort_by_key(|a| {
                    (
                        !a.display_name().to_lowercase().starts_with(&q),
                        a.display_name().to_lowercase(),
                    )
                });
                a.into_iter().take(30).map(Entry::App).collect()
            };
            for item in &items {
                let (text, detail, image) = match item {
                    Entry::App(a) => (
                        a.display_name().to_string(),
                        a.description().unwrap_or_default().to_string(),
                        a.icon()
                            .map(|icon| gtk::Image::from_gicon(&icon))
                            .unwrap_or_else(|| {
                                gtk::Image::from_icon_name("application-x-executable-symbolic")
                            }),
                    ),
                    Entry::Window(_, title, app_id) => (
                        title.clone(),
                        app_id.clone(),
                        gtk::Image::from_icon_name("focus-windows-symbolic"),
                    ),
                    Entry::Command(command) => (
                        format!("Run: {command}"),
                        "Shell command".into(),
                        gtk::Image::from_icon_name("utilities-terminal-symbolic"),
                    ),
                    Entry::Power(title, command) => (
                        title.to_string(),
                        "Session action".into(),
                        gtk::Image::from_icon_name(match *command {
                            "lock" => "system-lock-screen-symbolic",
                            "suspend" => "system-suspend-symbolic",
                            "reboot" => "system-reboot-symbolic",
                            "poweroff" => "system-shutdown-symbolic",
                            _ => "system-log-out-symbolic",
                        }),
                    ),
                };
                image.set_pixel_size(28);
                image.set_valign(gtk::Align::Center);
                let labels = gtk::Box::new(gtk::Orientation::Vertical, 1);
                labels.set_hexpand(true);
                let title = gtk::Label::new(Some(&text));
                title.add_css_class("title");
                title.set_xalign(0.0);
                title.set_ellipsize(gtk::pango::EllipsizeMode::End);
                labels.append(&title);
                if !detail.trim().is_empty() && detail != text {
                    let detail = gtk::Label::new(Some(&detail));
                    detail.add_css_class("muted");
                    detail.set_xalign(0.0);
                    detail.set_ellipsize(gtk::pango::EllipsizeMode::End);
                    labels.append(&detail);
                }
                let row = gtk::Box::new(gtk::Orientation::Horizontal, 12);
                row.append(&image);
                row.append(&labels);
                list.append(&row);
            }
            *entries.borrow_mut() = items;
            if let Some(row) = list.row_at_index(0) {
                list.select_row(Some(&row));
            }
        }
    };
    let refill = Rc::new(refill);
    refill("");
    let refresh = refill.clone();
    search.connect_search_changed(move |s| refresh(&s.text()));
    let rx = subscribe();
    let search_copy = search.clone();
    glib::MainContext::default().spawn_local(async move {
        while let Ok(s) = rx.recv().await {
            let changed = state.borrow().windows != s.windows;
            *state.borrow_mut() = s;
            if changed && search_copy.text().starts_with('@') {
                refill(&search_copy.text());
            }
        }
    });
    let activate = {
        let entries = entries.clone();
        let w = w.clone();
        move |idx: i32| {
            if let Some(e) = entries.borrow().get(idx as usize) {
                match e {
                    Entry::App(a) => {
                        if let Err(e) = a.launch(&[], gio::AppLaunchContext::NONE) {
                            eprintln!("launch: {e}")
                        }
                    }
                    Entry::Window(id, _, _) => command(format!("focus {id}")),
                    Entry::Command(c) => {
                        command(format!(
                            "exec {}",
                            serde_json::to_string(&["sh", "-lc", c]).unwrap()
                        ));
                    }
                    Entry::Power(_, cmd) => {
                        if ["lock", "quit"].contains(cmd) {
                            command(cmd.to_string())
                        } else {
                            let _ = std::process::Command::new("systemctl").arg(cmd).spawn();
                        }
                    }
                }
                w.close();
            }
        }
    };
    let activate = Rc::new(activate);
    let a = activate.clone();
    list.connect_row_activated(move |_, r| a(r.index()));
    let l = list.clone();
    let a = activate.clone();
    search.connect_activate(move |_| {
        if let Some(r) = l.selected_row() {
            a(r.index())
        }
    });
    let keys = gtk::EventControllerKey::new();
    keys.set_propagation_phase(gtk::PropagationPhase::Capture);
    let win = w.clone();
    let l = list.clone();
    keys.connect_key_pressed(move |_, key, _, _| {
        if key == gdk::Key::Escape {
            win.close();
            return glib::Propagation::Stop;
        }
        if key == gdk::Key::Down || key == gdk::Key::Up {
            let i = l.selected_row().map(|r| r.index()).unwrap_or(0)
                + if key == gdk::Key::Down { 1 } else { -1 };
            if let Some(r) = l.row_at_index(i.max(0)) {
                l.select_row(Some(&r));
            }
            return glib::Propagation::Stop;
        }
        glib::Propagation::Proceed
    });
    w.add_controller(keys);
    let _ = c;
    w.present();
    search.grab_focus();
}
