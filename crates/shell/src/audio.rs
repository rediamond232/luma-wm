//! Direct audio controls; Pulse-compatible events refresh WirePlumber state.
use gtk::{gio, glib, prelude::*};
use std::{
    cell::{Cell, RefCell},
    ffi::OsStr,
    rc::Rc,
    time::Duration,
};

fn spawn(args: &[&str], flags: gio::SubprocessFlags) -> Result<gio::Subprocess, glib::Error> {
    let launcher = gio::SubprocessLauncher::new(flags);
    launcher.setenv("LC_ALL", "C", true);
    launcher.spawn(&args.iter().map(OsStr::new).collect::<Vec<_>>())
}
async fn run(args: &[&str]) -> Result<String, String> {
    let process = spawn(
        args,
        gio::SubprocessFlags::STDOUT_PIPE | gio::SubprocessFlags::STDERR_PIPE,
    )
    .map_err(|e| e.to_string())?;
    let kill = process.clone();
    let timed_out = Rc::new(Cell::new(false));
    let expired = timed_out.clone();
    let timeout = glib::timeout_add_local(Duration::from_secs(2), move || {
        expired.set(true);
        kill.force_exit();
        glib::ControlFlow::Continue
    });
    let result = process.communicate_utf8_future(None).await;
    timeout.remove();
    if timed_out.get() {
        return Err("Audio command timed out".into());
    }
    let (out, err) = result.map_err(|e| e.to_string())?;
    if process.is_successful() {
        Ok(out.unwrap_or_default().to_string())
    } else {
        Err(err
            .filter(|s| !s.trim().is_empty())
            .map(|s| s.chars().take(256).collect())
            .unwrap_or_else(|| "Audio command failed".into()))
    }
}
fn volume(text: &str) -> Option<(f64, bool)> {
    let mut words = text.split_whitespace();
    if words.next()? != "Volume:" {
        return None;
    }
    let value = words.next()?.parse::<f64>().ok()?;
    (value.is_finite() && (0.0..=10.0).contains(&value))
        .then_some((value, words.any(|word| word == "[MUTED]")))
}
fn relevant_event(line: &[u8]) -> bool {
    let text = String::from_utf8_lossy(line);
    // Sink-input/client churn does not change default output volume.
    text.contains(" on sink #") || text.contains(" on server #") || text.contains(" on card #")
}
struct View {
    button: glib::WeakRef<gtk::MenuButton>,
    scale: glib::WeakRef<gtk::Scale>,
    mute: glib::WeakRef<gtk::Button>,
    error: glib::WeakRef<gtk::Label>,
    devices: glib::WeakRef<gtk::Box>,
    popover: glib::WeakRef<gtk::Popover>,
}
thread_local! {
    static SHARED: RefCell<std::rc::Weak<Audio>> = RefCell::new(std::rc::Weak::new());
}
#[derive(Default)]
struct Audio {
    input: bool,
    microphone: Option<Rc<Audio>>,
    views: RefCell<Vec<View>>,
    data: Cell<Option<(f64, bool)>>,
    last_error: RefCell<String>,
    reading: Cell<bool>,
    again: Cell<bool>,
    writing: Cell<bool>,
    pending_volume: RefCell<Option<Vec<String>>>,
    available: Cell<bool>,
    monitor: RefCell<Option<gio::Subprocess>>,
    retry: RefCell<Option<glib::SourceId>>,
    debounce: RefCell<Option<glib::SourceId>>,
    devices_reading: Cell<bool>,
    devices_again: Cell<bool>,
    devices_dirty: Cell<bool>,
}
impl Drop for Audio {
    fn drop(&mut self) {
        if let Some(process) = self.monitor.get_mut().take() {
            process.force_exit();
        }
        for source in [self.retry.get_mut().take(), self.debounce.get_mut().take()]
            .into_iter()
            .flatten()
        {
            source.remove();
        }
    }
}
impl Audio {
    fn visible(&self) -> bool {
        self.views
            .borrow()
            .iter()
            .any(|view| view.popover.upgrade().is_some_and(|p| p.is_visible()))
    }
    fn target(&self) -> &'static str {
        if self.input {
            "@DEFAULT_AUDIO_SOURCE@"
        } else {
            "@DEFAULT_AUDIO_SINK@"
        }
    }
    fn refresh_devices(self: &Rc<Self>) {
        if !self.visible() {
            return;
        }
        if self.devices_reading.replace(true) {
            self.devices_again.set(true);
            return;
        }
        let weak = Rc::downgrade(self);
        let input = self.input;
        glib::MainContext::default().spawn_local(async move {
            let selected = run(&[
                "pactl",
                if input {
                    "get-default-source"
                } else {
                    "get-default-sink"
                },
            ])
            .await;
            let devices = run(&[
                "pactl",
                "-f",
                "json",
                "list",
                if input { "sources" } else { "sinks" },
            ])
            .await
            .and_then(|text| outputs(&text).ok_or_else(|| "Invalid audio device list".into()));
            let Some(state) = weak.upgrade() else {
                return;
            };
            state.devices_reading.set(false);
            for view in state.views.borrow().iter() {
                if !view.popover.upgrade().is_some_and(|p| p.is_visible()) {
                    continue;
                }
                let Some(list) = view.devices.upgrade() else {
                    continue;
                };
                let mut child = list.first_child();
                let mut focused = None;
                while let Some(widget) = child {
                    if widget.has_focus() {
                        focused = Some(widget.widget_name());
                    }
                    child = widget.next_sibling();
                }
                while let Some(child) = list.first_child() {
                    list.remove(&child);
                }
                match (&selected, &devices) {
                    (Ok(selected), Ok(devices)) if !devices.is_empty() => {
                        for (name, description) in devices {
                            let active = selected.trim() == name;
                            let button = gtk::Button::with_label(&format!(
                                "{}{}",
                                if active { "✓ " } else { "" },
                                description
                            ));
                            button.set_widget_name(name);
                            button.set_tooltip_text(Some(description));
                            if let Some(label) =
                                button.child().and_then(|c| c.downcast::<gtk::Label>().ok())
                            {
                                label.set_max_width_chars(32);
                                label.set_ellipsize(gtk::pango::EllipsizeMode::End);
                            }
                            if active {
                                button.add_css_class("active");
                            }
                            let weak = Rc::downgrade(&state);
                            let restore = focused.as_deref() == Some(name.as_str());
                            let name = name.clone();
                            button.connect_clicked(move |_| {
                                if let Some(state) = weak.upgrade() {
                                    state.change(vec![
                                        "pactl".into(),
                                        if input {
                                            "set-default-source"
                                        } else {
                                            "set-default-sink"
                                        }
                                        .into(),
                                        name.clone(),
                                    ]);
                                }
                            });
                            list.append(&button);
                            if restore {
                                button.grab_focus();
                            }
                        }
                    }
                    _ => list.append(&gtk::Label::new(Some(if input {
                        "Inputs unavailable"
                    } else {
                        "Outputs unavailable"
                    }))),
                }
            }
            state.sensitive();
            if state.devices_again.replace(false) {
                state.refresh_devices();
            }
        });
    }
    fn sensitive(&self) {
        let enabled = self.available.get() && !self.writing.get();
        for view in self.views.borrow().iter() {
            if let Some(scale) = view.scale.upgrade() {
                scale.set_sensitive(self.available.get());
            }
            if let Some(mute) = view.mute.upgrade() {
                mute.set_sensitive(enabled);
            }
            if let Some(devices) = view.devices.upgrade() {
                devices.set_sensitive(!self.writing.get());
            }
        }
    }
    fn error(&self, text: &str) {
        self.last_error.replace(text.to_string());
        for view in self.views.borrow().iter() {
            if let Some(error) = view.error.upgrade() {
                error.set_text(text);
                error.set_visible(!text.is_empty());
            }
        }
    }
    fn render(&self) {
        self.views
            .borrow_mut()
            .retain(|view| view.button.upgrade().is_some());
        for view in self.views.borrow().iter() {
            if let Some(button) = view.button.upgrade() {
                if self.input && !view.popover.upgrade().is_some_and(|p| p.is_visible()) {
                    button.set_label("Microphone");
                } else if let Some((value, muted)) =
                    self.data.get().filter(|_| self.available.get())
                {
                    button.set_label(&if muted {
                        if self.input { "Mic muted" } else { "Muted" }.into()
                    } else {
                        format!(
                            "{} {:.0}%",
                            if self.input { "Mic" } else { "Vol" },
                            value * 100.0
                        )
                    });
                } else {
                    button.set_label(if self.input {
                        "Microphone"
                    } else {
                        "Audio unavailable"
                    });
                }
            }
            if let Some((value, muted)) = self.data.get() {
                if let Some(scale) = view.scale.upgrade() {
                    scale.set_value((value * 100.0).min(100.0));
                }
                if let Some(mute) = view.mute.upgrade() {
                    mute.set_label(if muted { "Unmute" } else { "Mute" });
                }
            }
            if let Some(error) = view.error.upgrade() {
                error.set_text(&self.last_error.borrow());
                error.set_visible(!self.last_error.borrow().is_empty());
            }
        }
        self.sensitive();
    }
    fn refresh(self: &Rc<Self>) {
        if self.reading.replace(true) {
            self.again.set(true);
            return;
        }
        let weak = Rc::downgrade(self);
        glib::MainContext::default().spawn_local(async move {
            let Some(state) = weak.upgrade() else {
                return;
            };
            let target = state.target();
            drop(state);
            let result = run(&["wpctl", "get-volume", target]).await;
            let Some(state) = weak.upgrade() else {
                return;
            };
            state.reading.set(false);
            match result
                .and_then(|text| volume(&text).ok_or_else(|| "Audio state unavailable".into()))
            {
                Ok((value, muted)) => {
                    if !state.available.replace(true) {
                        state.error("");
                    }
                    state.data.set(Some((value, muted)));
                }
                Err(error) => {
                    state.available.set(false);
                    state.error(&error);
                }
            }
            state.render();
            if state.again.replace(false) {
                state.refresh();
            }
        });
    }
    fn change(self: &Rc<Self>, args: Vec<String>) {
        if self.writing.replace(true) {
            if args.get(1).is_some_and(|arg| arg == "set-volume") {
                self.pending_volume.replace(Some(args));
            }
            return;
        }
        let restore_control_focus = self.views.borrow().iter().find_map(|view| {
            let mute = view
                .mute
                .upgrade()
                .filter(|button| button.has_focus())
                .map(|button| button.downgrade());
            if mute.is_some() {
                return mute;
            }
            let mut child = view.devices.upgrade().and_then(|list| list.first_child());
            while let Some(widget) = child {
                if widget.has_focus() {
                    return widget
                        .downcast::<gtk::Button>()
                        .ok()
                        .map(|button| button.downgrade());
                }
                child = widget.next_sibling();
            }
            None
        });
        self.error("");
        self.sensitive();
        let weak = Rc::downgrade(self);
        let device_change = args.first().is_some_and(|arg| arg == "pactl");
        glib::MainContext::default().spawn_local(async move {
            let result = run(&args.iter().map(String::as_str).collect::<Vec<_>>()).await;
            if let Some(state) = weak.upgrade() {
                state.writing.set(false);
                if let Err(error) = result {
                    state.error(&error);
                }
                state.sensitive();
                if device_change {
                    state.refresh_devices();
                }
                if let Some(button) = restore_control_focus
                    .and_then(|button| button.upgrade())
                    .filter(|button| button.is_mapped())
                {
                    button.grab_focus();
                }
                let pending = state.pending_volume.borrow_mut().take();
                if let Some(args) = pending {
                    state.change(args);
                } else {
                    state.refresh();
                }
            }
        });
    }
    fn changed(self: &Rc<Self>) {
        if self.debounce.borrow().is_some() {
            return;
        }
        let weak = Rc::downgrade(self);
        self.debounce.replace(Some(glib::timeout_add_local_once(
            Duration::from_millis(100),
            move || {
                if let Some(state) = weak.upgrade() {
                    state.debounce.borrow_mut().take();
                    state.refresh();
                    if state.devices_dirty.replace(false) {
                        state.refresh_devices();
                    }
                }
            },
        )));
    }
    fn reconnect(self: &Rc<Self>) {
        if self.retry.borrow().is_some() {
            return;
        }
        let weak = Rc::downgrade(self);
        self.retry
            .replace(Some(glib::timeout_add_seconds_local_once(5, move || {
                if let Some(state) = weak.upgrade() {
                    state.retry.borrow_mut().take();
                    state.refresh();
                    state.refresh_devices();
                    if let Some(microphone) = &state.microphone {
                        if microphone.visible() {
                            microphone.refresh();
                            microphone.refresh_devices();
                        }
                    }
                    state.subscribe();
                }
            })));
    }
    fn subscribe(self: &Rc<Self>) {
        let Ok(process) = spawn(
            &["pactl", "subscribe"],
            gio::SubprocessFlags::STDOUT_PIPE | gio::SubprocessFlags::STDERR_SILENCE,
        ) else {
            self.reconnect();
            return;
        };
        let stream = gio::DataInputStream::new(&process.stdout_pipe().unwrap());
        self.monitor.replace(Some(process.clone()));
        let weak = Rc::downgrade(self);
        glib::MainContext::default().spawn_local(async move {
            while let Ok(Some(line)) = stream.read_line_future(glib::Priority::DEFAULT).await {
                let Some(state) = weak.upgrade() else {
                    break;
                };
                let text = String::from_utf8_lossy(&line);
                if let Some(microphone) = &state.microphone {
                    if microphone.visible()
                        && (text.contains(" on source #")
                            || text.contains(" on server #")
                            || text.contains(" on card #"))
                    {
                        if !text.contains("Event 'change' on source #") {
                            microphone.devices_dirty.set(true);
                        }
                        microphone.changed();
                    }
                }
                if relevant_event(&line) {
                    if text.contains(" on server #")
                        || text.contains(" on card #")
                        || text.contains("Event 'new' on sink #")
                        || text.contains("Event 'remove' on sink #")
                    {
                        state.devices_dirty.set(true);
                    }
                    state.changed();
                }
            }
            let _ = process.wait_future().await;
            if let Some(state) = weak.upgrade() {
                state.monitor.borrow_mut().take();
                state.reconnect();
            }
        });
    }
}
pub(crate) fn widget() -> gtk::MenuButton {
    let (state, fresh) = SHARED.with(|slot| {
        if let Some(state) = slot.borrow().upgrade() {
            return (state, false);
        }
        let mut microphone = Audio::default();
        microphone.input = true;
        let mut state = Audio::default();
        state.microphone = Some(Rc::new(microphone));
        let state = Rc::new(state);
        slot.replace(Rc::downgrade(&state));
        (state, true)
    });
    let button = controls(&state);
    if fresh {
        state.subscribe();
        state.refresh();
    }
    button
}
fn controls(state: &Rc<Audio>) -> gtk::MenuButton {
    let input = state.input;
    let button = gtk::MenuButton::builder().label("Audio").build();
    let content = gtk::Box::new(gtk::Orientation::Vertical, 8);
    content.set_margin_top(8);
    content.set_margin_bottom(8);
    content.set_margin_start(12);
    content.set_margin_end(12);
    content.append(&gtk::Label::new(Some(if input {
        "Microphone volume"
    } else {
        "Output volume"
    })));
    let scale = gtk::Scale::with_range(gtk::Orientation::Horizontal, 0.0, 100.0, 1.0);
    scale.set_size_request(240, -1);
    scale.set_draw_value(true);
    scale.set_digits(0);
    scale.update_property(&[gtk::accessible::Property::Label(if input {
        "Microphone volume"
    } else {
        "Output volume"
    })]);
    content.append(&scale);
    let mute = gtk::Button::with_label("Mute");
    content.append(&mute);
    let mixer = gtk::Button::with_label("Sound settings…");
    mixer.connect_clicked(|_| {
        let _ = std::process::Command::new("pavucontrol").spawn();
    });
    content.append(&mixer);
    if let Some(microphone) = &state.microphone {
        let controls = controls(microphone);
        controls.set_widget_name("microphone-controls");
        content.append(&controls);
    }
    content.append(&gtk::Label::new(Some(if input {
        "Input device"
    } else {
        "Output device"
    })));
    let devices = gtk::Box::new(gtk::Orientation::Vertical, 2);
    devices.append(&gtk::Label::new(Some("Loading…")));
    let scroll = gtk::ScrolledWindow::builder()
        .child(&devices)
        .hscrollbar_policy(gtk::PolicyType::Never)
        .propagate_natural_height(true)
        .max_content_height(180)
        .build();
    content.append(&scroll);
    let error = gtk::Label::new(None);
    error.set_wrap(true);
    error.set_max_width_chars(40);
    error.set_visible(false);
    content.append(&error);
    let popover = gtk::Popover::builder().child(&content).build();
    button.set_popover(Some(&popover));
    if !input {
        crate::network::popover_keyboard_focus(&button, &popover);
    }
    state.views.borrow_mut().push(View {
        button: button.downgrade(),
        scale: scale.downgrade(),
        mute: mute.downgrade(),
        error: error.downgrade(),
        devices: devices.downgrade(),
        popover: popover.downgrade(),
    });
    state.render();
    let weak = Rc::downgrade(&state);
    scale.connect_change_value(move |_, _, value| {
        if let Some(state) = weak.upgrade() {
            if value.is_finite() {
                state.change(vec![
                    "wpctl".into(),
                    "set-volume".into(),
                    state.target().into(),
                    format!("{:.4}", value.clamp(0.0, 100.0) / 100.0),
                ]);
            }
        }
        glib::Propagation::Stop
    });
    let weak = Rc::downgrade(&state);
    mute.connect_clicked(move |_| {
        if let Some(state) = weak.upgrade() {
            state.change(vec![
                "wpctl".into(),
                "set-mute".into(),
                state.target().into(),
                "toggle".into(),
            ]);
        }
    });
    let weak = Rc::downgrade(&state);
    popover.connect_map(move |_| {
        if let Some(state) = weak.upgrade() {
            state.refresh();
            state.refresh_devices();
        }
    });
    if input {
        let weak = Rc::downgrade(state);
        popover.connect_unmap(move |_| {
            if let Some(state) = weak.upgrade() {
                state.render();
            }
        });
    }
    let state = state.clone();
    button.connect_destroy(move |_| {
        let _ = &state;
    });
    button
}
fn outputs(text: &str) -> Option<Vec<(String, String)>> {
    if text.len() > 1024 * 1024 {
        return None;
    }
    let value: serde_json::Value = serde_json::from_str(text).ok()?;
    let array = value.as_array()?;
    if array.len() > 64 {
        return None;
    }
    let mut names = std::collections::BTreeSet::new();
    Some(
        array
            .iter()
            .filter_map(|item| {
                let name = item.get("name")?.as_str()?;
                if name.is_empty()
                    || name.len() > 256
                    || name.starts_with('-')
                    || name.chars().any(char::is_control)
                    || !names.insert(name)
                {
                    return None;
                }
                let description = item
                    .get("description")
                    .and_then(|v| v.as_str())
                    .filter(|s| !s.is_empty())
                    .unwrap_or(name);
                Some((
                    name.to_string(),
                    description
                        .chars()
                        .take(128)
                        .map(|c| if c.is_control() { ' ' } else { c })
                        .collect(),
                ))
            })
            .collect(),
    )
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    #[ignore = "requires nested GTK and isolated mock audio commands"]
    fn live_controls_events_and_cleanup() {
        use gtk4_layer_shell::{Edge, KeyboardMode, Layer, LayerShell};
        let directory = std::path::PathBuf::from(std::env::var("WM_AUDIO_TEST_DIR").unwrap());
        assert_eq!(
            std::env::split_paths(&std::env::var_os("PATH").unwrap()).next(),
            Some(directory.join("bin")),
            "audio control tests require the isolated mock commands first in PATH"
        );
        let write_state = |value: f64, muted: bool, fail: bool| {
            std::fs::write(
                directory.join("next-state"),
                serde_json::json!({"volume":value,"muted":muted,"fail":fail}).to_string(),
            )
            .unwrap();
            std::fs::rename(directory.join("next-state"), directory.join("state")).unwrap();
        };
        write_state(0.42, false, false);
        gtk::init().unwrap();
        crate::css(&wm_core::Config::load().unwrap());
        let context = glib::MainContext::default();
        let _guard = context.acquire().unwrap();
        let button = widget();
        let window = gtk::Window::new();
        window.init_layer_shell();
        window.set_layer(Layer::Top);
        window.set_anchor(Edge::Top, true);
        window.set_anchor(Edge::Left, true);
        window.set_keyboard_mode(KeyboardMode::None);
        window.set_default_size(300, 40);
        window.set_child(Some(&button));
        window.present();
        let wait = |predicate: &dyn Fn() -> bool| {
            let deadline = std::time::Instant::now() + Duration::from_secs(8);
            while !predicate() {
                while context.pending() {
                    context.iteration(false);
                }
                assert!(
                    std::time::Instant::now() < deadline,
                    "audio state did not settle"
                );
                std::thread::sleep(Duration::from_millis(5));
            }
        };
        let pump = |ms| {
            context.block_on(glib::timeout_future(Duration::from_millis(ms)));
        };
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
        wait(&|| {
            button.label().as_deref() == Some("Vol 42%")
                && button.is_mapped()
                && directory.join("monitor-pid").exists()
        });
        pump(200);
        let before_attach = std::fs::read_to_string(directory.join("calls")).unwrap();
        let second = widget();
        assert_eq!(
            second.label().as_deref(),
            Some("Vol 42%"),
            "new bars should receive the cached state immediately"
        );
        pump(200);
        assert_eq!(
            std::fs::read_to_string(directory.join("calls")).unwrap(),
            before_attach,
            "attaching another view must not query audio again"
        );
        assert_eq!(
            std::fs::read_to_string(directory.join("monitor-starts"))
                .unwrap()
                .lines()
                .count(),
            1,
            "bars must share one subscription"
        );
        input(&[
            "mousemove",
            "--window",
            &std::env::var("WM_TEST_HOST_WINDOW").unwrap(),
            "100",
            "20",
            "click",
            "1",
        ]);
        let popover = button.popover().unwrap();
        wait(&|| popover.is_mapped());
        assert_eq!(window.keyboard_mode(), KeyboardMode::OnDemand);
        let content = popover.child().unwrap();
        let scale = content
            .first_child()
            .unwrap()
            .next_sibling()
            .unwrap()
            .downcast::<gtk::Scale>()
            .unwrap();
        let mute = scale
            .next_sibling()
            .unwrap()
            .downcast::<gtk::Button>()
            .unwrap();
        scale.grab_focus();
        pump(100);
        input(&["key", "Right"]);
        wait(&|| button.label().as_deref() == Some("Vol 43%"));
        assert_eq!(second.label(), button.label());
        assert!(
            scale.has_focus(),
            "volume writes must retain slider keyboard focus"
        );
        mute.grab_focus();
        input(&["key", "space"]);
        wait(&|| button.label().as_deref() == Some("Muted") && mute.is_sensitive());
        assert_eq!(second.label(), button.label());
        assert_eq!(mute.label().as_deref(), Some("Unmute"));
        let calls = || std::fs::read_to_string(directory.join("calls")).unwrap();
        assert!(calls().contains("0.4300"));
        assert!(calls().contains("set-mute"));
        let devices = content
            .last_child()
            .unwrap()
            .prev_sibling()
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
        let device = |name: &str| {
            let mut child = devices.first_child();
            while let Some(widget) = child {
                child = widget.next_sibling();
                if widget.widget_name() == name {
                    return widget.downcast::<gtk::Button>().ok();
                }
            }
            None
        };
        let selected = |name: &str| device(name).is_some_and(|b| b.has_css_class("active"));
        wait(&|| selected("speakers") && device("headphones").is_some());
        device("headphones").unwrap().grab_focus();
        input(&["key", "Return"]);
        wait(&|| selected("headphones") && devices.is_sensitive());
        pump(250);
        assert!(device("headphones").unwrap().has_focus());
        assert_eq!(
            std::fs::read_to_string(directory.join("default-sink")).unwrap(),
            "headphones"
        );
        let device_calls = || std::fs::read_to_string(directory.join("device-calls")).unwrap();
        assert!(device_calls().lines().any(|line| {
            serde_json::from_str::<Vec<String>>(line).unwrap() == ["set-default-sink", "headphones"]
        }));
        std::fs::write(directory.join("device-deny"), "1").unwrap();
        device("speakers").unwrap().emit_clicked();
        let device_error = content
            .last_child()
            .unwrap()
            .downcast::<gtk::Label>()
            .unwrap();
        wait(&|| {
            device_error
                .text()
                .contains("Simulated output selection failure")
                && devices.is_sensitive()
        });
        assert!(selected("headphones"));
        std::fs::remove_file(directory.join("device-deny")).unwrap();
        device("headphones").unwrap().emit_clicked();
        wait(&|| !device_error.is_visible() && devices.is_sensitive());
        let device_event = |event: &str| {
            use std::io::Write;
            std::fs::OpenOptions::new()
                .append(true)
                .open(directory.join("events"))
                .unwrap()
                .write_all(event.as_bytes())
                .unwrap();
        };
        std::fs::write(directory.join("sinks"), r#"[{"name":"headphones","description":"Headphones"},{"name":"hdmi","description":"HDMI display"}]"#).unwrap();
        device_event("Event 'new' on sink #3\nEvent 'remove' on sink #1\n");
        wait(&|| device("hdmi").is_some() && device("speakers").is_none());
        assert!(selected("headphones"));
        drop(device_error);
        {
            std::fs::write(
                directory.join("input-state"),
                r#"{"volume":0.55,"muted":false,"fail":false}"#,
            )
            .unwrap();
            std::fs::write(directory.join("sources"), r#"[{"name":"internal","description":"Internal microphone"},{"name":"usb-mic","description":"USB microphone"}]"#).unwrap();
            std::fs::write(directory.join("default-source"), "internal").unwrap();
            let mic = content
                .first_child()
                .unwrap()
                .next_sibling()
                .unwrap()
                .next_sibling()
                .unwrap()
                .next_sibling()
                .unwrap()
                .next_sibling()
                .unwrap()
                .downcast::<gtk::MenuButton>()
                .unwrap();
            assert_eq!(mic.widget_name(), "microphone-controls");
            mic.grab_focus();
            input(&["key", "space"]);
            let popup = mic.popover().unwrap();
            wait(&|| popup.is_mapped() && mic.label().as_deref() == Some("Mic 55%"));
            let body = popup.child().unwrap();
            let slider = body
                .first_child()
                .unwrap()
                .next_sibling()
                .unwrap()
                .downcast::<gtk::Scale>()
                .unwrap();
            let mute_input = slider
                .next_sibling()
                .unwrap()
                .downcast::<gtk::Button>()
                .unwrap();
            slider.grab_focus();
            input(&["key", "Right"]);
            wait(&|| mic.label().as_deref() == Some("Mic 56%"));
            assert!(slider.has_focus());
            mute_input.grab_focus();
            input(&["key", "space"]);
            wait(&|| mic.label().as_deref() == Some("Mic muted") && mute_input.is_sensitive());
            assert_eq!(button.label().as_deref(), Some("Muted"));
            assert_eq!(
                scale.value(),
                43.0,
                "microphone writes must not change output volume"
            );
            let inputs = body
                .last_child()
                .unwrap()
                .prev_sibling()
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
            wait(&|| {
                inputs
                    .last_child()
                    .is_some_and(|w| w.widget_name() == "usb-mic")
            });
            inputs.last_child().unwrap().grab_focus();
            input(&["key", "Return"]);
            wait(&|| inputs.last_child().unwrap().has_css_class("active") && inputs.is_sensitive());
            assert_eq!(
                std::fs::read_to_string(directory.join("default-source")).unwrap(),
                "usb-mic"
            );
            assert_eq!(
                std::fs::read_to_string(directory.join("default-sink")).unwrap(),
                "headphones"
            );
            pump(300);
            let before_output = calls();
            std::fs::write(
                directory.join("input-state"),
                r#"{"volume":0.63,"muted":false,"fail":false}"#,
            )
            .unwrap();
            device_event("Event 'change' on source #8\n");
            wait(&|| mic.label().as_deref() == Some("Mic 63%"));
            assert_eq!(
                calls(),
                before_output,
                "source events must not query output volume"
            );
            let error = body.last_child().unwrap().downcast::<gtk::Label>().unwrap();
            std::fs::write(directory.join("write-mode"), "deny").unwrap();
            mute_input.emit_clicked();
            wait(&|| error.text().contains("Simulated write failure") && mute_input.is_sensitive());
            assert_eq!(mic.label().as_deref(), Some("Mic 63%"));
            std::fs::remove_file(directory.join("write-mode")).unwrap();
            std::fs::write(
                directory.join("input-state"),
                r#"{"volume":0.63,"muted":false,"fail":true}"#,
            )
            .unwrap();
            device_event("Event 'change' on source #8\n");
            wait(&|| !slider.is_sensitive());
            assert!(
                scale.is_sensitive(),
                "missing microphone must not disable output controls"
            );
            std::fs::write(
                directory.join("input-state"),
                r#"{"volume":0.63,"muted":false,"fail":false}"#,
            )
            .unwrap();
            device_event("Event 'change' on source #8\n");
            wait(&|| slider.is_sensitive() && !error.is_visible());
            let input_calls = std::fs::read_to_string(directory.join("input-calls")).unwrap();
            assert!(input_calls.contains("0.5600") && input_calls.contains("set-mute"));
            assert_eq!(
                std::fs::read_to_string(directory.join("monitor-starts"))
                    .unwrap()
                    .lines()
                    .count(),
                1
            );
            if let Ok(path) = std::env::var("WM_AUDIO_TEST_SCREENSHOT") {
                pump(200);
                assert!(
                    std::process::Command::new("import")
                        .env("DISPLAY", std::env::var("WM_TEST_HOST_DISPLAY").unwrap())
                        .args([
                            "-window",
                            &std::env::var("WM_TEST_HOST_WINDOW").unwrap(),
                            &format!("{path}.microphone.png")
                        ])
                        .status()
                        .unwrap()
                        .success()
                );
            }
            input(&["key", "Escape"]);
            wait(&|| !popup.is_visible());
            assert!(popover.is_visible());
            assert_eq!(mic.label().as_deref(), Some("Microphone"));
            assert_eq!(window.keyboard_mode(), KeyboardMode::OnDemand);
            pump(250);
            let before = std::fs::read_to_string(directory.join("input-calls")).unwrap();
            device_event("Event 'change' on source #8\n");
            pump(250);
            assert_eq!(
                std::fs::read_to_string(directory.join("input-calls")).unwrap(),
                before
            );
        }
        if let Ok(path) = std::env::var("WM_AUDIO_TEST_SCREENSHOT") {
            pump(200);
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
        input(&["key", "Escape"]);
        wait(&|| !popover.is_visible());
        assert_eq!(window.keyboard_mode(), KeyboardMode::None);
        pump(300);
        let device_before = device_calls();
        device_event("Event 'change' on server #0\nEvent 'change' on card #0\n");
        pump(250);
        assert_eq!(
            device_calls(),
            device_before,
            "closed popovers must not read device lists"
        );
        let before = calls();
        pump(400);
        assert_eq!(calls(), before, "steady audio must not poll");
        write_state(0.61, false, false);
        use std::io::Write;
        let event = |text: &str| {
            let mut file = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(directory.join("events"))
                .unwrap();
            file.write_all(text.as_bytes()).unwrap();
        };
        event(&"Event 'change' on sink #1\n".repeat(30));
        wait(&|| button.label().as_deref() == Some("Vol 61%"));
        assert_eq!(second.label(), button.label());
        pump(250);
        assert_eq!(
            calls().lines().count(),
            before.lines().count() + 1,
            "burst must coalesce into one read"
        );
        let before = calls();
        event("Event 'new' on client #9\nEvent 'new' on sink-input #9\n");
        pump(250);
        assert_eq!(calls(), before);
        write_state(0.61, false, true);
        event("Event 'change' on sink #1\n");
        wait(&|| button.label().as_deref() == Some("Audio unavailable"));
        assert_eq!(second.label(), button.label());
        assert!(!scale.is_sensitive());
        write_state(0.72, false, false);
        event("Event 'change' on server #0\n");
        wait(&|| button.label().as_deref() == Some("Vol 72%"));
        assert!(scale.is_sensitive());
        assert!(
            !content.last_child().unwrap().is_visible(),
            "recovery must clear the unavailable error"
        );
        let error = content
            .last_child()
            .unwrap()
            .downcast::<gtk::Label>()
            .unwrap();
        std::fs::write(directory.join("write-mode"), "deny").unwrap();
        mute.emit_clicked();
        wait(&|| error.text().contains("Simulated write failure") && mute.is_sensitive());
        assert_eq!(button.label().as_deref(), Some("Vol 72%"));
        assert!(
            scale.is_sensitive(),
            "a rejected write must not disable readable audio state"
        );
        std::fs::write(directory.join("write-mode"), "hang").unwrap();
        std::fs::remove_file(directory.join("write-pid")).unwrap();
        scale.emit_by_name::<bool>("change-value", &[&gtk::ScrollType::Jump, &45f64]);
        wait(&|| directory.join("write-pid").exists());
        let hung_pid = std::fs::read_to_string(directory.join("write-pid")).unwrap();
        wait(&|| error.text() == "Audio command timed out" && mute.is_sensitive());
        wait(&|| !std::path::Path::new(&format!("/proc/{hung_pid}")).exists());
        assert_eq!(button.label().as_deref(), Some("Vol 72%"));
        std::fs::write(directory.join("write-mode"), "slow").unwrap();
        std::fs::remove_file(directory.join("write-pid")).unwrap();
        let before_writes = calls().lines().count();
        scale.emit_by_name::<bool>("change-value", &[&gtk::ScrollType::Jump, &20f64]);
        wait(&|| directory.join("write-pid").exists());
        scale.emit_by_name::<bool>("change-value", &[&gtk::ScrollType::Jump, &30f64]);
        scale.emit_by_name::<bool>("change-value", &[&gtk::ScrollType::Jump, &80f64]);
        wait(&|| button.label().as_deref() == Some("Vol 80%") && mute.is_sensitive());
        let recorded = calls();
        let writes = recorded
            .lines()
            .skip(before_writes)
            .filter(|line| line.contains("set-volume"))
            .collect::<Vec<_>>();
        assert_eq!(
            writes.len(),
            2,
            "volume bursts must retain only the active and latest target"
        );
        assert!(writes[0].contains("0.2000") && writes[1].contains("0.8000"));
        assert!(
            !error.is_visible(),
            "a successful new action must clear the prior error"
        );
        std::fs::remove_file(directory.join("write-mode")).unwrap();
        drop(error);
        button.popup();
        wait(&|| popover.is_mapped() && selected("headphones"));
        let old_pid = std::fs::read_to_string(directory.join("monitor-pid")).unwrap();
        std::fs::write(directory.join("disconnect"), "1").unwrap();
        wait(&|| !std::path::Path::new(&format!("/proc/{old_pid}")).exists());
        std::fs::write(directory.join("sinks"), r#"[{"name":"headphones","description":"Headphones"},{"name":"usb","description":"USB audio"}]"#).unwrap();
        std::fs::remove_file(directory.join("disconnect")).unwrap();
        wait(&|| std::fs::read_to_string(directory.join("monitor-pid")).unwrap() != old_pid);
        wait(&|| device("usb").is_some() && device("hdmi").is_none());
        button.popdown();
        wait(&|| !popover.is_visible());
        let pid = std::fs::read_to_string(directory.join("monitor-pid")).unwrap();
        let weak = button.downgrade();
        window.set_child(gtk::Widget::NONE);
        drop(scale);
        drop(mute);
        drop(devices);
        drop(content);
        drop(popover);
        drop(button);
        wait(&|| weak.upgrade().is_none());
        assert!(
            std::path::Path::new(&format!("/proc/{pid}")).exists(),
            "removing one bar must preserve the shared subscription"
        );
        assert_eq!(second.label().as_deref(), Some("Vol 80%"));
        drop(second);
        wait(&|| !std::path::Path::new(&format!("/proc/{pid}")).exists());
        let reopened = widget();
        wait(&|| {
            reopened.label().as_deref() == Some("Vol 80%")
                && std::fs::read_to_string(directory.join("monitor-pid")).unwrap() != pid
        });
        let reopened_pid = std::fs::read_to_string(directory.join("monitor-pid")).unwrap();
        drop(reopened);
        wait(&|| !std::path::Path::new(&format!("/proc/{reopened_pid}")).exists());
        window.close();
        std::fs::write(directory.join("ok"), "ok").unwrap();
    }
    #[test]
    fn output_list_bounds_and_names() {
        assert!(outputs("{}").is_none());
        assert!(outputs("invalid").is_none());
        assert!(outputs(&" ".repeat(1024 * 1024 + 1)).is_none());
        assert!(
            outputs(&serde_json::json!(vec![serde_json::json!({"name":"a"}); 65]).to_string())
                .is_none()
        );
        assert_eq!(outputs(r#"[{"name":"a","description":"Speaker\nOne"},{"name":"a"},{"name":"-bad"},{"name":""},{"name":"bad\tname"},{"name":"b"}]"#).unwrap(),
            vec![("a".into(), "Speaker One".into()), ("b".into(), "b".into())]);
    }
    #[test]
    fn volume_and_event_parsing() {
        assert_eq!(volume("Volume: 0.42\n"), Some((0.42, false)));
        assert_eq!(volume("Volume: 1.25 [MUTED]"), Some((1.25, true)));
        for invalid in [
            "",
            "Volume: NaN",
            "Volume: inf",
            "Volume: -0.1",
            "Volume: 99",
        ] {
            assert!(volume(invalid).is_none());
        }
        assert!(relevant_event(b"Event 'change' on sink #42"));
        assert!(relevant_event(b"Event 'change' on server #0"));
        assert!(!relevant_event(b"Event 'new' on sink-input #42"));
        assert!(!relevant_event(b"Event 'new' on client #42"));
    }
}
