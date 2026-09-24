//! Desktop policy, intentionally independent of backend device management.
use crate::{
    AnvilState,
    focus::KeyboardFocusTarget,
    recorder::RecorderController,
    shell::{FullscreenSurface, WindowElement},
    state::{Backend, RecorderCaptureSource},
};
use smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel::State as XdgState;
use smithay::{
    desktop::layer_map_for_output,
    input::keyboard::{Keysym, ModifiersState},
    utils::{IsAlive, Rectangle, SERIAL_COUNTER},
    wayland::{compositor::with_states, seat::WaylandFocus, shell::xdg::XdgToplevelSurfaceData},
};
use std::os::unix::fs::PermissionsExt;
use std::{
    collections::BTreeMap,
    path::PathBuf,
    process::{Child, Command},
    sync::{Arc, Mutex},
    time::Duration,
};
use wm_core::{Config, OutputInfo, Rect, Snapshot, WindowInfo};

/// Find the standalone lock theme shipped with Luma. Do not let hyprlock
/// implicitly load a user's Hyprland config: its `source` directives and
/// wallpaper commands are unrelated to this compositor session.
fn lock_theme_config_path() -> Result<PathBuf, String> {
    let mut candidates = Vec::with_capacity(3);
    if let Ok(exe) = std::env::current_exe() {
        if let Some(directory) = exe.parent() {
            // Release archives install the config beside `luma-wm`.
            candidates.push(directory.join("hyprlock.conf"));
            // System packages install shared configuration under /usr/share.
            candidates.push(directory.join("../share/luma-wm/hyprlock.conf"));
        }
    }
    // Developer builds use the repository config without depending on cwd.
    candidates
        .push(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../config/luma-hyprlock.conf"));
    candidates
        .into_iter()
        .find(|path| path.is_file())
        .map(|path| path.canonicalize().unwrap_or(path))
        .ok_or_else(|| "Luma hyprlock.conf is missing; reinstall the compositor package".into())
}

fn executable_in_path(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|directory| directory.join(name))
        .find(|candidate| {
            std::fs::metadata(candidate).is_ok_and(|metadata| {
                metadata.is_file() && metadata.permissions().mode() & 0o111 != 0
            })
        })
}

fn session_lock_command() -> Result<Vec<String>, String> {
    if let Some(hyprlock) = executable_in_path("hyprlock") {
        match lock_theme_config_path() {
            Ok(config) => {
                return Ok(vec![
                    hyprlock.to_string_lossy().into_owned(),
                    "--config".into(),
                    config.to_string_lossy().into_owned(),
                    "--grace".into(),
                    "0".into(),
                    "--no-fade-in".into(),
                    "--immediate-render".into(),
                ]);
            }
            Err(error) => tracing::warn!(%error, "Luma theme unavailable; trying swaylock"),
        }
    }
    if let Some(swaylock) = executable_in_path("swaylock") {
        tracing::warn!("hyprlock unavailable; using swaylock fallback");
        return Ok(vec![swaylock.to_string_lossy().into_owned()]);
    }
    Err("no session locker is available; install hyprlock or swaylock".into())
}

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
    pub recorder: RecorderController,
    /// Session-only override selected through wmctl. Configuration reloads do
    /// not overwrite it; `auto` explicitly returns to configured automation.
    performance_profile_override: Option<String>,
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
            recorder: RecorderController::default(),
            performance_profile_override: None,
            animation_timer: None,
        }
    }
}

impl Desktop {
    fn configured_performance_profile(&self) -> &str {
        self.performance_profile_override
            .as_deref()
            .unwrap_or(&self.config.performance.profile)
    }
}
impl Drop for Desktop {
    fn drop(&mut self) {
        // Applications belong to this compositor session. In particular,
        // single-instance background programs must not survive with stale
        // Wayland/X11/D-Bus endpoints and intercept launches in the next login.
        for child in &mut self.children {
            let _ = child.kill();
            let _ = child.wait();
        }
        for (_, child, _) in &mut self.services {
            if let Some(child) = child {
                let _ = child.kill();
                let _ = child.wait();
            }
        }
        // RecorderController finalizes a live recording before falling back to
        // terminating its child in its own Drop implementation.
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
    fn performance_gaming_outputs(&self) -> Vec<String> {
        let profile = self.desktop.configured_performance_profile();
        if profile == "desktop" {
            return Vec::new();
        }
        if profile == "gaming" {
            return self.space.outputs().map(|output| output.name()).collect();
        }
        self.space
            .outputs()
            .filter(|output| {
                let name = output.name();
                let workspace = self.desktop.outputs.get(&name).copied().unwrap_or(1);
                self.desktop.windows.iter().any(|window| {
                    window.output == name
                        && window.workspace == workspace
                        && window.fullscreen
                        && !window.scratchpad
                })
            })
            .map(|output| output.name())
            .collect()
    }

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
        if let Some(socket) = &self.socket_name {
            let private_bus = std::env::var_os("WM_PRIVATE_BUS").is_some();
            let mut command = activation_environment_command(socket, private_bus);
            let published = match command.status() {
                Ok(status) if !status.success() => {
                    tracing::warn!(
                        %status,
                        "failed to publish the Luma session environment to D-Bus activation"
                    );
                    false
                }
                Err(error) => {
                    tracing::warn!(
                        %error,
                        "could not publish the Luma session environment to D-Bus activation"
                    );
                    false
                }
                _ => true,
            };
            if published && !private_bus {
                refresh_portal_services();
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
            .env(
                "XDG_CURRENT_DESKTOP",
                application_desktop_environment(first),
            )
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
    fn recorder_source(&self, value: &str) -> Result<RecorderCaptureSource, String> {
        let mut parts = value.split_whitespace();
        match parts.next().unwrap_or("output") {
            "output" => Ok(RecorderCaptureSource::Output),
            "window" => {
                let id = parts
                    .next()
                    .ok_or("window source requires an id")?
                    .parse::<u64>()
                    .map_err(|_| "window id must be a number")?;
                if parts.next().is_some()
                    || !self.desktop.windows.iter().any(|window| window.id == id)
                {
                    return Err("recorder window does not exist".into());
                }
                Ok(RecorderCaptureSource::Window(id))
            }
            "region" => {
                let values = parts
                    .map(|value| {
                        value
                            .parse::<i32>()
                            .map_err(|_| "region values must be integers")
                    })
                    .collect::<Result<Vec<_>, _>>()?;
                if values.len() != 4 || values[2] <= 0 || values[3] <= 0 {
                    return Err(
                        "region source requires x y width height with a positive size".into(),
                    );
                }
                Ok(RecorderCaptureSource::Region(Rect {
                    x: values[0],
                    y: values[1],
                    w: values[2],
                    h: values[3],
                }))
            }
            _ => Err("recorder source must be output, window ID, or region X Y W H".into()),
        }
    }
    fn remembered_recorder_source(
        &self,
        config: &wm_core::Recorder,
    ) -> Result<RecorderCaptureSource, String> {
        if config.window_app_id.is_empty() && config.window_title.is_empty() {
            return Ok(RecorderCaptureSource::Output);
        }
        let wanted_app = config.window_app_id.to_lowercase();
        let wanted_title = config.window_title.to_lowercase();
        self.desktop
            .windows
            .iter()
            .find(|window| {
                let (app_id, title) = identity(&window.window);
                let app_id = app_id.to_lowercase();
                let title = title.to_lowercase();
                if !wanted_app.is_empty()
                    && (!app_id.is_empty() && app_id.contains(&wanted_app)
                        || app_id.is_empty() && title.contains(&wanted_app))
                {
                    return true;
                }
                !wanted_title.is_empty() && title.contains(&wanted_title)
            })
            .map(|window| RecorderCaptureSource::Window(window.id))
            .ok_or_else(|| {
                format!(
                    "remembered window '{}' is not open",
                    if config.window_app_id.is_empty() {
                        config.window_title.clone()
                    } else {
                        config.window_app_id.clone()
                    }
                )
            })
    }
    fn recorder_output(&self, source: RecorderCaptureSource) -> Result<String, String> {
        match source {
            RecorderCaptureSource::Output => self
                .active_output()
                .ok_or_else(|| "recorder has no active output".into()),
            RecorderCaptureSource::Window(id) => self
                .desktop
                .windows
                .iter()
                .find(|window| window.id == id)
                .map(|window| window.output.clone())
                .ok_or_else(|| "recorder window does not exist".into()),
            RecorderCaptureSource::Region(region) => self
                .space
                .outputs()
                .filter_map(|output| {
                    let geometry = self.space.output_geometry(output)?;
                    let left = region.x.max(geometry.loc.x);
                    let top = region.y.max(geometry.loc.y);
                    let right = (region.x + region.w).min(geometry.loc.x + geometry.size.w);
                    let bottom = (region.y + region.h).min(geometry.loc.y + geometry.size.h);
                    let area = i64::from((right - left).max(0)) * i64::from((bottom - top).max(0));
                    (area > 0).then(|| (area, output.name()))
                })
                .max_by_key(|(area, _)| *area)
                .map(|(_, name)| name)
                .ok_or_else(|| "recorder region does not intersect an output".into()),
        }
    }
    pub fn desktop_command(&mut self, cmd: &str) -> Result<(), String> {
        if self.lock.locked && cmd != "status" && cmd != "performance status" {
            return Err("session is locked".into());
        }
        let (v, arg) = cmd.split_once(' ').unwrap_or((cmd, ""));
        let out = self.active_output().unwrap_or_default();
        let focused = self.focused_index();
        match v {
            "status" => return Ok(()),
            "performance" => {
                let mut parts = arg.split_whitespace();
                match parts.next().unwrap_or("status") {
                    "status" if parts.next().is_none() => {
                        self.publish_snapshot();
                        return Ok(());
                    }
                    "reset" if parts.next().is_none() => {
                        self.backend_data.reset_performance_metrics();
                    }
                    "profile" => {
                        let profile = parts
                            .next()
                            .ok_or("performance profile requires auto, desktop, or gaming")?;
                        if parts.next().is_some()
                            || !["auto", "desktop", "gaming"].contains(&profile)
                        {
                            return Err(
                                "performance profile requires auto, desktop, or gaming".into()
                            );
                        }
                        self.desktop.performance_profile_override = Some(profile.into());
                        self.desktop.redraw = true;
                    }
                    _ => {
                        return Err(
                            "performance requires status, reset, or profile auto|desktop|gaming"
                                .into(),
                        );
                    }
                }
                self.desktop.dirty = true;
                return Ok(());
            }
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
            "recorder" if arg.is_empty() => {
                let p = std::env::current_exe()
                    .map_err(|e| e.to_string())?
                    // The recorder panel is implemented by the native shell
                    // and remains available even when the desktop bar uses
                    // the legacy GTK backend.
                    .with_file_name("wm-shell-sctk");
                return self.spawn_app(&[p.to_string_lossy().into_owned(), "--recorder".into()]);
            }
            "recorder" => {
                let mut request = arg.splitn(2, ' ');
                let action = request.next().unwrap_or_default();
                let source_arg = request.next().unwrap_or("");
                match action {
                    "start" => {
                        let config = self.desktop.config.recorder.clone();
                        let source = if source_arg.trim() == "remembered" {
                            self.remembered_recorder_source(&config)?
                        } else {
                            self.recorder_source(source_arg)?
                        };
                        let capture_output = self.recorder_output(source)?;
                        self.capture_override = source;
                        self.capture_commit_driven = false;
                        if let Err(error) = self.start_capture_boost(config.screen_fps) {
                            self.capture_override = RecorderCaptureSource::Output;
                            return Err(error);
                        }
                        if let Err(error) = self.desktop.recorder.start(
                            &config,
                            false,
                            self.socket_name.as_deref(),
                            &capture_output,
                            false,
                        ) {
                            self.stop_capture_boost();
                            return Err(error);
                        }
                        self.desktop.recorder.status.source = Some(match self.capture_override {
                            RecorderCaptureSource::Output => "output".into(),
                            RecorderCaptureSource::Window(id) => format!("window {id}"),
                            RecorderCaptureSource::Region(region) => format!(
                                "region {},{} {}x{}",
                                region.x, region.y, region.w, region.h
                            ),
                        });
                    }
                    "game-start" => {
                        let profile_name = source_arg.trim();
                        if profile_name.is_empty() {
                            return Err("game-start requires a configured game profile name".into());
                        }
                        let config = self.desktop.config.recorder.clone();
                        let profile = config
                            .game_profiles
                            .iter()
                            .find(|profile| profile.name == profile_name)
                            .cloned()
                            .ok_or_else(|| {
                                format!("recorder game profile '{profile_name}' does not exist")
                            })?;
                        if !matches!(profile.api.as_str(), "opengl" | "vulkan") {
                            return Err(format!(
                                "game capture profile '{profile_name}' has unsupported API {}",
                                profile.api
                            ));
                        }
                        // This path is the renderer's own API present hook. Do
                        // not request compositor copies or start the capture
                        // boost timer: doing so would re-render the desktop at
                        // the requested game FPS and defeat its purpose.
                        self.capture_override = RecorderCaptureSource::Output;
                        self.desktop.recorder.start_game(&config, &profile)?;
                    }
                    "game-attach" => {
                        let pid = source_arg.trim().parse::<u32>().map_err(
                            |_| "game-attach requires one positive graphics-process PID",
                        )?;
                        if pid == 0 {
                            return Err(
                                "game-attach requires one positive graphics-process PID".into()
                            );
                        }
                        self.capture_override = RecorderCaptureSource::Output;
                        let config = self.desktop.config.recorder.clone();
                        self.desktop.recorder.start_game_attach(&config, pid)?;
                    }
                    "xwayland-start" => {
                        let window = source_arg
                            .trim()
                            .parse::<u32>()
                            .map_err(|_| "xwayland-start requires one positive X11 window ID")?;
                        if window == 0 {
                            return Err("xwayland-start requires one positive X11 window ID".into());
                        }
                        #[cfg(feature = "xwayland")]
                        {
                            let (managed_id, capture_output) = self
                                .desktop
                                .windows
                                .iter()
                                .find(|managed| {
                                    managed
                                        .window
                                        .0
                                        .x11_surface()
                                        .is_some_and(|surface| surface.window_id() == window)
                                })
                                .map(|managed| (managed.id, managed.output.clone()))
                                .ok_or_else(|| {
                                    format!(
                                        "X11 window {window:#x} is not managed by this Luma session"
                                    )
                                })?;
                            let config = self.desktop.config.recorder.clone();
                            self.capture_override = RecorderCaptureSource::Window(managed_id);
                            self.capture_commit_driven = true;
                            if let Err(error) = self.start_capture_boost(config.fps) {
                                self.capture_override = RecorderCaptureSource::Output;
                                self.capture_commit_driven = false;
                                return Err(error);
                            }
                            if let Err(error) = self.desktop.recorder.start_game_xwayland(
                                &config,
                                self.socket_name.as_deref(),
                                &capture_output,
                                window,
                            ) {
                                self.stop_capture_boost();
                                return Err(error);
                            }
                        }
                        #[cfg(not(feature = "xwayland"))]
                        return Err("this Luma build has no Xwayland support".into());
                    }
                    "replay-start" => {
                        let config = self.desktop.config.recorder.clone();
                        let source = self.recorder_source(source_arg)?;
                        let capture_output = self.recorder_output(source)?;
                        self.capture_override = source;
                        self.capture_commit_driven = false;
                        if let Err(error) = self.start_capture_boost(config.screen_fps) {
                            self.capture_override = RecorderCaptureSource::Output;
                            return Err(error);
                        }
                        if let Err(error) = self.desktop.recorder.start(
                            &config,
                            true,
                            self.socket_name.as_deref(),
                            &capture_output,
                            false,
                        ) {
                            self.stop_capture_boost();
                            return Err(error);
                        }
                        self.desktop.recorder.status.source = Some(match self.capture_override {
                            RecorderCaptureSource::Output => "output".into(),
                            RecorderCaptureSource::Window(id) => format!("window {id}"),
                            RecorderCaptureSource::Region(region) => format!(
                                "region {},{} {}x{}",
                                region.x, region.y, region.w, region.h
                            ),
                        });
                    }
                    "stop" => {
                        let game_capture = self.desktop.recorder.is_game_capture();
                        let result = self.desktop.recorder.stop();
                        if !game_capture {
                            self.stop_capture_boost();
                        }
                        result?;
                    }
                    "toggle" => {
                        if self.desktop.recorder.is_running() {
                            let result = self.desktop.recorder.stop();
                            self.stop_capture_boost();
                            result?;
                        } else {
                            let config = self.desktop.config.recorder.clone();
                            let capture_output =
                                self.recorder_output(RecorderCaptureSource::Output)?;
                            self.capture_override = RecorderCaptureSource::Output;
                            self.capture_commit_driven = false;
                            if let Err(error) = self.start_capture_boost(config.screen_fps) {
                                return Err(error);
                            }
                            if let Err(error) = self.desktop.recorder.start(
                                &config,
                                false,
                                self.socket_name.as_deref(),
                                &capture_output,
                                false,
                            ) {
                                self.stop_capture_boost();
                                return Err(error);
                            }
                        }
                    }
                    "pause" => {
                        self.desktop.recorder.toggle_pause()?;
                    }
                    "replay-save" => self.desktop.recorder.save_replay()?,
                    "set" => {
                        let (key, value) = source_arg
                            .trim()
                            .split_once(' ')
                            .ok_or("recorder set requires a KEY and VALUE")?;
                        if key.is_empty() || value.trim().is_empty() {
                            return Err("recorder set requires a KEY and VALUE".into());
                        }
                        // Runtime settings apply to the compositor's recorder
                        // config immediately and persist to the settings
                        // overlay file; a running encoding picks them up on
                        // its next start because the engine consumes its
                        // options at spawn time.
                        self.desktop.config.recorder.apply(key.trim(), value)?;
                        wm_core::write_recorder_settings(&self.desktop.config.recorder)?;
                    }
                    "settings-reset" => {
                        wm_core::clear_recorder_settings()?;
                        // Reload with the overlay gone; only the recorder
                        // section is replaced so the rest of the live state
                        // is untouched.
                        let base = wm_core::Config::load()?;
                        self.desktop.config.recorder = base.recorder;
                    }
                    "status" => return Ok(()),
                    _ => {
                        return Err(
                            "recorder requires start [source], game-start PROFILE, game-attach PID, xwayland-start WINDOW, replay-start [source], stop, toggle, pause, replay-save, set KEY VALUE, settings-reset, or status"
                                .into(),
                        );
                    }
                }
                self.desktop.dirty = true;
                return Ok(());
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
                if !B::SUPPORTS_SESSION_LOCK {
                    return Err(
                        "session locking is unavailable on this nested compositor backend".into(),
                    );
                }
                return self.spawn_app(&session_lock_command()?);
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
        if self.desktop.recorder.poll() {
            if !self.desktop.recorder.is_running() {
                self.stop_capture_boost();
            }
            self.desktop.dirty = true;
        }
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
        let gaming_outputs = self.performance_gaming_outputs();
        let performance_errors = self.backend_data.apply_performance_policy(
            &self.desktop.config.performance,
            &gaming_outputs,
            &self.desktop.config.outputs,
        );
        if let Some(error) = performance_errors.into_iter().next() {
            if self.desktop.error.as_deref() != Some(&error) {
                self.desktop.error = Some(error);
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
            #[cfg(feature = "xwayland")]
            let x11_auxiliary = window
                .0
                .x11_surface()
                .is_some_and(crate::shell::x11::is_auxiliary_window);
            #[cfg(not(feature = "xwayland"))]
            let x11_auxiliary = false;
            floating |= x11_auxiliary;
            let mut requested_size = (None, None);
            if x11_auxiliary {
                let size = window.0.geometry().size;
                if size.w > 0 {
                    requested_size.0 = Some(size.w);
                }
                if size.h > 0 {
                    requested_size.1 = Some(size.h);
                }
            }
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
        let gaming_active = !gaming_outputs.is_empty();
        let animate_movement = !gaming_active
            && !self.lock.locked
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
                !gaming_active
                    && self.desktop.active
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
            let mut effective_theme = self.desktop.config.theme.clone();
            if gaming_outputs.contains(&output.name()) {
                effective_theme.blur = false;
                effective_theme.blur_passes = 0;
                effective_theme.shadow_size = 0;
                effective_theme.shadow_opacity = 0.0;
                effective_theme.radius = 0.0;
                effective_theme.opacity = 1.0;
                effective_theme.animation_ms = 0;
                effective_theme.reduced_motion = true;
            }
            let next_theme = &effective_theme;
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
                        .unwrap_or(self.desktop.config.theme.blur);
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
                let opacity = if w.fullscreen || gaming_outputs.contains(&w.output) {
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
                        .unwrap_or(self.desktop.config.theme.opacity)
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
        let gaming_outputs = self.performance_gaming_outputs();
        let active_profile = if gaming_outputs.is_empty() {
            "desktop"
        } else {
            "gaming"
        };
        let mut snapshot = Snapshot {
            version: 1,
            focused: self.focused_index().map(|i| self.desktop.windows[i].id),
            error: self.desktop.error.clone(),
            recorder: self.desktop.recorder.status.clone(),
            recorder_settings: self.desktop.config.recorder.clone(),
            performance: wm_core::PerformanceStatus {
                configured_profile: self.desktop.configured_performance_profile().into(),
                active_profile: active_profile.into(),
                outputs: self.backend_data.performance_status(),
            },
            ..Default::default()
        };
        for w in &self.desktop.windows {
            let (app_id, title) = identity(&w.window);
            snapshot.windows.push(WindowInfo {
                #[cfg(feature = "xwayland")]
                x11_window: w.window.0.x11_surface().map(|surface| surface.window_id()),
                #[cfg(not(feature = "xwayland"))]
                x11_window: None,
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
        // Keep the shared status snapshot current on every maintenance pass so
        // one-shot `wmctl status` calls see fresh counters. Performance counters
        // move on every repaint, and recorder telemetry is sampled while
        // recording; only send either to subscribers on the IPC heartbeat.
        let ui_changed = update_status_snapshot(&mut old, snapshot.clone());
        if ui_changed {
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

fn same_snapshot_for_immediate_subscribers(left: &Snapshot, right: &Snapshot) -> bool {
    left.version == right.version
        && left.windows == right.windows
        && left.outputs == right.outputs
        && left.focused == right.focused
        && left.error == right.error
        && left.layers == right.layers
        && left.recorder.state == right.recorder.state
        && left.recorder.source == right.recorder.source
        && left.recorder.requested_fps == right.recorder.requested_fps
        && left.recorder.output_path == right.recorder.output_path
        && left.recorder.error == right.recorder.error
        && left.recorder_settings == right.recorder_settings
}

fn update_status_snapshot(current: &mut Snapshot, latest: Snapshot) -> bool {
    let ui_changed = !same_snapshot_for_immediate_subscribers(current, &latest);
    *current = latest;
    ui_changed
}

fn binding_action<'a>(bindings: &'a BTreeMap<String, String>, key: &str) -> Option<&'a String> {
    bindings.get(key).or_else(|| {
        bindings
            .iter()
            .find(|(binding, _)| binding.eq_ignore_ascii_case(key))
            .map(|(_, action)| action)
    })
}

fn application_desktop_environment(executable: &str) -> &'static str {
    // GSR's native overlay currently selects wlr-layer-shell only for a short
    // compositor allowlist. Advertise compatibility to that application alone;
    // adding `river` session-wide changes desktop portal selection.
    if std::path::Path::new(executable).file_name() == Some(std::ffi::OsStr::new("gsr-ui")) {
        "wm:wlr:river"
    } else {
        "wm:wlr"
    }
}

fn activation_environment_command(socket: &str, private_bus: bool) -> Command {
    let mut command = Command::new("dbus-update-activation-environment");
    // A display-manager session shares its D-Bus activation environment with
    // the persistent user systemd manager. Portal backends are systemd user
    // services, so updating only dbus-daemon leaves them without a display.
    // Nested/TTY fixtures use dbus-run-session and must not modify the host
    // user's systemd environment.
    if !private_bus {
        command.arg("--systemd");
    }
    command.args([
        format!("WAYLAND_DISPLAY={socket}"),
        "XDG_CURRENT_DESKTOP=wm:wlr".into(),
        "XDG_SESSION_DESKTOP=wm".into(),
        "XDG_SESSION_TYPE=wayland".into(),
    ]);
    command
}

fn refresh_portal_services() {
    // The user manager survives compositor restarts, and an already-running
    // portal keeps the environment it was launched with. Refresh active
    // backends first, clear any old start-limit failures, then restart the
    // frontend so OpenURI launches inherit this session's Wayland display.
    // Activation must stay asynchronous: install_desktop runs before the
    // Wayland event loop, so waiting for a portal that connects to this
    // compositor deadlocks startup and leaves only the initial black frame.
    for mut command in portal_refresh_commands() {
        match command.status() {
            Ok(status) if !status.success() => {
                tracing::warn!(%status, ?command, "could not queue desktop portal refresh")
            }
            Err(error) => tracing::warn!(
                %error,
                ?command,
                "could not queue desktop portal refresh"
            ),
            _ => {}
        }
    }
}

fn portal_refresh_commands() -> [Command; 3] {
    let mut reset_failed = Command::new("systemctl");
    reset_failed.args(["--user", "reset-failed", "xdg-desktop-portal-*.service"]);

    let mut restart_backends = Command::new("systemctl");
    restart_backends.args([
        "--user",
        "--no-block",
        "try-restart",
        "xdg-desktop-portal-*.service",
    ]);

    let mut restart_frontend = Command::new("systemctl");
    restart_frontend.args([
        "--user",
        "--no-block",
        "restart",
        "xdg-desktop-portal.service",
    ]);

    [reset_failed, restart_backends, restart_frontend]
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
    use super::{
        activation_environment_command, application_desktop_environment, binding_action,
        portal_refresh_commands, same_snapshot_for_immediate_subscribers, update_status_snapshot,
    };
    use std::collections::BTreeMap;
    use wm_core::{OutputPerformanceStatus, RecorderState, Snapshot};

    #[test]
    fn subscriber_updates_suppress_fast_counters_but_keep_latest_status() {
        let mut current = Snapshot::default();
        let mut latest = current.clone();
        latest.performance.outputs.push(OutputPerformanceStatus {
            empty_frames: 12,
            ..Default::default()
        });
        latest.recorder.source_fps = 60.0;
        latest.recorder.encoded_fps = 60.0;
        latest.recorder.elapsed_ms = 2_000;
        latest.recorder.dropped_frames = 3;
        latest.recorder.replay_seconds = 1.5;
        latest.recorder.replay_bytes = 65_536;

        assert!(same_snapshot_for_immediate_subscribers(&current, &latest));
        assert!(!update_status_snapshot(&mut current, latest));
        assert_eq!(current.performance.outputs[0].empty_frames, 12);
        assert_eq!(current.recorder.source_fps, 60.0);
        assert_eq!(current.recorder.encoded_fps, 60.0);
        assert_eq!(current.recorder.elapsed_ms, 2_000);
        assert_eq!(current.recorder.dropped_frames, 3);
        assert_eq!(current.recorder.replay_seconds, 1.5);
        assert_eq!(current.recorder.replay_bytes, 65_536);

        let mut changed = current.clone();
        changed.recorder.state = RecorderState::Recording;
        assert!(update_status_snapshot(&mut current, changed));
        assert_eq!(current.recorder.state, RecorderState::Recording);

        let mut changed = current.clone();
        changed.recorder.source = Some("output:DP-1".into());
        assert!(update_status_snapshot(&mut current, changed));
        assert_eq!(current.recorder.source.as_deref(), Some("output:DP-1"));

        let mut changed = current.clone();
        changed.recorder.requested_fps = 60;
        assert!(update_status_snapshot(&mut current, changed));
        assert_eq!(current.recorder.requested_fps, 60);

        let mut changed = current.clone();
        changed.recorder.output_path = Some("capture.mp4".into());
        assert!(update_status_snapshot(&mut current, changed));
        assert_eq!(current.recorder.output_path.as_deref(), Some("capture.mp4"));

        let mut changed = current.clone();
        changed.recorder.error = Some("test failure".into());
        assert!(update_status_snapshot(&mut current, changed));
        assert_eq!(current.recorder.error.as_deref(), Some("test failure"));

        let mut focus_changed = current.clone();
        focus_changed.focused = Some(42);
        assert!(!same_snapshot_for_immediate_subscribers(
            &current,
            &focus_changed
        ));
        assert!(update_status_snapshot(&mut current, focus_changed));
        assert_eq!(current.focused, Some(42));
    }

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

    #[test]
    fn gsr_uses_native_layer_shell_without_spoofing_the_whole_session() {
        assert_eq!(application_desktop_environment("gsr-ui"), "wm:wlr:river");
        assert_eq!(
            application_desktop_environment("/usr/bin/gsr-ui"),
            "wm:wlr:river"
        );
        assert_eq!(application_desktop_environment("vesktop"), "wm:wlr");
    }

    #[test]
    fn real_session_publishes_wayland_environment_to_dbus_and_systemd() {
        let command = activation_environment_command("wayland-7", false);
        assert_eq!(command.get_program(), "dbus-update-activation-environment");
        assert_eq!(
            command.get_args().collect::<Vec<_>>(),
            [
                "--systemd",
                "WAYLAND_DISPLAY=wayland-7",
                "XDG_CURRENT_DESKTOP=wm:wlr",
                "XDG_SESSION_DESKTOP=wm",
                "XDG_SESSION_TYPE=wayland",
            ]
        );
    }

    #[test]
    fn private_test_bus_does_not_replace_host_systemd_environment() {
        let command = activation_environment_command("wm-nested.sock", true);
        assert_eq!(
            command.get_args().collect::<Vec<_>>(),
            [
                "WAYLAND_DISPLAY=wm-nested.sock",
                "XDG_CURRENT_DESKTOP=wm:wlr",
                "XDG_SESSION_DESKTOP=wm",
                "XDG_SESSION_TYPE=wayland",
            ]
        );
    }

    #[test]
    fn portal_refresh_never_waits_for_wayland_dependent_services() {
        let commands = portal_refresh_commands();
        assert_eq!(
            commands[0].get_args().collect::<Vec<_>>(),
            ["--user", "reset-failed", "xdg-desktop-portal-*.service",]
        );
        assert_eq!(
            commands[1].get_args().collect::<Vec<_>>(),
            [
                "--user",
                "--no-block",
                "try-restart",
                "xdg-desktop-portal-*.service",
            ]
        );
        assert_eq!(
            commands[2].get_args().collect::<Vec<_>>(),
            [
                "--user",
                "--no-block",
                "restart",
                "xdg-desktop-portal.service",
            ]
        );
    }
}
