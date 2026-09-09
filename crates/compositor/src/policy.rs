//! Desktop policy, intentionally independent of backend device management.
use crate::{
    AnvilState,
    focus::KeyboardFocusTarget,
    shell::{FullscreenSurface, WindowElement},
    state::Backend,
};
use smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel::State as XdgState;
use smithay::{
    desktop::layer_map_for_output,
    input::keyboard::{Keysym, ModifiersState},
    utils::{IsAlive, Rectangle, SERIAL_COUNTER},
    wayland::{compositor::with_states, seat::WaylandFocus, shell::xdg::XdgToplevelSurfaceData},
};
use std::{
    collections::BTreeMap,
    process::{Child, Command},
    sync::{Arc, Mutex},
    time::Duration,
};
use wm_core::{Config, OutputInfo, Rect, Snapshot, WindowInfo};
#[derive(Debug)]
pub struct Managed {
    pub window: WindowElement,
    pub id: u64,
    pub workspace: u8,
    pub output: String,
    pub floating: bool,
    pub fullscreen: bool,
    pub scratchpad: bool,
    pub rect: Option<Rect>,
    pub floating_rect: Option<Rect>,
    pub requested_size: (Option<i32>, Option<i32>),
    pub last_size: Option<(i32, i32)>,
    pub opacity: f32,
    opening_started: Option<std::time::Instant>,
    opened: bool,
    movement: Option<Movement>,
    resize: Option<Resize>,
}
#[derive(Debug)]
struct Movement {
    from: smithay::utils::Point<i32, smithay::utils::Logical>,
    started: std::time::Instant,
}
#[derive(Debug)]
struct Resize {
    from: smithay::utils::Size<i32, smithay::utils::Logical>,
    started: std::time::Instant,
}
#[derive(Debug)]
pub struct Desktop {
    pub config: Config,
    pub windows: Vec<Managed>,
    pub outputs: BTreeMap<String, u8>,
    pub next_id: u64,
    pub dirty: bool,
    pub redraw: bool,
    pub active: bool,
    pub error: Option<String>,
    pub snapshot: Arc<Mutex<Snapshot>>,
    pub subscribers: Arc<Mutex<Vec<std::sync::mpsc::SyncSender<Snapshot>>>>,
    pub children: Vec<Child>,
    pub services: Vec<(Vec<String>, Option<Child>, u8)>,
    pub installed: bool,
    pub watcher: Option<notify::RecommendedWatcher>,
    animation_timer: Option<smithay::reexports::calloop::RegistrationToken>,
}
impl Default for Desktop {
    fn default() -> Self {
        let (config, error) = match Config::load() {
            Ok(c) => (c, None),
            Err(e) => (Config::default(), Some(e)),
        };
        Self {
            config,
            windows: vec![],
            outputs: BTreeMap::new(),
            next_id: 1,
            dirty: true,
            redraw: true,
            active: true,
            error,
            snapshot: Arc::new(Mutex::new(Snapshot::default())),
            subscribers: Arc::new(Mutex::new(vec![])),
            children: vec![],
            services: vec![],
            installed: false,
            watcher: None,
            animation_timer: None,
        }
    }
}
impl Drop for Desktop {
    fn drop(&mut self) {
        for (_, child, _) in &mut self.services {
            if let Some(child) = child {
                let _ = child.kill();
                let _ = child.wait();
            }
        }
    }
}
pub fn identity(w: &WindowElement) -> (String, String) {
    if let Some(t) = w.0.toplevel() {
        with_states(t.wl_surface(), |s| {
            let a = s
                .data_map
                .get::<XdgToplevelSurfaceData>()
                .unwrap()
                .lock()
                .unwrap();
            (
                a.app_id.clone().unwrap_or_default(),
                a.title.clone().unwrap_or_default(),
            )
        })
    } else {
        #[cfg(feature = "xwayland")]
        if let Some(x) = w.0.x11_surface() {
            return (x.class(), x.title());
        }
        (String::new(), String::new())
    }
}
impl<B: Backend + 'static> AnvilState<B> {
    pub(crate) fn window_unmapped(
        &mut self,
        surface: &smithay::reexports::wayland_server::protocol::wl_surface::WlSurface,
    ) {
        let Some(index) = self
            .desktop
            .windows
            .iter()
            .position(|managed| managed.window.0.wl_surface().as_deref() == Some(surface))
        else {
            return;
        };

        let window = self.desktop.windows[index].window.clone();
        let was_fullscreen = self.desktop.windows[index].fullscreen;
        let managed = &mut self.desktop.windows[index];

        // An xdg-toplevel unmap resets its role state. Mirror that reset in
        // compositor policy so the next initial configure is a normal mapping
        // unless the client explicitly requests fullscreen again.
        managed.fullscreen = false;
        managed.last_size = None;
        managed.opened = false;
        managed.opening_started = None;
        managed.movement = None;
        managed.resize = None;

        if was_fullscreen {
            for output in self.space.outputs() {
                let Some(fullscreen) = output.user_data().get::<FullscreenSurface>() else {
                    continue;
                };
                if fullscreen.get().as_ref() == Some(&window) {
                    fullscreen.clear();
                    self.backend_data.reset_buffers(output);
                }
            }
        }
        self.desktop.redraw = true;
    }

    pub fn install_desktop(&mut self) {
        if self.desktop.installed {
            return;
        }
        self.desktop.installed = true;
        let (tx, rx) = smithay::reexports::calloop::channel::channel::<(
            String,
            Option<std::sync::mpsc::SyncSender<Result<(), String>>>,
        )>();
        self.handle
            .insert_source(rx, |event, _, state| {
                if let smithay::reexports::calloop::channel::Event::Msg((cmd, reply)) = event {
                    let result = state.desktop_command(&cmd);
                    state.maintain_desktop();
                    if let Some(r) = reply {
                        let _ = r.try_send(result);
                    }
                }
            })
            .unwrap();
        if let Err(e) = crate::ipc::serve(
            tx.clone(),
            self.desktop.snapshot.clone(),
            self.desktop.subscribers.clone(),
        ) {
            self.desktop.error = Some(e);
            self.running
                .store(false, std::sync::atomic::Ordering::SeqCst);
            return;
        }
        use notify::Watcher;
        let path = wm_core::config_path();
        let watch_path = path
            .parent()
            .filter(|p| p.exists())
            .map(|p| p.to_path_buf());
        if let Some(parent) = watch_path {
            let event_tx = tx;
            let target = path.clone();
            if let Ok(mut watcher) =
                notify::recommended_watcher(move |event: Result<notify::Event, notify::Error>| {
                    if let Ok(e) = event {
                        if !matches!(e.kind, notify::EventKind::Access(_))
                            && e.paths.iter().any(|p| p == &target)
                        {
                            let _ = event_tx.send(("reload".into(), None));
                        }
                    }
                })
            {
                if watcher
                    .watch(&parent, notify::RecursiveMode::NonRecursive)
                    .is_ok()
                {
                    self.desktop.watcher = Some(watcher)
                }
            }
        }
        let timer =
            smithay::reexports::calloop::timer::Timer::from_duration(Duration::from_secs(1));
        self.handle
            .insert_source(timer, |_, _, s| {
                s.supervise();
                smithay::reexports::calloop::timer::TimeoutAction::ToDuration(Duration::from_secs(
                    1,
                ))
            })
            .unwrap();
        self.apply_input_config();
        if std::env::var_os("WM_PRIVATE_BUS").is_some() {
            if let Some(socket) = &self.socket_name {
                let _ = std::process::Command::new("dbus-update-activation-environment")
                    .args([
                        format!("WAYLAND_DISPLAY={socket}"),
                        "XDG_CURRENT_DESKTOP=wm:wlr".into(),
                    ])
                    .status();
            }
        }
        for cmd in self.desktop.config.startup.clone() {
            let _ = self.spawn_app(&cmd);
        }
        if self.desktop.config.shell.enabled {
            if let Ok(exe) = std::env::current_exe() {
                let shell_name = if self.desktop.config.shell.backend == "sctk" {
                    "wm-shell-sctk"
                } else {
                    "wm-shell"
                };
                let shell = exe
                    .with_file_name(shell_name)
                    .to_string_lossy()
                    .into_owned();
                self.desktop.services.push((vec![shell.clone()], None, 0));
                self.desktop
                    .services
                    .push((vec![shell, "--wallpaper".into()], None, 0));
            }
        }
        self.supervise();
    }
    fn apply_input_config(&mut self) {
        let c = self.desktop.config.input.clone();
        self.backend_data.apply_device_config(&c);
        if let Some(k) = self.seat.get_keyboard() {
            k.change_repeat_info(c.repeat_rate, c.repeat_delay);
            let _ = k.set_xkb_config(
                self,
                smithay::input::keyboard::XkbConfig {
                    layout: &c.layout,
                    variant: &c.variant,
                    options: (!c.options.is_empty()).then(|| c.options.clone()),
                    ..Default::default()
                },
            );
        }
    }
    fn app_command(&self, args: &[String]) -> Result<Command, String> {
        let (first, rest) = args.split_first().ok_or("empty command")?;
        let mut c = Command::new(first);
        c.args(rest)
            .env_remove("DISPLAY")
            .env_remove("WAYLAND_SOCKET")
            .env("XDG_CURRENT_DESKTOP", "wm:wlr")
            .env("XDG_SESSION_TYPE", "wayland");
        if std::path::Path::new(first).file_name() == Some(std::ffi::OsStr::new("wm-shell")) {
            c.env("GDK_BACKEND", "wayland");
        }
        if let Some(s) = &self.socket_name {
            c.env("WAYLAND_DISPLAY", s);
        }
        #[cfg(feature = "xwayland")]
        if let Some(d) = self.xdisplay {
            c.env("DISPLAY", format!(":{d}"));
        }
        Ok(c)
    }
    pub fn spawn_app(&mut self, args: &[String]) -> Result<(), String> {
        let child = self
            .app_command(args)?
            .spawn()
            .map_err(|e| format!("{}: {e}", args[0]))?;
        self.desktop.children.push(child);
        Ok(())
    }
    fn supervise(&mut self) {
        self.desktop
            .children
            .retain_mut(|c| !matches!(c.try_wait(), Ok(Some(_))));
        for i in 0..self.desktop.services.len() {
            let dead = self.desktop.services[i]
                .1
                .as_mut()
                .is_none_or(|c| matches!(c.try_wait(), Ok(Some(_))));
            if dead && self.desktop.services[i].2 < 3 {
                let args = self.desktop.services[i].0.clone();
                self.desktop.services[i].2 += 1;
                match self
                    .app_command(&args)
                    .and_then(|mut c| c.spawn().map_err(|e| e.to_string()))
                {
                    Ok(c) => self.desktop.services[i].1 = Some(c),
                    Err(e) => {
                        tracing::warn!(%e,"desktop service failed");
                        self.desktop.error = Some(e);
                    }
                }
            }
        }
    }
    pub fn shortcut(&self, mods: ModifiersState, key: Keysym, raw: Keysym) -> Option<String> {
        let prefix = format!(
            "{}{}{}{}",
            if mods.logo { "Super+" } else { "" },
            if mods.ctrl { "Ctrl+" } else { "" },
            if mods.alt { "Alt+" } else { "" },
            if mods.shift { "Shift+" } else { "" }
        );
        for k in [key, raw] {
            let name = xkbcommon::xkb::keysym_get_name(k);
            if let Some(c) =
                binding_action(&self.desktop.config.bindings, &format!("{prefix}{name}"))
            {
                return Some(c.clone());
            }
        }
        None
    }
    fn focused_index(&self) -> Option<usize> {
        let f = self.seat.get_keyboard()?.current_focus()?;
        self.desktop
            .windows
            .iter()
            .position(|w| KeyboardFocusTarget::from(w.window.clone()) == f)
    }
    pub fn can_manipulate_window(&self, window: &WindowElement) -> bool {
        !self.lock.locked
            && self.desktop.windows.iter().any(|w| {
                &w.window == window
                    && w.floating
                    && !w.fullscreen
                    && !w.scratchpad
                    && self.desktop.outputs.get(&w.output) == Some(&w.workspace)
            })
    }
    fn focus_window(&mut self, index: usize, user_initiated: bool) {
        let w = self.desktop.windows[index].window.clone();
        self.space.raise_element(&w, true);
        if user_initiated {
            self.raise_above_closing(&w);
        }
        let k = self.seat.get_keyboard().unwrap();
        k.set_focus(self, Some(w.into()), SERIAL_COUNTER.next_serial());
        self.desktop.redraw = true;
    }
    fn active_output(&self) -> Option<String> {
        self.focused_index()
            .map(|i| self.desktop.windows[i].output.clone())
            .or_else(|| {
                self.space
                    .output_under(self.pointer.current_location())
                    .next()
                    .map(|o| o.name())
            })
            .or_else(|| self.space.outputs().next().map(|o| o.name()))
    }
    pub fn desktop_command(&mut self, cmd: &str) -> Result<(), String> {
        if self.lock.locked && !["status"].contains(&cmd) {
            return Err("session is locked".into());
        }
        let (v, arg) = cmd.split_once(' ').unwrap_or((cmd, ""));
        let out = self.active_output().unwrap_or_default();
        let focused = self.focused_index();
        match v {
            "status" => return Ok(()),
            "terminal" => {
                return self.spawn_app(&self.desktop.config.terminal.clone());
            }
            "launcher" => {
                let shell_name = if self.desktop.config.shell.backend == "sctk" {
                    "wm-shell-sctk"
                } else {
                    "wm-shell"
                };
                let p = std::env::current_exe()
                    .map_err(|e| e.to_string())?
                    .with_file_name(shell_name);
                return self.spawn_app(&[p.to_string_lossy().into_owned(), "--launcher".into()]);
            }
            "exec" => {
                let args: Vec<String> = serde_json::from_str(arg)
                    .map_err(|_| "exec requires a JSON array of executable and arguments")?;
                return self.spawn_app(&args);
            }
            "launch" => {
                let args = launch_args(arg)?;
                return self.spawn_app(&args);
            }
            "screenshot" => {
                let command = screenshot_copy_command();
                return self.spawn_app(&command);
            }
            "quit" => {
                self.running
                    .store(false, std::sync::atomic::Ordering::SeqCst);
                return Ok(());
            }
            "lock" => {
                return self.spawn_app(&["swaylock".into()]);
            }
            "reload" => match Config::load() {
                Ok(c) => {
                    self.desktop.config = c;
                    self.desktop.error = None;
                    self.apply_input_config();
                    let errors = self
                        .backend_data
                        .apply_output_config(&self.desktop.config.outputs);
                    if !errors.is_empty() {
                        self.desktop.error = Some(errors.join("; "));
                    }
                }
                Err(e) => {
                    self.desktop.error = Some(e.clone());
                    return Err(e);
                }
            },
            "workspace" | "send" => {
                let (number, target) = arg.split_once(' ').unwrap_or((arg, ""));
                let n: u8 = number.parse().map_err(|_| "workspace must be a number")?;
                let out = if target.is_empty() {
                    out
                } else if v == "workspace" && self.desktop.outputs.contains_key(target) {
                    target.to_owned()
                } else {
                    return Err("unknown output or unexpected argument".into());
                };
                if n == 0 || n > self.desktop.config.layout.workspaces {
                    return Err("workspace out of range".into());
                }
                if v == "send" {
                    if let Some(i) = focused {
                        let owner = self
                            .desktop
                            .outputs
                            .iter()
                            .find(|(_, w)| **w == n)
                            .map(|(o, _)| o.clone())
                            .unwrap_or(out.clone());
                        self.desktop.windows[i].workspace = n;
                        self.desktop.windows[i].output = owner;
                    }
                } else {
                    let previous = *self.desktop.outputs.get(&out).unwrap_or(&1);
                    let other = self
                        .desktop
                        .outputs
                        .iter()
                        .find(|(o, w)| **w == n && **o != out)
                        .map(|(o, _)| o.clone());
                    self.capture_workspace_transition(&out, previous, n);
                    if let Some(other) = other {
                        self.capture_workspace_transition(&other, n, previous);
                        self.desktop.outputs.insert(other.clone(), previous);
                        for w in &mut self.desktop.windows {
                            if w.workspace == previous {
                                w.output = other.clone()
                            } else if w.workspace == n {
                                w.output = out.clone()
                            }
                        }
                    }
                    self.desktop.outputs.insert(out.clone(), n);
                    for w in &mut self.desktop.windows {
                        if w.workspace == n {
                            w.output = out.clone()
                        }
                    }
                }
            }
            "layout" => {
                if !["master", "monocle"].contains(&arg) {
                    return Err("layout must be master or monocle".into());
                }
                self.desktop.config.layout.mode = arg.into();
            }
            "ratio" => {
                let d: f64 = arg.parse().map_err(|_| "ratio requires a numeric delta")?;
                if !d.is_finite() {
                    return Err("ratio must be finite".into());
                }
                self.desktop.config.layout.master_ratio =
                    (self.desktop.config.layout.master_ratio + d).clamp(0.1, 0.9)
            }
            "close" => {
                if let Some(i) = focused {
                    let w = &self.desktop.windows[i].window;
                    if let Some(t) = w.0.toplevel() {
                        t.send_close()
                    }
                    #[cfg(feature = "xwayland")]
                    if let Some(x) = w.0.x11_surface() {
                        x.close().map_err(|e| e.to_string())?;
                    }
                }
            }
            "fullscreen" | "floating" => {
                if let Some(i) = focused {
                    let w = &mut self.desktop.windows[i];
                    if v == "fullscreen" {
                        w.fullscreen = !w.fullscreen
                    } else {
                        w.floating = !w.floating;
                    }
                }
            }
            "scratchpad" => match arg {
                "send" => {
                    if let Some(i) = focused {
                        self.desktop.windows[i].scratchpad = true;
                    }
                }
                "show" => {
                    if let Some(i) = self.desktop.windows.iter().position(|w| w.scratchpad) {
                        let n = *self.desktop.outputs.get(&out).unwrap_or(&1);
                        let w = &mut self.desktop.windows[i];
                        w.scratchpad = false;
                        w.floating = true;
                        w.workspace = n;
                        w.output = out.clone();
                    }
                }
                _ => return Err("scratchpad send/show".into()),
            },
            "focus" | "move" => {
                if let Ok(id) = arg.parse::<u64>() {
                    if let Some(i) = self.desktop.windows.iter().position(|w| w.id == id) {
                        let w = &self.desktop.windows[i];
                        self.desktop.outputs.insert(w.output.clone(), w.workspace);
                        self.maintain_desktop();
                        self.focus_window(i, true);
                    }
                } else if let Some(i) = focused {
                    let a = self.desktop.windows[i].rect.unwrap_or(Rect {
                        x: 0,
                        y: 0,
                        w: 1,
                        h: 1,
                    });
                    let ac = (a.x + a.w / 2, a.y + a.h / 2);
                    let target = self
                        .desktop
                        .windows
                        .iter()
                        .enumerate()
                        .filter(|(j, w)| {
                            *j != i && self.space.element_location(&w.window).is_some()
                        })
                        .filter_map(|(j, w)| {
                            let b = w.rect?;
                            let dx = b.x + b.w / 2 - ac.0;
                            let dy = b.y + b.h / 2 - ac.1;
                            let valid = match arg {
                                "left" => dx < 0,
                                "right" => dx > 0,
                                "up" => dy < 0,
                                "down" => dy > 0,
                                _ => false,
                            };
                            valid.then_some((j, (dx as i64).pow(2) + (dy as i64).pow(2)))
                        })
                        .min_by_key(|(_, d)| *d)
                        .map(|(j, _)| j);
                    if let Some(j) = target {
                        if v == "move" {
                            self.desktop.windows.swap(i, j)
                        } else {
                            self.focus_window(j, true)
                        }
                    }
                }
            }
            _ => return Err(format!("unknown command: {v}")),
        }
        self.desktop.dirty = true;
        self.desktop.redraw = true;
        Ok(())
    }
    pub fn maintain_desktop(&mut self) {
        let outputs: Vec<_> = self.space.outputs().cloned().collect();
        let names: Vec<_> = outputs.iter().map(|o| o.name()).collect();
        self.desktop.outputs.retain(|n, _| names.contains(n));
        for o in &outputs {
            if !self.desktop.outputs.contains_key(&o.name()) {
                let n = (1..=9)
                    .find(|n| !self.desktop.outputs.values().any(|v| v == n))
                    .unwrap_or(1);
                self.desktop.outputs.insert(o.name(), n);
                self.desktop.dirty = true;
            }
        }
        let default_out = names.first().cloned().unwrap_or_default();
        let before = self.desktop.windows.len();
        self.desktop.windows.retain(|w| w.window.alive());
        if before != self.desktop.windows.len() {
            self.desktop.dirty = true;
            self.desktop.redraw = true;
        }
        for w in &mut self.desktop.windows {
            if !names.contains(&w.output) {
                w.output = default_out.clone();
                self.desktop.dirty = true;
            }
        }
        let candidates: Vec<_> = self.space.elements().cloned().collect();
        let active = self.active_output().unwrap_or(default_out);
        for window in candidates {
            if self.desktop.windows.iter().any(|w| w.window == window) {
                continue;
            }
            #[cfg(feature = "xwayland")]
            if window
                .0
                .x11_surface()
                .is_some_and(|x| x.is_override_redirect())
            {
                continue;
            }
            let (app, title) = identity(&window);
            let mut output = active.clone();
            let mut workspace = *self.desktop.outputs.get(&output).unwrap_or(&1);
            let mut floating = window.0.toplevel().is_some_and(|t| t.parent().is_some());
            let mut requested_size = (None, None);
            for r in &self.desktop.config.rules {
                if r.app_id.as_ref().is_none_or(|s| s == &app)
                    && r.title.as_ref().is_none_or(|s| title.contains(s))
                {
                    if let Some(o) = &r.output {
                        if names.contains(o) {
                            output = o.clone();
                            workspace = *self.desktop.outputs.get(o).unwrap_or(&1)
                        }
                    }
                    if let Some(n) = r.workspace {
                        workspace = n
                    }
                    if let Some(f) = r.floating {
                        floating = f
                    }
                    if r.width.is_some() {
                        requested_size.0 = r.width;
                    }
                    if r.height.is_some() {
                        requested_size.1 = r.height;
                    }
                }
            }
            window.set_ssd(false);
            let id = self.desktop.next_id;
            self.desktop.next_id += 1;
            self.desktop.windows.push(Managed {
                window,
                id,
                workspace,
                output,
                floating,
                fullscreen: false,
                scratchpad: false,
                rect: None,
                floating_rect: None,
                requested_size,
                last_size: None,
                opacity: 1.0,
                opening_started: None,
                movement: None,
                resize: None,
                opened: false,
            });
            self.desktop.dirty = true;
        }
        // Layer exclusive zones and client fullscreen requests can change on any commit.
        let focused = self.focused_index();
        let mut animating = false;
        let mut animation_refresh = 0;
        let mut scene_moved = false;
        let animate_movement = !self.lock.locked
            && self.desktop.active
            && !self.pointer.is_grabbed()
            && !self
                .seat
                .get_touch()
                .is_some_and(|touch| touch.is_grabbed())
            && !self.desktop.config.theme.reduced_motion
            && self.desktop.config.theme.animation_ms > 0;
        for output in &outputs {
            let output_refresh = output
                .current_mode()
                .map(|mode| mode.refresh)
                .unwrap_or(60_000);
            if crate::transitions::tick(
                output,
                self.desktop.active
                    && !self.lock.locked
                    && !self.desktop.config.theme.reduced_motion
                    && self.desktop.config.theme.animation_ms > 0
                    && output
                        .user_data()
                        .get::<crate::shell::FullscreenSurface>()
                        .and_then(|fullscreen| fullscreen.get())
                        .is_none(),
                *self.desktop.outputs.get(&output.name()).unwrap_or(&1),
            ) {
                animating = true;
                animation_refresh = animation_refresh.max(output_refresh);
                self.desktop.redraw = true;
            }

            output.user_data().insert_if_missing(|| {
                std::sync::Mutex::new(crate::effects::OutputTheme::default())
            });
            let mut output_theme = output
                .user_data()
                .get::<std::sync::Mutex<crate::effects::OutputTheme>>()
                .unwrap()
                .lock()
                .unwrap();
            let next_theme = &self.desktop.config.theme;
            if output_theme.0.radius != next_theme.radius
                || output_theme.0.blur != next_theme.blur
                || output_theme.0.blur_passes != next_theme.blur_passes
            {
                // Shader parameters are not client surface commits. Invalidate
                // retained output buffers when they change, including disabling
                // an effect on an otherwise idle surface.
                self.backend_data.reset_buffers(output);
                self.desktop.redraw = true;
            }
            output_theme.0 = next_theme.clone();
            drop(output_theme);
            if self.lock.locked {
                output.user_data().insert_if_missing(|| {
                    std::sync::Mutex::new(crate::lock::LockOutput {
                        locked: true,
                        generation: self.lock.generation,
                        ..Default::default()
                    })
                });
            }
            let name = output.name();
            let workspace = *self.desktop.outputs.get(&name).unwrap_or(&1);
            let Some(mut geo) = self.space.output_geometry(output) else {
                continue;
            };
            {
                let configured = self.desktop.config.outputs.get(&name);
                let scale = configured.map_or(1.0, |c| c.scale);
                use smithay::utils::Transform;
                let transform = match configured.map_or("normal", |c| c.transform.as_str()) {
                    "90" => Transform::_90,
                    "180" => Transform::_180,
                    "270" => Transform::_270,
                    "flipped" => Transform::Flipped,
                    "flipped-90" => Transform::Flipped90,
                    "flipped-180" => Transform::Flipped180,
                    "flipped-270" => Transform::Flipped270,
                    _ => Transform::Normal,
                };
                if output.current_scale().fractional_scale() != scale
                    || output.current_transform() != transform
                {
                    output.change_current_state(
                        None,
                        Some(transform),
                        Some(smithay::output::Scale::Fractional(scale)),
                        None,
                    );
                    layer_map_for_output(output).arrange();
                    geo = self.space.output_geometry(output).unwrap_or(geo);
                    self.desktop.redraw = true;
                }
                if let Some(c) = configured {
                    if geo.loc != (c.x, c.y).into() {
                        self.space.map_output(output, (c.x, c.y));
                        geo.loc = (c.x, c.y).into();
                    }
                }
            }
            crate::lock::configure_surface(output);
            let zone = layer_map_for_output(output).non_exclusive_zone();
            let area = Rect {
                x: geo.loc.x + zone.loc.x,
                y: geo.loc.y + zone.loc.y,
                w: zone.size.w,
                h: zone.size.h,
            };
            let visible: Vec<usize> = self
                .desktop
                .windows
                .iter()
                .enumerate()
                .filter(|(_, w)| w.output == name && w.workspace == workspace && !w.scratchpad)
                .map(|(i, _)| i)
                .collect();
            let tiles: Vec<_> = visible
                .iter()
                .copied()
                .filter(|i| {
                    !self.desktop.windows[*i].floating && !self.desktop.windows[*i].fullscreen
                })
                .collect();
            let rects = wm_core::tile(
                area,
                tiles.len(),
                self.desktop.config.theme.gap + self.desktop.config.theme.border,
                self.desktop.config.layout.master_ratio,
                self.desktop.config.layout.mode == "monocle",
            );
            output
                .user_data()
                .insert_if_missing(FullscreenSurface::default);
            output
                .user_data()
                .get::<FullscreenSurface>()
                .unwrap()
                .clear();
            for i in visible {
                let w = &mut self.desktop.windows[i];
                w.window
                    .0
                    .user_data()
                    .insert_if_missing(crate::shell::WindowOpening::default);
                let opening =
                    if w.opened || w.fullscreen || self.lock.locked || !self.desktop.active {
                        w.opened = true;
                        1.0
                    } else if w.window.0.bbox().is_empty() {
                        0.0
                    } else {
                        let started = *w
                            .opening_started
                            .get_or_insert_with(std::time::Instant::now);
                        let value = wm_core::opening_opacity(
                            started.elapsed(),
                            self.desktop.config.theme.animation_ms,
                            self.desktop.config.theme.reduced_motion,
                        );
                        w.opened = value >= 1.0;
                        animating |= !w.opened;
                        if !w.opened {
                            animation_refresh = animation_refresh.max(output_refresh);
                        }
                        value
                    };
                let mut previous = w
                    .window
                    .0
                    .user_data()
                    .get::<crate::shell::WindowOpening>()
                    .unwrap()
                    .0
                    .lock()
                    .unwrap();
                if *previous != opening {
                    *previous = opening;
                    self.desktop.redraw = true;
                }
                drop(previous);
                w.window
                    .0
                    .user_data()
                    .insert_if_missing(crate::effects::WindowFocused::default);
                let mut active = w
                    .window
                    .0
                    .user_data()
                    .get::<crate::effects::WindowFocused>()
                    .unwrap()
                    .0
                    .lock()
                    .unwrap();
                if *active != (focused == Some(i)) {
                    *active = focused == Some(i);
                    self.desktop.redraw = true;
                }
                drop(active);
                let (app, title) = identity(&w.window);
                let blur = !w.fullscreen
                    && self
                        .desktop
                        .config
                        .rules
                        .iter()
                        .filter(|rule| {
                            rule.app_id.as_ref().is_none_or(|id| id == &app)
                                && rule.title.as_ref().is_none_or(|text| title.contains(text))
                        })
                        .filter_map(|rule| rule.blur)
                        .last()
                        .unwrap_or(false);
                w.window
                    .0
                    .user_data()
                    .insert_if_missing(crate::shell::WindowBlur::default);
                let mut previous_blur = w
                    .window
                    .0
                    .user_data()
                    .get::<crate::shell::WindowBlur>()
                    .unwrap()
                    .0
                    .lock()
                    .unwrap();
                if *previous_blur != blur {
                    *previous_blur = blur;
                    self.backend_data.reset_buffers(&output);
                    self.desktop.redraw = true;
                }
                drop(previous_blur);
                let opacity = if w.fullscreen || self.desktop.config.rules.is_empty() {
                    1.0
                } else {
                    self.desktop
                        .config
                        .rules
                        .iter()
                        .filter(|rule| {
                            rule.app_id.as_ref().is_none_or(|id| id == &app)
                                && rule.title.as_ref().is_none_or(|text| title.contains(text))
                        })
                        .filter_map(|rule| rule.opacity)
                        .last()
                        .unwrap_or(1.0)
                };
                w.window
                    .0
                    .user_data()
                    .insert_if_missing(crate::shell::WindowOpacity::default);
                if w.opacity != opacity {
                    w.opacity = opacity;
                    *w.window
                        .0
                        .user_data()
                        .get::<crate::shell::WindowOpacity>()
                        .unwrap()
                        .0
                        .lock()
                        .unwrap() = opacity;
                    self.desktop.redraw = true;
                }
                w.window
                    .0
                    .user_data()
                    .insert_if_missing(crate::shell::WindowCornerRadius::default);
                *w.window
                    .0
                    .user_data()
                    .get::<crate::shell::WindowCornerRadius>()
                    .unwrap()
                    .0
                    .lock()
                    .unwrap() = if w.fullscreen
                    || output.current_transform() != smithay::utils::Transform::Normal
                {
                    0.
                } else {
                    self.desktop.config.theme.radius as f64
                };
                let r = if w.fullscreen {
                    Rect {
                        x: geo.loc.x,
                        y: geo.loc.y,
                        w: geo.size.w,
                        h: geo.size.h,
                    }
                } else if w.floating {
                    let r = wm_core::floating_rect(area, w.floating_rect, w.requested_size);
                    w.floating_rect = Some(r);
                    r
                } else {
                    rects[tiles.iter().position(|j| *j == i).unwrap()]
                };
                if w.fullscreen {
                    output
                        .user_data()
                        .get::<FullscreenSurface>()
                        .unwrap()
                        .set(w.window.clone());
                }
                let current_location = self.space.element_location(&w.window);
                let target_changed = w.rect != Some(r);
                w.window
                    .0
                    .user_data()
                    .insert_if_missing(crate::effects::WindowRenderSize::default);
                let render_size_state = w
                    .window
                    .0
                    .user_data()
                    .get::<crate::effects::WindowRenderSize>()
                    .unwrap();
                let current_render_size = *render_size_state.0.lock().unwrap();
                if !animate_movement || w.fullscreen || current_location.is_none() {
                    w.movement = None;
                } else if target_changed {
                    w.movement = current_location
                        .filter(|location| {
                            w.rect.is_some()
                                && *location != (r.x, r.y).into()
                                && geo.contains(*location)
                        })
                        .map(|from| Movement {
                            from,
                            started: std::time::Instant::now(),
                        });
                }
                let target_size = (r.w, r.h).into();
                if !animate_movement || w.fullscreen || current_location.is_none() {
                    w.resize = None;
                } else if target_changed
                    && w.rect
                        .is_some_and(|previous| (previous.w, previous.h) != (r.w, r.h))
                {
                    let from = current_render_size
                        .or_else(|| w.rect.map(|previous| (previous.w, previous.h).into()));
                    w.resize = from.filter(|from| *from != target_size).map(|from| Resize {
                        from,
                        started: std::time::Instant::now(),
                    });
                }
                let mut location = (r.x, r.y).into();
                if let Some(movement) = &w.movement {
                    let progress = wm_core::opening_opacity(
                        movement.started.elapsed(),
                        self.desktop.config.theme.animation_ms,
                        false,
                    );
                    if progress >= 1.0 {
                        w.movement = None;
                    } else {
                        let t = f64::from(progress);
                        location = (
                            (f64::from(movement.from.x)
                                + (f64::from(r.x) - f64::from(movement.from.x)) * t)
                                .round() as i32,
                            (f64::from(movement.from.y)
                                + (f64::from(r.y) - f64::from(movement.from.y)) * t)
                                .round() as i32,
                        )
                            .into();
                        animating = true;
                        animation_refresh = animation_refresh.max(output_refresh);
                    }
                }
                let mut render_size = target_size;
                if let Some(resize) = &w.resize {
                    let progress = wm_core::opening_opacity(
                        resize.started.elapsed(),
                        self.desktop.config.theme.animation_ms,
                        false,
                    );
                    if progress >= 1.0 {
                        w.resize = None;
                    } else {
                        let t = f64::from(progress);
                        render_size = (
                            (f64::from(resize.from.w)
                                + (f64::from(r.w) - f64::from(resize.from.w)) * t)
                                .round()
                                .max(1.0) as i32,
                            (f64::from(resize.from.h)
                                + (f64::from(r.h) - f64::from(resize.from.h)) * t)
                                .round()
                                .max(1.0) as i32,
                        )
                            .into();
                        animating = true;
                        animation_refresh = animation_refresh.max(output_refresh);
                    }
                }
                let committed_size = w.window.0.geometry().size;
                let next_render_size = (render_size != committed_size).then_some(render_size);
                let mut previous_render_size = render_size_state.0.lock().unwrap();
                if *previous_render_size != next_render_size {
                    *previous_render_size = next_render_size;
                    self.desktop.redraw = true;
                }
                drop(previous_render_size);
                if target_changed || current_location != Some(location) {
                    scene_moved |= current_location != Some(location);
                    self.desktop.redraw = true;
                    self.space.map_element(w.window.clone(), location, false);
                    w.rect = Some(r);
                }
                if let Some(t) = w.window.0.toplevel() {
                    t.with_pending_state(|s| {
                        s.size = Some((r.w, r.h).into());
                        s.bounds = Some((area.w, area.h).into());
                        s.states.set(XdgState::Activated);
                        if focused != Some(i) {
                            s.states.unset(XdgState::Activated);
                        }
                        if w.fullscreen {
                            s.states.set(XdgState::Fullscreen);
                        } else {
                            s.states.unset(XdgState::Fullscreen);
                        }
                        for state in [
                            XdgState::TiledLeft,
                            XdgState::TiledRight,
                            XdgState::TiledTop,
                            XdgState::TiledBottom,
                        ] {
                            if w.floating {
                                s.states.unset(state);
                            } else {
                                s.states.set(state);
                            }
                        }
                    });
                    if t.is_initial_configure_sent() {
                        t.send_pending_configure();
                    }
                }
                #[cfg(feature = "xwayland")]
                if let Some(x) = w.window.0.x11_surface() {
                    if target_changed || w.last_size != Some((r.w, r.h)) {
                        let _ =
                            x.configure(Some(Rectangle::new((r.x, r.y).into(), (r.w, r.h).into())));
                    }
                }
                w.last_size = Some((r.w, r.h));
            }
        }
        for w in &mut self.desktop.windows {
            let visible =
                !w.scratchpad && self.desktop.outputs.get(&w.output) == Some(&w.workspace);
            if !visible && self.space.element_location(&w.window).is_some() {
                scene_moved = true;
                w.movement = None;
                w.resize = None;
                if let Some(render_size) = w
                    .window
                    .0
                    .user_data()
                    .get::<crate::effects::WindowRenderSize>()
                {
                    *render_size.0.lock().unwrap() = None;
                }
                // Re-entering a workspace or restoring a scratchpad starts a
                // fresh fade. Hidden surfaces never keep the timer alive.
                w.opened = false;
                w.opening_started = None;
                crate::effects::clear_closing_backdrop(&w.window);
                self.space.unmap_elem(&w.window);
                self.desktop.redraw = true;
            }
        }
        // Mapping a changed tiled rectangle raises it in Smithay's Space. Restore
        // the floating layer only when necessary, preserving its existing order.
        let mut floats = Vec::new();
        let mut needs_restack = false;
        for window in self.space.elements() {
            if self
                .desktop
                .windows
                .iter()
                .any(|w| &w.window == window && w.floating)
            {
                floats.push(window.clone());
            } else if !floats.is_empty() {
                needs_restack = true;
            }
        }
        if needs_restack {
            for window in floats {
                self.space.raise_element(&window, false);
            }
            self.desktop.redraw = true;
        }
        let layer_focused = self
            .seat
            .get_keyboard()
            .and_then(|k| k.current_focus())
            .is_some_and(|f| {
                matches!(
                    f,
                    KeyboardFocusTarget::LayerSurface(_) | KeyboardFocusTarget::Popup(_)
                ) && f.alive()
            });
        let need_focus = !self.lock.locked
            && !layer_focused
            && self.focused_index().is_none_or(|i| {
                self.space
                    .element_location(&self.desktop.windows[i].window)
                    .is_none()
            });
        if need_focus {
            if let Some(i) = self
                .desktop
                .windows
                .iter()
                .rposition(|w| self.space.element_location(&w.window).is_some())
            {
                self.focus_window(i, false)
            } else if let Some(k) = self.seat.get_keyboard() {
                k.set_focus(self, None, SERIAL_COUNTER.next_serial());
            }
        }
        if self.desktop.active && !self.lock.locked && !self.pointer.is_grabbed() {
            let pointer = self.pointer.clone();
            let location = pointer.current_location();
            let under = self.surface_under(location);
            if pointer.current_focus() != under.as_ref().map(|(target, _)| target.clone())
                || scene_moved
            {
                pointer.motion(
                    self,
                    under,
                    &smithay::input::pointer::MotionEvent {
                        location,
                        serial: SERIAL_COUNTER.next_serial(),
                        time: smithay::backend::input::InputTime::now(),
                    },
                );
                pointer.frame(self);
            }
        }
        if animating && self.desktop.animation_timer.is_none() {
            // An idle high-refresh output must not increase animation work on
            // other outputs. Only displays with an unfinished transition count.
            let refresh = animation_refresh.clamp(30_000, 240_000);
            let delay = Duration::from_secs_f64(1000.0 / f64::from(refresh));
            self.desktop.animation_timer = self
                .handle
                .insert_source(
                    smithay::reexports::calloop::timer::Timer::from_duration(delay),
                    |_, _, state| {
                        state.desktop.animation_timer = None;
                        state.desktop.redraw = true;
                        smithay::reexports::calloop::timer::TimeoutAction::Drop
                    },
                )
                .ok();
        } else if !animating {
            if let Some(timer) = self.desktop.animation_timer.take() {
                self.handle.remove(timer);
            }
        }
        self.desktop.dirty = false;
        self.refresh_capture_sessions();
        self.publish_snapshot();
    }
    fn publish_snapshot(&mut self) {
        let mut snapshot = Snapshot {
            version: 1,
            focused: self.focused_index().map(|i| self.desktop.windows[i].id),
            error: self.desktop.error.clone(),
            ..Default::default()
        };
        for w in &self.desktop.windows {
            let (app_id, title) = identity(&w.window);
            snapshot.windows.push(WindowInfo {
                surface_size: w.window.wl_surface().and_then(|surface| {
                    smithay::backend::renderer::utils::with_renderer_surface_state(
                        &surface,
                        |state| state.surface_size().map(|size| [size.w, size.h]),
                    )
                    .flatten()
                }),
                id: w.id,
                title,
                app_id,
                workspace: w.workspace,
                output: w.output.clone(),
                floating: w.floating,
                fullscreen: w.fullscreen,
                scratchpad: w.scratchpad,
                geometry: w.rect,
                opacity: w.opacity,
            });
        }
        for o in self.space.outputs() {
            let layers = smithay::desktop::layer_map_for_output(o);
            for layer in layers.layers() {
                if let Some(g) = layers.layer_geometry(layer) {
                    snapshot.layers.push(wm_core::LayerInfo {
                        namespace: layer.namespace().into(),
                        output: o.name(),
                        surface_size:
                            smithay::backend::renderer::utils::with_renderer_surface_state(
                                layer.wl_surface(),
                                |state| state.surface_size().map(|size| [size.w, size.h]),
                            )
                            .flatten(),
                        geometry: Rect {
                            x: g.loc.x,
                            y: g.loc.y,
                            w: g.size.w,
                            h: g.size.h,
                        },
                    });
                }
            }
            if let Some(g) = self.space.output_geometry(o) {
                let workspace = *self.desktop.outputs.get(&o.name()).unwrap_or(&1);
                snapshot.outputs.push(OutputInfo {
                    name: o.name(),
                    workspace,
                    geometry: Rect {
                        x: g.loc.x,
                        y: g.loc.y,
                        w: g.size.w,
                        h: g.size.h,
                    },
                    wallpaper_visible: !self.lock.locked
                        && !o
                            .user_data()
                            .get::<crate::effects::BackgroundCovered>()
                            .is_some_and(|covered| {
                                covered.0.load(std::sync::atomic::Ordering::Relaxed)
                            })
                        && !self.desktop.windows.iter().any(|w| {
                            w.output == o.name() && w.workspace == workspace && w.fullscreen
                        }),
                    active: self.desktop.active,
                });
            }
        }
        let mut old = self.desktop.snapshot.lock().unwrap();
        if *old != snapshot {
            *old = snapshot.clone();
            self.desktop.subscribers.lock().unwrap().retain(|tx| {
                match tx.try_send(snapshot.clone()) {
                    Ok(()) => true,
                    Err(std::sync::mpsc::TrySendError::Full(_)) => true,
                    Err(_) => false,
                }
            });
        }
    }
}

fn binding_action<'a>(bindings: &'a BTreeMap<String, String>, key: &str) -> Option<&'a String> {
    bindings.get(key).or_else(|| {
        bindings
            .iter()
            .find(|(binding, _)| binding.eq_ignore_ascii_case(key))
            .map(|(_, action)| action)
    })
}

fn launch_args(command: &str) -> Result<Vec<String>, String> {
    let args = shlex::split(command).ok_or("launch has an unterminated quote")?;
    if args.is_empty() {
        Err("launch requires a program".into())
    } else {
        Ok(args)
    }
}

fn screenshot_copy_command() -> [String; 3] {
    [
        "sh".into(),
        "-c".into(),
        "grim -t png - | wl-copy --type image/png".into(),
    ]
}

#[cfg(test)]
mod binding_tests {
    use super::binding_action;
    use std::collections::BTreeMap;

    #[test]
    fn binding_lookup_accepts_shifted_keysym_case() {
        let bindings = BTreeMap::from([("Super+Shift+R".into(), "reload".into())]);
        assert_eq!(
            binding_action(&bindings, "Super+Shift+r"),
            Some(&"reload".into())
        );
    }

    #[test]
    fn exact_binding_spelling_wins_over_case_insensitive_fallback() {
        let bindings = BTreeMap::from([
            ("Super+q".into(), "close".into()),
            ("Super+Q".into(), "quit".into()),
        ]);
        assert_eq!(binding_action(&bindings, "Super+q"), Some(&"close".into()));
    }

    #[test]
    fn launch_arguments_preserve_quoted_values_without_using_a_shell() {
        assert_eq!(
            super::launch_args("firefox --new-window 'https://example.test/a b'").unwrap(),
            ["firefox", "--new-window", "https://example.test/a b"]
        );
        assert!(super::launch_args("").is_err());
        assert!(super::launch_args("firefox '").is_err());
    }

    #[test]
    fn screenshot_action_emits_png_to_the_wayland_clipboard() {
        assert_eq!(
            super::screenshot_copy_command(),
            ["sh", "-c", "grim -t png - | wl-copy --type image/png"]
        );
    }
}
