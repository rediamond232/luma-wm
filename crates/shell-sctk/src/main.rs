//! Low-overhead native Luma shell.
//!
//! This client deliberately has no GTK, GObject, or browser runtime.  It is
//! still opt-in while the feature-complete GTK shell remains the default.

use std::{
    collections::{BTreeMap, BTreeSet, HashMap, VecDeque},
    io::{BufRead, Read, Write},
    num::NonZeroU32,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc,
    },
    thread,
    time::{Duration, Instant},
};

use ab_glyph::{Font, FontArc, PxScale, ScaleFont, point};

use smithay_client_toolkit::reexports::{
    calloop::timer::{TimeoutAction, Timer},
    calloop::{EventLoop, RegistrationToken, channel},
    calloop_wayland_source::WaylandSource,
    client::{
        Connection, QueueHandle,
        globals::registry_queue_init,
        protocol::{wl_keyboard, wl_output, wl_pointer, wl_seat, wl_shm, wl_surface},
    },
};
use smithay_client_toolkit::{
    compositor::{CompositorHandler, CompositorState},
    delegate_compositor, delegate_keyboard, delegate_layer, delegate_output, delegate_pointer,
    delegate_registry, delegate_seat, delegate_shm,
    output::{OutputHandler, OutputState},
    registry::{ProvidesRegistryState, RegistryState},
    registry_handlers,
    seat::{
        Capability, SeatHandler, SeatState,
        keyboard::{KeyEvent, KeyboardHandler, Keysym, Modifiers, RawModifiers},
        pointer::{PointerEvent, PointerEventKind, PointerHandler},
    },
    shell::{
        WaylandSurface,
        wlr_layer::{
            Anchor, KeyboardInteractivity, Layer, LayerShell, LayerShellHandler, LayerSurface,
            LayerSurfaceConfigure,
        },
    },
    shm::{Shm, ShmHandler, slot::SlotPool},
};
use wm_core::{Config, RecorderState, Snapshot};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Mode {
    Bar,
    Wallpaper,
    Launcher,
    Recorder,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SurfaceKind {
    Bar,
    Wallpaper,
    Launcher,
    Recorder,
    Notifications,
    TrayMenu,
    Controls,
}

/// The native equivalents of GTK's bar popovers.  Keeping the selected panel
/// in the shell means the SCTK backend remains usable without a GTK process.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ControlPanel {
    Audio,
    Network,
    Bluetooth,
    Media,
    Notifications,
    Power,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PowerAction {
    LogOut,
    Reboot,
    Shutdown,
}

impl PowerAction {
    fn label(self) -> &'static str {
        match self {
            Self::LogOut => "Log out",
            Self::Reboot => "Reboot",
            Self::Shutdown => "Shut down",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RecorderCaptureMode {
    Screen,
    XwaylandDirect,
    OpenGlInject,
    OpenGlGame,
    VulkanGame,
}

impl RecorderCaptureMode {
    const ALL: [Self; 5] = [
        Self::Screen,
        Self::XwaylandDirect,
        Self::OpenGlInject,
        Self::OpenGlGame,
        Self::VulkanGame,
    ];

    fn label(self) -> &'static str {
        match self {
            Self::Screen => "Screen (low-lag)",
            Self::XwaylandDirect => "Xwayland Zero-Copy",
            Self::OpenGlInject => "OpenGL API Inject",
            Self::OpenGlGame => "OpenGL Launch Profile",
            Self::VulkanGame => "Vulkan API Layer",
        }
    }

    fn cycle(self, forward: bool) -> Self {
        let index = Self::ALL
            .iter()
            .position(|mode| *mode == self)
            .expect("capture mode is listed");
        let next = if forward {
            (index + 1) % Self::ALL.len()
        } else {
            (index + Self::ALL.len() - 1) % Self::ALL.len()
        };
        Self::ALL[next]
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RecorderView {
    Controls,
    Settings,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RecorderSettingsTab {
    Output,
    Capture,
    Video,
    Audio,
    Replay,
}

impl RecorderSettingsTab {
    const ALL: [Self; 5] = [
        Self::Output,
        Self::Capture,
        Self::Video,
        Self::Audio,
        Self::Replay,
    ];

    fn label(self) -> &'static str {
        match self {
            Self::Output => "Output",
            Self::Capture => "Capture",
            Self::Video => "Video",
            Self::Audio => "Audio",
            Self::Replay => "Replay",
        }
    }

    fn cycle(self, forward: bool) -> Self {
        let index = Self::ALL
            .iter()
            .position(|tab| *tab == self)
            .expect("settings tab is listed");
        let next = if forward {
            (index + 1) % Self::ALL.len()
        } else {
            (index + Self::ALL.len() - 1) % Self::ALL.len()
        };
        Self::ALL[next]
    }
}

/// What kind of editor a settings row renders. Shared by the drawing and the
/// pointer/keyboard hit tests so the two cannot drift apart.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RecorderRowKind {
    /// "< value >" cycle widget.
    Cycle,
    /// "< value >" where the value is a number.
    Step,
    /// On/off pill.
    Toggle,
    /// Text row; Enter enters an inline edit. Path rows edit a filesystem
    /// path, Identity rows edit a remembered window/process/profile match.
    Path,
    Identity,
    Action,
    /// Read-only informational row.
    Info,
}

/// One drawable settings row computed from the compositor's effective
/// settings (never the shell's local config copy).
#[derive(Clone)]
struct RecorderRowDisplay {
    label: &'static str,
    value: String,
    kind: RecorderRowKind,
}

/// A clickable region of the recorder panel, produced by
/// `recorder_hit_regions` and consumed by the pointer handler.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RecorderHit {
    StartStop,
    Pause,
    ReplaySave,
    Settings,
    Tab(usize),
    Row(usize, RecorderRowPart),
    CloseSettings,
    ResetSettings,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RecorderRowPart {
    Dec,
    Value,
    Inc,
}

fn recorder_capture_mode_label(settings: &wm_core::Recorder) -> String {
    match settings.capture_mode.as_str() {
        "xwayland" => "Xwayland window".into(),
        "inject" => "OpenGL inject".into(),
        "opengl" => "OpenGL profile".into(),
        "vulkan" => "Vulkan profile".into(),
        _ => "Screen".into(),
    }
}

/// Human-readable form of one remembered capture target. Empty identities
/// read as "None" instead of failing to render.
fn recorder_remembered_target_label(settings: &wm_core::Recorder) -> String {
    match settings.capture_mode.as_str() {
        "screen" | "xwayland" => recorder_window_identity_label(settings),
        "inject" => {
            if settings.inject_process.is_empty() {
                "None".into()
            } else {
                settings.inject_process.clone()
            }
        }
        "opengl" | "vulkan" => {
            if settings.game_profile.is_empty() {
                "None".into()
            } else {
                settings.game_profile.clone()
            }
        }
        _ => "None".into(),
    }
}

fn recorder_window_identity_label(settings: &wm_core::Recorder) -> String {
    if settings.window_app_id.is_empty() && settings.window_title.is_empty() {
        return "None".into();
    }
    if settings.window_title.is_empty() {
        return settings.window_app_id.clone();
    }
    if settings.window_app_id.is_empty() {
        return settings.window_title.clone();
    }
    format!("{} · {}", settings.window_app_id, settings.window_title)
}

fn recorder_window_identity_placeholder(settings: &wm_core::Recorder) -> String {
    if settings.window_app_id.is_empty() && settings.window_title.is_empty() {
        return String::new();
    }
    settings.window_app_id.clone()
}

fn recorder_settings_rows(
    settings: &wm_core::Recorder,
    tab: RecorderSettingsTab,
) -> Vec<RecorderRowDisplay> {
    match tab {
        RecorderSettingsTab::Output => vec![
            RecorderRowDisplay {
                label: "Encoder",
                value: format!("{} (NVENC)", settings.codec.to_uppercase()),
                kind: RecorderRowKind::Cycle,
            },
            RecorderRowDisplay {
                label: "Quality (CQ)",
                value: settings.quality.to_string(),
                kind: RecorderRowKind::Step,
            },
            RecorderRowDisplay {
                label: "HDR (10-bit PQ)",
                value: if settings.hdr { "On" } else { "Off" }.into(),
                kind: RecorderRowKind::Toggle,
            },
            RecorderRowDisplay {
                label: "Capture cursor",
                value: if settings.cursor { "On" } else { "Off" }.into(),
                kind: RecorderRowKind::Toggle,
            },
            RecorderRowDisplay {
                label: "Container",
                value: "MP4 (fragmented, streamable)".into(),
                kind: RecorderRowKind::Info,
            },
            RecorderRowDisplay {
                label: "Recording path",
                value: settings.output_directory.clone(),
                kind: RecorderRowKind::Path,
            },
        ],
        RecorderSettingsTab::Capture => vec![
            RecorderRowDisplay {
                label: "Method",
                value: recorder_capture_mode_label(settings),
                kind: RecorderRowKind::Cycle,
            },
            RecorderRowDisplay {
                label: "Remembered target",
                value: recorder_remembered_target_label(settings),
                kind: RecorderRowKind::Identity,
            },
            RecorderRowDisplay {
                label: "Remember current",
                value: "Save the selection below".into(),
                kind: RecorderRowKind::Action,
            },
            RecorderRowDisplay {
                label: "Method note",
                value: "Changed sources take effect now".into(),
                kind: RecorderRowKind::Info,
            },
        ],
        RecorderSettingsTab::Video => vec![
            RecorderRowDisplay {
                label: "Screen capture FPS",
                value: settings.screen_fps.to_string(),
                kind: RecorderRowKind::Step,
            },
            RecorderRowDisplay {
                label: "Injection FPS (attach)",
                value: settings.fps.to_string(),
                kind: RecorderRowKind::Step,
            },
            RecorderRowDisplay {
                label: "Output width",
                value: settings.output_width.to_string(),
                kind: RecorderRowKind::Step,
            },
            RecorderRowDisplay {
                label: "Output height",
                value: settings.output_height.to_string(),
                kind: RecorderRowKind::Step,
            },
            RecorderRowDisplay {
                label: "HDR note",
                value: "HDR needs the HEVC encoder".into(),
                kind: RecorderRowKind::Info,
            },
        ],
        RecorderSettingsTab::Audio => vec![
            RecorderRowDisplay {
                label: "Desktop audio",
                value: audio_source_label(&settings.desktop_audio, "default_output"),
                kind: RecorderRowKind::Cycle,
            },
            RecorderRowDisplay {
                label: "Microphone",
                value: audio_source_label(&settings.microphone, "default_input"),
                kind: RecorderRowKind::Cycle,
            },
            RecorderRowDisplay {
                label: "Program mix",
                value: "Opus 256 kbit · 48 kHz stereo".into(),
                kind: RecorderRowKind::Info,
            },
        ],
        RecorderSettingsTab::Replay => vec![
            RecorderRowDisplay {
                label: "Replay duration (s)",
                value: settings.replay_seconds.to_string(),
                kind: RecorderRowKind::Step,
            },
            RecorderRowDisplay {
                label: "Replay buffer (MiB)",
                value: settings.replay_max_mib.to_string(),
                kind: RecorderRowKind::Step,
            },
            RecorderRowDisplay {
                label: "Save replay",
                value: "Super+F8 during a recording".into(),
                kind: RecorderRowKind::Info,
            },
        ],
    }
}

fn audio_source_label(value: &str, default_name: &str) -> String {
    if value.is_empty() || value == "disabled" {
        "Disabled".into()
    } else if value == default_name {
        format!("Default ({value})")
    } else {
        value.to_string()
    }
}

/// Match a live window against the remembered identity: substring, case
/// insensitive, app_id preferred with title as the fallback. The same
/// matcher backs the panel's selection restore and the compositor's
/// `recorder start remembered` resolution.
fn window_matches_settings(window: &wm_core::WindowInfo, settings: &wm_core::Recorder) -> bool {
    let app_id = window.app_id.to_lowercase();
    let title = window.title.to_lowercase();
    let remembered_app = settings.window_app_id.to_lowercase();
    let remembered_title = settings.window_title.to_lowercase();
    if !remembered_app.is_empty()
        && (!app_id.is_empty() && app_id.contains(&remembered_app)
            || app_id.is_empty() && title.contains(&remembered_app))
    {
        return true;
    }
    !remembered_title.is_empty() && title.contains(&remembered_title)
}

/// Compute the ordered `recorder set KEY VALUE` commands for one Left/Right
/// adjustment of a settings row. Values are pre-validated against the same
/// rules the compositor enforces, so the UI never sends a command it knows
/// will be rejected.
fn adjust_recorder_setting(
    settings: &wm_core::Recorder,
    tab: RecorderSettingsTab,
    row: usize,
    direction: i32,
) -> Result<Vec<(String, String)>, String> {
    if direction == 0 {
        return Ok(vec![]);
    }
    let forward = direction > 0;
    let rows = recorder_settings_rows(settings, tab);
    if rows.get(row).is_none() {
        return Err("settings row is out of range".into());
    }
    let mut commands: Vec<(String, String)> = Vec::new();
    let step = |current: u64, min: u64, max: u64, amount: u64| -> String {
        let next = if forward {
            current.saturating_add(amount).min(max)
        } else {
            current.saturating_sub(amount).max(min)
        };
        next.to_string()
    };
    match tab {
        RecorderSettingsTab::Output => match row {
            0 => {
                // Two encoders: cycling either direction flips between them.
                let codec = if settings.codec == "h264" {
                    "hevc"
                } else {
                    "h264"
                };
                // HDR demands HEVC; dropping back to H.264 must drop HDR
                // first or the compositor rejects the command.
                if codec == "h264" && settings.hdr {
                    commands.push(("hdr".into(), "false".into()));
                }
                commands.push(("codec".into(), codec.into()));
            }
            1 => commands.push(("quality".into(), step(u64::from(settings.quality), 1, 51, 1))),
            2 => commands.push(("hdr".into(), (!settings.hdr).to_string())),
            3 => commands.push(("cursor".into(), (!settings.cursor).to_string())),
            4 => return Err("the container is fixed to fragmented MP4".into()),
            5 => return Err("press Enter to edit the recording path".into()),
            _ => return Err("settings row is out of range".into()),
        },
        RecorderSettingsTab::Capture => match row {
            0 => {
                let modes = ["screen", "xwayland", "inject", "opengl", "vulkan"];
                let current = modes
                    .iter()
                    .position(|mode| *mode == settings.capture_mode.as_str())
                    .unwrap_or(0);
                let next = if forward {
                    (current + 1) % modes.len()
                } else {
                    (current + modes.len() - 1) % modes.len()
                };
                commands.push(("capture_mode".into(), modes[next].into()));
            }
            1 => return Err("press Enter to edit the remembered target".into()),
            2 => return Err("press Enter to remember the current selection".into()),
            3 => return Ok(vec![]),
            _ => return Err("settings row is out of range".into()),
        },
        RecorderSettingsTab::Video => match row {
            0 => commands.push((
                "screen_fps".into(),
                step(u64::from(settings.screen_fps), 30, 480, 30),
            )),
            1 => commands.push((
                "fps".into(),
                step(u64::from(settings.fps), 30, 480, 30),
            )),
            2 => commands.push((
                "output_width".into(),
                step(u64::from(settings.output_width), 2, 16_384, 2),
            )),
            3 => commands.push((
                "output_height".into(),
                step(u64::from(settings.output_height), 2, 16_384, 2),
            )),
            _ => return Ok(vec![]),
        },
        RecorderSettingsTab::Audio => match row {
            0 => commands.push((
                "desktop_audio".into(),
                if settings.desktop_audio == "default_output" {
                    "disabled"
                } else {
                    "default_output"
                }
                .into(),
            )),
            1 => commands.push((
                "microphone".into(),
                if settings.microphone == "default_input" {
                    "disabled"
                } else {
                    "default_input"
                }
                .into(),
            )),
            _ => return Ok(vec![]),
        },
        RecorderSettingsTab::Replay => match row {
            0 => commands.push((
                "replay_seconds".into(),
                step(u64::from(settings.replay_seconds), 2, 3600, 5),
            )),
            1 => commands.push((
                "replay_max_mib".into(),
                step(u64::from(settings.replay_max_mib), 64, 8192, 64),
            )),
            _ => return Ok(vec![]),
        },
    }
    // Validate every command against the compositor's rules before sending.
    let mut candidate = settings.clone();
    for (key, value) in &commands {
        candidate
            .apply(key, value)
            .map_err(|error| format!("{key}: {error}"))?;
    }
    Ok(commands)
}

/// Pure Start-button decision, checked in the order a real start can fail so
/// the label and `recorder_can_start` can never disagree.
///
/// Direct graphics-API paths always encode their own SDR H.264 stream. Their
/// output is deliberately independent from the Screen/Xwayland codec, which
/// may remain HEVC HDR while a game profile is started.
fn recorder_start_blocker_for(
    mode: RecorderCaptureMode,
    settings: &wm_core::Recorder,
    has_xwayland_window: bool,
    has_attach_target: bool,
    profile_count: usize,
    has_selected_profile: bool,
) -> Option<&'static str> {
    if !settings.enabled {
        return Some("RECORDER DISABLED IN SETTINGS");
    }
    match mode {
        RecorderCaptureMode::Screen => None,
        RecorderCaptureMode::XwaylandDirect => {
            (!has_xwayland_window).then_some("NO XWAYLAND WINDOW OPEN")
        }
        RecorderCaptureMode::OpenGlInject => {
            if !has_attach_target {
                Some("NO RUNNING OPENGL PROCESS FOUND")
            } else {
                None
            }
        }
        RecorderCaptureMode::OpenGlGame | RecorderCaptureMode::VulkanGame => {
            if profile_count == 0 || !has_selected_profile {
                Some("NO MATCHING API PROFILE CONFIGURED")
            } else {
                None
            }
        }
    }
}

fn recorder_attach_targets() -> Vec<RecorderAttachTarget> {
    use std::os::unix::fs::MetadataExt;

    let own_uid = match std::fs::metadata("/proc/self") {
        Ok(metadata) => metadata.uid(),
        Err(_) => return Vec::new(),
    };
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return Vec::new();
    };
    let mut targets = Vec::new();
    for entry in entries.flatten() {
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|name| name.parse::<u32>().ok())
        else {
            continue;
        };
        if !entry
            .metadata()
            .is_ok_and(|metadata| metadata.uid() == own_uid)
        {
            continue;
        }
        let proc = entry.path();
        let Ok(maps) = std::fs::read_to_string(proc.join("maps")) else {
            continue;
        };
        let opengl = maps.lines().any(|line| {
            line.contains("/libGL.so")
                || line.contains("/libGLX.so")
                || line.contains("/libOpenGL.so")
                || line.contains("/libEGL.so")
        });
        if !opengl || pid == std::process::id() {
            continue;
        }
        // Never read /proc/PID/cmdline here: Minecraft launch arguments can
        // contain access tokens. `comm` is enough for a safe picker label.
        let comm = std::fs::read_to_string(proc.join("comm"))
            .ok()
            .map(|value| value.trim().to_owned())
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| "java".into());
        if matches!(comm.as_str(), "wm" | "wm-shell" | "wm-shell-sctk") {
            continue;
        }
        targets.push(RecorderAttachTarget {
            pid,
            comm,
            api: "OpenGL",
        });
    }
    targets.sort_by_key(|target| target.pid);
    targets
}

#[derive(Clone, Debug)]
struct RecorderAttachTarget {
    pid: u32,
    comm: String,
    api: &'static str,
}

impl Mode {
    fn from_args() -> Self {
        if std::env::args().any(|arg| arg == "--wallpaper") {
            Self::Wallpaper
        } else if std::env::args().any(|arg| arg == "--launcher") {
            Self::Launcher
        } else if std::env::args().any(|arg| arg == "--recorder") {
            Self::Recorder
        } else {
            Self::Bar
        }
    }
}

struct Surface {
    kind: SurfaceKind,
    layer: LayerSurface,
    pool: SlotPool,
    pool_size: usize,
    width: u32,
    height: u32,
    configured: bool,
    frame_pending: bool,
    redraw_requested: bool,
    output: Option<wl_output::WlOutput>,
}

#[derive(Clone)]
struct Notification {
    id: u32,
    icon: Option<image::RgbaImage>,
    summary: String,
    body: String,
    actions: Vec<(String, String)>,
    expires_at: Option<std::time::Instant>,
    critical: bool,
}

const CONFIG_ERROR_NOTIFICATION_ID: u32 = u32::MAX;
/// Bar geometry is shared by drawing and pointer hit testing. The left island
/// holds the launcher and the workspace ribbon; status chips run from right.
const BAR_RIGHT_INSET: u32 = 8;
const BAR_LAUNCHER_RIGHT: u32 = 40;
const BAR_WORKSPACE_START: u32 = 48;
const BAR_WORKSPACE_STEP: u32 = 26;
const NOTIFICATION_LIFETIME: Duration = Duration::from_secs(5);

fn application_notification_expiry(now: Instant) -> Instant {
    now + NOTIFICATION_LIFETIME
}

fn config_error_notification(error: &str) -> Notification {
    Notification {
        id: CONFIG_ERROR_NOTIFICATION_ID,
        icon: None,
        summary: "Configuration warning".into(),
        body: notification_body_text(error),
        actions: Vec::new(),
        // Retain the warning until a valid configuration replaces it.
        expires_at: None,
        critical: true,
    }
}

enum NotificationEvent {
    Upsert(Notification),
    Close { id: u32, reason: u32 },
    Dismiss { id: u32 },
}

enum NotificationSignal {
    Action { id: u32, key: String },
    Closed { id: u32, reason: u32 },
}

#[derive(Clone, PartialEq, Eq)]
struct AudioState {
    label: String,
}

impl Default for AudioState {
    fn default() -> Self {
        Self {
            label: "AUDIO —".into(),
        }
    }
}

#[derive(Clone, PartialEq, Eq)]
struct NetworkState {
    label: String,
    networking_enabled: bool,
    wireless_enabled: bool,
}

#[derive(Clone, PartialEq, Eq)]
struct MediaState {
    label: String,
}

#[derive(Clone, PartialEq, Eq)]
struct BluetoothState {
    label: String,
    powered: Option<bool>,
}

type BluezManagedObjects = std::collections::HashMap<
    zbus::zvariant::OwnedObjectPath,
    std::collections::HashMap<
        String,
        std::collections::HashMap<String, zbus::zvariant::OwnedValue>,
    >,
>;

#[derive(Clone, PartialEq, Eq)]
struct TrayItem {
    id: String,
    service: String,
    path: String,
    icon: Option<image::RgbaImage>,
    visible: bool,
    menu_path: Option<String>,
}

impl TrayItem {
    fn has_visible_icon(&self) -> bool {
        self.visible && self.icon.is_some()
    }
}

enum TrayEvent {
    Upsert(TrayItem),
    Remove(String),
}

#[derive(Clone)]
struct TrayMenuRow {
    id: i32,
    label: String,
    enabled: bool,
    separator: bool,
    submenu: bool,
    toggle_state: Option<i32>,
}

#[derive(Clone)]
struct TrayMenuState {
    service: String,
    path: String,
    menu_path: String,
    output: wl_output::WlOutput,
    local_x: i32,
    action_position: (i32, i32),
    parents: Vec<i32>,
    rows: Vec<TrayMenuRow>,
}

enum TrayMenuEvent {
    Loaded {
        service: String,
        menu_path: String,
        parent: i32,
        rows: Option<Vec<TrayMenuRow>>,
    },
}

struct TrayServer {
    sender: channel::Sender<TrayEvent>,
    items: Arc<Mutex<Vec<String>>>,
    host_registered: Arc<Mutex<bool>>,
}

#[zbus::interface(name = "org.kde.StatusNotifierWatcher")]
impl TrayServer {
    fn register_status_notifier_item(
        &self,
        value: String,
        #[zbus(header)] header: zbus::message::Header<'_>,
    ) {
        let value = value.chars().take(256).collect::<String>();
        let (service, path) = if value.starts_with('/') {
            let Some(sender) = header.sender() else {
                return;
            };
            (sender.to_string(), value)
        } else {
            (value, "/StatusNotifierItem".into())
        };
        let id = format!("{service}{path}");
        let mut items = self.items.lock().expect("tray item list poisoned");
        if !items.contains(&id) {
            items.push(id.clone());
            let _ = self.sender.send(TrayEvent::Upsert(load_tray_item(
                id.clone(),
                service.clone(),
                path.clone(),
            )));
            spawn_tray_item_monitor(id, service, path, self.sender.clone(), self.items.clone());
        }
    }

    fn register_status_notifier_host(&self, _service: String) {
        *self
            .host_registered
            .lock()
            .expect("tray host registration poisoned") = true;
    }

    #[zbus(property)]
    fn registered_status_notifier_items(&self) -> Vec<String> {
        self.items.lock().expect("tray item list poisoned").clone()
    }

    #[zbus(property)]
    fn is_status_notifier_host_registered(&self) -> bool {
        *self
            .host_registered
            .lock()
            .expect("tray host registration poisoned")
    }

    #[zbus(property)]
    fn protocol_version(&self) -> i32 {
        0
    }
}

fn tray_pixmap(pixmaps: Vec<(i32, i32, Vec<u8>)>) -> Option<image::RgbaImage> {
    let (width, height, pixels) = pixmaps
        .into_iter()
        .filter(|(width, height, pixels)| {
            *width > 0
                && *height > 0
                && *width <= 256
                && *height <= 256
                && pixels.len() == *width as usize * *height as usize * 4
        })
        .min_by_key(|(width, height, _)| {
            (width - 24).unsigned_abs() + (height - 24).unsigned_abs()
        })?;
    let mut rgba = Vec::with_capacity(pixels.len());
    for pixel in pixels.chunks_exact(4) {
        rgba.extend_from_slice(&[pixel[1], pixel[2], pixel[3], pixel[0]]);
    }
    image::RgbaImage::from_raw(width as u32, height as u32, rgba)
}

fn load_named_tray_icon(name: &str, theme_paths: Option<&str>) -> Option<image::RgbaImage> {
    if name.is_empty()
        || name.len() > 128
        || !name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
    {
        return None;
    }
    let mut roots = theme_paths
        .into_iter()
        .flat_map(|paths| paths.split(':'))
        .take(16)
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .collect::<Vec<_>>();
    roots.extend([
        PathBuf::from("/usr/share/icons"),
        PathBuf::from("/usr/share/pixmaps"),
    ]);
    if let Some(home) = std::env::var_os("HOME") {
        roots.insert(0, PathBuf::from(home).join(".local/share/icons"));
    }
    let candidates = roots.into_iter().flat_map(|root| {
        [
            root.join(format!("{name}.png")),
            root.join("hicolor/16x16/status")
                .join(format!("{name}.png")),
            root.join("hicolor/16x16/apps").join(format!("{name}.png")),
            root.join("hicolor/24x24/status")
                .join(format!("{name}.png")),
            root.join("hicolor/24x24/apps").join(format!("{name}.png")),
            root.join("hicolor/32x32/status")
                .join(format!("{name}.png")),
            root.join("hicolor/32x32/apps").join(format!("{name}.png")),
        ]
    });
    for path in candidates {
        let Ok(reader) = image::ImageReader::open(path) else {
            continue;
        };
        let Ok(image) = reader.decode() else {
            continue;
        };
        let image = image.into_rgba8();
        if image.width() <= 256 && image.height() <= 256 {
            return Some(image);
        }
    }
    None
}

fn load_tray_item(id: String, service: String, path: String) -> TrayItem {
    let (icon, visible, menu_path) = (|| {
        let connection = zbus::blocking::Connection::session().ok()?;
        let item = zbus::blocking::Proxy::new(
            &connection,
            service.as_str(),
            path.as_str(),
            "org.kde.StatusNotifierItem",
        )
        .ok()?;
        let status = item
            .get_property::<String>("Status")
            .unwrap_or_else(|_| "Active".into());
        let visible = status != "Passive";
        let theme_paths = item.get_property::<String>("IconThemePath").ok();
        let named_icon = |property| {
            item.get_property::<String>(property)
                .ok()
                .and_then(|name| load_named_tray_icon(&name, theme_paths.as_deref()))
        };
        let normal_icon = || {
            tray_pixmap(item.get_property("IconPixmap").unwrap_or_default())
                .or_else(|| named_icon("IconName"))
        };
        let icon = if status == "NeedsAttention" {
            tray_pixmap(item.get_property("AttentionIconPixmap").unwrap_or_default())
                .or_else(|| named_icon("AttentionIconName"))
                .or_else(normal_icon)
        } else {
            normal_icon()
        };
        let overlay = tray_pixmap(item.get_property("OverlayIconPixmap").unwrap_or_default())
            .or_else(|| named_icon("OverlayIconName"));
        let menu_path = item
            .get_property::<zbus::zvariant::OwnedObjectPath>("Menu")
            .ok()
            .map(|path| path.to_string())
            .filter(|path| path != "/");
        Some((
            icon.map(|icon| overlay_tray_icon(icon, overlay)),
            visible,
            menu_path,
        ))
    })()
    .unwrap_or((None, true, None));
    TrayItem {
        id,
        service,
        path,
        icon,
        visible,
        menu_path,
    }
}

fn overlay_tray_icon(
    mut icon: image::RgbaImage,
    overlay: Option<image::RgbaImage>,
) -> image::RgbaImage {
    let Some(overlay) = overlay else {
        return icon;
    };
    let width = overlay.width().min(icon.width());
    let height = overlay.height().min(icon.height());
    let x = icon.width().saturating_sub(width);
    let y = icon.height().saturating_sub(height);
    for row in 0..height {
        for column in 0..width {
            let pixel = overlay.get_pixel(column, row);
            if pixel.0[3] != 0 {
                let base = icon.get_pixel(x + column, y + row);
                let alpha = u32::from(pixel.0[3]);
                let base_alpha = u32::from(base.0[3]);
                let out_alpha = alpha + base_alpha * (255 - alpha) / 255;
                let blend = |foreground: u8, background: u8| {
                    if out_alpha == 0 {
                        0
                    } else {
                        ((u32::from(foreground) * alpha * 255
                            + u32::from(background) * base_alpha * (255 - alpha))
                            / (out_alpha * 255)) as u8
                    }
                };
                icon.put_pixel(
                    x + column,
                    y + row,
                    image::Rgba([
                        blend(pixel.0[0], base.0[0]),
                        blend(pixel.0[1], base.0[1]),
                        blend(pixel.0[2], base.0[2]),
                        out_alpha as u8,
                    ]),
                );
            }
        }
    }
    icon
}

fn spawn_tray_item_monitor(
    id: String,
    service: String,
    path: String,
    sender: channel::Sender<TrayEvent>,
    items: Arc<Mutex<Vec<String>>>,
) {
    let owner_id = id.clone();
    let owner_service = service.clone();
    let owner_sender = sender.clone();
    let owner_items = items.clone();
    thread::spawn(move || {
        let Ok(connection) = zbus::blocking::Connection::session() else {
            remove_tray_item(&owner_id, &owner_sender, &owner_items);
            return;
        };
        let Ok(bus) = zbus::blocking::Proxy::new(
            &connection,
            "org.freedesktop.DBus",
            "/org/freedesktop/DBus",
            "org.freedesktop.DBus",
        ) else {
            remove_tray_item(&owner_id, &owner_sender, &owner_items);
            return;
        };
        let Ok(signals) = bus.receive_signal("NameOwnerChanged") else {
            remove_tray_item(&owner_id, &owner_sender, &owner_items);
            return;
        };
        let Ok(owner) = bus.call::<_, _, String>("GetNameOwner", &(owner_service.as_str(),)) else {
            remove_tray_item(&owner_id, &owner_sender, &owner_items);
            return;
        };
        // The match rule is installed before querying the owner. Re-check it
        // afterwards so an application that exits in that small window cannot
        // leave a permanent icon behind.
        if bus
            .call::<_, _, String>("GetNameOwner", &(owner_service.as_str(),))
            .ok()
            .as_deref()
            != Some(owner.as_str())
        {
            remove_tray_item(&owner_id, &owner_sender, &owner_items);
            return;
        }
        for signal in signals {
            let removed = signal
                .body()
                .deserialize::<(String, String, String)>()
                .ok()
                .is_some_and(|(name, old_owner, new_owner)| {
                    tray_service_owner_lost(&name, &old_owner, &new_owner, &owner_service, &owner)
                });
            if removed {
                remove_tray_item(&owner_id, &owner_sender, &owner_items);
                return;
            }
        }
        remove_tray_item(&owner_id, &owner_sender, &owner_items);
    });

    thread::spawn(move || {
        let Ok(connection) = zbus::blocking::Connection::session() else {
            remove_tray_item(&id, &sender, &items);
            return;
        };
        let Ok(item) = zbus::blocking::Proxy::new(
            &connection,
            service.as_str(),
            path.as_str(),
            "org.kde.StatusNotifierItem",
        ) else {
            remove_tray_item(&id, &sender, &items);
            return;
        };
        let Ok(signals) = item.receive_all_signals() else {
            remove_tray_item(&id, &sender, &items);
            return;
        };
        for signal in signals {
            if !tray_signal_needs_refresh(signal.header().member().map(|member| member.as_str())) {
                continue;
            }
            if sender
                .send(TrayEvent::Upsert(load_tray_item(
                    id.clone(),
                    service.clone(),
                    path.clone(),
                )))
                .is_err()
            {
                return;
            }
        }
        remove_tray_item(&id, &sender, &items);
    });
}

fn remove_tray_item(
    id: &str,
    sender: &channel::Sender<TrayEvent>,
    items: &Arc<Mutex<Vec<String>>>,
) {
    let removed = {
        let mut registered = items.lock().expect("tray item list poisoned");
        let Some(index) = registered.iter().position(|known| known == id) else {
            return;
        };
        registered.remove(index);
        true
    };
    if removed {
        let _ = sender.send(TrayEvent::Remove(id.to_owned()));
    }
}

fn tray_service_owner_lost(
    name: &str,
    old_owner: &str,
    new_owner: &str,
    service: &str,
    owner: &str,
) -> bool {
    name == service && old_owner == owner && new_owner != owner
}

fn tray_signal_needs_refresh(member: Option<&str>) -> bool {
    matches!(
        member,
        Some(
            "PropertiesChanged"
                | "NewIcon"
                | "NewAttentionIcon"
                | "NewOverlayIcon"
                | "NewStatus"
                | "NewIconThemePath"
        )
    )
}

type RawTrayMenuLayout = (
    i32,
    HashMap<String, zbus::zvariant::OwnedValue>,
    Vec<zbus::zvariant::OwnedValue>,
);

fn tray_menu_property_string(
    properties: &HashMap<String, zbus::zvariant::OwnedValue>,
    key: &str,
) -> String {
    properties
        .get(key)
        .and_then(|value| <&str>::try_from(value).ok())
        .unwrap_or_default()
        .chars()
        .take(256)
        .map(|character| {
            if character.is_control() {
                ' '
            } else {
                character
            }
        })
        .collect()
}

fn parse_tray_menu_layout(layout: RawTrayMenuLayout, parent: i32) -> Option<Vec<TrayMenuRow>> {
    if layout.0 != parent || layout.2.len() > 128 {
        return None;
    }
    let mut ids = BTreeSet::new();
    let mut rows = Vec::new();
    for child in layout.2 {
        let structure: zbus::zvariant::Structure<'static> = child.try_into().ok()?;
        let (id, properties, _children): RawTrayMenuLayout = structure.try_into().ok()?;
        if id == parent || !ids.insert(id) {
            return None;
        }
        let visible = properties
            .get("visible")
            .and_then(|value| bool::try_from(value).ok())
            .unwrap_or(true);
        if !visible {
            continue;
        }
        let separator = tray_menu_property_string(&properties, "type") == "separator";
        rows.push(TrayMenuRow {
            id,
            label: tray_menu_property_string(&properties, "label"),
            enabled: properties
                .get("enabled")
                .and_then(|value| bool::try_from(value).ok())
                .unwrap_or(true),
            separator,
            submenu: tray_menu_property_string(&properties, "children-display") == "submenu",
            toggle_state: properties
                .get("toggle-state")
                .and_then(|value| i32::try_from(value).ok()),
        });
    }
    Some(rows)
}

fn load_tray_menu_page(
    service: String,
    menu_path: String,
    parent: i32,
    sender: channel::Sender<TrayMenuEvent>,
) {
    thread::spawn(move || {
        let rows = (|| {
            let connection = zbus::blocking::Connection::session().ok()?;
            let menu = zbus::blocking::Proxy::new(
                &connection,
                service.as_str(),
                menu_path.as_str(),
                "com.canonical.dbusmenu",
            )
            .ok()?;
            let _ = menu.call::<_, _, bool>("AboutToShow", &(parent,));
            let properties = vec![
                "label",
                "type",
                "enabled",
                "visible",
                "toggle-type",
                "toggle-state",
                "children-display",
            ];
            let (_, layout) = menu
                .call::<_, _, (u32, RawTrayMenuLayout)>("GetLayout", &(parent, 1i32, properties))
                .ok()?;
            parse_tray_menu_layout(layout, parent)
        })();
        let _ = sender.send(TrayMenuEvent::Loaded {
            service,
            menu_path,
            parent,
            rows,
        });
    });
}

fn activate_tray_menu_entry(service: String, menu_path: String, id: i32) {
    thread::spawn(move || {
        let Ok(connection) = zbus::blocking::Connection::session() else {
            return;
        };
        let Ok(menu) = zbus::blocking::Proxy::new(
            &connection,
            service.as_str(),
            menu_path.as_str(),
            "com.canonical.dbusmenu",
        ) else {
            return;
        };
        let timestamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |duration| duration.as_millis() as u32);
        let _ = menu.call::<_, _, ()>(
            "Event",
            &(id, "clicked", zbus::zvariant::Value::I32(0), timestamp),
        );
    });
}

struct NotificationServer {
    next_id: std::sync::atomic::AtomicU32,
    sender: channel::Sender<NotificationEvent>,
}

#[zbus::interface(name = "org.freedesktop.Notifications")]
impl NotificationServer {
    fn get_capabilities(&self) -> Vec<String> {
        vec!["actions".into(), "body".into(), "body-markup".into()]
    }

    fn get_server_information(&self) -> (String, String, String, String) {
        (
            "Luma SCTK shell".into(),
            "Luma".into(),
            "0.1.0".into(),
            "1.2".into(),
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn notify(
        &self,
        _app_name: String,
        replaces_id: u32,
        app_icon: String,
        summary: String,
        body: String,
        actions: Vec<String>,
        _hints: BTreeMap<String, zbus::zvariant::OwnedValue>,
        _expire_timeout: i32,
    ) -> u32 {
        let id = if replaces_id == 0 {
            self.next_id
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
                .wrapping_add(1)
                .max(1)
        } else {
            replaces_id
        };
        let expires_at = Some(application_notification_expiry(Instant::now()));
        let _ = self.sender.send(NotificationEvent::Upsert(Notification {
            id,
            icon: load_notification_icon(&app_icon),
            summary: notification_summary_text(&summary),
            body: notification_body_text(&body),
            actions: actions
                .chunks_exact(2)
                .take(4)
                .map(|pair| {
                    (
                        pair[0].chars().take(128).collect(),
                        pair[1].chars().take(128).collect(),
                    )
                })
                .collect(),
            expires_at,
            critical: false,
        }));
        id
    }

    fn close_notification(&self, id: u32) {
        let _ = self.sender.send(NotificationEvent::Close { id, reason: 3 });
    }

    #[zbus(signal)]
    async fn action_invoked(
        emitter: &zbus::object_server::SignalEmitter<'_>,
        id: u32,
        action_key: &str,
    ) -> zbus::Result<()>;

    #[zbus(signal)]
    async fn notification_closed(
        emitter: &zbus::object_server::SignalEmitter<'_>,
        id: u32,
        reason: u32,
    ) -> zbus::Result<()>;
}

#[derive(Clone)]
struct DesktopEntry {
    name: String,
    exec: Vec<String>,
}

#[derive(Clone)]
enum LauncherItem {
    App(usize),
    Window(u64),
    Command(String),
    Action(&'static str),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum LauncherCategory {
    Apps,
    Windows,
    Commands,
    Power,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum LauncherHit {
    Category(LauncherCategory),
    Result(usize),
}

const LAUNCHER_CATEGORY_CHIPS: [(LauncherCategory, &str, u32); 4] = [
    (LauncherCategory::Apps, "APPS", 47),
    (LauncherCategory::Windows, "@ WINDOWS", 79),
    (LauncherCategory::Commands, "> COMMANDS", 91),
    (LauncherCategory::Power, ": POWER", 67),
];

const LAUNCHER_CATEGORY_TOP: u32 = 140;
const LAUNCHER_RESULTS_TOP: u32 = 185;
const LAUNCHER_RESULT_STEP: u32 = 35;
const LAUNCHER_RESULT_HEIGHT: u32 = 30;
const LAUNCHER_RESULTS_BOTTOM_INSET: u32 = 48;

fn launcher_panel_rect(width: u32, height: u32) -> [u32; 4] {
    let panel_w = width.min(680);
    let panel_h = height.min(420);
    [
        (width - panel_w) / 2,
        (height - panel_h) / 2,
        panel_w,
        panel_h,
    ]
}

fn launcher_category_regions(
    width: u32,
    height: u32,
) -> [(LauncherCategory, &'static str, [u32; 4]); 4] {
    let [panel_x, panel_y, _, _] = launcher_panel_rect(width, height);
    let mut category_x = panel_x + 92;
    std::array::from_fn(|index| {
        let (category, label, chip_width) = LAUNCHER_CATEGORY_CHIPS[index];
        let rect = [category_x, panel_y + LAUNCHER_CATEGORY_TOP, chip_width, 20];
        category_x = category_x.saturating_add(chip_width + 6);
        (category, label, rect)
    })
}

fn launcher_result_row_rect(width: u32, height: u32, index: usize) -> Option<[u32; 4]> {
    let [panel_x, panel_y, panel_w, panel_h] = launcher_panel_rect(width, height);
    let row_offset = u32::try_from(index)
        .unwrap_or(u32::MAX)
        .saturating_mul(LAUNCHER_RESULT_STEP);
    let row_y = panel_y
        .saturating_add(LAUNCHER_RESULTS_TOP)
        .saturating_add(row_offset);
    let list_bottom = panel_y + panel_h.saturating_sub(LAUNCHER_RESULTS_BOTTOM_INSET);
    (row_y.saturating_add(LAUNCHER_RESULT_HEIGHT) <= list_bottom).then_some([
        panel_x + 20,
        row_y,
        panel_w.saturating_sub(40),
        LAUNCHER_RESULT_HEIGHT,
    ])
}

fn launcher_rect_contains(pointer_x: f64, pointer_y: f64, rect: [u32; 4]) -> bool {
    pointer_x.is_finite()
        && pointer_y.is_finite()
        && pointer_x >= rect[0] as f64
        && pointer_x < rect[0].saturating_add(rect[2]) as f64
        && pointer_y >= rect[1] as f64
        && pointer_y < rect[1].saturating_add(rect[3]) as f64
}

fn launcher_pointer_hit(
    width: u32,
    height: u32,
    pointer_x: f64,
    pointer_y: f64,
    result_count: usize,
) -> Option<LauncherHit> {
    for (category, _, rect) in launcher_category_regions(width, height) {
        if launcher_rect_contains(pointer_x, pointer_y, rect) {
            return Some(LauncherHit::Category(category));
        }
    }
    for index in 0..result_count {
        if launcher_result_row_rect(width, height, index)
            .is_some_and(|rect| launcher_rect_contains(pointer_x, pointer_y, rect))
        {
            return Some(LauncherHit::Result(index));
        }
    }
    None
}

fn launcher_category_for_query(query: &str) -> LauncherCategory {
    match query.trim_start().chars().next() {
        Some('@') => LauncherCategory::Windows,
        Some('>') => LauncherCategory::Commands,
        Some(':') => LauncherCategory::Power,
        _ => LauncherCategory::Apps,
    }
}

fn launcher_query_for_category(category: LauncherCategory, query: &str) -> String {
    let query = query.trim_start();
    let term = query
        .strip_prefix('@')
        .or_else(|| query.strip_prefix('>'))
        .or_else(|| query.strip_prefix(':'))
        .unwrap_or(query)
        .trim_start();
    match category {
        LauncherCategory::Apps => term.to_string(),
        LauncherCategory::Windows => format!("@{term}"),
        LauncherCategory::Commands => format!("> {term}"),
        LauncherCategory::Power => format!(":{term}"),
    }
}

impl LauncherItem {
    fn label(&self, apps: &[DesktopEntry], snapshot: &Snapshot) -> String {
        match self {
            Self::App(index) => apps[*index].name.clone(),
            Self::Window(id) => snapshot
                .windows
                .iter()
                .find(|window| window.id == *id)
                .map(|window| format!("{}  —  {}", window.title, window.app_id))
                .unwrap_or_else(|| "Closed window".into()),
            Self::Command(command) => format!("> {command}"),
            Self::Action(action) => (*action).into(),
        }
    }
}

struct App {
    registry_state: RegistryState,
    seat_state: SeatState,
    output_state: OutputState,
    shm: Shm,
    compositor: CompositorState,
    layer_shell: LayerShell,
    config: Config,
    snapshot: Snapshot,
    wallpapers: BTreeMap<String, image::RgbaImage>,
    video_frame: Option<image::RgbaImage>,
    video_sender: channel::SyncSender<image::RgbaImage>,
    video_recycler: mpsc::SyncSender<Vec<u8>>,
    video_recycled: Arc<Mutex<mpsc::Receiver<Vec<u8>>>>,
    video_generation: Arc<AtomicU64>,
    video_suspended: bool,
    font: Option<FontArc>,
    mode: Mode,
    surfaces: Vec<Surface>,
    keyboard: Option<wl_keyboard::WlKeyboard>,
    pointer: Option<wl_pointer::WlPointer>,
    apps: Vec<DesktopEntry>,
    apps_loaded: bool,
    launcher_query: String,
    launcher_selected: usize,
    recorder_selected: usize,
    recorder_capture_mode: RecorderCaptureMode,
    recorder_game_profile_selected: usize,
    recorder_attach_selected: usize,
    recorder_xwayland_selected: usize,
    recorder_view: RecorderView,
    recorder_settings_tab: RecorderSettingsTab,
    recorder_settings_row: usize,
    recorder_path_edit: Option<String>,
    recorder_feedback: Option<(String, bool)>,
    recorder_settings_events: Option<channel::Sender<Option<String>>>,
    recorder_selection_restored: bool,
    notifications: VecDeque<Notification>,
    notification_timers: BTreeMap<u32, RegistrationToken>,
    notification_events: Option<channel::Sender<NotificationEvent>>,
    notification_offset: usize,
    notification_signals: mpsc::Sender<NotificationSignal>,
    do_not_disturb: Arc<AtomicBool>,
    battery: Option<String>,
    on_battery: bool,
    next_battery_refresh: Instant,
    clock: String,
    audio: AudioState,
    network: NetworkState,
    media: MediaState,
    bluetooth: BluetoothState,
    control_panel: Option<ControlPanel>,
    pending_power_action: Option<PowerAction>,
    tray: Vec<TrayItem>,
    tray_menu: Option<TrayMenuState>,
    tray_menu_events: Option<channel::Sender<TrayMenuEvent>>,
    exit: bool,
}

fn main() {
    let mode = Mode::from_args();
    let config = Config::load().unwrap_or_else(|error| {
        eprintln!("wm-shell-sctk: config: {error}");
        Config::default()
    });
    if let Err(error) = run(mode, config) {
        eprintln!("wm-shell-sctk: {error}");
        std::process::exit(1);
    }
}

fn run(mode: Mode, config: Config) -> Result<(), String> {
    let connection = Connection::connect_to_env().map_err(|error| error.to_string())?;
    let (globals, event_queue) =
        registry_queue_init(&connection).map_err(|error| error.to_string())?;
    let qh = event_queue.handle();
    let mut event_loop: EventLoop<App> = EventLoop::try_new().map_err(|error| error.to_string())?;
    let handle = event_loop.handle();
    WaylandSource::new(connection.clone(), event_queue)
        .insert(handle.clone())
        .map_err(|error| error.to_string())?;

    let compositor = CompositorState::bind(&globals, &qh).map_err(|error| error.to_string())?;
    let layer_shell = LayerShell::bind(&globals, &qh).map_err(|error| error.to_string())?;
    let shm = Shm::bind(&globals, &qh).map_err(|error| error.to_string())?;
    // The launcher does not paint a wallpaper.  Decoding a large image here can
    // otherwise delay its first surface by seconds.
    let wallpapers = (mode == Mode::Wallpaper)
        .then(|| load_wallpapers(&config))
        .unwrap_or_default();
    let font_config = (mode != Mode::Wallpaper).then(|| config.clone());
    let (notification_signals, notification_signal_receiver) = mpsc::channel();
    let (video_sender, video_receiver) = channel::sync_channel::<image::RgbaImage>(1);
    let (video_recycler, video_recycled) = mpsc::sync_channel::<Vec<u8>>(2);
    let video_recycled = Arc::new(Mutex::new(video_recycled));
    let (font_sender, font_receiver) = channel::channel::<Option<FontArc>>();
    let video_generation = Arc::new(AtomicU64::new(0));
    let do_not_disturb = Arc::new(AtomicBool::new(config.shell.do_not_disturb));
    let (battery, on_battery) = battery_state();
    let mut app = App {
        registry_state: RegistryState::new(&globals),
        seat_state: SeatState::new(&globals, &qh),
        output_state: OutputState::new(&globals, &qh),
        shm,
        compositor,
        layer_shell,
        config,
        snapshot: Snapshot::default(),
        wallpapers,
        video_frame: None,
        video_sender: video_sender.clone(),
        video_recycler,
        video_recycled: video_recycled.clone(),
        video_generation: video_generation.clone(),
        video_suspended: false,
        font: None,
        mode,
        surfaces: Vec::new(),
        keyboard: None,
        pointer: None,
        apps: Vec::new(),
        apps_loaded: false,
        launcher_query: String::new(),
        launcher_selected: 0,
        recorder_selected: 0,
        recorder_capture_mode: RecorderCaptureMode::Screen,
        recorder_game_profile_selected: 0,
        recorder_attach_selected: 0,
        recorder_xwayland_selected: 0,
        recorder_view: RecorderView::Controls,
        recorder_settings_tab: RecorderSettingsTab::Output,
        recorder_settings_row: 0,
        recorder_path_edit: None,
        recorder_feedback: None,
        recorder_settings_events: None,
        recorder_selection_restored: false,
        notifications: VecDeque::new(),
        notification_timers: BTreeMap::new(),
        notification_events: None,
        notification_offset: 0,
        notification_signals,
        do_not_disturb: do_not_disturb.clone(),
        battery,
        on_battery,
        next_battery_refresh: Instant::now() + Duration::from_secs(30),
        clock: clock_label(),
        audio: AudioState::default(),
        network: NetworkState {
            label: "NET —".into(),
            networking_enabled: false,
            wireless_enabled: false,
        },
        media: MediaState {
            label: "MEDIA —".into(),
        },
        bluetooth: BluetoothState {
            label: "BT —".into(),
            powered: None,
        },
        control_panel: None,
        pending_power_action: None,
        tray: Vec::new(),
        tray_menu: None,
        tray_menu_events: None,
        exit: false,
    };

    if mode == Mode::Launcher {
        let (desktop_entries_sender, desktop_entries_receiver) =
            channel::channel::<Vec<DesktopEntry>>();
        handle
            .insert_source(desktop_entries_receiver, |event, _, app| {
                if let channel::Event::Msg(entries) = event {
                    app.apps = entries;
                    app.apps_loaded = true;
                    app.launcher_selected = app
                        .launcher_selected
                        .min(app.launcher_items().len().saturating_sub(1));
                    app.redraw_all(&qh);
                }
            })
            .map_err(|error| error.to_string())?;
        thread::spawn(move || {
            let _ = desktop_entries_sender.send(load_desktop_entries());
        });
    }
    handle
        .insert_source(font_receiver, |event, _, app| {
            if let channel::Event::Msg(font) = event {
                app.font = font;
                app.redraw_all(&qh);
            }
        })
        .map_err(|error| error.to_string())?;
    if let Some(config) = font_config {
        thread::spawn(move || {
            let _ = font_sender.send(load_font(&config));
        });
    }

    // The compositor publishes complete snapshots after relevant state changes.
    // Module services use event channels; the notification-expiry housekeeping
    // timer refreshes the inexpensive sysfs battery cache every 30 seconds.
    if mode != Mode::Launcher {
        let (sender, receiver) = channel::channel::<Snapshot>();
        handle
            .insert_source(receiver, |event, _, app| match event {
                channel::Event::Msg(snapshot) => {
                    if app.snapshot != snapshot {
                        let config_error_changed = app.snapshot.error != snapshot.error;
                        let visible_state_changed = match app.mode {
                            Mode::Bar => bar_snapshot_changed(&app.snapshot, &snapshot),
                            Mode::Wallpaper => false,
                            Mode::Launcher => false,
                            Mode::Recorder => {
                                app.snapshot.recorder != snapshot.recorder
                                    || app.snapshot.recorder_settings != snapshot.recorder_settings
                                    || app.snapshot.outputs != snapshot.outputs
                                    || app.snapshot.windows != snapshot.windows
                            }
                        };
                        app.snapshot = snapshot;
                        app.recorder_selected =
                            app.recorder_selected.min(app.snapshot.windows.len());
                        if app.mode == Mode::Recorder {
                            app.recorder_settings_row = app
                                .recorder_settings_row
                                .min(app.recorder_settings_row_count().saturating_sub(1));
                            // The panel is a fresh process on every open, so
                            // restore the remembered method/source from the
                            // persisted settings on the first snapshot.
                            if !app.recorder_selection_restored {
                                app.restore_recorder_selection();
                                app.recorder_selection_restored = true;
                            }
                        }
                        if config_error_changed {
                            if let Some(error) = app.snapshot.error.as_deref() {
                                if let Some(sender) = app.notification_events.as_ref() {
                                    let _ = sender.send(NotificationEvent::Upsert(
                                        config_error_notification(error),
                                    ));
                                }
                            } else if let Some(sender) = app.notification_events.as_ref() {
                                let _ = sender.send(NotificationEvent::Close {
                                    id: CONFIG_ERROR_NOTIFICATION_ID,
                                    reason: 3,
                                });
                            }
                        }
                        app.update_video_wallpaper_playback();
                        if visible_state_changed {
                            app.redraw_all(&qh);
                        }
                    }
                }
                channel::Event::Closed => {}
            })
            .map_err(|error| error.to_string())?;
        spawn_subscription(sender);
    }

    if mode == Mode::Recorder {
        // Settings commands run on a worker so a slow compositor never blocks
        // the UI thread; the response arrives here as error feedback.
        let (settings_sender, settings_receiver) = channel::channel::<Option<String>>();
        app.recorder_settings_events = Some(settings_sender);
        handle
            .insert_source(settings_receiver, |event, _, app| {
                if let channel::Event::Msg(outcome) = event {
                    match outcome {
                        None => {
                            app.recorder_feedback =
                                Some(("Saved".into(), false));
                        }
                        Some(error) => {
                            app.recorder_feedback = Some((error, true));
                        }
                    }
                    app.redraw_all(&qh);
                }
            })
            .map_err(|error| error.to_string())?;
    }

    if mode == Mode::Bar {
        let (tray_menu_sender, tray_menu_receiver) = channel::channel::<TrayMenuEvent>();
        app.tray_menu_events = Some(tray_menu_sender);
        handle
            .insert_source(tray_menu_receiver, |event, _, app| {
                let channel::Event::Msg(TrayMenuEvent::Loaded {
                    service,
                    menu_path,
                    parent,
                    rows,
                }) = event
                else {
                    return;
                };
                let Some(current) = app.tray_menu.as_mut() else {
                    return;
                };
                if current.service != service
                    || current.menu_path != menu_path
                    || current.parents.last().copied() != Some(parent)
                {
                    return;
                }
                if let Some(rows) = rows {
                    current.rows = rows;
                    app.show_tray_menu_surface(&qh);
                } else {
                    let fallback = (
                        current.service.clone(),
                        current.path.clone(),
                        current.action_position,
                    );
                    app.close_tray_menu(&qh);
                    App::activate_tray_item(
                        fallback.0,
                        fallback.1,
                        "ContextMenu",
                        fallback.2.0,
                        fallback.2.1,
                    );
                }
            })
            .map_err(|error| error.to_string())?;

        let (sender, receiver) = channel::channel::<NotificationEvent>();
        app.notification_events = Some(sender.clone());
        let notification_timer_handle = handle.clone();
        let notification_timer_qh = qh.clone();
        handle
            .insert_source(receiver, move |event, _, app| {
                if let channel::Event::Msg(event) = event {
                    match event {
                        NotificationEvent::Upsert(notification) => {
                            let id = notification.id;
                            let expiry = notification.expires_at;
                            if let Some(timer) = app.notification_timers.remove(&id) {
                                notification_timer_handle.remove(timer);
                            }
                            if let Some(existing) = app
                                .notifications
                                .iter_mut()
                                .find(|existing| existing.id == notification.id)
                            {
                                *existing = notification;
                            } else {
                                app.notifications.push_back(notification);
                            }
                            while app.notifications.len() > 100 {
                                if let Some(discarded) = app.notifications.pop_front()
                                    && let Some(timer) =
                                        app.notification_timers.remove(&discarded.id)
                                {
                                    notification_timer_handle.remove(timer);
                                }
                            }
                            app.notification_offset = 0;
                            if let Some(expiry) = expiry {
                                let qh = notification_timer_qh.clone();
                                let timer = notification_timer_handle.insert_source(
                                    Timer::from_deadline(expiry),
                                    move |_, _, app| {
                                        app.notification_timers.remove(&id);
                                        if expire_notifications(app, Instant::now()) {
                                            app.clamp_notification_offset();
                                            app.sync_notification_surface(&qh);
                                            app.redraw_all(&qh);
                                        }
                                        TimeoutAction::Drop
                                    },
                                );
                                match timer {
                                    Ok(timer) => {
                                        app.notification_timers.insert(id, timer);
                                    }
                                    Err(error) => {
                                        eprintln!(
                                            "wm-shell-sctk: schedule notification expiry: {error}"
                                        );
                                    }
                                }
                            }
                        }
                        NotificationEvent::Close { id, reason } => {
                            if let Some(timer) = app.notification_timers.remove(&id) {
                                notification_timer_handle.remove(timer);
                            }
                            app.notifications
                                .retain(|notification| notification.id != id);
                            app.clamp_notification_offset();
                            let _ = app
                                .notification_signals
                                .send(NotificationSignal::Closed { id, reason });
                        }
                        NotificationEvent::Dismiss { id } => {
                            if let Some(timer) = app.notification_timers.remove(&id) {
                                notification_timer_handle.remove(timer);
                            }
                            app.notifications
                                .retain(|notification| notification.id != id);
                            app.clamp_notification_offset();
                        }
                    }
                    app.sync_notification_surface(&notification_timer_qh);
                    app.redraw_all(&notification_timer_qh);
                }
            })
            .map_err(|error| error.to_string())?;
        spawn_notification_server(sender, notification_signal_receiver);

        let (sender, receiver) = channel::channel::<TrayEvent>();
        handle
            .insert_source(receiver, |event, _, app| {
                if let channel::Event::Msg(event) = event {
                    let changed = match event {
                        TrayEvent::Upsert(item) => {
                            if let Some(existing) =
                                app.tray.iter_mut().find(|known| known.id == item.id)
                            {
                                if *existing == item {
                                    false
                                } else {
                                    *existing = item;
                                    true
                                }
                            } else {
                                app.tray.push(item);
                                true
                            }
                        }
                        TrayEvent::Remove(id) => {
                            let before = app.tray.len();
                            app.tray.retain(|item| item.id != id);
                            app.tray.len() != before
                        }
                    };
                    if changed {
                        app.redraw_all(&qh);
                    }
                }
            })
            .map_err(|error| error.to_string())?;
        spawn_tray_server(sender);

        let (sender, receiver) = channel::channel::<AudioState>();
        handle
            .insert_source(receiver, |event, _, app| {
                if let channel::Event::Msg(audio) = event
                    && app.audio != audio
                {
                    app.audio = audio;
                    app.redraw_all(&qh);
                }
            })
            .map_err(|error| error.to_string())?;
        spawn_audio_monitor(sender);

        let (sender, receiver) = channel::channel::<NetworkState>();
        handle
            .insert_source(receiver, |event, _, app| {
                if let channel::Event::Msg(network) = event
                    && app.network != network
                {
                    app.network = network;
                    app.redraw_all(&qh);
                }
            })
            .map_err(|error| error.to_string())?;
        spawn_network_monitor(sender);

        let (sender, receiver) = channel::channel::<MediaState>();
        handle
            .insert_source(receiver, |event, _, app| {
                if let channel::Event::Msg(media) = event
                    && app.media != media
                {
                    app.media = media;
                    app.redraw_all(&qh);
                }
            })
            .map_err(|error| error.to_string())?;
        spawn_media_monitor(sender);

        let (sender, receiver) = channel::channel::<BluetoothState>();
        handle
            .insert_source(receiver, |event, _, app| {
                if let channel::Event::Msg(bluetooth) = event
                    && app.bluetooth != bluetooth
                {
                    app.bluetooth = bluetooth;
                    app.redraw_all(&qh);
                }
            })
            .map_err(|error| error.to_string())?;
        spawn_bluetooth_monitor(sender);

        handle
            .insert_source(Timer::from_duration(Duration::from_secs(1)), |_, _, app| {
                let now = Instant::now();
                let clock = clock_label();
                let clock_changed = app.clock != clock;
                if clock_changed {
                    app.clock = clock;
                }
                let battery_changed = if now >= app.next_battery_refresh {
                    app.next_battery_refresh = now + Duration::from_secs(30);
                    let (battery, on_battery) = battery_state();
                    let changed = app.battery != battery || app.on_battery != on_battery;
                    app.battery = battery;
                    app.on_battery = on_battery;
                    changed
                } else {
                    false
                };
                let notifications_changed = expire_notifications(app, now);
                if clock_changed || battery_changed || notifications_changed {
                    if notifications_changed {
                        app.clamp_notification_offset();
                        app.sync_notification_surface(&qh);
                    }
                    app.redraw_all(&qh);
                }
                TimeoutAction::ToDuration(next_bar_maintenance_delay(app, Instant::now()))
            })
            .map_err(|error| error.to_string())?;
    }

    if mode == Mode::Wallpaper {
        handle
            .insert_source(
                Timer::from_duration(Duration::from_secs(30)),
                |_, _, app| {
                    let (_, on_battery) = battery_state();
                    if app.on_battery != on_battery {
                        app.on_battery = on_battery;
                        app.update_video_wallpaper_playback();
                    }
                    TimeoutAction::ToDuration(Duration::from_secs(30))
                },
            )
            .map_err(|error| error.to_string())?;
        handle
            .insert_source(video_receiver, |event, _, app| {
                if let channel::Event::Msg(frame) = event {
                    if let Some(previous) = app.video_frame.replace(frame) {
                        let _ = app.video_recycler.try_send(previous.into_raw());
                    }
                    app.redraw_all(&qh);
                }
            })
            .map_err(|error| error.to_string())?;
        if app.config.wallpaper.kind == "video" {
            spawn_video_wallpaper(
                app.config.wallpaper.path.clone(),
                app.config.wallpaper.fps,
                video_sender,
                video_recycled,
                video_generation,
                0,
            );
        }
    }

    let (sender, receiver) = channel::channel::<Config>();
    handle
        .insert_source(receiver, |event, _, app| match event {
            channel::Event::Msg(config) => app.apply_config(config, &qh),
            channel::Event::Closed => {}
        })
        .map_err(|error| error.to_string())?;
    spawn_config_watcher(sender);

    if matches!(mode, Mode::Launcher | Mode::Recorder) {
        app.add_surface(&qh, None);
    }

    while !app.exit {
        event_loop
            .dispatch(None, &mut app)
            .map_err(|error| error.to_string())?;
    }
    Ok(())
}

fn notification_body_text(body: &str) -> String {
    let mut text = String::with_capacity(body.len().min(2048));
    let mut in_tag = false;
    let mut count = 0usize;
    for character in body.chars() {
        match character {
            '<' => in_tag = true,
            '>' if in_tag => in_tag = false,
            _ if !in_tag => {
                text.push(character);
                count += 1;
            }
            _ => {}
        }
        if count >= 2048 {
            break;
        }
    }
    text.replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .chars()
        .take(2048)
        .collect()
}

fn notification_summary_text(summary: &str) -> String {
    notification_body_text(summary).chars().take(256).collect()
}

fn notification_text_x(notification: &Notification) -> u32 {
    if notification.icon.is_some() { 60 } else { 18 }
}

fn load_notification_icon(value: &str) -> Option<image::RgbaImage> {
    let path = Path::new(value);
    if path.is_absolute() {
        let (width, height) = image::ImageReader::open(path)
            .ok()?
            .into_dimensions()
            .ok()?;
        if wallpaper_byte_len(width, height).is_none_or(|bytes| bytes > 4 * 1024 * 1024) {
            return None;
        }
        return image::ImageReader::open(path)
            .ok()?
            .decode()
            .ok()
            .map(image::DynamicImage::into_rgba8);
    }
    load_named_tray_icon(value, None)
}

fn spawn_notification_server(
    sender: channel::Sender<NotificationEvent>,
    signals: mpsc::Receiver<NotificationSignal>,
) {
    thread::spawn(move || {
        let Ok(connection) = zbus::blocking::Connection::session() else {
            return;
        };
        if connection
            .request_name("org.freedesktop.Notifications")
            .is_err()
        {
            return;
        }
        let server = NotificationServer {
            next_id: std::sync::atomic::AtomicU32::new(0),
            sender,
        };
        if connection
            .object_server()
            .at("/org/freedesktop/Notifications", server)
            .is_err()
        {
            return;
        }
        let Ok(interface) = connection
            .object_server()
            .interface::<_, NotificationServer>("/org/freedesktop/Notifications")
        else {
            return;
        };
        while let Ok(signal) = signals.recv() {
            match signal {
                NotificationSignal::Action { id, key } => {
                    let _ = zbus::block_on(NotificationServer::action_invoked(
                        interface.signal_emitter(),
                        id,
                        &key,
                    ));
                }
                NotificationSignal::Closed { id, reason } => {
                    let _ = zbus::block_on(NotificationServer::notification_closed(
                        interface.signal_emitter(),
                        id,
                        reason,
                    ));
                }
            }
        }
    });
}

fn spawn_tray_server(sender: channel::Sender<TrayEvent>) {
    thread::spawn(move || {
        let Ok(connection) = zbus::blocking::Connection::session() else {
            return;
        };
        if connection
            .request_name("org.kde.StatusNotifierWatcher")
            .is_err()
        {
            return;
        }
        if connection
            .object_server()
            .at(
                "/StatusNotifierWatcher",
                TrayServer {
                    sender,
                    items: Arc::new(Mutex::new(Vec::new())),
                    // The native bar is the StatusNotifier host. Advertise it
                    // immediately so applications do not wait for a separate
                    // GTK host to appear before registering their item.
                    host_registered: Arc::new(Mutex::new(true)),
                },
            )
            .is_err()
        {
            return;
        }
        std::thread::park();
    });
}

fn read_audio_state() -> AudioState {
    let volume = std::process::Command::new("pactl")
        .args(["get-sink-volume", "@DEFAULT_SINK@"])
        .output()
        .ok()
        .filter(|output| output.status.success())
        .and_then(|output| String::from_utf8(output.stdout).ok())
        .and_then(|output| {
            output
                .split_whitespace()
                .find_map(|word| word.strip_suffix('%'))
                .and_then(|value| value.parse::<u16>().ok())
        });
    let muted = std::process::Command::new("pactl")
        .args(["get-sink-mute", "@DEFAULT_SINK@"])
        .output()
        .ok()
        .filter(|output| output.status.success())
        .and_then(|output| String::from_utf8(output.stdout).ok())
        .is_some_and(|output| output.to_ascii_lowercase().contains("yes"));
    AudioState {
        label: volume.map_or_else(
            || "AUDIO —".into(),
            |volume| {
                if muted {
                    "MUTED".into()
                } else {
                    format!("VOL {volume}%")
                }
            },
        ),
    }
}

fn spawn_audio_monitor(sender: channel::Sender<AudioState>) {
    thread::spawn(move || {
        let _ = sender.send(read_audio_state());
        loop {
            let Ok(mut child) = std::process::Command::new("pactl")
                .arg("subscribe")
                .stdout(std::process::Stdio::piped())
                .spawn()
            else {
                return;
            };
            let Some(stdout) = child.stdout.take() else {
                return;
            };
            for line in std::io::BufReader::new(stdout).lines() {
                if line.is_err() || sender.send(read_audio_state()).is_err() {
                    return;
                }
            }
            let _ = child.wait();
            thread::sleep(Duration::from_secs(2));
        }
    });
}

fn compact_network_bar_label(label: &str) -> &'static str {
    if label == "NET OFF" {
        "NET OFF"
    } else if label.starts_with("WIFI") {
        "WIFI"
    } else {
        "NET"
    }
}

fn read_network_state(proxy: &zbus::blocking::Proxy<'_>) -> NetworkState {
    let networking = proxy
        .get_property::<bool>("NetworkingEnabled")
        .unwrap_or(false);
    let wireless = proxy
        .get_property::<bool>("WirelessEnabled")
        .unwrap_or(false);
    if !networking {
        return NetworkState {
            label: "NET OFF".into(),
            networking_enabled: false,
            wireless_enabled: false,
        };
    }
    let active = proxy
        .get_property::<Vec<zbus::zvariant::OwnedObjectPath>>("ActiveConnections")
        .unwrap_or_default();
    for path in active {
        let Ok(connection) = zbus::blocking::Proxy::new(
            proxy.connection(),
            "org.freedesktop.NetworkManager",
            path.as_str(),
            "org.freedesktop.NetworkManager.Connection.Active",
        ) else {
            continue;
        };
        let kind = connection
            .get_property::<String>("Type")
            .unwrap_or_default();
        let id = connection.get_property::<String>("Id").unwrap_or_default();
        let id = bar_label_text(&id, 64);
        if !id.is_empty() {
            return NetworkState {
                label: format!(
                    "{} {id}",
                    if kind == "802-11-wireless" {
                        "WIFI"
                    } else {
                        "NET"
                    }
                ),
                networking_enabled: true,
                wireless_enabled: wireless,
            };
        }
    }
    NetworkState {
        label: if wireless { "WIFI" } else { "NET" }.into(),
        networking_enabled: true,
        wireless_enabled: wireless,
    }
}

fn spawn_network_monitor(sender: channel::Sender<NetworkState>) {
    thread::spawn(move || {
        loop {
            let connected = || -> Result<bool, zbus::Error> {
                let connection = zbus::blocking::Connection::system()?;
                let proxy = zbus::blocking::Proxy::new(
                    &connection,
                    "org.freedesktop.NetworkManager",
                    "/org/freedesktop/NetworkManager",
                    "org.freedesktop.NetworkManager",
                )?;
                let _ = sender.send(read_network_state(&proxy));
                let mut signals = proxy.receive_all_signals()?;
                while signals.next().is_some() {
                    if sender.send(read_network_state(&proxy)).is_err() {
                        return Ok(false);
                    }
                }
                Ok(true)
            };
            match connected() {
                Ok(false) => return,
                Ok(true) | Err(_) => {}
            }
            if sender
                .send(NetworkState {
                    label: "NET —".into(),
                    networking_enabled: false,
                    wireless_enabled: false,
                })
                .is_err()
            {
                return;
            }
            thread::sleep(Duration::from_secs(5));
        }
    });
}

fn media_label(name: &str, player: &zbus::blocking::Proxy<'_>) -> String {
    let fallback = name
        .trim_start_matches("org.mpris.MediaPlayer2.")
        .to_string();
    let title = player
        .get_property::<std::collections::HashMap<String, zbus::zvariant::OwnedValue>>("Metadata")
        .ok()
        .and_then(|mut metadata| metadata.remove("xesam:title"))
        .and_then(|value| String::try_from(value).ok())
        .filter(|title| !title.trim().is_empty())
        .unwrap_or(fallback);
    format!("MEDIA {}", bar_label_text(&title, 96))
}

fn bar_label_text(value: &str, maximum: usize) -> String {
    if maximum == 0 {
        return String::new();
    }
    let mut label = String::with_capacity(value.len().min(maximum));
    let mut pending_space = false;
    let mut count = 0usize;
    for character in value.chars() {
        if character.is_control() || character.is_whitespace() {
            pending_space = !label.is_empty();
            continue;
        }
        if pending_space {
            label.push(' ');
            pending_space = false;
        }
        label.push(character);
        count += 1;
        if count >= maximum {
            break;
        }
    }
    label
}

fn read_media_state(connection: &zbus::blocking::Connection) -> MediaState {
    let Ok(bus) = zbus::blocking::Proxy::new(
        connection,
        "org.freedesktop.DBus",
        "/org/freedesktop/DBus",
        "org.freedesktop.DBus",
    ) else {
        return MediaState {
            label: "MEDIA —".into(),
        };
    };
    let Ok(names) = bus.call::<_, _, Vec<String>>("ListNames", &()) else {
        return MediaState {
            label: "MEDIA —".into(),
        };
    };
    let mut fallback = None;
    for name in names
        .into_iter()
        .filter(|name| name.starts_with("org.mpris.MediaPlayer2."))
    {
        let Ok(player) = zbus::blocking::Proxy::new(
            connection,
            name.as_str(),
            "/org/mpris/MediaPlayer2",
            "org.mpris.MediaPlayer2.Player",
        ) else {
            continue;
        };
        let playing = player
            .get_property::<String>("PlaybackStatus")
            .unwrap_or_default();
        if playing == "Playing" {
            return MediaState {
                label: media_label(&name, &player),
            };
        }
        fallback.get_or_insert_with(|| media_label(&name, &player));
    }
    MediaState {
        label: fallback.unwrap_or_else(|| "MEDIA —".into()),
    }
}

fn spawn_media_monitor(sender: channel::Sender<MediaState>) {
    thread::spawn(move || {
        loop {
            let connected = || -> Result<bool, zbus::Error> {
                let connection = zbus::blocking::Connection::session()?;
                let watched = Arc::new(Mutex::new(BTreeSet::new()));
                spawn_media_property_monitors(sender.clone(), watched.clone());
                if sender.send(read_media_state(&connection)).is_err() {
                    return Ok(false);
                }
                let bus = zbus::blocking::Proxy::new(
                    &connection,
                    "org.freedesktop.DBus",
                    "/org/freedesktop/DBus",
                    "org.freedesktop.DBus",
                )?;
                let mut signals = bus.receive_signal("NameOwnerChanged")?;
                while signals.next().is_some() {
                    spawn_media_property_monitors(sender.clone(), watched.clone());
                    if sender.send(read_media_state(&connection)).is_err() {
                        return Ok(false);
                    }
                }
                Ok(true)
            };
            match connected() {
                Ok(false) => return,
                Ok(true) | Err(_) => {
                    if sender
                        .send(MediaState {
                            label: "MEDIA —".into(),
                        })
                        .is_err()
                    {
                        return;
                    }
                    thread::sleep(Duration::from_secs(2));
                }
            }
        }
    });
}

fn spawn_media_property_monitors(
    sender: channel::Sender<MediaState>,
    watched: Arc<Mutex<BTreeSet<String>>>,
) {
    let Ok(connection) = zbus::blocking::Connection::session() else {
        return;
    };
    let Ok(bus) = zbus::blocking::Proxy::new(
        &connection,
        "org.freedesktop.DBus",
        "/org/freedesktop/DBus",
        "org.freedesktop.DBus",
    ) else {
        return;
    };
    let Ok(names) = bus.call::<_, _, Vec<String>>("ListNames", &()) else {
        return;
    };
    for name in names
        .into_iter()
        .filter(|name| name.starts_with("org.mpris.MediaPlayer2."))
    {
        if !watched
            .lock()
            .expect("media watcher set poisoned")
            .insert(name.clone())
        {
            continue;
        }
        let sender = sender.clone();
        let watched = watched.clone();
        thread::spawn(move || {
            let Ok(connection) = zbus::blocking::Connection::session() else {
                watched
                    .lock()
                    .expect("media watcher set poisoned")
                    .remove(&name);
                return;
            };
            let Ok(properties) = zbus::blocking::Proxy::new(
                &connection,
                name.as_str(),
                "/org/mpris/MediaPlayer2",
                "org.freedesktop.DBus.Properties",
            ) else {
                watched
                    .lock()
                    .expect("media watcher set poisoned")
                    .remove(&name);
                return;
            };
            let Ok(mut signals) = properties.receive_signal("PropertiesChanged") else {
                watched
                    .lock()
                    .expect("media watcher set poisoned")
                    .remove(&name);
                return;
            };
            while signals.next().is_some() {
                if sender.send(read_media_state(&connection)).is_err() {
                    return;
                }
            }
            watched
                .lock()
                .expect("media watcher set poisoned")
                .remove(&name);
        });
    }
}

fn read_bluetooth_state(bus: &zbus::blocking::Proxy<'_>) -> BluetoothState {
    let active = bus
        .call::<_, _, bool>("NameHasOwner", &("org.bluez",))
        .unwrap_or(false);
    if !active {
        return BluetoothState {
            label: "BT —".into(),
            powered: None,
        };
    }
    let objects = zbus::blocking::Proxy::new(
        bus.connection(),
        "org.bluez",
        "/",
        "org.freedesktop.DBus.ObjectManager",
    )
    .ok()
    .and_then(|objects| {
        objects
            .call::<_, _, BluezManagedObjects>("GetManagedObjects", &())
            .ok()
    });
    let powered = objects.as_ref().and_then(|objects| {
        objects.values().find_map(|interfaces| {
            interfaces
                .get("org.bluez.Adapter1")
                .and_then(|properties| properties.get("Powered"))
                .and_then(|value| bool::try_from(value).ok())
        })
    });
    let connected = objects
        .as_ref()
        .map(|objects| {
            objects
                .values()
                .filter(|interfaces| {
                    interfaces
                        .get("org.bluez.Device1")
                        .and_then(|properties| properties.get("Connected"))
                        .and_then(|value| bool::try_from(value).ok())
                        .unwrap_or(false)
                })
                .count()
        })
        .unwrap_or(0);
    BluetoothState {
        label: bluetooth_label(powered, connected),
        powered,
    }
}

fn bluetooth_label(powered: Option<bool>, connected: usize) -> String {
    if powered == Some(false) {
        "BT OFF".into()
    } else if connected == 0 {
        "BT".into()
    } else {
        format!("BT {connected}")
    }
}

fn spawn_bluetooth_monitor(sender: channel::Sender<BluetoothState>) {
    let watched = Arc::new(Mutex::new(BTreeSet::new()));
    spawn_bluetooth_property_watchers(sender.clone(), watched.clone());
    spawn_bluetooth_topology_monitor(sender.clone(), watched.clone());
    thread::spawn(move || {
        let Ok(connection) = zbus::blocking::Connection::system() else {
            return;
        };
        let Ok(bus) = zbus::blocking::Proxy::new(
            &connection,
            "org.freedesktop.DBus",
            "/org/freedesktop/DBus",
            "org.freedesktop.DBus",
        ) else {
            return;
        };
        let _ = sender.send(read_bluetooth_state(&bus));
        let Ok(mut signals) = bus.receive_signal("NameOwnerChanged") else {
            return;
        };
        while let Some(signal) = signals.next() {
            let changed = signal
                .body()
                .deserialize::<(String, String, String)>()
                .ok()
                .is_some_and(|(name, _, _)| name == "org.bluez");
            if changed {
                if sender.send(read_bluetooth_state(&bus)).is_err() {
                    return;
                }
                watched
                    .lock()
                    .expect("Bluetooth watcher set poisoned")
                    .clear();
                spawn_bluetooth_property_watchers(sender.clone(), watched.clone());
                spawn_bluetooth_topology_monitor(sender.clone(), watched.clone());
            }
        }
    });
}

fn spawn_bluetooth_topology_monitor(
    sender: channel::Sender<BluetoothState>,
    watched: Arc<Mutex<BTreeSet<String>>>,
) {
    thread::spawn(move || {
        let Ok(connection) = zbus::blocking::Connection::system() else {
            return;
        };
        let Ok(manager) = zbus::blocking::Proxy::new(
            &connection,
            "org.bluez",
            "/",
            "org.freedesktop.DBus.ObjectManager",
        ) else {
            return;
        };
        let Ok(mut signals) = manager.receive_signal("InterfacesAdded") else {
            return;
        };
        while signals.next().is_some() {
            spawn_bluetooth_property_watchers(sender.clone(), watched.clone());
        }
    });
}

fn spawn_bluetooth_property_watchers(
    sender: channel::Sender<BluetoothState>,
    watched: Arc<Mutex<BTreeSet<String>>>,
) {
    thread::spawn(move || {
        let Ok(connection) = zbus::blocking::Connection::system() else {
            return;
        };
        let Ok(manager) = zbus::blocking::Proxy::new(
            &connection,
            "org.bluez",
            "/",
            "org.freedesktop.DBus.ObjectManager",
        ) else {
            return;
        };
        let Ok(objects) = manager.call::<_, _, BluezManagedObjects>("GetManagedObjects", &())
        else {
            return;
        };
        for path in objects.keys().take(64) {
            let path = path.to_string();
            if !watched
                .lock()
                .expect("Bluetooth watcher set poisoned")
                .insert(path.clone())
            {
                continue;
            }
            let sender = sender.clone();
            let watched = watched.clone();
            thread::spawn(move || {
                let Ok(connection) = zbus::blocking::Connection::system() else {
                    watched
                        .lock()
                        .expect("Bluetooth watcher set poisoned")
                        .remove(&path);
                    return;
                };
                let Ok(bus) = zbus::blocking::Proxy::new(
                    &connection,
                    "org.freedesktop.DBus",
                    "/org/freedesktop/DBus",
                    "org.freedesktop.DBus",
                ) else {
                    watched
                        .lock()
                        .expect("Bluetooth watcher set poisoned")
                        .remove(&path);
                    return;
                };
                let Ok(properties) = zbus::blocking::Proxy::new(
                    &connection,
                    "org.bluez",
                    path.as_str(),
                    "org.freedesktop.DBus.Properties",
                ) else {
                    watched
                        .lock()
                        .expect("Bluetooth watcher set poisoned")
                        .remove(&path);
                    return;
                };
                let Ok(mut signals) = properties.receive_signal("PropertiesChanged") else {
                    watched
                        .lock()
                        .expect("Bluetooth watcher set poisoned")
                        .remove(&path);
                    return;
                };
                while signals.next().is_some() {
                    if sender.send(read_bluetooth_state(&bus)).is_err() {
                        return;
                    }
                }
                watched
                    .lock()
                    .expect("Bluetooth watcher set poisoned")
                    .remove(&path);
            });
        }
    });
}

fn spawn_subscription(sender: channel::Sender<Snapshot>) {
    thread::spawn(move || {
        let result = || -> Result<(), Box<dyn std::error::Error>> {
            let mut stream = std::os::unix::net::UnixStream::connect(wm_core::socket_path()?)?;
            stream.write_all(b"{\"version\":1,\"command\":\"subscribe\"}\n")?;
            for line in std::io::BufReader::new(stream).lines() {
                let response: wm_core::Response = serde_json::from_str(&line?)?;
                if sender.send(response.state).is_err() {
                    break;
                }
            }
            Ok(())
        };
        if let Err(error) = result() {
            eprintln!("wm-shell-sctk: subscription: {error}");
        }
    });
}

fn spawn_config_watcher(sender: channel::Sender<Config>) {
    let path = wm_core::config_path();
    let Some(parent) = path.parent().map(std::path::Path::to_path_buf) else {
        return;
    };
    thread::spawn(move || {
        use notify::Watcher;
        let (events, receive) = mpsc::channel();
        let target = path.clone();
        let Ok(mut watcher) =
            notify::recommended_watcher(move |event: Result<notify::Event, notify::Error>| {
                if let Ok(event) = event {
                    if !matches!(event.kind, notify::EventKind::Access(_))
                        && event.paths.contains(&target)
                    {
                        let _ = events.send(());
                    }
                }
            })
        else {
            return;
        };
        if watcher
            .watch(&parent, notify::RecursiveMode::NonRecursive)
            .is_err()
        {
            return;
        }
        while receive.recv().is_ok() {
            // Editors commonly emit a short burst of write/rename notifications.
            thread::sleep(Duration::from_millis(75));
            while receive.try_recv().is_ok() {}
            if let Ok(config) = Config::load() {
                let _ = sender.send(config);
            }
        }
    });
}

fn load_wallpaper_path(kind: &str, path: &str) -> Option<image::RgbaImage> {
    if kind != "static" || path.is_empty() {
        return None;
    }
    let (width, height) = image::ImageReader::open(path)
        .ok()?
        .into_dimensions()
        .ok()?;
    if wallpaper_byte_len(width, height).is_none_or(|bytes| bytes > 64 * 1024 * 1024) {
        return None;
    }
    image::ImageReader::open(path)
        .ok()?
        .decode()
        .ok()
        .map(image::DynamicImage::into_rgba8)
}

fn wallpaper_byte_len(width: u32, height: u32) -> Option<usize> {
    (width as usize)
        .checked_mul(height as usize)
        .and_then(|pixels| pixels.checked_mul(4))
}

fn load_wallpapers(config: &Config) -> BTreeMap<String, image::RgbaImage> {
    let mut wallpapers = BTreeMap::new();
    if let Some(wallpaper) = load_wallpaper_path(&config.wallpaper.kind, &config.wallpaper.path) {
        wallpapers.insert(String::new(), wallpaper);
    }
    for (output, path) in &config.wallpaper.outputs {
        if let Some(wallpaper) = load_wallpaper_path(&config.wallpaper.kind, path) {
            wallpapers.insert(output.clone(), wallpaper);
        }
    }
    wallpapers
}

fn spawn_video_wallpaper(
    path: String,
    fps: u32,
    sender: channel::SyncSender<image::RgbaImage>,
    recycled: Arc<Mutex<mpsc::Receiver<Vec<u8>>>>,
    generation: Arc<AtomicU64>,
    expected_generation: u64,
) {
    if path.is_empty() {
        return;
    }
    thread::spawn(move || {
        let probe = std::process::Command::new("ffprobe")
            .args([
                "-v",
                "error",
                "-select_streams",
                "v:0",
                "-show_entries",
                "stream=width,height",
                "-of",
                "csv=p=0:s=x",
                &path,
            ])
            .output()
            .ok()
            .filter(|output| output.status.success())
            .and_then(|output| String::from_utf8(output.stdout).ok());
        let Some((width, height)) = probe.and_then(|size| {
            size.trim().split_once('x').and_then(|(width, height)| {
                Some((width.parse::<u32>().ok()?, height.parse::<u32>().ok()?))
            })
        }) else {
            return;
        };
        let frame_size = (width as usize)
            .checked_mul(height as usize)
            .and_then(|pixels| pixels.checked_mul(4));
        let Some(frame_size) = frame_size.filter(|size| *size <= 64 * 1024 * 1024) else {
            return;
        };
        let Ok(mut child) = std::process::Command::new("ffmpeg")
            .args([
                "-loglevel",
                "error",
                "-filter_threads",
                "1",
                "-re",
                "-stream_loop",
                "-1",
                "-threads",
                "2",
                "-i",
                &path,
                "-vf",
                &format!("fps={}", fps.clamp(1, 60)),
                "-f",
                "rawvideo",
                "-pix_fmt",
                "rgba",
                "-threads",
                "1",
                "-",
            ])
            .stdout(std::process::Stdio::piped())
            .spawn()
        else {
            return;
        };
        let Some(mut stdout) = child.stdout.take() else {
            return;
        };
        loop {
            if generation.load(Ordering::Relaxed) != expected_generation {
                let _ = child.kill();
                let _ = child.wait();
                return;
            }
            let mut pixels = recycled
                .lock()
                .ok()
                .and_then(|receiver| receiver.try_recv().ok())
                .filter(|pixels| pixels.len() == frame_size)
                .unwrap_or_else(|| vec![0; frame_size]);
            if stdout.read_exact(&mut pixels).is_err() {
                let _ = child.wait();
                return;
            }
            if generation.load(Ordering::Relaxed) != expected_generation {
                let _ = child.kill();
                let _ = child.wait();
                return;
            }
            match sender.send(
                image::RgbaImage::from_raw(width, height, pixels).expect("validated frame size"),
            ) {
                Ok(()) => {}
                Err(std::sync::mpsc::SendError(_)) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return;
                }
            }
        }
    });
}

fn load_font(config: &Config) -> Option<FontArc> {
    let file = std::process::Command::new("fc-match")
        .args(["-f", "%{file}", &config.theme.font])
        .output()
        .ok()?
        .stdout;
    let file = String::from_utf8(file).ok()?;
    let file = file.trim();
    FontArc::try_from_vec(std::fs::read(file).ok()?).ok()
}

fn load_desktop_entries() -> Vec<DesktopEntry> {
    let mut entries = BTreeMap::new();
    let mut directories = vec![
        PathBuf::from("/usr/share/applications"),
        PathBuf::from("/usr/local/share/applications"),
    ];
    if let Some(home) = std::env::var_os("HOME") {
        directories.insert(0, PathBuf::from(home).join(".local/share/applications"));
    }
    for directory in directories {
        let Ok(files) = std::fs::read_dir(directory) else {
            continue;
        };
        for file in files.flatten() {
            let path = file.path();
            if path
                .extension()
                .is_some_and(|extension| extension == "desktop")
            {
                if let Some(entry) = parse_desktop_entry(&path) {
                    entries.entry(entry.name.to_lowercase()).or_insert(entry);
                }
            }
        }
    }
    entries.into_values().collect()
}

fn parse_desktop_entry(path: &Path) -> Option<DesktopEntry> {
    let text = std::fs::read_to_string(path).ok()?;
    parse_desktop_entry_text(&text)
}

fn parse_desktop_entry_text(text: &str) -> Option<DesktopEntry> {
    let mut in_entry = false;
    let mut values = BTreeMap::new();
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            in_entry = line == "[Desktop Entry]";
            continue;
        }
        if !in_entry || line.starts_with('#') {
            continue;
        }
        if let Some((key, value)) = line.split_once('=') {
            values.entry(key).or_insert(value);
        }
    }
    if values.get("Type") != Some(&"Application")
        || matches!(values.get("NoDisplay"), Some(&"true"))
        || matches!(values.get("Hidden"), Some(&"true"))
    {
        return None;
    }
    let name = values.get("Name")?.trim();
    let exec = shlex::split(values.get("Exec")?.trim())?
        .into_iter()
        .filter(|argument| argument == "%%" || !argument.starts_with('%'))
        .map(|argument| argument.replace("%%", "%"))
        .collect::<Vec<_>>();
    (!name.is_empty() && !exec.is_empty()).then(|| DesktopEntry {
        name: name.into(),
        exec,
    })
}

impl App {
    fn launcher_rows(&self) -> Vec<String> {
        self.launcher_items()
            .into_iter()
            .map(|item| item.label(&self.apps, &self.snapshot))
            .collect()
    }

    fn launcher_items(&self) -> Vec<LauncherItem> {
        let query = self.launcher_query.trim();
        if let Some(command) = query.strip_prefix('>') {
            return (!command.trim().is_empty())
                .then(|| vec![LauncherItem::Command(command.trim().into())])
                .unwrap_or_default();
        }
        if let Some(query) = query.strip_prefix('@') {
            let query = query.trim().to_lowercase();
            return self
                .snapshot
                .windows
                .iter()
                .filter(|window| {
                    query.is_empty()
                        || window.title.to_lowercase().contains(&query)
                        || window.app_id.to_lowercase().contains(&query)
                })
                .map(|window| LauncherItem::Window(window.id))
                .take(8)
                .collect();
        }
        if let Some(query) = query.strip_prefix(':') {
            let query = query.trim().to_lowercase();
            return ["Lock session", "Quit Luma"]
                .into_iter()
                .filter(|action| query.is_empty() || action.to_lowercase().contains(&query))
                .map(LauncherItem::Action)
                .collect();
        }
        let query = query.to_lowercase();
        self.apps
            .iter()
            .enumerate()
            .filter(|(_, app)| query.is_empty() || app.name.to_lowercase().contains(&query))
            .map(|(index, _)| LauncherItem::App(index))
            .take(8)
            .collect()
    }

    fn activate_launcher_item(&mut self) {
        let Some(item) = self.launcher_items().get(self.launcher_selected).cloned() else {
            return;
        };
        match item {
            LauncherItem::App(index) => {
                let args = self.apps[index].exec.clone();
                thread::spawn(move || {
                    let _ = wm_core::connect_command(&format!("exec {}", serde_json::json!(args)));
                });
                self.exit = true;
            }
            LauncherItem::Window(id) => {
                thread::spawn(move || {
                    let _ = wm_core::connect_command(&format!("focus {id}"));
                });
                self.exit = true;
            }
            LauncherItem::Command(command) => {
                thread::spawn(move || {
                    let args = serde_json::json!(["sh", "-lc", command]);
                    let _ = wm_core::connect_command(&format!("exec {args}"));
                });
                self.exit = true;
            }
            LauncherItem::Action("Lock session") => {
                thread::spawn(|| {
                    let _ = wm_core::connect_command("lock");
                });
                self.exit = true;
            }
            LauncherItem::Action("Quit Luma") => {
                thread::spawn(|| {
                    let _ = wm_core::connect_command("quit");
                });
                self.exit = true;
            }
            LauncherItem::Action(_) => {}
        }
    }

    fn click_launcher(&mut self, pointer_x: f64, pointer_y: f64, qh: &QueueHandle<Self>) {
        let Some((width, height)) = self
            .surfaces
            .iter()
            .find(|surface| surface.kind == SurfaceKind::Launcher)
            .map(|surface| (surface.width, surface.height))
        else {
            return;
        };
        let result_count = self.launcher_items().len();
        match launcher_pointer_hit(width, height, pointer_x, pointer_y, result_count) {
            Some(LauncherHit::Category(category)) => {
                self.launcher_query = launcher_query_for_category(category, &self.launcher_query);
                self.launcher_selected = 0;
                self.redraw_all(qh);
            }
            Some(LauncherHit::Result(index)) => {
                self.launcher_selected = index;
                self.activate_launcher_item();
            }
            None => {}
        }
    }

    fn run_command(command: String) {
        thread::spawn(move || {
            let _ = wm_core::connect_command(&command);
        });
    }

    /// Send `recorder set` commands through the compositor with response
    /// feedback. Settings always come from `snapshot.recorder_settings`, so
    /// the UI cannot drift from what the compositor will actually use.
    fn send_recorder_commands(&mut self, commands: Vec<(String, String)>) {
        self.recorder_feedback = None;
        let Some(sender) = self.recorder_settings_events.clone() else {
            return;
        };
        thread::spawn(move || {
            for (key, value) in commands {
                let command = format!("recorder set {key} {value}");
                let error = match wm_core::connect_command(&command) {
                    Ok(response) if response.ok => None,
                    Ok(response) => {
                        Some(response.error.unwrap_or_else(|| "recorder command failed".into()))
                    }
                    Err(error) => Some(error),
                };
                if let Some(error) = error {
                    let _ = sender.send(Some(error));
                    return;
                }
            }
            let _ = sender.send(None);
        });
    }

    fn apply_recorder_adjustment(&mut self, direction: i32) {
        let settings = self.snapshot.recorder_settings.clone();
        match adjust_recorder_setting(
            &settings,
            self.recorder_settings_tab,
            self.recorder_settings_row,
            direction,
        ) {
            Ok(commands) if commands.is_empty() => {}
            Ok(commands) => self.send_recorder_commands(commands),
            Err(error) => self.recorder_feedback = Some((error, true)),
        }
    }

    /// Whether a text row (path or remembered-target identity) is being
    /// edited right now. Both share the same inline buffer.
    fn recorder_identity_editing(&self) -> bool {
        self.recorder_path_edit.is_some()
    }

    /// Kind of the selected row when it supports Enter activation.
    fn recorder_editable_row_kind(&self) -> Option<RecorderRowKind> {
        recorder_settings_rows(
            &self.snapshot.recorder_settings,
            self.recorder_settings_tab,
        )
        .get(self.recorder_settings_row)
        .map(|row| row.kind)
        .filter(|kind| {
            matches!(
                kind,
                RecorderRowKind::Path
                    | RecorderRowKind::Identity
                    | RecorderRowKind::Action
            )
        })
    }

    /// Commit the shared text buffer to whichever row is selected: the
    /// recording path, or the remembered capture target for the current
    /// method.
    fn commit_recorder_text_edit(&mut self) {
        let Some(buffer) = self.recorder_path_edit.take() else {
            return;
        };
        let buffer = buffer.trim().to_string();
        match self.recorder_editable_row_kind() {
            Some(RecorderRowKind::Identity) => {
                if buffer.is_empty() {
                    self.recorder_feedback =
                        Some(("Remembered target cannot be empty".into(), true));
                    return;
                }
                match self.snapshot.recorder_settings.capture_mode.as_str() {
                    "screen" | "xwayland" => self.send_recorder_commands(vec![(
                        "window_match".into(),
                        buffer,
                    )]),
                    "inject" => self.send_recorder_commands(vec![(
                        "inject_process".into(),
                        buffer,
                    )]),
                    "opengl" | "vulkan" => self.send_recorder_commands(vec![(
                        "game_profile".into(),
                        buffer,
                    )]),
                    _ => {}
                }
            }
            _ => {
                if buffer.is_empty() {
                    self.recorder_feedback =
                        Some(("Recording path cannot be empty".into(), true));
                    return;
                }
                self.send_recorder_commands(vec![("output_directory".into(), buffer)]);
            }
        }
    }

    fn recorder_settings_row_count(&self) -> usize {
        recorder_settings_rows(&self.snapshot.recorder_settings, self.recorder_settings_tab).len()
    }

    /// Snap the Controls-page method and per-method selection cursor to the
    /// remembered settings, so reopening the panel (or restarting the shell)
    /// lands on the same source. Remembered state is stable text identity,
    /// never the previous session's window IDs.
    fn restore_recorder_selection(&mut self) {
        let settings = self.snapshot.recorder_settings.clone();
        self.recorder_capture_mode = match settings.capture_mode.as_str() {
            "xwayland" => RecorderCaptureMode::XwaylandDirect,
            "inject" => RecorderCaptureMode::OpenGlInject,
            "opengl" => RecorderCaptureMode::OpenGlGame,
            "vulkan" => RecorderCaptureMode::VulkanGame,
            _ => RecorderCaptureMode::Screen,
        };
        match self.recorder_capture_mode {
            RecorderCaptureMode::Screen => {
                self.recorder_selected = self.window_index_for_settings(&settings);
            }
            RecorderCaptureMode::XwaylandDirect => {
                self.recorder_xwayland_selected = self
                    .snapshot
                    .windows
                    .iter()
                    .filter(|window| window.x11_window.is_some())
                    .position(|window| window_matches_settings(window, &settings))
                    .unwrap_or(0);
            }
            RecorderCaptureMode::OpenGlInject => {
                self.recorder_attach_selected = recorder_attach_targets()
                    .iter()
                    .position(|target| {
                        !settings.inject_process.is_empty()
                            && target
                                .comm
                                .to_lowercase()
                                .contains(&settings.inject_process.to_lowercase())
                    })
                    .unwrap_or(0);
            }
            RecorderCaptureMode::OpenGlGame | RecorderCaptureMode::VulkanGame => {
                let api = match self.recorder_capture_mode {
                    RecorderCaptureMode::VulkanGame => "vulkan",
                    _ => "opengl",
                };
                self.recorder_game_profile_selected = settings
                    .game_profiles
                    .iter()
                    .filter(|profile| profile.api == api)
                    .position(|profile| profile.name == settings.game_profile)
                    .unwrap_or(0);
            }
        }
        // Selection list lengths can differ from where the cursor restored.
        self.cycle_recorder_capture_mode(true);
        self.cycle_recorder_capture_mode(false);
    }

    /// Index into the Screen-mode source list for the remembered window, or
    /// 0 (the monitor/region picker) when nothing matches.
    fn window_index_for_settings(&self, settings: &wm_core::Recorder) -> usize {
        if settings.window_app_id.is_empty() && settings.window_title.is_empty() {
            return 0;
        }
        self.snapshot
            .windows
            .iter()
            .position(|window| window_matches_settings(window, settings))
            .map(|index| index + 1)
            .unwrap_or(0)
    }

    fn send_recorder_command_string(&mut self, command: &str) {
        self.recorder_feedback = None;
        let Some(sender) = self.recorder_settings_events.clone() else {
            Self::run_command(command.into());
            return;
        };
        let command = command.to_string();
        thread::spawn(move || {
            let error = match wm_core::connect_command(&command) {
                Ok(response) if response.ok => None,
                Ok(response) => {
                    Some(response.error.unwrap_or_else(|| "recorder command failed".into()))
                }
                Err(error) => Some(error),
            };
            let _ = sender.send(error);
        });
    }

    /// Resolve a click position to a recorder hit region using the same
    /// geometry functions the renderer uses.
    fn recorder_hit_at(&self, pointer_x: f64, pointer_y: f64) -> Option<RecorderHit> {
        let surface = self
            .surfaces
            .iter()
            .find(|surface| surface.kind == SurfaceKind::Recorder)?;
        let (px, py, panel_w, _) = recorder_panel(surface.width, surface.height);
        let x = pointer_x.max(0.0) as u32;
        let y = pointer_y.max(0.0) as u32;
        let inside = |rect: [u32; 4]| {
            x >= px.saturating_add(rect[0])
                && x < px.saturating_add(rect[0].saturating_add(rect[2]))
                && y >= py.saturating_add(rect[1])
                && y < py.saturating_add(rect[1].saturating_add(rect[3]))
        };
        if self.recorder_view == RecorderView::Settings {
            if inside(recorder_settings_close_rect(panel_w)) {
                return Some(RecorderHit::CloseSettings);
            }
            for index in 0..RecorderSettingsTab::ALL.len() {
                if inside(recorder_settings_tab_rect(index)) {
                    return Some(RecorderHit::Tab(index));
                }
            }
            if inside(recorder_settings_reset_rect()) {
                return Some(RecorderHit::ResetSettings);
            }
            let rows = recorder_settings_rows(
                &self.snapshot.recorder_settings,
                self.recorder_settings_tab,
            );
            for (index, row) in rows.iter().enumerate() {
                let row_rect = recorder_settings_row_rect(index, panel_w);
                if !inside(row_rect) {
                    continue;
                }
                return match row.kind {
                    RecorderRowKind::Cycle | RecorderRowKind::Step => {
                        let (dec, _, inc) = recorder_settings_widget_rects(row_rect);
                        let part = if inside(dec) {
                            RecorderRowPart::Dec
                        } else if inside(inc) {
                            RecorderRowPart::Inc
                        } else {
                            RecorderRowPart::Value
                        };
                        Some(RecorderHit::Row(index, part))
                    }
                    _ => Some(RecorderHit::Row(index, RecorderRowPart::Value)),
                };
            }
            return None;
        }
        recorder_control_button_rects(panel_w)
            .iter()
            .find(|(rect, _)| inside(*rect))
            .map(|(_, hit)| *hit)
    }

    /// Dispatch one recorder hit. Returns true when the panel should close.
    fn activate_recorder_hit(&mut self, hit: Option<RecorderHit>) -> bool {
        let Some(hit) = hit else {
            return false;
        };
        let running = !matches!(
            self.snapshot.recorder.state,
            RecorderState::Idle | RecorderState::Error
        );
        match hit {
            RecorderHit::StartStop => {
                if running {
                    Self::run_command("recorder stop".into());
                    self.exit = true;
                    return true;
                }
                if self.recorder_can_start() {
                    if let Some(command) = self.recorder_start_command() {
                        Self::run_command(command);
                        self.exit = true;
                        return true;
                    }
                }
                false
            }
            RecorderHit::Pause => {
                if running {
                    Self::run_command("recorder pause".into());
                }
                false
            }
            RecorderHit::ReplaySave => {
                if running {
                    Self::run_command("recorder replay-save".into());
                }
                false
            }
            RecorderHit::Settings => {
                self.recorder_view = RecorderView::Settings;
                false
            }
            RecorderHit::CloseSettings => {
                self.recorder_view = RecorderView::Controls;
                false
            }
            RecorderHit::ResetSettings => {
                self.send_recorder_command_string("recorder settings-reset");
                false
            }
            RecorderHit::Tab(index) => {
                if let Some(tab) = RecorderSettingsTab::ALL.get(index) {
                    self.recorder_settings_tab = *tab;
                    self.recorder_settings_row = self
                        .recorder_settings_row
                        .min(self.recorder_settings_row_count().saturating_sub(1));
                    self.recorder_path_edit = None;
                }
                false
            }
            RecorderHit::Row(index, part) => {
                self.recorder_settings_row = index;
                let settings = self.snapshot.recorder_settings.clone();
                let row_count = recorder_settings_rows(&settings, self.recorder_settings_tab).len();
                let Some(row) = recorder_settings_rows(&settings, self.recorder_settings_tab)
                    .get(index.min(row_count.saturating_sub(1)))
                    .cloned()
                else {
                    return false;
                };
                if index >= row_count {
                    self.recorder_settings_row = row_count.saturating_sub(1);
                    return false;
                }
                match row.kind {
                    RecorderRowKind::Toggle => self.apply_recorder_adjustment(1),
                    RecorderRowKind::Path => {
                        self.recorder_path_edit = Some(settings.output_directory.clone());
                    }
                    RecorderRowKind::Identity => {
                        self.recorder_path_edit = Some(self.recorder_identity_edit_seed());
                    }
                    RecorderRowKind::Action => {
                        self.remember_current_recorder_source();
                    }
                    RecorderRowKind::Cycle | RecorderRowKind::Step => {
                        self.apply_recorder_adjustment(match part {
                            RecorderRowPart::Dec => -1,
                            _ => 1,
                        });
                    }
                    RecorderRowKind::Info => {}
                }
                false
            }
        }
    }

    /// Seed for editing the remembered target: the Controls-page selection
    /// that the action row would store, or the stored identity placeholder
    /// when the cursor has nothing applicable selected.
    fn recorder_identity_edit_seed(&self) -> String {
        let settings = self.snapshot.recorder_settings.clone();
        match settings.capture_mode.as_str() {
            "screen" | "xwayland" => self
                .recorder_selected_window_identity()
                .unwrap_or_else(|| recorder_window_identity_placeholder(&settings)),
            "inject" => self
                .selected_recorder_attach_target()
                .map(|target| target.comm)
                .unwrap_or_else(|| settings.inject_process.clone()),
            "opengl" | "vulkan" => self
                .selected_recorder_game_profile()
                .map(|profile| profile.name.clone())
                .unwrap_or_else(|| settings.game_profile.clone()),
            _ => String::new(),
        }
    }

    /// app_id first, title as fallback, for the Controls-page window cursor.
    fn recorder_selected_window_identity(&self) -> Option<String> {
        let window = match self.recorder_capture_mode {
            RecorderCaptureMode::XwaylandDirect => {
                self.selected_recorder_xwayland_window()?.clone()
            }
            _ => self
                .snapshot
                .windows
                .get(self.recorder_selected.saturating_sub(1))?
                .clone(),
        };
        if !window.app_id.trim().is_empty() {
            return Some(window.app_id.clone());
        }
        if !window.title.trim().is_empty() {
            return Some(window.title.clone());
        }
        None
    }

    /// Remember the Controls-page selection as settings ("Remember current").
    /// Window memory stores app_id/title identity, process memory the command
    /// name, profile memory the profile name — never PIDs or window IDs.
    fn remember_current_recorder_source(&mut self) {
        let mut commands: Vec<(String, String)> = vec![(
            "capture_mode".into(),
            match self.recorder_capture_mode {
                RecorderCaptureMode::XwaylandDirect => "xwayland",
                RecorderCaptureMode::OpenGlInject => "inject",
                RecorderCaptureMode::OpenGlGame => "opengl",
                RecorderCaptureMode::VulkanGame => "vulkan",
                RecorderCaptureMode::Screen => "screen",
            }
            .into(),
        )];
        match self.recorder_capture_mode {
            RecorderCaptureMode::Screen | RecorderCaptureMode::XwaylandDirect => {
                let window = match self.recorder_capture_mode {
                    RecorderCaptureMode::XwaylandDirect => {
                        self.selected_recorder_xwayland_window().cloned()
                    }
                    _ => self
                        .snapshot
                        .windows
                        .get(self.recorder_selected.saturating_sub(1))
                        .cloned(),
                };
                let Some(window) = window else {
                    self.recorder_feedback =
                        Some(("Select a source on the Controls page first".into(), true));
                    return;
                };
                if !window.app_id.trim().is_empty() {
                    commands.push(("window_match".into(), window.app_id.clone()));
                }
                if !window.title.trim().is_empty() {
                    commands.push(("window_title".into(), window.title.clone()));
                }
                if commands.len() == 1 {
                    self.recorder_feedback =
                        Some(("That window has no app_id or title to remember".into(), true));
                    return;
                }
            }
            RecorderCaptureMode::OpenGlInject => {
                let Some(target) = self.selected_recorder_attach_target() else {
                    self.recorder_feedback =
                        Some(("Select a process on the Controls page first".into(), true));
                    return;
                };
                commands.push(("inject_process".into(), target.comm));
            }
            RecorderCaptureMode::OpenGlGame | RecorderCaptureMode::VulkanGame => {
                let Some(profile) = self.selected_recorder_game_profile() else {
                    self.recorder_feedback =
                        Some(("Select a profile on the Controls page first".into(), true));
                    return;
                };
                commands.push(("game_profile".into(), profile.name.clone()));
            }
        }
        self.send_recorder_commands(commands);
    }

    fn recorder_source_label(&self) -> String {
        if self.recorder_selected == 0 {
            "Monitor / region portal picker".into()
        } else {
            self.snapshot
                .windows
                .get(self.recorder_selected - 1)
                .map(|window| format!("Window {}: {}", window.id, window.title))
                .unwrap_or_else(|| "Monitor / region portal picker".into())
        }
    }

    fn recorder_capture_label(&self) -> String {
        self.recorder_capture_mode.label().into()
    }

    fn recorder_game_profile_count(&self) -> usize {
        let api = match self.recorder_capture_mode {
            RecorderCaptureMode::OpenGlGame => "opengl",
            RecorderCaptureMode::VulkanGame => "vulkan",
            RecorderCaptureMode::Screen
            | RecorderCaptureMode::XwaylandDirect
            | RecorderCaptureMode::OpenGlInject => return 0,
        };
        self.snapshot
            .recorder_settings
            .game_profiles
            .iter()
            .filter(|profile| profile.api == api)
            .count()
    }

    fn selected_recorder_game_profile(&self) -> Option<&wm_core::GameCaptureProfile> {
        let api = match self.recorder_capture_mode {
            RecorderCaptureMode::OpenGlGame => "opengl",
            RecorderCaptureMode::VulkanGame => "vulkan",
            RecorderCaptureMode::Screen
            | RecorderCaptureMode::XwaylandDirect
            | RecorderCaptureMode::OpenGlInject => return None,
        };
        self.snapshot
            .recorder_settings
            .game_profiles
            .iter()
            .filter(|profile| profile.api == api)
            .nth(self.recorder_game_profile_selected)
    }

    fn recorder_selection_heading(&self) -> &'static str {
        match self.recorder_capture_mode {
            RecorderCaptureMode::Screen => "SOURCE",
            RecorderCaptureMode::XwaylandDirect => "WINDOW",
            RecorderCaptureMode::OpenGlInject => "PROCESS",
            RecorderCaptureMode::OpenGlGame | RecorderCaptureMode::VulkanGame => "PROFILE",
        }
    }

    fn recorder_selection_label(&self) -> String {
        match self.recorder_capture_mode {
            RecorderCaptureMode::Screen => self.recorder_source_label(),
            RecorderCaptureMode::XwaylandDirect => self
                .selected_recorder_xwayland_window()
                .map(|window| {
                    format!(
                        "XID {:#x}  ·  {}",
                        window.x11_window.expect("filtered Xwayland window"),
                        window.title
                    )
                })
                .unwrap_or_else(|| "No Xwayland window found".into()),
            RecorderCaptureMode::OpenGlInject => self
                .selected_recorder_attach_target()
                .map(|target| format!("PID {}  ·  {}  ·  {}", target.pid, target.comm, target.api))
                .unwrap_or_else(|| "No running OpenGL process found".into()),
            RecorderCaptureMode::OpenGlGame | RecorderCaptureMode::VulkanGame => self
                .selected_recorder_game_profile()
                .map(|profile| format!("{}  ·  {} FPS", profile.name, profile.fps))
                .unwrap_or_else(|| "No matching game profile configured".into()),
        }
    }

    fn recorder_capture_note(&self) -> &'static str {
        match self.recorder_capture_mode {
            RecorderCaptureMode::Screen => {
                "GAME CAPTURE  inject into a running graphics process or launch a profile"
            }
            RecorderCaptureMode::XwaylandDirect => {
                "XWAYLAND  commit-paced compositor DMA-BUF copy; no injection or CPU pixels"
            }
            RecorderCaptureMode::OpenGlInject => {
                "OPENGL INJECT  hooks resolved GLX/EGL presents; stopping leaves the game running"
            }
            RecorderCaptureMode::OpenGlGame => {
                "OPENGL  starts the selected Luma launch profile and game hook"
            }
            RecorderCaptureMode::VulkanGame => {
                "VULKAN  launches through a DMA-BUF present layer and external GPU encoder"
            }
        }
    }

    /// Why the Start button cannot start the selected method right now, using
    /// the live panel state. Returns None when the selection is usable.
    fn recorder_start_blocker(&self) -> Option<&'static str> {
        if !matches!(
            self.snapshot.recorder.state,
            RecorderState::Idle | RecorderState::Error
        ) {
            return None;
        }
        recorder_start_blocker_for(
            self.recorder_capture_mode,
            &self.snapshot.recorder_settings,
            self.selected_recorder_xwayland_window().is_some(),
            self.selected_recorder_attach_target().is_some(),
            self.recorder_game_profile_count(),
            self.selected_recorder_game_profile().is_some(),
        )
    }

    fn recorder_can_start(&self) -> bool {
        if !matches!(
            self.snapshot.recorder.state,
            RecorderState::Idle | RecorderState::Error
        ) {
            return false;
        }
        self.recorder_start_blocker().is_none()
    }

    fn recorder_start_command(&self) -> Option<String> {
        if self.snapshot.recorder.state != RecorderState::Idle
            && self.snapshot.recorder.state != RecorderState::Error
        {
            return Some("recorder stop".into());
        }
        match self.recorder_capture_mode {
            // Screen, Xwayland, and inject selections re-resolve the
            // remembered window/identity at start: titles and PIDs change
            // across restarts, so the stored app_id/title is matched fresh.
            RecorderCaptureMode::Screen => {
                Some(self.remembered_screen_command("start", self.recorder_selected))
            }
            RecorderCaptureMode::XwaylandDirect => self
                .selected_recorder_xwayland_window()
                .and_then(|window| window.x11_window)
                .map(|window| format!("recorder xwayland-start {window}")),
            RecorderCaptureMode::OpenGlInject => self
                .selected_recorder_attach_target()
                .map(|target| format!("recorder game-attach {}", target.pid)),
            RecorderCaptureMode::OpenGlGame | RecorderCaptureMode::VulkanGame => self
                .selected_recorder_game_profile()
                .map(|profile| format!("recorder game-start {}", profile.name)),
        }
    }

    /// Screen-mode start command: the remembered window wins when it is open
    /// right now; otherwise the picker selection is used verbatim.
    fn remembered_screen_command(&self, action: &str, selected: usize) -> String {
        let settings = &self.snapshot.recorder_settings;
        if (!settings.window_app_id.is_empty() || !settings.window_title.is_empty())
            && self
                .snapshot
                .windows
                .iter()
                .any(|window| window_matches_settings(window, settings))
        {
            return format!("recorder {action} remembered");
        }
        self.snapshot
            .windows
            .get(selected.saturating_sub(1))
            .filter(|_| selected > 0)
            .map(|window| format!("recorder {action} window {}", window.id))
            .unwrap_or_else(|| format!("recorder {action} output"))
    }

    fn recorder_display_fps(&self) -> u32 {
        let settings = &self.snapshot.recorder_settings;
        match self.recorder_capture_mode {
            RecorderCaptureMode::Screen => settings.screen_fps,
            RecorderCaptureMode::XwaylandDirect => settings.fps,
            RecorderCaptureMode::OpenGlInject => settings.fps,
            RecorderCaptureMode::OpenGlGame | RecorderCaptureMode::VulkanGame => self
                .selected_recorder_game_profile()
                .map(|profile| profile.fps)
                .unwrap_or(settings.fps),
        }
    }

    fn move_recorder_selection(&mut self, forward: bool) {
        if self.recorder_capture_mode == RecorderCaptureMode::Screen {
            if forward {
                self.recorder_selected =
                    (self.recorder_selected + 1).min(self.snapshot.windows.len());
            } else {
                self.recorder_selected = self.recorder_selected.saturating_sub(1);
            }
            return;
        }
        if self.recorder_capture_mode == RecorderCaptureMode::XwaylandDirect {
            let count = self
                .snapshot
                .windows
                .iter()
                .filter(|window| window.x11_window.is_some())
                .count();
            if count == 0 {
                self.recorder_xwayland_selected = 0;
            } else if forward {
                self.recorder_xwayland_selected =
                    (self.recorder_xwayland_selected + 1).min(count - 1);
            } else {
                self.recorder_xwayland_selected = self.recorder_xwayland_selected.saturating_sub(1);
            }
            return;
        }
        if self.recorder_capture_mode == RecorderCaptureMode::OpenGlInject {
            let count = recorder_attach_targets().len();
            if count == 0 {
                self.recorder_attach_selected = 0;
            } else if forward {
                self.recorder_attach_selected = (self.recorder_attach_selected + 1).min(count - 1);
            } else {
                self.recorder_attach_selected = self.recorder_attach_selected.saturating_sub(1);
            }
            return;
        }
        let count = self.recorder_game_profile_count();
        if count == 0 {
            self.recorder_game_profile_selected = 0;
        } else if forward {
            self.recorder_game_profile_selected =
                (self.recorder_game_profile_selected + 1).min(count - 1);
        } else {
            self.recorder_game_profile_selected =
                self.recorder_game_profile_selected.saturating_sub(1);
        }
    }

    fn cycle_recorder_capture_mode(&mut self, forward: bool) {
        self.recorder_capture_mode = self.recorder_capture_mode.cycle(forward);
        let count = self.recorder_game_profile_count();
        self.recorder_game_profile_selected = self
            .recorder_game_profile_selected
            .min(count.saturating_sub(1));
        let attach_count = recorder_attach_targets().len();
        self.recorder_attach_selected = self
            .recorder_attach_selected
            .min(attach_count.saturating_sub(1));
        let xwayland_count = self
            .snapshot
            .windows
            .iter()
            .filter(|window| window.x11_window.is_some())
            .count();
        self.recorder_xwayland_selected = self
            .recorder_xwayland_selected
            .min(xwayland_count.saturating_sub(1));
    }

    fn selected_recorder_attach_target(&self) -> Option<RecorderAttachTarget> {
        recorder_attach_targets()
            .into_iter()
            .nth(self.recorder_attach_selected)
    }

    fn selected_recorder_xwayland_window(&self) -> Option<&wm_core::WindowInfo> {
        self.snapshot
            .windows
            .iter()
            .filter(|window| window.x11_window.is_some())
            .nth(self.recorder_xwayland_selected)
    }

    fn bar_module_at(&self, surface: &wl_surface::WlSurface, x: f64) -> Option<&'static str> {
        let surface = self
            .surfaces
            .iter()
            .find(|candidate| candidate.layer.wl_surface() == surface)?;
        self.bar_module_rects(surface.width, surface.height)
            .into_iter()
            .find(|(_, left, right)| x >= f64::from(*left) && x <= f64::from(*right))
            .map(|(module, _, _)| module)
    }

    /// Right-side bar values in the exact order and form the renderer draws
    /// them, so pointer hit tests always measure the same pixels.
    fn bar_right_modules(&self) -> Vec<(&'static str, String)> {
        let modules = &self.config.shell.modules;
        let has = |name: &str| modules.iter().any(|module| module == name);
        let has_tray_icons = self.tray.iter().any(TrayItem::has_visible_icon);
        [
            has("battery")
                .then(|| self.battery.clone())
                .flatten()
                .map(|value| ("battery", value)),
            (has("media") && self.media.label != "MEDIA —")
                .then(|| ("media", "MEDIA".into())),
            has("bluetooth").then(|| ("bluetooth", self.bluetooth.label.clone())),
            (has("tray") && has_tray_icons)
                .then(|| ("tray", format!("TRAY {}", self.tray.len()))),
            has("notifications")
                .then(|| self.notifications_bar_label())
                .flatten()
                .map(|value| ("notifications", value)),
            has("network").then(|| {
                ("network", compact_network_bar_label(&self.network.label).into())
            }),
            has("audio").then(|| ("audio", self.audio.label.clone())),
            has("clock").then(|| ("clock", self.clock.clone())),
            // The recorder label is drawn regardless of the configured module
            // list, so the hit test must mirror it unconditionally.
            self.recorder_bar_label().map(|value| ("recorder", value)),
            has("power").then(|| ("power", "POWER".to_string())),
        ]
        .into_iter()
        .flatten()
        .collect()
    }

    fn bar_module_rects(&self, width: u32, height: u32) -> Vec<(&'static str, u32, u32)> {
        bar_module_layout(
            width,
            height,
            self.font.as_ref(),
            self.config.theme.font_size,
            self.tray
                .iter()
                .filter(|item| item.has_visible_icon())
                .take(6)
                .count(),
            if self.config.shell.modules.iter().any(|module| module == "workspaces") {
                BAR_WORKSPACE_START
                    + BAR_WORKSPACE_STEP * u32::from(self.config.layout.workspaces)
                    + 16
            } else {
                72
            },
            &self.bar_right_modules(),
        )
    }

    /// The recorder label exactly as the bar draws it.
    fn recorder_bar_label(&self) -> Option<String> {
        match self.snapshot.recorder.state {
            RecorderState::Recording | RecorderState::Starting => {
                Some(format!("REC {}", self.snapshot.recorder.requested_fps))
            }
            RecorderState::Paused => Some("REC PAUSED".into()),
            RecorderState::Replay => Some("REPLAY".into()),
            RecorderState::Error => Some("REC ERR".into()),
            RecorderState::Idle => None,
        }
    }

    /// Keep the bar count compact; the panel carries notification details.
    fn notifications_bar_label(&self) -> Option<String> {
        if self.do_not_disturb.load(Ordering::Relaxed) {
            return Some("DND".into());
        }
        Some(format!("NOT {}", self.notifications.len()))
    }

    fn tray_item_at(&self, surface: &wl_surface::WlSurface, x: f64) -> Option<TrayItem> {
        let surface = self
            .surfaces
            .iter()
            .find(|candidate| candidate.layer.wl_surface() == surface)?;
        if !self
            .config
            .shell
            .modules
            .iter()
            .any(|module| module == "tray")
        {
            return None;
        }
        let (_, left, right) = self
            .bar_module_rects(surface.width, surface.height)
            .into_iter()
            .find(|(module, _, _)| *module == "tray")?;
        if x < f64::from(left) || x >= f64::from(right) {
            return None;
        }
        let icon_size = surface.height.saturating_sub(12).clamp(12, 24);
        let icon_index =
            ((x as u32).saturating_sub(left.saturating_add(4)) / (icon_size + 2)) as usize;
        self.tray
            .iter()
            .filter(|item| item.has_visible_icon())
            .take(6)
            .nth(icon_index)
            .cloned()
    }

    fn change_audio(command: &[&str]) {
        let args = command
            .iter()
            .map(|value| (*value).to_owned())
            .collect::<Vec<_>>();
        thread::spawn(move || {
            let _ = std::process::Command::new("pactl").args(args).status();
        });
    }

    fn set_wireless(enabled: bool) {
        thread::spawn(move || {
            let Ok(connection) = zbus::blocking::Connection::system() else {
                return;
            };
            let Ok(network_manager) = zbus::blocking::Proxy::new(
                &connection,
                "org.freedesktop.NetworkManager",
                "/org/freedesktop/NetworkManager",
                "org.freedesktop.NetworkManager",
            ) else {
                return;
            };
            let _ = network_manager.set_property("WirelessEnabled", enabled);
        });
    }

    fn set_networking(enabled: bool) {
        thread::spawn(move || {
            let Ok(connection) = zbus::blocking::Connection::system() else {
                return;
            };
            let Ok(network_manager) = zbus::blocking::Proxy::new(
                &connection,
                "org.freedesktop.NetworkManager",
                "/org/freedesktop/NetworkManager",
                "org.freedesktop.NetworkManager",
            ) else {
                return;
            };
            let _ = network_manager.set_property("NetworkingEnabled", enabled);
        });
    }

    fn set_bluetooth_powered(enabled: bool) {
        thread::spawn(move || {
            let Ok(connection) = zbus::blocking::Connection::system() else {
                return;
            };
            let Ok(manager) = zbus::blocking::Proxy::new(
                &connection,
                "org.bluez",
                "/",
                "org.freedesktop.DBus.ObjectManager",
            ) else {
                return;
            };
            let Ok(objects) = manager.call::<_, _, BluezManagedObjects>("GetManagedObjects", &())
            else {
                return;
            };
            let Some(path) = objects.into_iter().find_map(|(path, interfaces)| {
                interfaces
                    .contains_key("org.bluez.Adapter1")
                    .then_some(path)
            }) else {
                return;
            };
            let Ok(adapter) = zbus::blocking::Proxy::new(
                &connection,
                "org.bluez",
                path.as_str(),
                "org.bluez.Adapter1",
            ) else {
                return;
            };
            let _ = adapter.set_property("Powered", enabled);
        });
    }

    fn set_bluetooth_discovery(enabled: bool) {
        thread::spawn(move || {
            let Ok(connection) = zbus::blocking::Connection::system() else {
                return;
            };
            let Ok(manager) = zbus::blocking::Proxy::new(
                &connection,
                "org.bluez",
                "/",
                "org.freedesktop.DBus.ObjectManager",
            ) else {
                return;
            };
            let Ok(objects) = manager.call::<_, _, BluezManagedObjects>("GetManagedObjects", &())
            else {
                return;
            };
            let Some(path) = objects.into_iter().find_map(|(path, interfaces)| {
                interfaces
                    .contains_key("org.bluez.Adapter1")
                    .then_some(path)
            }) else {
                return;
            };
            if let Ok(adapter) = zbus::blocking::Proxy::new(
                &connection,
                "org.bluez",
                path.as_str(),
                "org.bluez.Adapter1",
            ) {
                let _ = adapter.call::<_, _, ()>(
                    if enabled {
                        "StartDiscovery"
                    } else {
                        "StopDiscovery"
                    },
                    &(),
                );
            }
        });
    }

    fn spawn_desktop_tool(program: &'static str) {
        thread::spawn(move || {
            let _ = std::process::Command::new(program).spawn();
        });
    }

    fn change_media(method: &'static str) {
        thread::spawn(move || {
            let Ok(connection) = zbus::blocking::Connection::session() else {
                return;
            };
            let Ok(bus) = zbus::blocking::Proxy::new(
                &connection,
                "org.freedesktop.DBus",
                "/org/freedesktop/DBus",
                "org.freedesktop.DBus",
            ) else {
                return;
            };
            let Ok(names) = bus.call::<_, _, Vec<String>>("ListNames", &()) else {
                return;
            };
            let mut fallback = None::<String>;
            for name in names
                .into_iter()
                .filter(|name| name.starts_with("org.mpris.MediaPlayer2."))
            {
                let Ok(player) = zbus::blocking::Proxy::new(
                    &connection,
                    name.as_str(),
                    "/org/mpris/MediaPlayer2",
                    "org.mpris.MediaPlayer2.Player",
                ) else {
                    continue;
                };
                if player
                    .get_property::<String>("PlaybackStatus")
                    .is_ok_and(|status| status == "Playing")
                {
                    let _ = player.call::<_, _, ()>(method, &());
                    return;
                }
                fallback = Some(name.clone());
            }
            if let Some(name) = fallback {
                if let Ok(player) = zbus::blocking::Proxy::new(
                    &connection,
                    name.as_str(),
                    "/org/mpris/MediaPlayer2",
                    "org.mpris.MediaPlayer2.Player",
                ) {
                    let _ = player.call::<_, _, ()>(method, &());
                }
            }
        });
    }

    fn activate_tray_item(service: String, path: String, method: &'static str, x: i32, y: i32) {
        thread::spawn(move || {
            let Ok(connection) = zbus::blocking::Connection::session() else {
                return;
            };
            let Ok(item) = zbus::blocking::Proxy::new(
                &connection,
                service.as_str(),
                path.as_str(),
                "org.kde.StatusNotifierItem",
            ) else {
                return;
            };
            let _ = item.call::<_, _, ()>(method, &(x, y));
        });
    }

    fn request_tray_menu(
        &mut self,
        item: TrayItem,
        surface: &wl_surface::WlSurface,
        local_x: f64,
        action_position: (i32, i32),
        qh: &QueueHandle<Self>,
    ) -> bool {
        let Some(menu_path) = item.menu_path.clone() else {
            return false;
        };
        let Some(output) = self
            .surfaces
            .iter()
            .find(|candidate| candidate.layer.wl_surface() == surface)
            .and_then(|surface| surface.output.clone())
        else {
            return false;
        };
        let state = TrayMenuState {
            service: item.service,
            path: item.path,
            menu_path,
            output,
            local_x: local_x.round().clamp(0.0, i32::MAX as f64) as i32,
            action_position,
            parents: vec![0],
            rows: Vec::new(),
        };
        let Some(sender) = self.tray_menu_events.clone() else {
            return false;
        };
        self.tray_menu = Some(state.clone());
        self.ensure_tray_menu_surface(qh, state.output.clone());
        load_tray_menu_page(state.service, state.menu_path, 0, sender);
        true
    }

    fn ensure_tray_menu_surface(&mut self, qh: &QueueHandle<Self>, output: wl_output::WlOutput) {
        if self.surfaces.iter().any(|surface| {
            surface.kind == SurfaceKind::TrayMenu && surface.output.as_ref() == Some(&output)
        }) {
            return;
        }
        self.add_kind_surface(qh, SurfaceKind::TrayMenu, Some(output));
    }

    fn show_tray_menu_surface(&mut self, qh: &QueueHandle<Self>) {
        const WIDTH: u32 = 280;
        const ROW_HEIGHT: u32 = 28;
        let Some(menu) = self.tray_menu.as_ref() else {
            return;
        };
        let rows = menu.rows.len() + usize::from(menu.parents.len() > 1);
        let height = (rows.max(1) as u32 * ROW_HEIGHT + 12).min(480);
        let output_width = self
            .output_state
            .info(&menu.output)
            .and_then(|info| info.logical_size.map(|size| size.0))
            .unwrap_or(WIDTH as i32)
            .max(WIDTH as i32);
        let left = menu
            .local_x
            .saturating_sub(WIDTH as i32 - 16)
            .clamp(0, output_width.saturating_sub(WIDTH as i32));
        let bottom = self.config.shell.position == "bottom";
        let Some(index) = self.surfaces.iter().position(|surface| {
            surface.kind == SurfaceKind::TrayMenu && surface.output.as_ref() == Some(&menu.output)
        }) else {
            return;
        };
        let surface = &self.surfaces[index];
        surface
            .layer
            .set_anchor((if bottom { Anchor::BOTTOM } else { Anchor::TOP }) | Anchor::LEFT);
        surface.layer.set_margin(
            if bottom { 0 } else { self.config.shell.height },
            0,
            if bottom { self.config.shell.height } else { 0 },
            left,
        );
        surface.layer.set_size(WIDTH, height);
        surface.layer.commit();
        if surface.configured {
            self.draw(index, qh);
        }
    }

    fn close_tray_menu(&mut self, qh: &QueueHandle<Self>) {
        self.tray_menu = None;
        let indices = self
            .surfaces
            .iter()
            .enumerate()
            .filter_map(|(index, surface)| (surface.kind == SurfaceKind::TrayMenu).then_some(index))
            .collect::<Vec<_>>();
        for index in indices {
            let surface = &self.surfaces[index];
            surface.layer.set_size(1, 1);
            surface.layer.set_margin(0, 0, 0, 0);
            surface.layer.commit();
            if surface.configured {
                self.draw(index, qh);
            }
        }
    }

    fn click_tray_menu(&mut self, y: f64, button: u32, qh: &QueueHandle<Self>) {
        if button != 0x110 {
            self.close_tray_menu(qh);
            return;
        }
        const ROW_HEIGHT: u32 = 28;
        let Some(menu) = self.tray_menu.as_mut() else {
            return;
        };
        let row = (y.max(0.0) as u32 / ROW_HEIGHT) as usize;
        if menu.parents.len() > 1 && row == 0 {
            menu.parents.pop();
            menu.rows.clear();
            let parent = *menu.parents.last().unwrap_or(&0);
            if let Some(sender) = self.tray_menu_events.clone() {
                load_tray_menu_page(menu.service.clone(), menu.menu_path.clone(), parent, sender);
            }
            return;
        }
        let offset = usize::from(menu.parents.len() > 1);
        let Some(item) = row
            .checked_sub(offset)
            .and_then(|row| menu.rows.get(row))
            .cloned()
        else {
            self.close_tray_menu(qh);
            return;
        };
        if item.separator || !item.enabled {
            return;
        }
        if item.submenu && menu.parents.len() < 16 {
            menu.parents.push(item.id);
            menu.rows.clear();
            if let Some(sender) = self.tray_menu_events.clone() {
                load_tray_menu_page(
                    menu.service.clone(),
                    menu.menu_path.clone(),
                    item.id,
                    sender,
                );
            }
        } else {
            activate_tray_menu_entry(menu.service.clone(), menu.menu_path.clone(), item.id);
            self.close_tray_menu(qh);
        }
    }

    fn scroll_tray_item(service: String, path: String, delta: i32, axis: &'static str) {
        if delta == 0 {
            return;
        }
        thread::spawn(move || {
            let Ok(connection) = zbus::blocking::Connection::session() else {
                return;
            };
            let Ok(item) = zbus::blocking::Proxy::new(
                &connection,
                service.as_str(),
                path.as_str(),
                "org.kde.StatusNotifierItem",
            ) else {
                return;
            };
            let _ = item.call::<_, _, ()>("Scroll", &(delta, axis));
        });
    }

    fn tray_action_position(&self, surface: &wl_surface::WlSurface, x: f64, y: f64) -> (i32, i32) {
        let geometry = self
            .surfaces
            .iter()
            .find(|candidate| candidate.layer.wl_surface() == surface)
            .and_then(|surface| surface.output.as_ref())
            .and_then(|output| self.output_state.info(output))
            .and_then(|info| info.name)
            .and_then(|name| {
                self.snapshot
                    .outputs
                    .iter()
                    .find(|output| output.name == name)
            })
            .map(|output| output.geometry);
        let local_x = x.round().clamp(i32::MIN as f64, i32::MAX as f64) as i32;
        let local_y = y.round().clamp(i32::MIN as f64, i32::MAX as f64) as i32;
        geometry.map_or((local_x, local_y), |geometry| {
            (
                geometry.x.saturating_add(local_x),
                geometry.y.saturating_add(local_y),
            )
        })
    }

    fn click_bar(
        &mut self,
        surface: &wl_surface::WlSurface,
        x: f64,
        y: f64,
        button: u32,
        qh: &QueueHandle<Self>,
    ) {
        if self.tray_menu.is_some() {
            self.close_tray_menu(qh);
        }
        if x < f64::from(BAR_LAUNCHER_RIGHT) {
            if button == 0x110 {
                Self::run_command("launcher".into());
            }
            return;
        }
        if let Some(item) = self.tray_item_at(surface, x) {
            let (action_x, action_y) = self.tray_action_position(surface, x, y);
            match button {
                0x110 => Self::activate_tray_item(
                    item.service,
                    item.path,
                    "Activate",
                    action_x,
                    action_y,
                ),
                0x111 => {
                    if !self.request_tray_menu(item.clone(), surface, x, (action_x, action_y), qh) {
                        Self::activate_tray_item(
                            item.service,
                            item.path,
                            "ContextMenu",
                            action_x,
                            action_y,
                        );
                    }
                }
                0x112 => Self::activate_tray_item(
                    item.service,
                    item.path,
                    "SecondaryActivate",
                    action_x,
                    action_y,
                ),
                _ => {}
            }
            return;
        }
        let workspace_span = BAR_WORKSPACE_STEP * u32::from(self.config.layout.workspaces);
        if self.config.shell.modules.iter().any(|module| module == "workspaces")
            && x >= f64::from(BAR_WORKSPACE_START)
            && x < f64::from(BAR_WORKSPACE_START + workspace_span)
        {
            if button == 0x110 {
                let workspace = ((x as u32 - BAR_WORKSPACE_START) / BAR_WORKSPACE_STEP + 1) as u8;
                let output = self
                    .surfaces
                    .iter()
                    .find(|candidate| candidate.layer.wl_surface() == surface)
                    .and_then(|candidate| candidate.output.as_ref())
                    .and_then(|output| self.output_state.info(output))
                    .and_then(|info| info.name)
                    .unwrap_or_default();
                Self::run_command(format!("workspace {workspace} {output}"));
            }
            return;
        }
        match (self.bar_module_at(surface, x), button) {
                (Some("audio"), 0x110) => self.show_controls(ControlPanel::Audio, surface, qh),
                (Some("network"), 0x110) => self.show_controls(ControlPanel::Network, surface, qh),
                (Some("bluetooth"), 0x110) => {
                    self.show_controls(ControlPanel::Bluetooth, surface, qh)
                }
                (Some("media"), 0x110) => self.show_controls(ControlPanel::Media, surface, qh),
                (Some("notifications"), 0x110) => {
                    self.show_controls(ControlPanel::Notifications, surface, qh)
                }
                (Some("power"), 0x110) => self.show_controls(ControlPanel::Power, surface, qh),
                (Some("media"), 0x111) => Self::change_media("Previous"),
                (Some("media"), 0x112) => Self::change_media("Next"),
                (Some("network"), 0x112) => {
                    Self::set_wireless(!self.network.wireless_enabled);
                }
                (Some("bluetooth"), 0x112) => {
                    if let Some(powered) = self.bluetooth.powered {
                        Self::set_bluetooth_powered(!powered);
                    }
                }
                (Some("notifications"), 0x112) => {
                    self.do_not_disturb.fetch_xor(true, Ordering::Relaxed);
                    self.sync_notification_surface(qh);
                    self.redraw_all(qh);
                }
                _ => {}
        }
    }

    fn scroll_bar(&self, surface: &wl_surface::WlSurface, x: f64, steps: i32) {
        if steps == 0 {
            return;
        }
        if let Some(item) = self.tray_item_at(surface, x) {
            Self::scroll_tray_item(item.service, item.path, steps, "vertical");
            return;
        }
        if self.bar_module_at(surface, x) != Some("audio") {
            return;
        }
        let delta = if steps > 0 { "-5%" } else { "+5%" };
        Self::change_audio(&["set-sink-volume", "@DEFAULT_SINK@", delta]);
    }

    fn click_notification(&mut self, x: f64, y: f64, button: u32, qh: &QueueHandle<Self>) {
        if button == 0x111 {
            for notification in &self.notifications {
                let _ = self.notification_signals.send(NotificationSignal::Closed {
                    id: notification.id,
                    reason: 2,
                });
                if let Some(sender) = self.notification_events.as_ref() {
                    let _ = sender.send(NotificationEvent::Dismiss {
                        id: notification.id,
                    });
                }
            }
            self.notifications.clear();
            self.notification_offset = 0;
        } else if button == 0x110 {
            if y < 32.0 {
                return;
            }
            let row = ((y.max(32.0) as u32 - 32) / 88) as usize;
            let notification = self
                .notifications
                .iter()
                .rev()
                .nth(self.notification_offset + row)
                .cloned();
            if let Some(notification) = notification {
                let row_y = 32 + row as u32 * 88;
                let local_y = (y.max(0.0) as u32).saturating_sub(row_y);
                let action = (39..57)
                    .contains(&local_y)
                    .then(|| {
                        let mut action_x = notification_text_x(&notification);
                        notification.actions.iter().take(4).find_map(|(key, label)| {
                            let width = text_width(
                                self.font.as_ref(),
                                label,
                                self.config.theme.font_size.saturating_sub(2).max(9),
                            ) + 12;
                            let hit = x >= action_x as f64 && x < (action_x + width) as f64;
                            action_x = action_x.saturating_add(width + 12);
                            hit.then(|| key.clone())
                        })
                    })
                    .flatten();
                if let Some(action) = action {
                    let _ = self.notification_signals.send(NotificationSignal::Action {
                        id: notification.id,
                        key: action,
                    });
                }
                let _ = self.notification_signals.send(NotificationSignal::Closed {
                    id: notification.id,
                    reason: 2,
                });
                if let Some(sender) = self.notification_events.as_ref() {
                    let _ = sender.send(NotificationEvent::Dismiss {
                        id: notification.id,
                    });
                }
                self.notifications
                    .retain(|entry| entry.id != notification.id);
                self.clamp_notification_offset();
            }
        } else {
            return;
        }
        self.sync_notification_surface(qh);
        self.redraw_all(qh);
    }

    fn clamp_notification_offset(&mut self) {
        self.notification_offset = self
            .notification_offset
            .min(self.notifications.len().saturating_sub(1));
    }

    fn scroll_notifications(&mut self, steps: i32, qh: &QueueHandle<Self>) {
        if steps == 0 {
            return;
        }
        self.notification_offset = if steps < 0 {
            self.notification_offset
                .saturating_add(steps.unsigned_abs() as usize)
        } else {
            self.notification_offset.saturating_sub(steps as usize)
        }
        .min(self.notifications.len().saturating_sub(1));
        self.redraw_all(qh);
    }

    fn update_video_wallpaper_playback(&mut self) {
        if self.mode != Mode::Wallpaper || self.config.wallpaper.kind != "video" {
            return;
        }
        let suspended = video_wallpaper_is_suspended(
            &self.snapshot,
            self.config.wallpaper.pause_on_battery,
            self.on_battery,
        );
        if self.video_suspended == suspended {
            return;
        }
        self.video_suspended = suspended;
        let generation = self.video_generation.fetch_add(1, Ordering::Relaxed) + 1;
        if !suspended {
            spawn_video_wallpaper(
                self.config.wallpaper.path.clone(),
                self.config.wallpaper.fps,
                self.video_sender.clone(),
                self.video_recycled.clone(),
                self.video_generation.clone(),
                generation,
            );
        }
    }

    fn apply_config(&mut self, config: Config, qh: &QueueHandle<Self>) {
        let wallpaper_changed = self.config.wallpaper.path != config.wallpaper.path
            || self.config.wallpaper.kind != config.wallpaper.kind
            || self.config.wallpaper.outputs != config.wallpaper.outputs;
        let font_changed = self.config.theme.font != config.theme.font;
        self.config = config;
        self.do_not_disturb
            .store(self.config.shell.do_not_disturb, Ordering::Relaxed);
        self.update_video_wallpaper_playback();
        self.sync_notification_surface(qh);
        if wallpaper_changed && self.mode == Mode::Wallpaper {
            self.wallpapers = load_wallpapers(&self.config);
            if let Some(previous) = self.video_frame.take() {
                let _ = self.video_recycler.try_send(previous.into_raw());
            }
            let generation = self.video_generation.fetch_add(1, Ordering::Relaxed) + 1;
            if self.mode == Mode::Wallpaper
                && self.config.wallpaper.kind == "video"
                && !self.video_suspended
            {
                spawn_video_wallpaper(
                    self.config.wallpaper.path.clone(),
                    self.config.wallpaper.fps,
                    self.video_sender.clone(),
                    self.video_recycled.clone(),
                    self.video_generation.clone(),
                    generation,
                );
            }
        }
        if font_changed && self.mode != Mode::Wallpaper {
            self.font = load_font(&self.config);
        }
        for surface in &self.surfaces {
            if surface.kind == SurfaceKind::Bar {
                let anchor = if self.config.shell.position == "bottom" {
                    Anchor::BOTTOM
                } else {
                    Anchor::TOP
                };
                surface
                    .layer
                    .set_anchor(anchor | Anchor::LEFT | Anchor::RIGHT);
                surface.layer.set_size(0, self.config.shell.height as u32);
                surface.layer.set_exclusive_zone(self.config.shell.height);
                surface.layer.commit();
            }
        }
        self.redraw_all(qh);
    }

    fn add_surface(&mut self, qh: &QueueHandle<Self>, output: Option<wl_output::WlOutput>) {
        let kind = match self.mode {
            Mode::Bar => SurfaceKind::Bar,
            Mode::Wallpaper => SurfaceKind::Wallpaper,
            Mode::Launcher => SurfaceKind::Launcher,
            Mode::Recorder => SurfaceKind::Recorder,
        };
        self.add_kind_surface(qh, kind, output);
    }

    fn ensure_notification_surface(&mut self, qh: &QueueHandle<Self>) {
        if let Some(index) = self
            .surfaces
            .iter()
            .position(|surface| surface.kind == SurfaceKind::Notifications)
        {
            let surface = &self.surfaces[index];
            surface.layer.set_anchor(Anchor::TOP | Anchor::RIGHT);
            surface.layer.set_margin(48, 12, 0, 0);
            surface.layer.set_size(380, 320);
            surface.layer.commit();
            if surface.configured {
                self.draw(index, qh);
            }
        } else {
            self.add_kind_surface(qh, SurfaceKind::Notifications, None);
        }
    }

    fn hide_notification_surface(&mut self, qh: &QueueHandle<Self>) {
        let Some(index) = self
            .surfaces
            .iter()
            .position(|surface| surface.kind == SurfaceKind::Notifications)
        else {
            return;
        };
        let surface = &self.surfaces[index];
        surface.layer.set_size(1, 1);
        surface.layer.set_margin(0, 0, 0, 0);
        surface.layer.commit();
        if surface.configured {
            self.draw(index, qh);
        }
    }

    fn sync_notification_surface(&mut self, qh: &QueueHandle<Self>) {
        let muted = self.do_not_disturb.load(Ordering::Relaxed);
        if self
            .notifications
            .iter()
            .any(|notification| notification.critical || !muted)
        {
            self.ensure_notification_surface(qh);
        } else {
            self.hide_notification_surface(qh);
        }
    }

    fn show_controls(
        &mut self,
        panel: ControlPanel,
        source: &wl_surface::WlSurface,
        qh: &QueueHandle<Self>,
    ) {
        self.control_panel = Some(panel);
        self.pending_power_action = None;
        let output = self
            .surfaces
            .iter()
            .find(|surface| surface.layer.wl_surface() == source)
            .and_then(|surface| surface.output.clone());
        if let Some(index) = self
            .surfaces
            .iter()
            .position(|surface| surface.kind == SurfaceKind::Controls && surface.output == output)
        {
            let surface = &self.surfaces[index];
            let below = self.config.shell.position != "bottom";
            surface.layer.set_anchor(if below {
                Anchor::TOP | Anchor::RIGHT
            } else {
                Anchor::BOTTOM | Anchor::RIGHT
            });
            surface.layer.set_margin(
                if below { self.config.shell.height } else { 0 },
                12,
                if below { 0 } else { self.config.shell.height },
                0,
            );
            surface.layer.set_size(380, 300);
            surface.layer.commit();
            if surface.configured {
                self.draw(index, qh);
            }
        } else {
            self.add_kind_surface(qh, SurfaceKind::Controls, output);
        }
    }

    fn hide_controls(&mut self, qh: &QueueHandle<Self>) {
        self.control_panel = None;
        self.pending_power_action = None;
        for index in 0..self.surfaces.len() {
            if self.surfaces[index].kind == SurfaceKind::Controls {
                let surface = &self.surfaces[index];
                surface.layer.set_size(1, 1);
                surface.layer.set_margin(0, 0, 0, 0);
                surface.layer.commit();
                if surface.configured {
                    self.draw(index, qh);
                }
            }
        }
    }

    fn click_controls(&mut self, x: f64, y: f64, button: u32, qh: &QueueHandle<Self>) {
        if button != 0x110 {
            self.hide_controls(qh);
            return;
        }
        let Some(panel) = self.control_panel else {
            return;
        };
        match panel {
            ControlPanel::Audio => {
                // The volume slider track is only 6 px tall; keep the generous
                // band so it stays easy to grab.
                if (58.0..=92.0).contains(&y) {
                    let value = ((x - 24.0) / 332.0 * 100.0).round().clamp(0.0, 100.0);
                    Self::change_audio(&[
                        "set-sink-volume",
                        "@DEFAULT_SINK@",
                        &format!("{value}%"),
                    ]);
                } else if control_button(104).contains(&y) {
                    Self::change_audio(&["set-sink-mute", "@DEFAULT_SINK@", "toggle"]);
                } else if control_button(152).contains(&y) {
                    Self::spawn_desktop_tool("pavucontrol");
                }
            }
            ControlPanel::Network => {
                if control_button(56).contains(&y) {
                    Self::set_networking(!self.network.networking_enabled);
                } else if control_button(100).contains(&y) {
                    Self::set_wireless(!self.network.wireless_enabled);
                } else if control_button(152).contains(&y) {
                    Self::spawn_desktop_tool("nm-connection-editor");
                }
            }
            ControlPanel::Bluetooth => {
                if control_button(56).contains(&y) {
                    if let Some(powered) = self.bluetooth.powered {
                        Self::set_bluetooth_powered(!powered);
                    }
                } else if control_button(100).contains(&y) {
                    Self::set_bluetooth_discovery(true);
                } else if control_button(152).contains(&y) {
                    Self::spawn_desktop_tool("blueman-manager");
                }
            }
            ControlPanel::Media => {
                if (60.0..104.0).contains(&y) {
                    // Split the transport buttons at the midpoints of their
                    // gaps: the drawn columns are 18..126, 136..244, 254..362.
                    if x < 131.0 {
                        Self::change_media("Previous");
                    } else if x < 249.0 {
                        Self::change_media("PlayPause");
                    } else {
                        Self::change_media("Next");
                    }
                } else if control_button(116).contains(&y) {
                    Self::change_media("Stop");
                }
            }
            ControlPanel::Notifications => {
                if control_button(56).contains(&y) {
                    self.do_not_disturb.fetch_xor(true, Ordering::Relaxed);
                    self.sync_notification_surface(qh);
                } else if control_button(100).contains(&y) {
                    for notification in &self.notifications {
                        let _ = self.notification_signals.send(NotificationSignal::Closed {
                            id: notification.id,
                            reason: 2,
                        });
                    }
                    self.notifications.clear();
                    self.notification_offset = 0;
                    self.sync_notification_surface(qh);
                }
            }
            ControlPanel::Power => {
                if let Some(action) = self.pending_power_action {
                    if control_button(76).contains(&y) {
                        self.hide_controls(qh);
                        Self::perform_power_action(action);
                        return;
                    }
                    if control_button(120).contains(&y) {
                        self.pending_power_action = None;
                    }
                } else if control_button(56).contains(&y) {
                    self.pending_power_action = Some(PowerAction::LogOut);
                } else if control_button(100).contains(&y) {
                    self.pending_power_action = Some(PowerAction::Reboot);
                } else if control_button(144).contains(&y) {
                    self.pending_power_action = Some(PowerAction::Shutdown);
                }
            }
        }
        self.redraw_all(qh);
    }

    fn perform_power_action(action: PowerAction) {
        match action {
            PowerAction::LogOut => Self::run_command("quit".into()),
            PowerAction::Reboot => {
                thread::spawn(|| {
                    let _ = std::process::Command::new("systemctl")
                        .arg("reboot")
                        .status();
                });
            }
            PowerAction::Shutdown => {
                thread::spawn(|| {
                    let _ = std::process::Command::new("systemctl")
                        .arg("poweroff")
                        .status();
                });
            }
        }
    }

    fn add_kind_surface(
        &mut self,
        qh: &QueueHandle<Self>,
        kind: SurfaceKind,
        output: Option<wl_output::WlOutput>,
    ) {
        if output.as_ref().is_some_and(|candidate| {
            self.surfaces
                .iter()
                .any(|surface| surface.kind == kind && surface.output.as_ref() == Some(candidate))
        }) {
            return;
        }
        let surface = self.compositor.create_surface(qh);
        let (layer_kind, namespace) = match kind {
            SurfaceKind::Bar => (Layer::Top, "wm-bar"),
            SurfaceKind::Wallpaper => (Layer::Background, "wm-wallpaper"),
            SurfaceKind::Launcher => (Layer::Overlay, "wm-launcher"),
            SurfaceKind::Recorder => (Layer::Overlay, "wm-recorder"),
            SurfaceKind::Notifications => (Layer::Overlay, "wm-notifications"),
            SurfaceKind::TrayMenu => (Layer::Overlay, "wm-tray-menu"),
            SurfaceKind::Controls => (Layer::Overlay, "wm-controls"),
        };
        let layer = self.layer_shell.create_layer_surface(
            qh,
            surface,
            layer_kind,
            Some(namespace),
            output.as_ref(),
        );
        match kind {
            SurfaceKind::Bar => {
                let anchor = if self.config.shell.position == "bottom" {
                    Anchor::BOTTOM
                } else {
                    Anchor::TOP
                };
                layer.set_anchor(anchor | Anchor::LEFT | Anchor::RIGHT);
                layer.set_size(0, self.config.shell.height as u32);
                layer.set_exclusive_zone(self.config.shell.height);
                layer.set_keyboard_interactivity(KeyboardInteractivity::None);
            }
            SurfaceKind::Wallpaper => {
                layer.set_anchor(Anchor::TOP | Anchor::BOTTOM | Anchor::LEFT | Anchor::RIGHT);
                layer.set_size(0, 0);
                layer.set_exclusive_zone(-1);
                layer.set_keyboard_interactivity(KeyboardInteractivity::None);
            }
            SurfaceKind::Launcher => {
                // A layer with no opposing anchors is centered by layer-shell.
                // Keeping this surface exactly panel-sized confines the compositor
                // backdrop effect to one continuous region.
                layer.set_anchor(Anchor::empty());
                layer.set_size(680, 420);
                layer.set_keyboard_interactivity(KeyboardInteractivity::Exclusive);
            }
            SurfaceKind::Recorder => {
                layer.set_anchor(Anchor::empty());
                layer.set_size(720, 500);
                layer.set_keyboard_interactivity(KeyboardInteractivity::Exclusive);
            }
            SurfaceKind::Notifications => {
                layer.set_anchor(Anchor::TOP | Anchor::RIGHT);
                layer.set_margin(48, 12, 0, 0);
                layer.set_size(380, 320);
                layer.set_keyboard_interactivity(KeyboardInteractivity::None);
            }
            SurfaceKind::TrayMenu => {
                layer.set_anchor(Anchor::TOP | Anchor::LEFT);
                layer.set_size(1, 1);
                layer.set_exclusive_zone(-1);
                layer.set_keyboard_interactivity(KeyboardInteractivity::None);
            }
            SurfaceKind::Controls => {
                let below = self.config.shell.position != "bottom";
                layer.set_anchor(if below {
                    Anchor::TOP | Anchor::RIGHT
                } else {
                    Anchor::BOTTOM | Anchor::RIGHT
                });
                layer.set_margin(
                    if below { self.config.shell.height } else { 0 },
                    12,
                    if below { 0 } else { self.config.shell.height },
                    0,
                );
                layer.set_size(380, 300);
                layer.set_keyboard_interactivity(KeyboardInteractivity::None);
            }
        }
        layer.commit();
        self.surfaces.push(Surface {
            kind,
            layer,
            pool: SlotPool::new(4 * 1024 * 1024, &self.shm).expect("allocate shell SHM pool"),
            pool_size: 4 * 1024 * 1024,
            width: 1,
            height: 1,
            configured: false,
            frame_pending: false,
            redraw_requested: false,
            output,
        });
    }

    fn redraw_all(&mut self, qh: &QueueHandle<Self>) {
        for index in 0..self.surfaces.len() {
            if self.surfaces[index].configured {
                if self.surfaces[index].frame_pending {
                    self.surfaces[index].redraw_requested = true;
                } else {
                    self.draw(index, qh);
                }
            }
        }
    }

    fn draw(&mut self, index: usize, qh: &QueueHandle<Self>) {
        if self.surfaces[index].frame_pending {
            self.surfaces[index].redraw_requested = true;
            return;
        }
        let colors = Colors::from_config(&self.config);
        let output_name = self.surfaces[index]
            .output
            .as_ref()
            .and_then(|output| self.output_state.info(output))
            .and_then(|info| info.name);
        let active_workspace = output_name
            .as_deref()
            .and_then(|name| self.snapshot.outputs.iter().find(|output| output.name == name))
            .map(|output| output.workspace)
            .unwrap_or(1);
        let title = visible_bar_title(&self.snapshot, active_workspace, output_name.as_deref());
        let font = self.font.clone();
        let font_size = self.config.theme.font_size;
        let modules = self.config.shell.modules.clone();
        let workspaces = self.config.layout.workspaces;
        let kind = self.surfaces[index].kind;
        let launcher_rows = if kind == SurfaceKind::Launcher {
            self.launcher_rows()
        } else {
            Vec::new()
        };
        let clock = modules
            .iter()
            .any(|module| module == "clock")
            .then(|| self.clock.clone());
        let battery = modules
            .iter()
            .any(|module| module == "battery")
            .then(|| self.battery.clone())
            .flatten();
        let audio = modules
            .iter()
            .any(|module| module == "audio")
            .then(|| self.audio.label.as_str());
        let network = modules
            .iter()
            .any(|module| module == "network")
            .then(|| compact_network_bar_label(&self.network.label));
        let media = (modules.iter().any(|module| module == "media")
            && self.media.label != "MEDIA —")
            .then_some("MEDIA");
        let bluetooth = modules
            .iter()
            .any(|module| module == "bluetooth")
            .then(|| self.bluetooth.label.as_str());
        let tray = (modules.iter().any(|module| module == "tray")
            && self.tray.iter().any(TrayItem::has_visible_icon))
            .then(|| format!("TRAY {}", self.tray.len()));
        let notifications = modules
            .iter()
            .any(|module| module == "notifications")
            .then(|| self.notifications_bar_label())
            .flatten();
        let power = modules
            .iter()
            .any(|module| module == "power")
            .then_some("POWER");
        let recorder = self.recorder_bar_label();
        let notification_rows = self
            .notifications
            .iter()
            .rev()
            .filter(|notification| {
                !self.do_not_disturb.load(Ordering::Relaxed) || notification.critical
            })
            .skip(self.notification_offset)
            .take(3)
            .cloned()
            .collect::<Vec<_>>();
        let tray_menu = (kind == SurfaceKind::TrayMenu)
            .then(|| self.tray_menu.clone())
            .flatten();
        let controls = (kind == SurfaceKind::Controls)
            .then_some(self.control_panel)
            .flatten();
        let wallpaper_output_name = self.surfaces[index]
            .output
            .as_ref()
            .and_then(|output| self.output_state.info(output))
            .and_then(|info| info.name);
        let wallpaper = if self.config.wallpaper.kind == "video" {
            self.video_frame.as_ref()
        } else {
            wallpaper_output_name
                .as_ref()
                .and_then(|name| self.wallpapers.get(name))
                .or_else(|| self.wallpapers.get(""))
        };
        let recorder_selection_label = (kind == SurfaceKind::Recorder)
            .then(|| self.recorder_selection_label())
            .unwrap_or_default();
        let recorder_selection_heading = (kind == SurfaceKind::Recorder)
            .then(|| self.recorder_selection_heading())
            .unwrap_or_default();
        let recorder_capture_label = (kind == SurfaceKind::Recorder)
            .then(|| self.recorder_capture_label())
            .unwrap_or_default();
        let recorder_capture_note = (kind == SurfaceKind::Recorder)
            .then(|| self.recorder_capture_note())
            .unwrap_or_default();
        let recorder_start_blocker = (kind == SurfaceKind::Recorder)
            .then(|| self.recorder_start_blocker())
            .flatten();
        let recorder_display_fps = (kind == SurfaceKind::Recorder)
            .then(|| self.recorder_display_fps())
            .unwrap_or(self.snapshot.recorder_settings.screen_fps);
        let surface = &mut self.surfaces[index];
        let width = surface.width;
        let height = surface.height;
        let stride = width as i32 * 4;
        let (buffer, canvas) = match surface.pool.create_buffer(
            width as i32,
            height as i32,
            stride,
            wl_shm::Format::Argb8888,
        ) {
            Ok(buffer) => buffer,
            Err(error) => {
                eprintln!("wm-shell-sctk: allocate frame: {error}");
                return;
            }
        };
        match kind {
            SurfaceKind::Bar => draw_bar(
                canvas,
                width,
                height,
                colors,
                active_workspace,
                &title,
                font.as_ref(),
                font_size,
                &modules,
                workspaces,
                clock.as_deref(),
                battery.as_deref(),
                audio,
                network,
                media,
                bluetooth,
                &self.tray,
                tray.as_deref(),
                notifications.as_deref(),
                recorder.as_deref(),
                power,
            ),
            SurfaceKind::Wallpaper => draw_wallpaper(
                canvas,
                width,
                height,
                colors,
                wallpaper,
                &self.config.wallpaper.fit,
            ),
            SurfaceKind::Launcher => draw_launcher(
                canvas,
                width,
                height,
                colors,
                self.config.theme.radius.round().clamp(0.0, 100.0) as u32,
                font.as_ref(),
                font_size,
                &self.launcher_query,
                &launcher_rows,
                self.launcher_selected,
                self.apps_loaded,
            ),
            SurfaceKind::Recorder => draw_recorder(
                canvas,
                width,
                height,
                colors,
                self.config.theme.radius.round().clamp(0.0, 100.0) as u32,
                font.as_ref(),
                font_size,
                &self.snapshot,
                &self.snapshot.recorder_settings,
                self.recorder_view,
                self.recorder_settings_tab,
                self.recorder_settings_row,
                self.recorder_path_edit.as_deref(),
                self.recorder_feedback.as_ref(),
                recorder_start_blocker,
                &recorder_capture_label,
                recorder_selection_heading,
                &recorder_selection_label,
                &recorder_capture_note,
                recorder_display_fps,
            ),
            SurfaceKind::Notifications => draw_notifications(
                canvas,
                width,
                height,
                colors,
                font.as_ref(),
                font_size,
                &notification_rows,
                self.notifications.len(),
                self.notification_offset,
                self.do_not_disturb.load(Ordering::Relaxed),
            ),
            SurfaceKind::TrayMenu => draw_tray_menu(
                canvas,
                width,
                height,
                colors,
                font.as_ref(),
                font_size,
                tray_menu.as_ref(),
            ),
            SurfaceKind::Controls => draw_controls(
                canvas,
                width,
                height,
                colors,
                font.as_ref(),
                font_size,
                controls,
                self.pending_power_action,
                &self.audio,
                &self.network,
                &self.media,
                &self.bluetooth,
                self.do_not_disturb.load(Ordering::Relaxed),
                self.notifications.len(),
            ),
        }
        surface
            .layer
            .wl_surface()
            .damage_buffer(0, 0, width as i32, height as i32);
        if buffer.attach_to(surface.layer.wl_surface()).is_ok() {
            surface
                .layer
                .wl_surface()
                .frame(qh, surface.layer.wl_surface().clone());
            surface.layer.commit();
            surface.frame_pending = true;
            surface.redraw_requested = false;
        }
    }
}

#[derive(Clone, Copy)]
struct Colors {
    background: u32,
    foreground: u32,
    accent: u32,
    muted: u32,
    radius: u32,
}

impl Colors {
    fn from_config(config: &Config) -> Self {
        let rgba = |value: &str, alpha: f32| {
            let [red, green, blue, _] = wm_core::color(value).unwrap_or([0.0, 0.0, 0.0, 1.0]);
            let alpha = (alpha.clamp(0.0, 1.0) * 255.0).round() as u32;
            let channel = |value: f32| ((value * alpha as f32).round() as u32).min(255);
            alpha << 24 | channel(red) << 16 | channel(green) << 8 | channel(blue)
        };
        Self {
            background: rgba(&config.theme.background, config.theme.opacity),
            foreground: rgba(&config.theme.foreground, 1.0),
            accent: rgba(&config.theme.accent, 1.0),
            muted: rgba(&config.theme.muted, 1.0),
            radius: config.theme.radius.max(0.0).round() as u32,
        }
    }

    /// Alpha-scale a premultiplied ARGB color; every channel scales together
    /// so the result stays valid premultiplied-alpha.
    fn scaled(color: u32, alpha: f32) -> u32 {
        let channel = |shift: u32| {
            (((color >> shift) & 0xff) as f32 * alpha)
                .round()
                .min(255.0) as u32
        };
        channel(24) << 24 | channel(16) << 16 | channel(8) << 8 | channel(0)
    }
}

fn draw_wallpaper(
    canvas: &mut [u8],
    width: u32,
    height: u32,
    colors: Colors,
    image: Option<&image::RgbaImage>,
    fit: &str,
) {
    if let Some(image) = image {
        draw_image(canvas, width, height, image, fit);
        return;
    }
    fill(canvas, colors.background);
    // A very cheap two-tone gradient supplies a pleasant fallback when no
    // static image or decoded video frame is available.
    for y in 0..height {
        let amount = (y * 24 / height.max(1)) as u32;
        for x in 0..width {
            let index = ((y * width + x) * 4) as usize;
            let base = colors.background & 0x00ff_ffff;
            let r = ((base >> 16) & 0xff).saturating_add(amount);
            let g = ((base >> 8) & 0xff).saturating_add(amount / 2);
            let b = (base & 0xff).saturating_add(amount);
            canvas[index..index + 4]
                .copy_from_slice(&(0xff00_0000 | r << 16 | g << 8 | b).to_le_bytes());
        }
    }
}

fn draw_image(canvas: &mut [u8], width: u32, height: u32, image: &image::RgbaImage, fit: &str) {
    let source_w = image.width().max(1);
    let source_h = image.height().max(1);
    let cover = fit != "contain";
    let scale = if cover {
        (width as f64 / source_w as f64).max(height as f64 / source_h as f64)
    } else {
        (width as f64 / source_w as f64).min(height as f64 / source_h as f64)
    };
    let draw_w = (source_w as f64 * scale).round().max(1.0) as u32;
    let draw_h = (source_h as f64 * scale).round().max(1.0) as u32;
    let offset_x = width.saturating_sub(draw_w) / 2;
    let offset_y = height.saturating_sub(draw_h) / 2;
    fill(canvas, 0xff00_0000);
    for y in 0..height {
        for x in 0..width {
            if !cover
                && (x < offset_x
                    || y < offset_y
                    || x >= offset_x + draw_w
                    || y >= offset_y + draw_h)
            {
                continue;
            }
            let local_x = if cover {
                x + (draw_w - width) / 2
            } else {
                x - offset_x
            };
            let local_y = if cover {
                y + (draw_h - height) / 2
            } else {
                y - offset_y
            };
            let source_x = (u64::from(local_x) * u64::from(source_w) / u64::from(draw_w)) as u32;
            let source_y = (u64::from(local_y) * u64::from(source_h) / u64::from(draw_h)) as u32;
            let [red, green, blue, alpha] = image.get_pixel(source_x, source_y).0;
            let pixel = u32::from(alpha) << 24
                | u32::from(red) << 16
                | u32::from(green) << 8
                | u32::from(blue);
            let index = ((y * width + x) * 4) as usize;
            canvas[index..index + 4].copy_from_slice(&pixel.to_le_bytes());
        }
    }
}

fn clock_label() -> String {
    chrono::Local::now().format("%a %H:%M").to_string()
}

fn expire_notifications(app: &mut App, now: Instant) -> bool {
    let before = app.notifications.len();
    let expired = app
        .notifications
        .iter()
        .filter(|notification| notification.expires_at.is_some_and(|expiry| expiry <= now))
        .map(|notification| notification.id)
        .collect::<Vec<_>>();
    app.notifications
        .retain(|notification| notification.expires_at.is_none_or(|expiry| expiry > now));
    for id in expired {
        let _ = app
            .notification_signals
            .send(NotificationSignal::Closed { id, reason: 1 });
    }
    app.notifications.len() != before
}

fn next_bar_maintenance_delay(app: &App, now: Instant) -> Duration {
    use chrono::Timelike;

    let until_clock = Duration::from_secs(u64::from(60 - chrono::Local::now().second()).max(1));
    let until_battery = app.next_battery_refresh.saturating_duration_since(now);
    let until_notification = app
        .notifications
        .iter()
        .filter_map(|notification| notification.expires_at)
        .map(|expiry| expiry.saturating_duration_since(now))
        .min();

    [until_clock, until_battery]
        .into_iter()
        .chain(until_notification)
        .min()
        .unwrap_or(Duration::from_secs(30))
        .max(Duration::from_millis(1))
}

fn video_wallpaper_is_suspended(
    snapshot: &Snapshot,
    pause_on_battery: bool,
    on_battery: bool,
) -> bool {
    snapshot.windows.iter().any(|window| window.fullscreen) || (pause_on_battery && on_battery)
}

fn bar_snapshot_changed(previous: &Snapshot, current: &Snapshot) -> bool {
    if focused_bar_identity(previous) != focused_bar_identity(current)
        || previous.outputs.len() != current.outputs.len()
    {
        return true;
    }
    previous.outputs.iter().any(|old| {
        current
            .outputs
            .iter()
            .find(|new| new.name == old.name)
            .is_none_or(|new| new.workspace != old.workspace || new.active != old.active)
    })
}

fn focused_bar_identity(snapshot: &Snapshot) -> Option<(u64, &str, &str)> {
    snapshot.focused.and_then(|id| {
        snapshot
            .windows
            .iter()
            .find(|window| window.id == id)
            .map(|window| (id, window.title.as_str(), window.app_id.as_str()))
    })
}

fn visible_bar_title(snapshot: &Snapshot, workspace: u8, output: Option<&str>) -> String {
    snapshot
        .focused
        .and_then(|id| snapshot.windows.iter().find(|window| window.id == id))
        .filter(|window| {
            window.workspace == workspace && output.is_none_or(|name| window.output == name)
        })
        .map(|window| bar_label_text(&window.title, 128))
        .filter(|title| !title.is_empty())
        .unwrap_or_else(|| "Luma".into())
}

fn battery_state() -> (Option<String>, bool) {
    let Ok(supplies) = std::fs::read_dir("/sys/class/power_supply") else {
        return (None, false);
    };
    for supply in supplies.flatten() {
        let path = supply.path();
        if std::fs::read_to_string(path.join("type"))
            .ok()
            .is_none_or(|kind| kind.trim() != "Battery")
        {
            continue;
        }
        let Some(capacity) = std::fs::read_to_string(path.join("capacity"))
            .ok()
            .and_then(|value| value.trim().parse::<u8>().ok())
        else {
            continue;
        };
        let status = std::fs::read_to_string(path.join("status")).unwrap_or_default();
        let symbol = if status.trim() == "Charging" { "+" } else { "" };
        return (
            Some(format!("BAT {capacity}%{symbol}")),
            status.trim() == "Discharging",
        );
    }
    (None, false)
}

fn draw_bar(
    canvas: &mut [u8],
    width: u32,
    height: u32,
    colors: Colors,
    active_workspace: u8,
    title: &str,
    font: Option<&FontArc>,
    font_size: u32,
    modules: &[String],
    workspace_count: u8,
    clock: Option<&str>,
    battery: Option<&str>,
    audio: Option<&str>,
    network: Option<&str>,
    media: Option<&str>,
    bluetooth: Option<&str>,
    tray_items: &[TrayItem],
    tray: Option<&str>,
    notifications: Option<&str>,
    recorder: Option<&str>,
    power: Option<&str>,
) {
    fill(canvas, 0);
    let island_y = 4.min(height.saturating_sub(1) / 2);
    let island_h = height.saturating_sub(island_y * 2);
    let island_radius = colors.radius.min(island_h / 2);
    let has = |name| modules.iter().any(|module| module == name);
    let workspace_end = if has("workspaces") {
        BAR_WORKSPACE_START + BAR_WORKSPACE_STEP * u32::from(workspace_count)
    } else {
        BAR_WORKSPACE_START
    };
    let left_end = workspace_end.saturating_add(8);
    panel(
        canvas,
        width,
        4,
        island_y,
        left_end.saturating_sub(4),
        island_h,
        island_radius,
        colors,
    );
    let logo_y = height.saturating_sub(24) / 2;
    rounded_rect(canvas, width, 10, logo_y, 26, 24, 8, colors.accent);
    text(
        canvas,
        width,
        font,
        19,
        logo_y + 17,
        "L",
        14,
        colors.background | 0xff00_0000,
        16,
    );
    if has("workspaces") {
        for workspace in 1..=workspace_count {
            let x = BAR_WORKSPACE_START + (u32::from(workspace) - 1) * BAR_WORKSPACE_STEP;
            let active = workspace == active_workspace;
            rounded_rect(
                canvas,
                width,
                x,
                height.saturating_sub(22) / 2,
                22,
                22,
                8,
                if active {
                    colors.accent
                } else {
                    Colors::scaled(colors.muted, 0.16)
                },
            );
            let numeral = workspace.to_string();
            let numeral_width = text_width(font, &numeral, 12);
            let numeral_x = x + 11u32.saturating_sub(numeral_width / 2);
            text(
                canvas,
                width,
                font,
                numeral_x,
                height / 2 + 5,
                &numeral,
                12,
                if active {
                    colors.background | 0xff00_0000
                } else {
                    colors.muted
                },
                18,
            );
            if active {
                rect(canvas, width, x + 7, island_y + island_h - 3, 8, 2, colors.accent);
            }
        }
    }

    // Status chips are independently bounded so their painted and clickable
    // areas agree. This also leaves real transparency between the three bar
    // islands instead of blurring a full-width opaque strip.
    let mut values: Vec<(&'static str, String)> = Vec::new();
    for (module, value) in [
        ("battery", battery),
        ("media", media),
        ("bluetooth", bluetooth),
        ("tray", tray),
        ("notifications", notifications),
        ("network", network),
        ("audio", audio),
        ("clock", clock),
        ("recorder", recorder),
        ("power", power),
    ] {
        if let Some(value) = value {
            values.push((module, value.to_owned()));
        }
    }
    let tray_count = tray_items
        .iter()
        .filter(|item| item.has_visible_icon())
        .take(6)
        .count();
    let module_rects = bar_module_layout(
        width,
        height,
        font,
        font_size,
        tray_count,
        left_end.saturating_add(8),
        &values,
    );
    let right_start = module_rects
        .iter()
        .map(|(_, left, _)| *left)
        .min()
        .unwrap_or(width.saturating_sub(BAR_RIGHT_INSET));

    if has("title") && !title.is_empty() {
        let gap_start = left_end.saturating_add(12);
        let gap_width = right_start.saturating_sub(gap_start + 12);
        if gap_width >= 120 {
            let title_w = (text_width(font, title, font_size) + 44)
                .clamp(160, 560)
                .min(gap_width);
            let title_x = gap_start + (gap_width - title_w) / 2;
            panel(
                canvas,
                width,
                title_x,
                island_y,
                title_w,
                island_h,
                island_radius,
                colors,
            );
            rounded_rect(
                canvas,
                width,
                title_x + 12,
                height.saturating_sub(6) / 2,
                6,
                6,
                3,
                colors.accent,
            );
            title_text(
                canvas,
                width,
                title_x + 26,
                height / 2 + font_size.min(16) / 2,
                title,
                title_w.saturating_sub(38) as usize,
                colors.foreground,
                font,
                font_size,
            );
        }
    }

    for (module, left, right) in module_rects {
        let Some((_, value)) = values.iter().find(|(name, _)| *name == module) else {
            continue;
        };
        let chip_w = right.saturating_sub(left);
        let active = value == "DND" || value.starts_with("REC") || value == "REPLAY";
        rounded_rect(
            canvas,
            width,
            left,
            island_y,
            chip_w,
            island_h,
            island_radius,
            if active {
                Colors::scaled(colors.accent, 0.20)
            } else {
                colors.background
            },
        );
        rounded_rect_outline(
            canvas,
            width,
            left,
            island_y,
            chip_w,
            island_h,
            island_radius,
            Colors::scaled(colors.accent, if active { 0.45 } else { 0.18 }),
        );
        if module == "tray" && tray_count > 0 {
            draw_tray_icons(canvas, width, height, left, tray_items);
        } else {
            text(
                canvas,
                width,
                font,
                left + 10,
                height / 2 + font_size.min(16) / 2,
                value,
                font_size,
                if active || module == "clock" {
                    colors.foreground
                } else {
                    colors.muted
                },
                chip_w.saturating_sub(20),
            );
        }
    }
}

fn draw_tray_icons(canvas: &mut [u8], canvas_width: u32, height: u32, x: u32, items: &[TrayItem]) {
    let icon_size = height.saturating_sub(12).clamp(12, 24);
    let mut icon_x = x.saturating_add(4);
    for item in items
        .iter()
        .filter(|item| item.visible)
        .filter_map(|item| item.icon.as_ref())
        .take(6)
    {
        if icon_x.saturating_add(icon_size) > canvas_width.saturating_sub(4) {
            break;
        }
        draw_icon(
            canvas,
            canvas_width,
            icon_x,
            height.saturating_sub(icon_size) / 2,
            icon_size,
            item,
        );
        icon_x = icon_x.saturating_add(icon_size + 2);
    }
}

fn draw_icon(
    canvas: &mut [u8],
    canvas_width: u32,
    x: u32,
    y: u32,
    size: u32,
    icon: &image::RgbaImage,
) {
    let canvas_height = (canvas.len() / 4 / canvas_width.max(1) as usize) as u32;
    let source_width = icon.width().max(1);
    let source_height = icon.height().max(1);
    let scale = (size as f64 / source_width as f64).min(size as f64 / source_height as f64);
    let draw_width = (source_width as f64 * scale).round().max(1.0) as u32;
    let draw_height = (source_height as f64 * scale).round().max(1.0) as u32;
    let offset_x = (size.saturating_sub(draw_width)) / 2;
    let offset_y = (size.saturating_sub(draw_height)) / 2;
    for row in 0..draw_height {
        for column in 0..draw_width {
            let target_x = x.saturating_add(offset_x).saturating_add(column);
            let target_y = y.saturating_add(offset_y).saturating_add(row);
            if target_x >= canvas_width || target_y >= canvas_height {
                continue;
            }
            let source_x = column * source_width / draw_width;
            let source_y = row * source_height / draw_height;
            let [red, green, blue, alpha] = icon.get_pixel(source_x, source_y).0;
            if alpha == 0 {
                continue;
            }
            let index = ((target_y * canvas_width + target_x) * 4) as usize;
            blend_rgba_pixel(canvas, index, red, green, blue, alpha);
        }
    }
}

fn blend_rgba_pixel(canvas: &mut [u8], index: usize, red: u8, green: u8, blue: u8, alpha: u8) {
    let destination = u32::from_le_bytes(canvas[index..index + 4].try_into().unwrap());
    let destination_alpha = (destination >> 24) & 0xff;
    let inverse_alpha = 255 - u32::from(alpha);
    let source = |channel: u8| u32::from(channel) * u32::from(alpha) / 255;
    let blend = |source: u32, destination: u32| source + destination * inverse_alpha / 255;
    let output = (blend(source(red), (destination >> 16) & 0xff).min(255) << 16)
        | (blend(source(green), (destination >> 8) & 0xff).min(255) << 8)
        | blend(source(blue), destination & 0xff).min(255)
        | (blend(u32::from(alpha), destination_alpha).min(255) << 24);
    canvas[index..index + 4].copy_from_slice(&output.to_le_bytes());
}

fn draw_launcher(
    canvas: &mut [u8],
    width: u32,
    height: u32,
    colors: Colors,
    radius: u32,
    font: Option<&FontArc>,
    font_size: u32,
    query: &str,
    rows: &[String],
    selected: usize,
    apps_loaded: bool,
) {
    fill(canvas, 0);
    let [x, y, panel_w, panel_h] = launcher_panel_rect(width, height);
    panel(canvas, width, x, y, panel_w, panel_h, radius, colors);
    let kicker_size = font_size.saturating_sub(2).max(11);
    let title_size = font_size.saturating_add(7).clamp(19, 22);
    let entry_size = font_size.saturating_add(2).clamp(15, 18);
    let chip_text_size = font_size.saturating_sub(3).max(10);
    let meta_size = font_size.saturating_sub(2).max(11);
    let row_text_size = font_size.saturating_add(3).clamp(15, 18);
    let hint_size = font_size.saturating_sub(3).max(10);
    let rendered_width = |value: &str, size: u32| {
        if font.is_some() {
            text_width(font, value, size)
        } else {
            value.len().saturating_mul(6).min(u32::MAX as usize) as u32
        }
    };

    // Keep dynamic labels inside their allocated columns. The cutoff follows
    // UTF-8 character boundaries, and the small reserve covers glyph bearings.
    let fit_text = |value: &str, size: u32, max_width: u32| -> String {
        let available = max_width.saturating_sub(3);
        if rendered_width(value, size) <= available {
            return value.to_string();
        }
        let ellipsis = "...";
        let ellipsis_width = rendered_width(ellipsis, size);
        if ellipsis_width > available {
            return String::new();
        }
        let content_width = available.saturating_sub(ellipsis_width);
        let Some(font) = font else {
            let content_bytes = (content_width / 6) as usize;
            let end_byte = value
                .char_indices()
                .map(|(byte_index, character)| byte_index + character.len_utf8())
                .take_while(|end_byte| *end_byte <= content_bytes)
                .last()
                .unwrap_or(0);
            return format!("{}{}", &value[..end_byte], ellipsis);
        };
        let scaled = font.as_scaled(PxScale::from(size.max(8) as f32));
        let mut measured = 0.0;
        let mut previous = None;
        let mut end_byte = 0;
        for (byte_index, character) in value.char_indices() {
            let glyph = font.glyph_id(character);
            let kerning = previous.map_or(0.0, |previous| scaled.kern(previous, glyph));
            let next = measured + kerning + scaled.h_advance(glyph);
            if next.ceil() > content_width as f32 {
                break;
            }
            measured = next;
            previous = Some(glyph);
            end_byte = byte_index + character.len_utf8();
        }
        format!("{}{}", &value[..end_byte], ellipsis)
    };

    // The panel keeps the configured theme and accent outline, while this
    // inset pass makes the surface read as a solid command palette over a
    // bright wallpaper without changing any shell-wide opacity settings.
    rounded_rect(
        canvas,
        width,
        x.saturating_add(1),
        y.saturating_add(1),
        panel_w.saturating_sub(2),
        panel_h.saturating_sub(2),
        radius.saturating_sub(1),
        Colors::scaled(colors.background, 0.75),
    );

    // A small geometric L mark keeps the header recognizable without loading
    // an icon or adding any work to the launcher's startup path.
    let mark_x = x + 25;
    let mark_y = y + 24;
    rounded_rect(canvas, width, mark_x, mark_y, 28, 28, 8, colors.accent);
    rect(
        canvas,
        width,
        mark_x + 8,
        mark_y + 6,
        3,
        15,
        colors.background,
    );
    rect(
        canvas,
        width,
        mark_x + 8,
        mark_y + 18,
        12,
        3,
        colors.background,
    );

    text(
        canvas,
        width,
        font,
        x + 65,
        y + 37,
        "QUICK ACCESS",
        kicker_size,
        colors.muted,
        panel_w.saturating_sub(200),
    );

    text(
        canvas,
        width,
        font,
        x + 65,
        y + 61,
        "Search everything",
        title_size,
        colors.foreground,
        panel_w.saturating_sub(200),
    );

    let shortcut_x = x + panel_w.saturating_sub(132);
    let shortcut_y = y + 25;
    rounded_rect(
        canvas,
        width,
        shortcut_x,
        shortcut_y,
        106,
        25,
        7,
        Colors::scaled(colors.accent, 0.10),
    );
    rounded_rect_outline(
        canvas,
        width,
        shortcut_x,
        shortcut_y,
        106,
        25,
        7,
        Colors::scaled(colors.accent, 0.28),
    );
    let shortcut_size = font_size.saturating_sub(2).max(10);
    let shortcut_text = fit_text("SUPER + SPACE", shortcut_size, 88);
    text(
        canvas,
        width,
        font,
        shortcut_x + 10,
        shortcut_y + 16,
        &shortcut_text,
        shortcut_size,
        colors.accent,
        88,
    );

    // Search well: the light olive tint provides an inset edge against the
    // charcoal panel, with a tiny hand-drawn magnifier as its focus marker.
    let entry_x = x + 24;
    let entry_y = y + 78;
    let entry_w = panel_w.saturating_sub(48);
    let entry_h = 48;
    rounded_rect(
        canvas,
        width,
        entry_x,
        entry_y,
        entry_w,
        entry_h,
        11,
        Colors::scaled(colors.muted, 0.10),
    );
    rounded_rect_outline(
        canvas,
        width,
        entry_x,
        entry_y,
        entry_w,
        entry_h,
        11,
        Colors::scaled(colors.muted, 0.30),
    );
    rounded_rect_outline(
        canvas,
        width,
        entry_x + 17,
        entry_y + 15,
        12,
        12,
        6,
        colors.accent,
    );
    rect(
        canvas,
        width,
        entry_x + 27,
        entry_y + 26,
        6,
        2,
        colors.accent,
    );
    let prompt = if query.is_empty() {
        "Search apps, windows, commands, and power"
    } else {
        query
    };
    let baseline = entry_y + 32;
    let display_query = if query.is_empty() {
        prompt.to_string()
    } else {
        fit_text(query, entry_size, entry_w.saturating_sub(58))
    };
    text(
        canvas,
        width,
        font,
        entry_x + 42,
        baseline,
        &display_query,
        entry_size,
        if query.is_empty() {
            colors.muted
        } else {
            colors.foreground
        },
        entry_w.saturating_sub(58),
    );
    if !query.is_empty() {
        // The launcher redraws on every keypress, so a steady caret needs no
        // frame loop for blinking.
        let caret_x = (entry_x + 42 + rendered_width(&display_query, entry_size))
            .min(entry_x + entry_w.saturating_sub(18));
        let caret_y = baseline.saturating_sub(18);
        rect(canvas, width, caret_x, caret_y, 2, 22, colors.accent);
    }

    // Search modes are discoverable without spending space in the input
    // placeholder or requiring the user to guess each prefix.
    let category_y = y + LAUNCHER_CATEGORY_TOP;
    text(
        canvas,
        width,
        font,
        x + 24,
        category_y + 14,
        &fit_text("SEARCH IN", chip_text_size, 62),
        chip_text_size,
        colors.muted,
        62,
    );
    let active_category = launcher_category_for_query(query);
    for (category, label, chip) in launcher_category_regions(width, height) {
        let (category_x, category_y, chip_w, chip_h) = (chip[0], chip[1], chip[2], chip[3]);
        let active = category == active_category;
        rounded_rect(
            canvas,
            width,
            category_x,
            category_y,
            chip_w,
            chip_h,
            7,
            Colors::scaled(
                if active { colors.accent } else { colors.muted },
                if active { 0.16 } else { 0.10 },
            ),
        );
        if active {
            rounded_rect_outline(
                canvas,
                width,
                category_x,
                category_y,
                chip_w,
                chip_h,
                7,
                Colors::scaled(colors.accent, 0.32),
            );
        }
        text(
            canvas,
            width,
            font,
            category_x + 8,
            category_y + 14,
            &fit_text(label, chip_text_size, chip_w.saturating_sub(16)),
            chip_text_size,
            if active { colors.accent } else { colors.muted },
            chip_w.saturating_sub(16),
        );
    }

    text(
        canvas,
        width,
        font,
        x + 25,
        y + 178,
        "MATCHES",
        meta_size,
        colors.accent,
        160,
    );
    let result_count = format!("{} ITEMS", rows.len());
    let result_count_width = rendered_width(&result_count, meta_size);
    text(
        canvas,
        width,
        font,
        x + panel_w.saturating_sub(25 + result_count_width),
        y + 178,
        &result_count,
        meta_size,
        colors.muted,
        result_count_width,
    );

    let list_top = y + LAUNCHER_RESULTS_TOP;
    if rows.is_empty() {
        let message = if apps_loaded || !query.is_empty() {
            "No matching results"
        } else {
            "Loading apps…"
        };
        let message = fit_text(message, row_text_size, panel_w.saturating_sub(80));
        text(
            canvas,
            width,
            font,
            x + 40,
            list_top + 29,
            &message,
            row_text_size,
            colors.muted,
            panel_w.saturating_sub(80),
        );
    }
    for (index, row) in rows.iter().enumerate() {
        let Some(row_rect) = launcher_result_row_rect(width, height, index) else {
            break;
        };
        let row_y = row_rect[1];
        let is_selected = index == selected;
        if index == selected {
            rounded_rect(
                canvas,
                width,
                row_rect[0],
                row_rect[1],
                row_rect[2],
                row_rect[3],
                9,
                Colors::scaled(colors.accent, 0.14),
            );
            rounded_rect(canvas, width, x + 21, row_y + 6, 3, 18, 1, colors.accent);
        }

        let icon_x = x + 33;
        let icon_y = row_y + 5;
        rounded_rect(
            canvas,
            width,
            icon_x,
            icon_y,
            20,
            20,
            6,
            Colors::scaled(colors.accent, if is_selected { 0.28 } else { 0.12 }),
        );
        if let Some(initial) = row.chars().next() {
            let monogram = initial.to_uppercase().to_string();
            let monogram_size = font_size.saturating_add(1).clamp(13, 15);
            let monogram_width = rendered_width(&monogram, monogram_size);
            text(
                canvas,
                width,
                font,
                icon_x + 10u32.saturating_sub(monogram_width / 2),
                icon_y + 14,
                &monogram,
                monogram_size,
                if is_selected {
                    colors.accent
                } else {
                    colors.muted
                },
                18,
            );
        }

        let display_row = fit_text(row, row_text_size, panel_w.saturating_sub(148));
        text(
            canvas,
            width,
            font,
            x + 64,
            row_y + 21,
            &display_row,
            row_text_size,
            if is_selected {
                colors.foreground
            } else {
                Colors::scaled(colors.foreground, 0.86)
            },
            panel_w.saturating_sub(148),
        );

        if is_selected {
            let key_x = x + panel_w.saturating_sub(76);
            rounded_rect(
                canvas,
                width,
                key_x,
                row_y + 6,
                50,
                18,
                6,
                Colors::scaled(colors.accent, 0.11),
            );
            text(
                canvas,
                width,
                font,
                key_x + 8,
                row_y + 18,
                "ENTER",
                hint_size,
                colors.accent,
                36,
            );
        } else {
            let ordinal = format!("{:02}", index + 1);
            let ordinal_width = rendered_width(&ordinal, hint_size);
            text(
                canvas,
                width,
                font,
                x + panel_w.saturating_sub(26 + ordinal_width),
                row_y + 19,
                &ordinal,
                hint_size,
                colors.muted,
                ordinal_width,
            );
        }
    }

    let divider_y = y + panel_h.saturating_sub(39);
    rect(
        canvas,
        width,
        x + 24,
        divider_y,
        panel_w.saturating_sub(48),
        1,
        Colors::scaled(colors.muted, 0.22),
    );
    let key_y = y + panel_h.saturating_sub(30);
    let key_baseline = key_y + 13;
    let mut hint_x = x + 24;
    for (key, key_w, label, label_w) in [
        ("UP/DN", 42, "MOVE", 36),
        ("ENTER", 44, "OPEN", 34),
        ("ESC", 32, "CLOSE", 41),
    ] {
        rounded_rect(
            canvas,
            width,
            hint_x,
            key_y,
            key_w,
            18,
            5,
            Colors::scaled(colors.muted, 0.12),
        );
        text(
            canvas,
            width,
            font,
            hint_x + 6,
            key_baseline,
            key,
            hint_size,
            colors.foreground,
            key_w.saturating_sub(12),
        );
        text(
            canvas,
            width,
            font,
            hint_x + key_w + 6,
            key_baseline,
            label,
            hint_size,
            colors.muted,
            label_w,
        );
        hint_x = hint_x.saturating_add(key_w + label_w + 18);
    }
}

fn draw_recorder(
    canvas: &mut [u8],
    width: u32,
    height: u32,
    colors: Colors,
    radius: u32,
    font: Option<&FontArc>,
    font_size: u32,
    snapshot: &Snapshot,
    settings: &wm_core::Recorder,
    view: RecorderView,
    settings_tab: RecorderSettingsTab,
    settings_row: usize,
    path_edit: Option<&str>,
    feedback: Option<&(String, bool)>,
    start_blocker: Option<&str>,
    capture_label: &str,
    selection_heading: &str,
    selection_label: &str,
    capture_note: &str,
    capture_fps: u32,
) {
    fill(canvas, 0);
    let (x, y, panel_w, panel_h) = recorder_panel(width, height);
    rounded_rect(
        canvas,
        width,
        x,
        y,
        panel_w,
        panel_h,
        radius,
        colors.background,
    );
    // A quiet olive edge gives the recorder the same lifted surface language
    // as the rest of the shell without adding another backdrop effect.
    rect(
        canvas,
        width,
        x + radius.min(panel_w / 2),
        y,
        panel_w.saturating_sub(radius.min(panel_w / 2) * 2),
        1,
        Colors::scaled(colors.muted, 0.16),
    );
    match view {
        RecorderView::Settings => draw_recorder_settings(
            canvas,
            width,
            x,
            y,
            panel_w,
            colors,
            font,
            font_size,
            snapshot,
            settings,
            settings_tab,
            settings_row,
            path_edit,
            feedback,
        ),
        RecorderView::Controls => draw_recorder_controls(
            canvas,
            width,
            x,
            y,
            panel_w,
            colors,
            font,
            font_size,
            settings,
            snapshot,
            start_blocker,
            capture_label,
            selection_heading,
            selection_label,
            capture_note,
            capture_fps,
        ),
    }
    text(
        canvas,
        width,
        font,
        x + panel_w.saturating_sub(180),
        y + 488,
        if view == RecorderView::Settings {
            "Esc  ·  Back"
        } else {
            "Esc  ·  Close"
        },
        font_size,
        colors.muted,
        150,
    );
}

fn draw_recorder_controls(
    canvas: &mut [u8],
    width: u32,
    x: u32,
    y: u32,
    panel_w: u32,
    colors: Colors,
    font: Option<&FontArc>,
    font_size: u32,
    settings: &wm_core::Recorder,
    snapshot: &Snapshot,
    start_blocker: Option<&str>,
    capture_label: &str,
    selection_heading: &str,
    selection_label: &str,
    capture_note: &str,
    capture_fps: u32,
) {
    let title_size = font_size.saturating_add(7);
    let body_size = font_size.saturating_add(3);
    let label_size = font_size.saturating_add(1);
    text(
        canvas,
        width,
        font,
        x + 32,
        y + 48,
        "Luma Recorder",
        title_size,
        colors.foreground,
        panel_w.saturating_sub(64),
    );
    let (state, active) = match snapshot.recorder.state {
        RecorderState::Idle => ("Ready", false),
        RecorderState::Starting => ("Starting", true),
        RecorderState::Recording => ("Recording", true),
        RecorderState::Paused => ("Paused", true),
        RecorderState::Replay => ("Saving replay", true),
        RecorderState::Error => ("Error", false),
    };
    rounded_rect(
        canvas,
        width,
        x + 24,
        y + 60,
        214,
        26,
        8,
        if active {
            Colors::scaled(colors.accent, 0.13)
        } else {
            Colors::scaled(colors.muted, 0.09)
        },
    );
    rounded_rect(
        canvas,
        width,
        x + 38,
        y + 69,
        8,
        8,
        4,
        if active { colors.accent } else { colors.muted },
    );
    text(
        canvas,
        width,
        font,
        x + 56,
        y + 78,
        "STATUS",
        font_size.saturating_sub(1).max(10),
        colors.muted,
        54,
    );
    text(
        canvas,
        width,
        font,
        x + 114,
        y + 78,
        state,
        font_size.saturating_add(1),
        if active { colors.accent } else { colors.foreground },
        112,
    );
    rect(
        canvas,
        width,
        x + 32,
        y + 96,
        panel_w.saturating_sub(64),
        1,
        Colors::scaled(colors.muted, 0.18),
    );
    let source = format!("{}  ·  {selection_label}", selection_heading.to_lowercase());
    let lines = [
        ("Capture", capture_label.to_string(), Some("← / →")),
        ("Target", source, Some("↑ / ↓")),
        (
            "Video",
            format!(
                "{} × {}  ·  up to {} FPS",
                settings.output_width, settings.output_height, capture_fps
            ),
            None,
        ),
        (
            "Encoder",
            format!(
                "{}{}  ·  quality {}",
                settings.codec.to_uppercase(),
                if settings.hdr { " Main10 HDR10" } else { "" },
                settings.quality
            ),
            None,
        ),
        (
            "Audio",
            if capture_label != "Screen (low-lag)" {
                "Direct game capture is video-only".into()
            } else {
                format!(
                    "Desktop: {}  ·  mic: {}",
                    settings.desktop_audio, settings.microphone
                )
            },
            None,
        ),
        ("Method", capture_note.into(), None),
        (
            "Live",
            format!(
                "source {:.1}  ·  encoded {:.1}  ·  dropped {}",
                snapshot.recorder.source_fps,
                snapshot.recorder.encoded_fps,
                snapshot.recorder.dropped_frames
            ),
            None,
        ),
    ];
    for (index, (label, value, hint)) in lines.iter().enumerate() {
        let row_y = y + 124 + index as u32 * 31;
        if index < 2 {
            rounded_rect(
                canvas,
                width,
                x + 24,
                row_y.saturating_sub(16),
                panel_w.saturating_sub(48),
                27,
                8,
                Colors::scaled(colors.muted, 0.07),
            );
            rect(
                canvas,
                width,
                x + 24,
                row_y.saturating_sub(10),
                2,
                14,
                Colors::scaled(colors.accent, 0.65),
            );
        } else if index == 6 {
            rounded_rect(
                canvas,
                width,
                x + 24,
                row_y.saturating_sub(16),
                panel_w.saturating_sub(48),
                27,
                8,
                Colors::scaled(colors.accent, 0.09),
            );
        }
        text(
            canvas,
            width,
            font,
            x + 40,
            row_y,
            label,
            label_size,
            colors.muted,
            88,
        );
        text(
            canvas,
            width,
            font,
            x + 132,
            row_y,
            value,
            body_size,
            if index == 5 { colors.muted } else { colors.foreground },
            panel_w.saturating_sub(196 + if hint.is_some() { 70 } else { 0 }),
        );
        if let Some(hint) = hint {
            text(
                canvas,
                width,
                font,
                x + panel_w.saturating_sub(88),
                row_y,
                hint,
                font_size,
                colors.accent,
                64,
            );
        }
    }
    if let Some(error) = snapshot.recorder.error.as_deref() {
        text(
            canvas,
            width,
            font,
            x + 32,
            y + 341,
            error,
            font_size.saturating_add(2),
            colors.accent,
            panel_w.saturating_sub(64),
        );
    }
    let running = !matches!(
        snapshot.recorder.state,
        RecorderState::Idle | RecorderState::Error
    );
    // OBS-style controls dock: settings + start/stop on one row, pause and
    // replay save beneath. The rectangles come from
    // `recorder_control_button_rects` so pointer hitboxes cannot drift from
    // the drawn pixels.
    let buttons = recorder_control_button_rects(panel_w);
    for (rect, hit) in &buttons {
        let rect = (x + rect[0], y + rect[1], rect[2], rect[3]);
        match hit {
            RecorderHit::Settings => {
                rounded_rect(
                    canvas,
                    width,
                    rect.0,
                    rect.1,
                    rect.2,
                    rect.3,
                    8,
                    Colors::scaled(colors.muted, 0.12),
                );
                text(
                    canvas,
                    width,
                    font,
                    rect.0 + 16,
                    rect.1 + 27,
                    "Settings  [Tab]",
                    font_size.saturating_add(2),
                    colors.foreground,
                    rect.2.saturating_sub(24),
                );
            }
            RecorderHit::StartStop => {
                let blocked = !running && start_blocker.is_some();
                rounded_rect(
                    canvas,
                    width,
                    rect.0,
                    rect.1,
                    rect.2,
                    rect.3,
                    8,
                    if blocked {
                        Colors::scaled(colors.muted, 0.24)
                    } else {
                        colors.accent
                    },
                );
                let label = if running {
                    "Stop recording  [Enter]"
                } else if let Some(blocker) = start_blocker {
                    blocker
                } else if capture_label == "Xwayland Zero-Copy" {
                    "Start Xwayland capture  [Enter]"
                } else if capture_label == "OpenGL API Inject" {
                    "Inject into running game  [Enter]"
                } else if capture_label == "OpenGL Launch Profile" {
                    "Start OpenGL game capture  [Enter]"
                } else if capture_label == "Vulkan API Layer" {
                    "Start Vulkan game capture  [Enter]"
                } else {
                    "Start recording  [Enter]"
                };
                text(
                    canvas,
                    width,
                    font,
                    rect.0 + 20,
                    rect.1 + 27,
                    label,
                    font_size.saturating_add(2),
                    if blocked { colors.muted } else { colors.background },
                    rect.2.saturating_sub(40),
                );
            }
            RecorderHit::Pause => {
                let paused = snapshot.recorder.state == RecorderState::Paused;
                rounded_rect(
                    canvas,
                    width,
                    rect.0,
                    rect.1,
                    rect.2,
                    rect.3,
                    8,
                    if running {
                        Colors::scaled(colors.accent, 0.22)
                    } else {
                        Colors::scaled(colors.muted, 0.12)
                    },
                );
                text(
                    canvas,
                    width,
                    font,
                    rect.0 + 16,
                    rect.1 + 25,
                    if paused {
                        "Resume  [Space]"
                    } else {
                        "Pause  [Space]"
                    },
                    font_size.saturating_add(2),
                    if running {
                        colors.foreground
                    } else {
                        Colors::scaled(colors.muted, 0.55)
                    },
                    rect.2.saturating_sub(24),
                );
            }
            RecorderHit::ReplaySave => {
                rounded_rect(
                    canvas,
                    width,
                    rect.0,
                    rect.1,
                    rect.2,
                    rect.3,
                    8,
                    if running {
                        Colors::scaled(colors.accent, 0.22)
                    } else {
                        Colors::scaled(colors.muted, 0.12)
                    },
                );
                text(
                    canvas,
                    width,
                    font,
                    rect.0 + 16,
                    rect.1 + 25,
                    "Save replay  [F8]",
                    font_size.saturating_add(2),
                    if running {
                        colors.foreground
                    } else {
                        Colors::scaled(colors.muted, 0.55)
                    },
                    rect.2.saturating_sub(24),
                );
            }
            _ => {}
        }
    }
}

/// Recorder panel rectangle shared by every recorder view. The panel stays
/// panel-sized so the compositor's backdrop blur confines to one region, as
/// with the launcher.
fn recorder_panel(width: u32, height: u32) -> (u32, u32, u32, u32) {
    let panel_w = width.min(720);
    let panel_h = height.min(500);
    let x = (width - panel_w) / 2;
    let y = (height - panel_h) / 2;
    (x, y, panel_w, panel_h)
}

/// Transport-button rectangles on the Controls page, relative to the panel
/// origin. Shared by `draw_recorder_controls` and the pointer hit test.
fn recorder_control_button_rects(panel_w: u32) -> Vec<([u32; 4], RecorderHit)> {
    let settings_w = 150;
    let start_x = 24 + settings_w + 8;
    let half = (panel_w - 56) / 2;
    vec![
        ([24, 370, settings_w, 42], RecorderHit::Settings),
        (
            [
                start_x,
                370,
                panel_w.saturating_sub(start_x + 24),
                42,
            ],
            RecorderHit::StartStop,
        ),
        ([24, 420, half, 38], RecorderHit::Pause),
        (
            [
                24 + half + 8,
                420,
                panel_w.saturating_sub(24 + half + 8 + 24),
                38,
            ],
            RecorderHit::ReplaySave,
        ),
    ]
}

fn recorder_settings_tab_rect(index: usize) -> [u32; 4] {
    [24, 104 + index as u32 * 46, 196, 42]
}

fn recorder_settings_reset_rect() -> [u32; 4] {
    [24, 420, 196, 40]
}

fn recorder_settings_close_rect(panel_w: u32) -> [u32; 4] {
    [panel_w.saturating_sub(56), 26, 30, 30]
}

fn recorder_settings_row_rect(index: usize, panel_w: u32) -> [u32; 4] {
    let content_x = 244u32;
    [
        content_x,
        104 + index as u32 * 52,
        panel_w.saturating_sub(content_x + 24),
        46,
    ]
}

/// Dec/value/inc widget rectangles inside a row, right-aligned. Shared by the
/// renderer and the hit test.
fn recorder_settings_widget_rects(row: [u32; 4]) -> ([u32; 4], [u32; 4], [u32; 4]) {
    let widget_w = 240u32;
    let wx = row[0] + row[2].saturating_sub(widget_w);
    let wy = row[1] + (row[3].saturating_sub(34)) / 2;
    (
        [wx, wy, 34, 34],
        [wx + 42, wy, widget_w.saturating_sub(84), 34],
        [wx + widget_w.saturating_sub(34), wy, 34, 34],
    )
}

fn draw_recorder_settings(
    canvas: &mut [u8],
    width: u32,
    x: u32,
    y: u32,
    panel_w: u32,
    colors: Colors,
    font: Option<&FontArc>,
    font_size: u32,
    snapshot: &Snapshot,
    settings: &wm_core::Recorder,
    tab: RecorderSettingsTab,
    selected_row: usize,
    path_edit: Option<&str>,
    feedback: Option<&(String, bool)>,
) {
    let title_size = font_size.saturating_add(7);
    text(
        canvas,
        width,
        font,
        x + 32,
        y + 46,
        "Recorder Settings",
        title_size,
        colors.foreground,
        320,
    );
    text(
        canvas,
        width,
        font,
        x + 244,
        y + 74,
        tab.label(),
        font_size.saturating_add(1),
        colors.accent,
        300,
    );
    let close = recorder_settings_close_rect(panel_w);
    rounded_rect(
        canvas,
        width,
        x + close[0],
        y + close[1],
        close[2],
        close[3],
        8,
        Colors::scaled(colors.muted, 0.10),
    );
    text(
        canvas,
        width,
        font,
        x + close[0] + 10,
        y + close[1] + 22,
        "×",
        font_size.saturating_add(2),
        colors.foreground,
        close[2].saturating_sub(12),
    );
    rect(
        canvas,
        width,
        x + 24,
        y + 86,
        panel_w.saturating_sub(48),
        1,
        Colors::scaled(colors.muted, 0.18),
    );
    rect(
        canvas,
        width,
        x + 24,
        y + 85,
        44,
        2,
        Colors::scaled(colors.accent, 0.72),
    );
    let rows = recorder_settings_rows(settings, tab);
    // The quiet category rail keeps the current section easy to spot without
    // turning every tab into a gold pill.
    for (index, category) in RecorderSettingsTab::ALL.iter().enumerate() {
        let rect = recorder_settings_tab_rect(index);
        let selected = *category == tab;
        rounded_rect(
            canvas,
            width,
            x + rect[0],
            y + rect[1],
            rect[2],
            rect[3],
            8,
            if selected {
                Colors::scaled(colors.accent, 0.13)
            } else {
                Colors::scaled(colors.muted, 0.055)
            },
        );
        if selected {
            rounded_rect(
                canvas,
                width,
                x + rect[0],
                y + rect[1] + 10,
                3,
                22,
                2,
                colors.accent,
            );
        }
        text(
            canvas,
            width,
            font,
            x + rect[0] + 16,
            y + rect[1] + 27,
            category.label(),
            font_size.saturating_add(2),
            if selected { colors.foreground } else { colors.muted },
            rect[2].saturating_sub(24),
        );
    }
    text(
        canvas,
        width,
        font,
        x + 24,
        y + 330,
        match tab {
            RecorderSettingsTab::Output => "Codec, quality & folder",
            RecorderSettingsTab::Capture => "Screen, window, or game",
            RecorderSettingsTab::Video => "Output size and frame rate",
            RecorderSettingsTab::Audio => "Desktop and mic mix",
            RecorderSettingsTab::Replay => "Instant replay buffer limits",
        },
        font_size,
        Colors::scaled(colors.muted, 0.75),
        196,
    );
    let reset = recorder_settings_reset_rect();
    rounded_rect(
        canvas,
        width,
        x + reset[0],
        y + reset[1],
        reset[2],
        reset[3],
        8,
        Colors::scaled(colors.muted, 0.12),
    );
    text(
        canvas,
        width,
        font,
        x + reset[0] + 16,
        y + reset[1] + 25,
        "Restore defaults",
        font_size.saturating_add(2),
        colors.accent,
        reset[2].saturating_sub(24),
    );
    draw_recorder_settings_rows(
        canvas,
        width,
        x,
        y,
        panel_w,
        colors,
        font,
        font_size.saturating_add(2),
        snapshot,
        &rows,
        selected_row, path_edit, feedback,
    );
}

fn draw_recorder_settings_rows(
    canvas: &mut [u8],
    width: u32,
    x: u32,
    y: u32,
    panel_w: u32,
    colors: Colors,
    font: Option<&FontArc>,
    font_size: u32,
    snapshot: &Snapshot,
    rows: &[RecorderRowDisplay],
    selected_row: usize,
    path_edit: Option<&str>,
    feedback: Option<&(String, bool)>,
) {
    for (index, row) in rows.iter().enumerate() {
        let row_rect = recorder_settings_row_rect(index, panel_w);
        let selected = index == selected_row;
        rounded_rect(
            canvas,
            width,
            x + row_rect[0],
            y + row_rect[1],
            row_rect[2],
            row_rect[3],
            8,
            if selected {
                Colors::scaled(colors.accent, 0.14)
            } else {
                Colors::scaled(colors.muted, 0.055)
            },
        );
        if selected {
            rect(
                canvas,
                width,
                x + row_rect[0],
                y + row_rect[1] + 11,
                3,
                24,
                colors.accent,
            );
        }
        text(
            canvas,
            width,
            font,
            x + row_rect[0] + 14,
            y + row_rect[1] + 29,
            row.label,
            font_size,
            if selected {
                colors.accent
            } else if row.kind == RecorderRowKind::Info {
                Colors::scaled(colors.muted, 0.65)
            } else {
                colors.foreground
            },
            row_rect[2].saturating_sub(260),
        );
        draw_recorder_settings_widget(
            canvas, width, x, y, colors, font, font_size, row, selected, path_edit, row_rect,
        );
    }
    draw_recorder_settings_footer(
        canvas, width, x, y, panel_w, colors, font, font_size, snapshot, feedback,
    );
}

fn draw_recorder_settings_widget(
    canvas: &mut [u8],
    width: u32,
    x: u32,
    y: u32,
    colors: Colors,
    font: Option<&FontArc>,
    font_size: u32,
    row: &RecorderRowDisplay,
    selected: bool,
    path_edit: Option<&str>,
    rect: [u32; 4],
) {
    match row.kind {
        RecorderRowKind::Cycle | RecorderRowKind::Step => {
            let (dec, value, inc) = recorder_settings_widget_rects(rect);
            for (widget, label) in [(dec, "<"), (inc, ">")] {
                rounded_rect(
                    canvas,
                    width,
                    x + widget[0],
                    y + widget[1],
                    widget[2],
                    widget[3],
                    8,
                    if selected {
                        Colors::scaled(colors.accent, 0.18)
                    } else {
                        Colors::scaled(colors.muted, 0.09)
                    },
                );
                text(
                    canvas,
                    width,
                    font,
                    x + widget[0] + 13,
                    y + widget[1] + 23,
                    label,
                    font_size,
                    colors.foreground,
                    widget[2].saturating_sub(8),
                );
            }
            rounded_rect(
                canvas,
                width,
                x + value[0],
                y + value[1],
                value[2],
                value[3],
                8,
                Colors::scaled(colors.background, 0.35),
            );
            let value_text = path_edit.unwrap_or(&row.value).to_string();
            text(
                canvas,
                width,
                font,
                x + value[0] + 8,
                y + value[1] + 23,
                &value_text,
                font_size,
                if selected { colors.accent } else { colors.foreground },
                value[2].saturating_sub(8),
            );
        }
        RecorderRowKind::Toggle => {
            let (_, value, _) = recorder_settings_widget_rects(rect);
            let on = row.value == "On";
            rounded_rect(
                canvas,
                width,
                x + value[0],
                y + value[1],
                110,
                value[3],
                8,
                if on {
                    Colors::scaled(colors.accent, 0.16)
                } else {
                    Colors::scaled(colors.muted, 0.09)
                },
            );
            text(
                canvas,
                width,
                font,
                x + value[0] + 12,
                y + value[1] + 23,
                &row.value,
                font_size,
                if on { colors.accent } else { colors.muted },
                56,
            );
            rounded_rect(
                canvas,
                width,
                x + value[0] + 88,
                y + value[1] + 11,
                12,
                12,
                6,
                if on {
                    colors.accent
                } else {
                    Colors::scaled(colors.muted, 0.52)
                },
            );
        }
        RecorderRowKind::Path | RecorderRowKind::Identity => {
            let (field, value, _) = recorder_settings_widget_rects(rect);
            let field_width = value[0] + value[2] - field[0];
            let editing = selected && path_edit.is_some();
            let buffer = path_edit.unwrap_or(&row.value).to_string();
            let shown = if editing {
                format!("{buffer}_")
            } else {
                buffer
            };
            rounded_rect(
                canvas,
                width,
                x + field[0],
                y + field[1],
                field_width,
                field[3],
                8,
                Colors::scaled(colors.background, 0.35),
            );
            if editing {
                rounded_rect_outline(
                    canvas,
                    width,
                    x + field[0],
                    y + field[1],
                    field_width,
                    field[3],
                    8,
                    Colors::scaled(colors.accent, 0.62),
                );
            }
            text(
                canvas,
                width,
                font,
                x + field[0] + 8,
                y + field[1] + 23,
                &shown,
                font_size,
                if editing { colors.accent } else { colors.foreground },
                field_width.saturating_sub(16),
            );
        }
        RecorderRowKind::Action => {
            let action = [
                rect[0] + rect[2].saturating_sub(240),
                rect[1] + 6,
                240,
                34,
            ];
            rounded_rect(
                canvas,
                width,
                x + action[0],
                y + action[1],
                action[2],
                action[3],
                8,
                if selected {
                    Colors::scaled(colors.accent, 0.17)
                } else {
                    Colors::scaled(colors.muted, 0.09)
                },
            );
            text(
                canvas,
                width,
                font,
                x + action[0] + 12,
                y + action[1] + 23,
                &row.value,
                font_size,
                if selected { colors.accent } else { colors.foreground },
                action[2].saturating_sub(24),
            );
        }
        RecorderRowKind::Info => {
            text(
                canvas,
                width,
                font,
                x + rect[0] + rect[2].saturating_sub(250),
                y + rect[1] + 29,
                &row.value,
                font_size,
                Colors::scaled(colors.muted, 0.65),
                240,
            );
        }
    }
}

fn draw_recorder_settings_footer(
    canvas: &mut [u8],
    width: u32,
    x: u32,
    y: u32,
    panel_w: u32,
    colors: Colors,
    font: Option<&FontArc>,
    font_size: u32,
    snapshot: &Snapshot,
    feedback: Option<&(String, bool)>,
) {
    let running = !matches!(
        snapshot.recorder.state,
        RecorderState::Idle | RecorderState::Error
    );
    if let Some((message, error)) = feedback {
        let shown = if *error {
            format!("Error  ·  {message}")
        } else {
            message.clone()
        };
        text(
            canvas,
            width,
            font,
            x + 244,
            y + 448,
            &shown,
            font_size,
            if *error { colors.foreground } else { colors.accent },
            panel_w.saturating_sub(280),
        );
    } else if running {
        text(
            canvas,
            width,
            font,
            x + 244,
            y + 448,
            "Changes apply to the next recording",
            font_size,
            colors.muted,
            panel_w.saturating_sub(280),
        );
    } else {
        text(
            canvas,
            width,
            font,
            x + 244,
            y + 448,
            "Settings persist across restarts  ·  ← / → edits",
            font_size,
            colors.muted,
            panel_w.saturating_sub(280),
        );
    }
}

fn draw_notifications(
    canvas: &mut [u8],
    width: u32,
    height: u32,
    colors: Colors,
    font: Option<&FontArc>,
    font_size: u32,
    rows: &[Notification],
    total: usize,
    offset: usize,
    muted: bool,
) {
    fill(canvas, 0);
    let panel_height = (rows.len().max(1) as u32 * 88 + 18).min(height);
    panel(
        canvas,
        width,
        0,
        0,
        width,
        panel_height,
        colors.radius.min(panel_height / 2).min(width / 2),
        colors,
    );
    let card_radius = colors.radius.clamp(10, 18);
    let state_size = font_size.saturating_add(2).clamp(16, 20);
    let title_size = font_size.saturating_add(3).clamp(16, 20);
    let body_size = font_size.saturating_add(1).clamp(14, 18);
    if muted && !rows.iter().any(|notification| notification.critical) {
        rounded_rect(
            canvas,
            width,
            14,
            32,
            width.saturating_sub(28),
            44,
            card_radius,
            Colors::scaled(colors.muted, 0.12),
        );
        rounded_rect_outline(
            canvas,
            width,
            14,
            32,
            width.saturating_sub(28),
            44,
            card_radius,
            Colors::scaled(colors.muted, 0.24),
        );
        rounded_rect(
            canvas,
            width,
            26,
            50,
            8,
            8,
            4,
            Colors::scaled(colors.accent, 0.18),
        );
        rounded_rect_outline(canvas, width, 26, 50, 8, 8, 4, colors.accent);
        text(
            canvas,
            width,
            font,
            46,
            54,
            "Do Not Disturb is on",
            state_size,
            colors.foreground,
            width.saturating_sub(36),
        );
        return;
    }
    if rows.is_empty() {
        rounded_rect(
            canvas,
            width,
            14,
            32,
            width.saturating_sub(28),
            44,
            card_radius,
            Colors::scaled(colors.muted, 0.12),
        );
        rounded_rect_outline(
            canvas,
            width,
            14,
            32,
            width.saturating_sub(28),
            44,
            card_radius,
            Colors::scaled(colors.muted, 0.24),
        );
        rounded_rect(
            canvas,
            width,
            26,
            50,
            8,
            8,
            4,
            Colors::scaled(colors.accent, 0.18),
        );
        rounded_rect_outline(canvas, width, 26, 50, 8, 8, 4, colors.accent);
        text(
            canvas,
            width,
            font,
            46,
            54,
            "No notifications",
            state_size,
            colors.foreground,
            width.saturating_sub(36),
        );
        return;
    }
    rounded_rect(
        canvas,
        width,
        12,
        4,
        width.saturating_sub(24),
        24,
        12,
        Colors::scaled(colors.muted, 0.07),
    );
    rounded_rect_outline(
        canvas,
        width,
        12,
        4,
        width.saturating_sub(24),
        24,
        12,
        Colors::scaled(colors.muted, 0.2),
    );
    rounded_rect(canvas, width, 18, 13, 7, 7, 4, colors.accent);
    text(
        canvas,
        width,
        font,
        30,
        21,
        &format!("Notifications {}/{}", offset + 1, total),
        state_size,
        colors.foreground,
        width.saturating_sub(36),
    );
    for (index, notification) in rows.iter().enumerate() {
        let y = 40 + index as u32 * 88;
        let card_y = y.saturating_sub(12);
        rounded_rect(
            canvas,
            width,
            10,
            card_y,
            width.saturating_sub(20),
            76,
            card_radius,
            Colors::scaled(colors.muted, 0.12),
        );
        rounded_rect_outline(
            canvas,
            width,
            10,
            card_y,
            width.saturating_sub(20),
            76,
            card_radius,
            Colors::scaled(
                if notification.critical {
                    colors.accent
                } else {
                    colors.muted
                },
                if notification.critical { 0.4 } else { 0.25 },
            ),
        );
        if notification.critical {
            rounded_rect(
                canvas,
                width,
                14,
                card_y + 10,
                3,
                56,
                2,
                colors.accent,
            );
        }
        let text_x = notification_text_x(notification);
        if let Some(icon) = notification.icon.as_ref() {
            draw_icon(canvas, width, 18, y.saturating_sub(14), 32, icon);
        }
        text(
            canvas,
            width,
            font,
            text_x,
            y + 4,
            &notification.summary,
            title_size,
            if notification.critical {
                colors.accent
            } else {
                colors.foreground
            },
            width.saturating_sub(text_x + 18),
        );
        text(
            canvas,
            width,
            font,
            text_x,
            y + 26,
            &notification.body,
            body_size,
            colors.muted,
            width.saturating_sub(text_x + 18),
        );
        let mut action_x = text_x;
        for (_, label) in notification.actions.iter().take(4) {
            let action_width = text_width(font, label, font_size.saturating_sub(2).max(9)) + 12;
            rounded_rect(
                canvas,
                width,
                action_x,
                y + 31,
                action_width,
                18,
                9,
                Colors::scaled(colors.accent, 0.12),
            );
            rounded_rect_outline(
                canvas,
                width,
                action_x,
                y + 31,
                action_width,
                18,
                9,
                Colors::scaled(colors.accent, 0.3),
            );
            text(
                canvas,
                width,
                font,
                action_x + 6,
                y + 44,
                label,
                font_size.saturating_sub(2).max(9),
                colors.accent,
                action_width.saturating_sub(12),
            );
            action_x = action_x.saturating_add(action_width + 12);
        }
    }
}

fn draw_controls(
    canvas: &mut [u8],
    width: u32,
    height: u32,
    colors: Colors,
    font: Option<&FontArc>,
    font_size: u32,
    panel_kind: Option<ControlPanel>,
    pending_power_action: Option<PowerAction>,
    audio: &AudioState,
    network: &NetworkState,
    media: &MediaState,
    bluetooth: &BluetoothState,
    dnd: bool,
    notification_count: usize,
) {
    fill(canvas, 0);
    panel(
        canvas,
        width,
        0,
        0,
        width,
        height,
        colors.radius.min(height / 2).min(width / 2),
        colors,
    );
    let Some(panel) = panel_kind else {
        return;
    };
    let content_width = width.saturating_sub(48);
    let control_radius = colors.radius.clamp(8, 16);
    let heading = match panel {
        ControlPanel::Audio => "Audio",
        ControlPanel::Network => "Network & Wi-Fi",
        ControlPanel::Bluetooth => "Bluetooth",
        ControlPanel::Media => "Media",
        ControlPanel::Notifications => "Notifications",
        ControlPanel::Power => "Session",
    };
    rounded_rect(
        canvas,
        width,
        14,
        12,
        width.saturating_sub(28),
        30,
        control_radius,
        Colors::scaled(colors.muted, 0.055),
    );
    rounded_rect_outline(
        canvas,
        width,
        14,
        12,
        width.saturating_sub(28),
        30,
        control_radius,
        Colors::scaled(colors.muted, 0.15),
    );
    rounded_rect(
        canvas,
        width,
        19,
        21,
        6,
        6,
        3,
        Colors::scaled(colors.accent, 0.18),
    );
    rounded_rect_outline(canvas, width, 19, 21, 6, 6, 3, colors.accent);
    text(
        canvas,
        width,
        font,
        32,
        30,
        heading,
        font_size + 2,
        colors.foreground,
        content_width,
    );
    let status_line = |canvas: &mut [u8], label: &str| {
        rounded_rect(
            canvas,
            width,
            24,
            46,
            5,
            5,
            3,
            colors.accent,
        );
        text(
            canvas,
            width,
            font,
            36,
            52,
            label,
            font_size,
            colors.muted,
            width.saturating_sub(60),
        );
    };
    let button = |canvas: &mut [u8], y: u32, label: &str, active: bool| {
        let row_width = width.saturating_sub(36);
        rounded_rect(
            canvas,
            width,
            18,
            y,
            row_width,
            34,
            control_radius,
            if active {
                Colors::scaled(colors.accent, 0.16)
            } else {
                Colors::scaled(colors.muted, 0.075)
            },
        );
        rounded_rect_outline(
            canvas,
            width,
            18,
            y,
            row_width,
            34,
            control_radius,
            Colors::scaled(
                if active { colors.accent } else { colors.muted },
                if active { 0.38 } else { 0.2 },
            ),
        );
        rounded_rect(
            canvas,
            width,
            23,
            y + 13,
            4,
            8,
            2,
            if active {
                colors.accent
            } else {
                Colors::scaled(colors.muted, 0.55)
            },
        );
        text(
            canvas,
            width,
            font,
            34,
            y + 22,
            label,
            font_size,
            if active { colors.accent } else { colors.foreground },
            width.saturating_sub(66),
        );
    };
    match panel {
        ControlPanel::Audio => {
            status_line(canvas, &audio.label);
            let trough_width = width.saturating_sub(48);
            let volume = audio
                .label
                .strip_prefix("VOL ")
                .and_then(|value| value.strip_suffix('%'))
                .and_then(|value| value.parse::<u32>().ok())
                .unwrap_or(0)
                .min(100);
            // A shallow material track keeps this native scale easy to read
            // without adding a compositor effect or a second render pass.
            rounded_rect(
                canvas,
                width,
                24,
                67,
                trough_width,
                15,
                7,
                Colors::scaled(colors.muted, 0.07),
            );
            rounded_rect_outline(
                canvas,
                width,
                24,
                67,
                trough_width,
                15,
                7,
                Colors::scaled(colors.muted, 0.16),
            );
            rounded_rect(
                canvas,
                width,
                24,
                72,
                trough_width,
                4,
                2,
                Colors::scaled(colors.muted, 0.25),
            );
            let highlight_width = trough_width * volume / 100;
            if highlight_width > 0 {
                rounded_rect(canvas, width, 24, 72, highlight_width, 4, 2, colors.accent);
            }
            let knob_x = (24 + highlight_width)
                .saturating_sub(6)
                .clamp(24, 24 + trough_width.saturating_sub(12));
            rounded_rect(
                canvas,
                width,
                knob_x.saturating_sub(2),
                67,
                16,
                16,
                8,
                Colors::scaled(colors.accent, 0.24),
            );
            rounded_rect(canvas, width, knob_x, 69, 12, 12, 6, colors.accent);
            button(canvas, 104, "Mute / unmute", audio.label == "MUTED");
            button(canvas, 152, "Sound settings…", false);
        }
        ControlPanel::Network => {
            status_line(canvas, &network.label);
            button(
                canvas,
                56,
                if network.networking_enabled {
                    "Networking: on"
                } else {
                    "Networking: off"
                },
                network.networking_enabled,
            );
            button(
                canvas,
                100,
                if network.wireless_enabled {
                    "Wi-Fi: on"
                } else {
                    "Wi-Fi: off"
                },
                network.wireless_enabled,
            );
            button(canvas, 152, "Connection settings…", false);
        }
        ControlPanel::Bluetooth => {
            status_line(canvas, &bluetooth.label);
            button(
                canvas,
                56,
                if bluetooth.powered == Some(true) {
                    "Bluetooth: on"
                } else {
                    "Bluetooth: off"
                },
                bluetooth.powered == Some(true),
            );
            button(canvas, 100, "Find devices", false);
            button(canvas, 152, "Bluetooth settings…", false);
        }
        ControlPanel::Media => {
            status_line(canvas, &media.label);
            for (x, label) in [(18, "Previous"), (136, "Play / pause"), (254, "Next")] {
                rounded_rect(
                    canvas,
                    width,
                    x,
                    60,
                    108,
                    44,
                    control_radius,
                    Colors::scaled(colors.muted, 0.075),
                );
                rounded_rect_outline(
                    canvas,
                    width,
                    x,
                    60,
                    108,
                    44,
                    control_radius,
                    Colors::scaled(colors.muted, 0.22),
                );
                text(
                    canvas,
                    width,
                    font,
                    x + 8,
                    87,
                    label,
                    font_size.saturating_sub(2).max(9),
                    colors.foreground,
                    92,
                );
            }
            button(canvas, 116, "Stop playback", false);
        }
        ControlPanel::Notifications => {
            text(
                canvas,
                width,
                font,
                24,
                52,
                &format!("{notification_count} notification(s)"),
                font_size,
                colors.muted,
                content_width,
            );
            button(
                canvas,
                56,
                if dnd {
                    "Do Not Disturb: on"
                } else {
                    "Do Not Disturb: off"
                },
                dnd,
            );
            button(canvas, 100, "Clear notifications", false);
        }
        ControlPanel::Power => {
            if let Some(action) = pending_power_action {
                text(
                    canvas,
                    width,
                    font,
                    24,
                    52,
                    &format!("Confirm {}?", action.label()),
                    font_size,
                    colors.muted,
                    content_width,
                );
                button(canvas, 76, &format!("Confirm {}", action.label()), true);
                button(canvas, 120, "Cancel", false);
            } else {
                text(
                    canvas,
                    width,
                    font,
                    24,
                    52,
                    "Choose a session action",
                    font_size,
                    colors.muted,
                    content_width,
                );
                button(canvas, 56, "Log out", false);
                button(canvas, 100, "Reboot", false);
                button(canvas, 144, "Shut down", false);
            }
        }
    }
    rounded_rect(
        canvas,
        width,
        14,
        height.saturating_sub(31),
        width.saturating_sub(28),
        24,
        control_radius,
        Colors::scaled(colors.muted, 0.055),
    );
    rounded_rect_outline(
        canvas,
        width,
        14,
        height.saturating_sub(31),
        width.saturating_sub(28),
        24,
        control_radius,
        Colors::scaled(colors.muted, 0.14),
    );
    text(
        canvas,
        width,
        font,
        24,
        height.saturating_sub(16),
        "Right-click outside a control to close",
        font_size.saturating_sub(3).max(9),
        colors.muted,
        content_width,
    );
}

fn draw_tray_menu(
    canvas: &mut [u8],
    width: u32,
    height: u32,
    colors: Colors,
    font: Option<&FontArc>,
    font_size: u32,
    menu: Option<&TrayMenuState>,
) {
    fill(canvas, 0);
    let Some(menu) = menu else {
        return;
    };
    panel(
        canvas,
        width,
        0,
        0,
        width,
        height,
        colors.radius.min(height / 2).min(width / 2),
        colors,
    );
    const ROW_HEIGHT: u32 = 28;
    let row_radius = colors.radius.clamp(7, 14);
    let mut row_index = 0u32;
    if menu.parents.len() > 1 {
        rounded_rect(
            canvas,
            width,
            8,
            2,
            width.saturating_sub(16),
            24,
            row_radius,
            Colors::scaled(colors.accent, 0.14),
        );
        rounded_rect_outline(
            canvas,
            width,
            8,
            2,
            width.saturating_sub(16),
            24,
            row_radius,
            Colors::scaled(colors.accent, 0.32),
        );
        text(
            canvas,
            width,
            font,
            14,
            20,
            "‹ Back",
            font_size,
            colors.accent,
            width.saturating_sub(28),
        );
        row_index += 1;
    }
    for row in &menu.rows {
        let top = row_index * ROW_HEIGHT;
        if top + ROW_HEIGHT > height {
            break;
        }
        if row.separator {
            rect(
                canvas,
                width,
                12,
                top + ROW_HEIGHT / 2,
                width.saturating_sub(24),
                1,
                Colors::scaled(colors.muted, 0.25),
            );
        } else {
            let selected = row.toggle_state == Some(1);
            let row_color = if selected {
                colors.accent
            } else {
                colors.muted
            };
            rounded_rect(
                canvas,
                width,
                8,
                top + 2,
                width.saturating_sub(16),
                ROW_HEIGHT - 4,
                row_radius,
                Colors::scaled(
                    row_color,
                    if selected {
                        0.14
                    } else if row.enabled {
                        0.055
                    } else {
                        0.025
                    },
                ),
            );
            rounded_rect_outline(
                canvas,
                width,
                8,
                top + 2,
                width.saturating_sub(16),
                ROW_HEIGHT - 4,
                row_radius,
                Colors::scaled(
                    row_color,
                    if selected {
                        0.32
                    } else if row.enabled {
                        0.14
                    } else {
                        0.08
                    },
                ),
            );
            let color = if !row.enabled {
                Colors::scaled(colors.muted, 0.55)
            } else if selected {
                colors.accent
            } else {
                colors.foreground
            };
            let marker = match row.toggle_state {
                Some(1) => "✓ ",
                Some(_) => "  ",
                None => "",
            };
            let label = format!(
                "{marker}{}{}",
                row.label,
                if row.submenu { "  ›" } else { "" }
            );
            text(
                canvas,
                width,
                font,
                14,
                top + 20,
                &label,
                font_size,
                color,
                width.saturating_sub(28),
            );
        }
        row_index += 1;
    }
}

fn fill(canvas: &mut [u8], color: u32) {
    for pixel in canvas.chunks_exact_mut(4) {
        pixel.copy_from_slice(&color.to_le_bytes());
    }
}

fn rect(canvas: &mut [u8], width: u32, x: u32, y: u32, w: u32, h: u32, color: u32) {
    let height = (canvas.len() / 4 / width as usize) as u32;
    for py in y.min(height)..y.saturating_add(h).min(height) {
        for px in x.min(width)..x.saturating_add(w).min(width) {
            let index = ((py * width + px) * 4) as usize;
            paint_premultiplied_pixel(canvas, index, color, u8::MAX);
        }
    }
}

fn rounded_rect(
    canvas: &mut [u8],
    width: u32,
    x: u32,
    y: u32,
    w: u32,
    h: u32,
    radius: u32,
    color: u32,
) {
    let height = (canvas.len() / 4 / width as usize) as u32;
    let radius = radius.min(w / 2).min(h / 2);
    if radius == 0 {
        rect(canvas, width, x, y, w, h, color);
        return;
    }
    for py in y.min(height)..y.saturating_add(h).min(height) {
        for px in x.min(width)..x.saturating_add(w).min(width) {
            let local_x = px.saturating_sub(x);
            let local_y = py.saturating_sub(y);
            let corner_x = if local_x < radius {
                radius as f32
            } else if local_x >= w.saturating_sub(radius) {
                w.saturating_sub(radius) as f32
            } else {
                local_x as f32 + 0.5
            };
            let corner_y = if local_y < radius {
                radius as f32
            } else if local_y >= h.saturating_sub(radius) {
                h.saturating_sub(radius) as f32
            } else {
                local_y as f32 + 0.5
            };
            let distance = ((local_x as f32 + 0.5 - corner_x).powi(2)
                + (local_y as f32 + 0.5 - corner_y).powi(2))
            .sqrt();
            // A one-pixel coverage ramp removes the visibly stair-stepped edges
            // of native SHM surfaces without requiring a second render pass.
            let coverage = ((radius as f32 + 0.5 - distance).clamp(0.0, 1.0) * 255.0).round() as u8;
            if coverage != 0 {
                let index = ((py * width + px) * 4) as usize;
                paint_premultiplied_pixel(canvas, index, color, coverage);
            }
        }
    }
}

/// Shared native panel treatment: theme background with a quiet muted edge.
/// The accent is reserved for selection, recording, and direct actions.
fn panel(
    canvas: &mut [u8],
    width: u32,
    x: u32,
    y: u32,
    w: u32,
    h: u32,
    radius: u32,
    colors: Colors,
) {
    rounded_rect(canvas, width, x, y, w, h, radius, colors.background);
    rounded_rect_outline(
        canvas,
        width,
        x,
        y,
        w,
        h,
        radius,
        Colors::scaled(colors.muted, 0.28),
    );
}

/// One-pixel antialiased rounded-rectangle stroke centered on the given
/// geometry, used for panel borders.
fn rounded_rect_outline(
    canvas: &mut [u8],
    width: u32,
    x: u32,
    y: u32,
    w: u32,
    h: u32,
    radius: u32,
    color: u32,
) {
    let height = (canvas.len() / 4 / width as usize) as u32;
    if w == 0 || h == 0 {
        return;
    }
    let radius = radius.min(w / 2).min(h / 2);
    let center_x = w as f32 / 2.0;
    let center_y = h as f32 / 2.0;
    let inner_x = center_x - radius as f32;
    let inner_y = center_y - radius as f32;
    for py in y.min(height)..y.saturating_add(h).min(height) {
        for px in x.min(width)..x.saturating_add(w).min(width) {
            let qx = (px.saturating_sub(x) as f32 + 0.5 - center_x).abs() - inner_x;
            let qy = (py.saturating_sub(y) as f32 + 0.5 - center_y).abs() - inner_y;
            // Signed distance to the rounded-rect boundary: zero on the edge,
            // negative inside, positive outside.
            let distance = qx.max(0.0).hypot(qy.max(0.0)) + qx.max(qy).min(0.0)
                - radius as f32;
            let coverage = ((1.0 - distance.abs()) * 255.0).round().clamp(0.0, 255.0) as u8;
            if coverage != 0 {
                let index = ((py * width + px) * 4) as usize;
                blend_premultiplied_pixel(canvas, index, color, coverage);
            }
        }
    }
}

fn paint_premultiplied_pixel(canvas: &mut [u8], index: usize, color: u32, coverage: u8) {
    if coverage == u8::MAX && color >> 24 == 0xff {
        canvas[index..index + 4].copy_from_slice(&color.to_le_bytes());
    } else {
        blend_premultiplied_pixel(canvas, index, color, coverage);
    }
}

fn blend_premultiplied_pixel(canvas: &mut [u8], index: usize, color: u32, coverage: u8) {
    let destination = u32::from_le_bytes(canvas[index..index + 4].try_into().unwrap());
    let coverage = u32::from(coverage);
    let source = |channel: u32| channel * coverage / 255;
    let alpha = source((color >> 24) & 0xff);
    let inverse_alpha = 255 - alpha;
    let blend =
        |channel: u32, destination: u32| source(channel) + destination * inverse_alpha / 255;
    let output = (blend((color >> 16) & 0xff, (destination >> 16) & 0xff).min(255) << 16)
        | (blend((color >> 8) & 0xff, (destination >> 8) & 0xff).min(255) << 8)
        | blend(color & 0xff, destination & 0xff).min(255)
        | (alpha + ((destination >> 24) & 0xff) * inverse_alpha / 255).min(255) << 24;
    canvas[index..index + 4].copy_from_slice(&output.to_le_bytes());
}

fn text(
    canvas: &mut [u8],
    width: u32,
    font: Option<&FontArc>,
    x: u32,
    baseline: u32,
    value: &str,
    size: u32,
    color: u32,
    max_width: u32,
) {
    if let Some(font) = font {
        draw_text(
            canvas, width, font, x, baseline, value, size, color, max_width,
        );
    } else {
        word(canvas, width, x, baseline.saturating_sub(10), value, color);
    }
}

fn text_width(font: Option<&FontArc>, value: &str, size: u32) -> u32 {
    let Some(font) = font else {
        return value.chars().count() as u32 * 6;
    };
    let scaled = font.as_scaled(PxScale::from(size.max(8) as f32));
    let mut width = 0.0;
    let mut previous = None;
    for character in value.chars() {
        let id = font.glyph_id(character);
        if let Some(previous) = previous {
            width += scaled.kern(previous, id);
        }
        width += scaled.h_advance(id);
        previous = Some(id);
    }
    width.ceil().max(0.0) as u32
}

/// Layout of the right-side bar chips, shared by drawing and hit testing.
/// Rectangles are ordered right to left. Long dynamic labels are clipped so
/// notifications and media titles cannot consume the entire bar.
fn bar_module_layout(
    width: u32,
    height: u32,
    font: Option<&FontArc>,
    font_size: u32,
    tray_count: usize,
    left_guard: u32,
    values: &[(&'static str, String)],
) -> Vec<(&'static str, u32, u32)> {
    let icon_width = height.saturating_sub(12).clamp(12, 24) + 2;
    let tray_width = tray_count as u32 * icon_width + 8;
    let mut right = width.saturating_sub(BAR_RIGHT_INSET);
    let mut rects = Vec::with_capacity(values.len());
    for (module, value) in values.iter().rev() {
        let max_content = match *module {
            "notifications" => 180,
            "media" => 140,
            "tray" => tray_width.max(40),
            _ => 110,
        };
        let content_width = text_width(font, value, font_size)
            .max(if *module == "tray" { tray_width } else { 0 })
            .min(max_content);
        let chip_width = content_width
            .saturating_add(20)
            .min(right.saturating_sub(left_guard));
        if chip_width < 24 {
            break;
        }
        let left = right.saturating_sub(chip_width);
        rects.push((*module, left, right));
        if left <= left_guard {
            break;
        }
        right = left.saturating_sub(6);
    }
    rects
}

/// Hit range of a control-panel button row. The buttons are drawn 34 px tall;
/// the hit range must cover exactly those pixels.
fn control_button(y: u32) -> std::ops::RangeInclusive<f64> {
    let start = f64::from(y);
    start..=(start + 34.0)
}

fn draw_text(
    canvas: &mut [u8],
    width: u32,
    font: &FontArc,
    x: u32,
    baseline: u32,
    value: &str,
    size: u32,
    color: u32,
    max_width: u32,
) {
    let scale = PxScale::from(size.max(8) as f32);
    let scaled = font.as_scaled(scale);
    let mut pen_x = x as f32;
    let limit = x.saturating_add(max_width) as f32;
    let mut previous = None;
    for character in value.chars() {
        let id = font.glyph_id(character);
        if let Some(previous) = previous {
            pen_x += scaled.kern(previous, id);
        }
        let advance = scaled.h_advance(id);
        if pen_x + advance > limit {
            break;
        }
        let glyph = id.with_scale_and_position(scale, point(pen_x, baseline as f32));
        if let Some(outline) = font.outline_glyph(glyph) {
            let bounds = outline.px_bounds();
            outline.draw(|glyph_x, glyph_y, coverage| {
                let px = bounds.min.x as i32 + glyph_x as i32;
                let py = bounds.min.y as i32 + glyph_y as i32;
                blend(canvas, width, px, py, color, coverage);
            });
        }
        pen_x += advance;
        previous = Some(id);
    }
}

fn blend(canvas: &mut [u8], width: u32, x: i32, y: i32, color: u32, coverage: f32) {
    let height = (canvas.len() / 4 / width as usize) as i32;
    if x < 0 || y < 0 || x >= width as i32 || y >= height {
        return;
    }
    let index = ((y as u32 * width + x as u32) * 4) as usize;
    let existing = u32::from_le_bytes(canvas[index..index + 4].try_into().unwrap());
    let alpha = coverage.clamp(0.0, 1.0);
    let mix = |shift: u32| {
        let old = ((existing >> shift) & 0xff) as f32;
        let new = ((color >> shift) & 0xff) as f32;
        (old + (new - old) * alpha).round() as u32
    };
    let out = 0xff00_0000 | mix(16) << 16 | mix(8) << 8 | mix(0);
    canvas[index..index + 4].copy_from_slice(&out.to_le_bytes());
}

fn word(canvas: &mut [u8], width: u32, mut x: u32, y: u32, text: &str, color: u32) {
    for ch in text.chars() {
        if ch == ' ' {
            x += 6;
            continue;
        }
        if ch == '…' {
            for offset in [0, 2, 4] {
                glyph(canvas, width, x + offset, y, b'.', color);
            }
            x += 6;
            continue;
        }
        let fallback = match ch {
            '–' | '—' | '−' => b'-',
            '‘' | '’' => b'\'',
            '“' | '”' => b'"',
            '·' | '•' => b'.',
            '×' => b'x',
            '←' | '≤' => b'<',
            '→' | '≥' => b'>',
            _ if ch.is_ascii() => ch as u8,
            _ => b'?',
        };
        glyph(canvas, width, x, y, fallback, color);
        x += 6;
    }
}

fn glyph(canvas: &mut [u8], width: u32, x: u32, y: u32, ch: u8, color: u32) {
    let rows = match ch.to_ascii_uppercase() {
        b'!' => [
            0b00100, 0b00100, 0b00100, 0b00100, 0b00100, 0b00000, 0b00100,
        ],
        b'"' => [
            0b01010, 0b01010, 0b01010, 0b00000, 0b00000, 0b00000, 0b00000,
        ],
        b'#' => [
            0b01010, 0b11111, 0b01010, 0b01010, 0b11111, 0b01010, 0b01010,
        ],
        b'$' => [
            0b00100, 0b01111, 0b10100, 0b01110, 0b00101, 0b11110, 0b00100,
        ],
        b'%' => [
            0b11001, 0b11010, 0b00100, 0b01000, 0b10110, 0b00110, 0b00000,
        ],
        b'&' => [
            0b01100, 0b10010, 0b10100, 0b01000, 0b10101, 0b10010, 0b01101,
        ],
        b'\'' => [
            0b00100, 0b00100, 0b00010, 0b00000, 0b00000, 0b00000, 0b00000,
        ],
        b'(' => [
            0b00010, 0b00100, 0b01000, 0b01000, 0b01000, 0b00100, 0b00010,
        ],
        b')' => [
            0b01000, 0b00100, 0b00010, 0b00010, 0b00010, 0b00100, 0b01000,
        ],
        b'*' => [
            0b00000, 0b10101, 0b01110, 0b11111, 0b01110, 0b10101, 0b00000,
        ],
        b'+' => [
            0b00000, 0b00100, 0b00100, 0b11111, 0b00100, 0b00100, 0b00000,
        ],
        b',' => [
            0b00000, 0b00000, 0b00000, 0b00000, 0b00110, 0b00100, 0b01000,
        ],
        b'-' => [
            0b00000, 0b00000, 0b00000, 0b11111, 0b00000, 0b00000, 0b00000,
        ],
        b'.' => [
            0b00000, 0b00000, 0b00000, 0b00000, 0b00000, 0b00110, 0b00110,
        ],
        b'/' => [
            0b00001, 0b00010, 0b00010, 0b00100, 0b01000, 0b01000, 0b10000,
        ],
        b'A' => [
            0b01110, 0b10001, 0b10001, 0b11111, 0b10001, 0b10001, 0b10001,
        ],
        b'B' => [
            0b11110, 0b10001, 0b10001, 0b11110, 0b10001, 0b10001, 0b11110,
        ],
        b'C' => [
            0b01111, 0b10000, 0b10000, 0b10000, 0b10000, 0b10000, 0b01111,
        ],
        b'D' => [
            0b11110, 0b10001, 0b10001, 0b10001, 0b10001, 0b10001, 0b11110,
        ],
        b'E' => [
            0b11111, 0b10000, 0b10000, 0b11110, 0b10000, 0b10000, 0b11111,
        ],
        b'F' => [
            0b11111, 0b10000, 0b10000, 0b11110, 0b10000, 0b10000, 0b10000,
        ],
        b'G' => [
            0b01111, 0b10000, 0b10000, 0b10111, 0b10001, 0b10001, 0b01110,
        ],
        b'H' => [
            0b10001, 0b10001, 0b10001, 0b11111, 0b10001, 0b10001, 0b10001,
        ],
        b'I' => [
            0b01110, 0b00100, 0b00100, 0b00100, 0b00100, 0b00100, 0b01110,
        ],
        b'J' => [
            0b00111, 0b00010, 0b00010, 0b00010, 0b10010, 0b10010, 0b01100,
        ],
        b'K' => [
            0b10001, 0b10010, 0b10100, 0b11000, 0b10100, 0b10010, 0b10001,
        ],
        b'L' => [
            0b10000, 0b10000, 0b10000, 0b10000, 0b10000, 0b10000, 0b11111,
        ],
        b'M' => [
            0b10001, 0b11011, 0b10101, 0b10101, 0b10001, 0b10001, 0b10001,
        ],
        b'N' => [
            0b10001, 0b11001, 0b10101, 0b10011, 0b10001, 0b10001, 0b10001,
        ],
        b'O' => [
            0b01110, 0b10001, 0b10001, 0b10001, 0b10001, 0b10001, 0b01110,
        ],
        b'P' => [
            0b11110, 0b10001, 0b10001, 0b11110, 0b10000, 0b10000, 0b10000,
        ],
        b'Q' => [
            0b01110, 0b10001, 0b10001, 0b10001, 0b10101, 0b10010, 0b01101,
        ],
        b'R' => [
            0b11110, 0b10001, 0b10001, 0b11110, 0b10100, 0b10010, 0b10001,
        ],
        b'S' => [
            0b01111, 0b10000, 0b10000, 0b01110, 0b00001, 0b00001, 0b11110,
        ],
        b'T' => [
            0b11111, 0b00100, 0b00100, 0b00100, 0b00100, 0b00100, 0b00100,
        ],
        b'U' => [
            0b10001, 0b10001, 0b10001, 0b10001, 0b10001, 0b10001, 0b01110,
        ],
        b'V' => [
            0b10001, 0b10001, 0b10001, 0b10001, 0b10001, 0b01010, 0b00100,
        ],
        b'W' => [
            0b10001, 0b10001, 0b10001, 0b10101, 0b10101, 0b10101, 0b01010,
        ],
        b'X' => [
            0b10001, 0b10001, 0b01010, 0b00100, 0b01010, 0b10001, 0b10001,
        ],
        b'Y' => [
            0b10001, 0b10001, 0b01010, 0b00100, 0b00100, 0b00100, 0b00100,
        ],
        b'Z' => [
            0b11111, 0b00001, 0b00010, 0b00100, 0b01000, 0b10000, 0b11111,
        ],
        b'0' => [
            0b01110, 0b10001, 0b10011, 0b10101, 0b11001, 0b10001, 0b01110,
        ],
        b'1' => [
            0b00100, 0b01100, 0b00100, 0b00100, 0b00100, 0b00100, 0b01110,
        ],
        b'2' => [
            0b01110, 0b10001, 0b00001, 0b00010, 0b00100, 0b01000, 0b11111,
        ],
        b'3' => [
            0b11110, 0b00001, 0b00001, 0b01110, 0b00001, 0b00001, 0b11110,
        ],
        b'4' => [
            0b00010, 0b00110, 0b01010, 0b10010, 0b11111, 0b00010, 0b00010,
        ],
        b'5' => [
            0b11111, 0b10000, 0b10000, 0b11110, 0b00001, 0b00001, 0b11110,
        ],
        b'6' => [
            0b01110, 0b10000, 0b10000, 0b11110, 0b10001, 0b10001, 0b01110,
        ],
        b'7' => [
            0b11111, 0b00001, 0b00010, 0b00100, 0b01000, 0b01000, 0b01000,
        ],
        b'8' => [
            0b01110, 0b10001, 0b10001, 0b01110, 0b10001, 0b10001, 0b01110,
        ],
        b'9' => [
            0b01110, 0b10001, 0b10001, 0b01111, 0b00001, 0b00001, 0b01110,
        ],
        b':' => [
            0b00000, 0b00110, 0b00110, 0b00000, 0b00110, 0b00110, 0b00000,
        ],
        b';' => [
            0b00000, 0b00110, 0b00110, 0b00000, 0b00110, 0b00100, 0b01000,
        ],
        b'<' => [
            0b00010, 0b00100, 0b01000, 0b10000, 0b01000, 0b00100, 0b00010,
        ],
        b'=' => [
            0b00000, 0b11111, 0b00000, 0b11111, 0b00000, 0b00000, 0b00000,
        ],
        b'>' => [
            0b01000, 0b00100, 0b00010, 0b00001, 0b00010, 0b00100, 0b01000,
        ],
        b'?' => [
            0b01110, 0b10001, 0b00001, 0b00010, 0b00100, 0b00000, 0b00100,
        ],
        b'@' => [
            0b01110, 0b10001, 0b10111, 0b10101, 0b10111, 0b10000, 0b01110,
        ],
        b'[' => [
            0b01110, 0b01000, 0b01000, 0b01000, 0b01000, 0b01000, 0b01110,
        ],
        b'\\' => [
            0b10000, 0b01000, 0b01000, 0b00100, 0b00010, 0b00010, 0b00001,
        ],
        b']' => [
            0b01110, 0b00010, 0b00010, 0b00010, 0b00010, 0b00010, 0b01110,
        ],
        b'^' => [
            0b00000, 0b00100, 0b01010, 0b10001, 0b00000, 0b00000, 0b00000,
        ],
        b'_' => [
            0b00000, 0b00000, 0b00000, 0b00000, 0b00000, 0b00000, 0b11111,
        ],
        b'`' => [
            0b01000, 0b00100, 0b00010, 0b00000, 0b00000, 0b00000, 0b00000,
        ],
        b'{' => [
            0b00010, 0b00100, 0b00100, 0b01000, 0b00100, 0b00100, 0b00010,
        ],
        b'|' => [
            0b00100, 0b00100, 0b00100, 0b00100, 0b00100, 0b00100, 0b00100,
        ],
        b'}' => [
            0b01000, 0b00100, 0b00100, 0b00010, 0b00100, 0b00100, 0b01000,
        ],
        b'~' => [
            0b00000, 0b00000, 0b01001, 0b10110, 0b00000, 0b00000, 0b00000,
        ],
        _ => [0, 0, 0, 0, 0, 0, 0],
    };
    for (row, bits) in rows.into_iter().enumerate() {
        for column in 0..5 {
            if bits & (1 << (4 - column)) != 0 {
                rect(canvas, width, x + column, y + row as u32, 1, 1, color);
            }
        }
    }
}
fn title_text(
    canvas: &mut [u8],
    width: u32,
    x: u32,
    y: u32,
    title: &str,
    available: usize,
    color: u32,
    font: Option<&FontArc>,
    font_size: u32,
) {
    text(
        canvas,
        width,
        font,
        x,
        y,
        title,
        font_size,
        color,
        available as u32,
    );
}

fn scroll_steps(axis: smithay_client_toolkit::seat::pointer::AxisScroll) -> i32 {
    if axis.discrete != 0 {
        axis.discrete
    } else if axis.value120 != 0 {
        axis.value120 / 120
    } else {
        axis.absolute.signum() as i32
    }
}

impl CompositorHandler for App {
    fn scale_factor_changed(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        _: i32,
    ) {
    }
    fn transform_changed(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        _: wl_output::Transform,
    ) {
    }
    fn frame(
        &mut self,
        _: &Connection,
        qh: &QueueHandle<Self>,
        surface: &wl_surface::WlSurface,
        _: u32,
    ) {
        let Some(index) = self
            .surfaces
            .iter()
            .position(|candidate| candidate.layer.wl_surface() == surface)
        else {
            return;
        };
        self.surfaces[index].frame_pending = false;
        if self.surfaces[index].redraw_requested {
            self.surfaces[index].redraw_requested = false;
            self.draw(index, qh);
        }
    }
    fn surface_enter(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        _: &wl_output::WlOutput,
    ) {
    }
    fn surface_leave(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        _: &wl_output::WlOutput,
    ) {
    }
}

impl OutputHandler for App {
    fn output_state(&mut self) -> &mut OutputState {
        &mut self.output_state
    }
    fn new_output(&mut self, _: &Connection, qh: &QueueHandle<Self>, output: wl_output::WlOutput) {
        if !matches!(self.mode, Mode::Launcher | Mode::Recorder) {
            self.add_surface(qh, Some(output));
        }
    }
    fn update_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
    fn output_destroyed(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        output: wl_output::WlOutput,
    ) {
        self.surfaces
            .retain(|surface| surface.output.as_ref() != Some(&output));
    }
}

impl LayerShellHandler for App {
    fn closed(&mut self, _: &Connection, _: &QueueHandle<Self>, layer: &LayerSurface) {
        self.surfaces.retain(|surface| &surface.layer != layer);
        if self.surfaces.is_empty() {
            self.exit = true;
        }
    }
    fn configure(
        &mut self,
        _: &Connection,
        qh: &QueueHandle<Self>,
        layer: &LayerSurface,
        configure: LayerSurfaceConfigure,
        _: u32,
    ) {
        if let Some(index) = self
            .surfaces
            .iter()
            .position(|surface| &surface.layer == layer)
        {
            let surface = &mut self.surfaces[index];
            surface.width = NonZeroU32::new(configure.new_size.0).map_or(680, NonZeroU32::get);
            let default_height = match surface.kind {
                SurfaceKind::Bar => self.config.shell.height as u32,
                SurfaceKind::Wallpaper => 1,
                SurfaceKind::Launcher => 480,
                SurfaceKind::Recorder => 500,
                SurfaceKind::Notifications => 320,
                SurfaceKind::TrayMenu => 1,
                SurfaceKind::Controls => 300,
            };
            surface.height =
                NonZeroU32::new(configure.new_size.1).map_or(default_height, NonZeroU32::get);
            let minimum_pool = 4 * 1024 * 1024usize;
            let required = (surface.width as usize)
                .checked_mul(surface.height as usize)
                .and_then(|pixels| pixels.checked_mul(4))
                .unwrap_or(minimum_pool);
            let pool_size = required.saturating_mul(2).max(minimum_pool);
            if pool_size > surface.pool_size {
                match SlotPool::new(pool_size, &self.shm) {
                    Ok(pool) => {
                        surface.pool = pool;
                        surface.pool_size = pool_size;
                    }
                    Err(error) => eprintln!("wm-shell-sctk: allocate layer SHM pool: {error}"),
                }
            }
            surface.configured = true;
            self.draw(index, qh);
        }
    }
}

impl SeatHandler for App {
    fn seat_state(&mut self) -> &mut SeatState {
        &mut self.seat_state
    }
    fn new_seat(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_seat::WlSeat) {}
    fn new_capability(
        &mut self,
        _: &Connection,
        qh: &QueueHandle<Self>,
        seat: wl_seat::WlSeat,
        capability: Capability,
    ) {
        if matches!(self.mode, Mode::Launcher | Mode::Recorder)
            && capability == Capability::Keyboard
            && self.keyboard.is_none()
        {
            self.keyboard = self.seat_state.get_keyboard(qh, &seat, None).ok();
        }
        if capability == Capability::Pointer && self.pointer.is_none() {
            self.pointer = self.seat_state.get_pointer(qh, &seat).ok();
        }
    }
    fn remove_capability(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: wl_seat::WlSeat,
        capability: Capability,
    ) {
        if capability == Capability::Keyboard {
            if let Some(keyboard) = self.keyboard.take() {
                keyboard.release();
            }
        }
        if capability == Capability::Pointer {
            if let Some(pointer) = self.pointer.take() {
                pointer.release();
            }
        }
    }
    fn remove_seat(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_seat::WlSeat) {}
}

impl KeyboardHandler for App {
    fn enter(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        _: &wl_surface::WlSurface,
        _: u32,
        _: &[u32],
        _: &[Keysym],
    ) {
    }
    fn leave(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        _: &wl_surface::WlSurface,
        _: u32,
    ) {
    }
    fn press_key(
        &mut self,
        _: &Connection,
        qh: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        _: u32,
        event: KeyEvent,
    ) {
        if self.mode == Mode::Recorder {
            if event.keysym == Keysym::Escape {
                if self.recorder_view == RecorderView::Settings {
                    // First Escape cancels a path edit, then leaves Settings;
                    // only on the Controls page does it close the panel.
                    if self.recorder_path_edit.take().is_none() {
                        self.recorder_view = RecorderView::Controls;
                    }
                    self.redraw_all(qh);
                    return;
                }
                self.exit = true;
                return;
            }
            if self.recorder_view == RecorderView::Settings {
                match event.keysym {
                    Keysym::Tab => {
                        self.recorder_settings_tab = self.recorder_settings_tab.cycle(true);
                        self.recorder_settings_row = self
                            .recorder_settings_row
                            .min(self.recorder_settings_row_count().saturating_sub(1));
                        self.recorder_path_edit = None;
                    }
                    Keysym::Up => {
                        self.recorder_settings_row = self.recorder_settings_row.saturating_sub(1);
                        self.recorder_path_edit = None;
                    }
                    Keysym::Down => {
                        self.recorder_settings_row = self
                            .recorder_settings_row
                            .saturating_add(1)
                            .min(self.recorder_settings_row_count().saturating_sub(1));
                        self.recorder_path_edit = None;
                    }
                    Keysym::Left => self.apply_recorder_adjustment(-1),
                    Keysym::Right => self.apply_recorder_adjustment(1),
                    Keysym::Return => {
                        if self.recorder_identity_editing() {
                            self.commit_recorder_text_edit();
                        } else {
                            match self.recorder_editable_row_kind() {
                                Some(RecorderRowKind::Path) => {
                                    let directory = self
                                        .snapshot
                                        .recorder_settings
                                        .output_directory
                                        .clone();
                                    self.recorder_path_edit = Some(directory);
                                }
                                Some(RecorderRowKind::Identity) => {
                                    self.recorder_path_edit =
                                        Some(self.recorder_identity_edit_seed());
                                }
                                Some(RecorderRowKind::Action) => {
                                    self.remember_current_recorder_source();
                                }
                                _ => self.apply_recorder_adjustment(1),
                            }
                        }
                    }
                    Keysym::BackSpace => {
                        if let Some(buffer) = self.recorder_path_edit.as_mut() {
                            buffer.pop();
                        }
                    }
                    Keysym::space => {}
                    _ => {
                        if let (Some(buffer), Some(text)) =
                            (self.recorder_path_edit.as_mut(), event.utf8.as_ref())
                        {
                            buffer.push_str(text);
                        }
                    }
                }
                self.redraw_all(qh);
                return;
            }
            match event.keysym {
                Keysym::Tab => {
                    self.recorder_view = RecorderView::Settings;
                    self.recorder_settings_row = self
                        .recorder_settings_row
                        .min(self.recorder_settings_row_count().saturating_sub(1));
                }
                Keysym::Return => {
                    if self.snapshot.recorder.state != RecorderState::Idle
                        && self.snapshot.recorder.state != RecorderState::Error
                    {
                        Self::run_command("recorder stop".into());
                        self.exit = true;
                    } else if self.recorder_can_start() {
                        if let Some(command) = self.recorder_start_command() {
                            Self::run_command(command);
                            self.exit = true;
                        }
                    }
                }
                Keysym::Left => self.cycle_recorder_capture_mode(false),
                Keysym::Right => self.cycle_recorder_capture_mode(true),
                Keysym::Up => {
                    self.move_recorder_selection(false);
                }
                Keysym::Down => {
                    self.move_recorder_selection(true);
                }
                Keysym::space => Self::run_command("recorder pause".into()),
                Keysym::r | Keysym::R => {}
                _ => {}
            }
            self.redraw_all(qh);
            return;
        }
        if event.keysym == Keysym::Escape {
            self.exit = true;
            return;
        }
        if self.mode != Mode::Launcher {
            return;
        }
        if event.keysym == Keysym::BackSpace {
            self.launcher_query.pop();
            self.launcher_selected = 0;
        } else if event.keysym == Keysym::Up {
            self.launcher_selected = self.launcher_selected.saturating_sub(1);
        } else if event.keysym == Keysym::Down {
            let count = self.launcher_items().len();
            if count > 0 {
                self.launcher_selected = (self.launcher_selected + 1).min(count - 1);
            }
        } else if event.keysym == Keysym::Return {
            self.activate_launcher_item();
        } else if let Some(text) = event.utf8 {
            self.launcher_query.push_str(&text);
            self.launcher_selected = 0;
        }
        self.redraw_all(qh);
    }
    fn repeat_key(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        _: u32,
        _: KeyEvent,
    ) {
    }
    fn release_key(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        _: u32,
        _: KeyEvent,
    ) {
    }
    fn update_modifiers(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        _: u32,
        _: Modifiers,
        _: RawModifiers,
        _: u32,
    ) {
    }
}

impl PointerHandler for App {
    fn pointer_frame(
        &mut self,
        _: &Connection,
        qh: &QueueHandle<Self>,
        _: &wl_pointer::WlPointer,
        events: &[PointerEvent],
    ) {
        for event in events {
            if !self
                .surfaces
                .iter()
                .any(|surface| surface.layer.wl_surface() == &event.surface)
            {
                continue;
            }
            if let PointerEventKind::Press { button, .. } = event.kind {
                let kind = self
                    .surfaces
                    .iter()
                    .find(|surface| surface.layer.wl_surface() == &event.surface)
                    .map(|surface| surface.kind);
                match kind {
                    Some(SurfaceKind::Bar) if self.mode == Mode::Bar => {
                        self.click_bar(
                            &event.surface,
                            event.position.0,
                            event.position.1,
                            button,
                            qh,
                        );
                    }
                    Some(SurfaceKind::Notifications) => {
                        self.click_notification(event.position.0, event.position.1, button, qh);
                    }
                    Some(SurfaceKind::TrayMenu) => {
                        self.click_tray_menu(event.position.1, button, qh);
                    }
                    Some(SurfaceKind::Controls) => {
                        self.click_controls(event.position.0, event.position.1, button, qh);
                    }
                    Some(SurfaceKind::Recorder) if button == 0x110 => {
                        let hit = self.recorder_hit_at(event.position.0, event.position.1);
                        if !self.activate_recorder_hit(hit) {
                            self.redraw_all(qh);
                        }
                    }
                    Some(SurfaceKind::Launcher)
                        if self.mode == Mode::Launcher && button == 0x110 =>
                    {
                        self.click_launcher(event.position.0, event.position.1, qh);
                    }
                    _ => {}
                }
            }
            if let PointerEventKind::Axis {
                horizontal,
                vertical,
                ..
            } = event.kind
            {
                let steps = scroll_steps(vertical);
                if self.mode == Mode::Bar {
                    self.scroll_bar(&event.surface, event.position.0, steps);
                    let horizontal_steps = scroll_steps(horizontal);
                    if horizontal_steps != 0 {
                        if let Some(item) = self.tray_item_at(&event.surface, event.position.0) {
                            Self::scroll_tray_item(
                                item.service,
                                item.path,
                                horizontal_steps,
                                "horizontal",
                            );
                        }
                    }
                }
                let notification_surface = self.surfaces.iter().any(|surface| {
                    surface.layer.wl_surface() == &event.surface
                        && surface.kind == SurfaceKind::Notifications
                });
                if notification_surface {
                    self.scroll_notifications(steps, qh);
                }
            }
        }
    }
}

impl ShmHandler for App {
    fn shm_state(&mut self) -> &mut Shm {
        &mut self.shm
    }
}

delegate_compositor!(App);
delegate_output!(App);
delegate_shm!(App);
delegate_seat!(App);
delegate_keyboard!(App);
delegate_pointer!(App);
delegate_layer!(App);
delegate_registry!(App);

impl ProvidesRegistryState for App {
    fn registry(&mut self) -> &mut RegistryState {
        &mut self.registry_state
    }
    registry_handlers![OutputState, SeatState];
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fallback_font_draws_visible_ascii_glyphs() {
        for ch in b'!'..=b'~' {
            let mut canvas = vec![0; 5 * 7 * 4];
            glyph(&mut canvas, 5, 0, 0, ch, 0xffff_ffff);
            assert!(
                canvas.chunks_exact(4).any(|pixel| pixel[3] != 0),
                "fallback glyph {:?} should contain pixels",
                ch as char
            );
        }
    }

    #[test]
    fn recorder_settings_rows_cover_every_editable_field_per_tab() {
        let settings = wm_core::Recorder::default();
        let output = recorder_settings_rows(&settings, RecorderSettingsTab::Output);
        assert_eq!(output.len(), 6);
        assert_eq!(output[5].kind, RecorderRowKind::Path);
        assert_eq!(recorder_settings_rows(&settings, RecorderSettingsTab::Capture).len(), 4);
        assert_eq!(recorder_settings_rows(&settings, RecorderSettingsTab::Video).len(), 5);
        assert_eq!(recorder_settings_rows(&settings, RecorderSettingsTab::Audio).len(), 3);
        assert_eq!(recorder_settings_rows(&settings, RecorderSettingsTab::Replay).len(), 3);

        // The Capture tab remembers text identity, never session-local IDs.
        let mut remembered = wm_core::Recorder::default();
        remembered.capture_mode = "xwayland".into();
        remembered.window_app_id = "Minecraft".into();
        let capture = recorder_settings_rows(&remembered, RecorderSettingsTab::Capture);
        assert_eq!(capture[0].value, "Xwayland window");
        assert_eq!(capture[1].value, "Minecraft");
        assert_eq!(capture[1].kind, RecorderRowKind::Identity);
        assert_eq!(capture[2].kind, RecorderRowKind::Action);
        assert_eq!(capture[3].kind, RecorderRowKind::Info);

        let window = wm_core::WindowInfo {
            id: 7,
            x11_window: None,
            title: "Minecraft 1.21".into(),
            app_id: "minecraft-launcher".into(),
            workspace: 1,
            output: "DP-1".into(),
            floating: false,
            fullscreen: false,
            scratchpad: false,
            geometry: None,
            opacity: 1.0,
            surface_size: None,
        };
        assert!(window_matches_settings(&window, &remembered));
        let mut other = remembered.clone();
        other.window_app_id = "firefox".into();
        assert!(!window_matches_settings(&window, &other));
    }

    #[test]
    fn recorder_setting_adjustments_stay_inside_compositor_limits() {
        let mut settings = wm_core::Recorder::default();
        assert_eq!(settings.quality, 20);

        let commands = adjust_recorder_setting(
            &settings,
            RecorderSettingsTab::Output,
            1,
            1,
        )
        .unwrap();
        assert_eq!(commands, vec![("quality".to_string(), "21".to_string())]);

        // The stepper clamps instead of wrapping past the compositor limits.
        settings.quality = 51;
        let commands = adjust_recorder_setting(
            &settings,
            RecorderSettingsTab::Output,
            1,
            1,
        )
        .unwrap();
        assert_eq!(commands, vec![("quality".to_string(), "51".to_string())]);

        // Informational rows and the path row do not adjust with Left/Right.
        assert!(
            adjust_recorder_setting(&settings, RecorderSettingsTab::Output, 4, 1)
                .unwrap_err()
                .contains("container")
        );
        assert!(
            adjust_recorder_setting(&settings, RecorderSettingsTab::Output, 5, 1)
                .unwrap_err()
                .contains("Enter")
        );

        // HDR on forces HEVC; switching back to H.264 drops HDR first.
        settings.codec = "hevc".into();
        settings.hdr = true;
        let commands = adjust_recorder_setting(
            &settings,
            RecorderSettingsTab::Output,
            0,
            1,
        )
        .unwrap();
        assert_eq!(
            commands,
            vec![
                ("hdr".to_string(), "false".to_string()),
                ("codec".to_string(), "h264".to_string())
            ]
        );

        // FPS stepping clamps at the engine range.
        settings.screen_fps = 480;
        let commands = adjust_recorder_setting(
            &settings,
            RecorderSettingsTab::Video,
            0,
            1,
        )
        .unwrap();
        assert_eq!(
            commands,
            vec![("screen_fps".to_string(), "480".to_string())]
        );
        let commands = adjust_recorder_setting(
            &settings,
            RecorderSettingsTab::Video,
            0,
            -1,
        )
        .unwrap();
        assert_eq!(
            commands,
            vec![("screen_fps".to_string(), "450".to_string())]
        );

        // Audio sources cycle between disabled and the default device.
        let commands = adjust_recorder_setting(
            &settings,
            RecorderSettingsTab::Audio,
            0,
            1,
        )
        .unwrap();
        assert_eq!(
            commands,
            vec![("desktop_audio".to_string(), "disabled".to_string())]
        );

        // The recording method cycles through the stable mode list.
        let commands = adjust_recorder_setting(
            &settings,
            RecorderSettingsTab::Capture,
            0,
            1,
        )
        .unwrap();
        assert_eq!(
            commands,
            vec![("capture_mode".to_string(), "xwayland".to_string())]
        );
        assert!(
            adjust_recorder_setting(&settings, RecorderSettingsTab::Capture, 1, 1)
                .unwrap_err()
                .contains("Enter")
        );
    }

    #[test]
    fn recorder_start_blocker_matches_each_method_and_encoder_limit() {
        let mut settings = wm_core::Recorder::default();
        assert_eq!(settings.codec, "h264");

        // Screen capture accepts either encoder.
        assert_eq!(
            recorder_start_blocker_for(RecorderCaptureMode::Screen, &settings, false, false, 0, false),
            None
        );

        // Regression: Xwayland capture is not H.264-only, so the shipped HEVC
        // default must not block it (it previously reported a bogus
        // "no matching API profile configured").
        settings.codec = "hevc".into();
        assert_eq!(
            recorder_start_blocker_for(
                RecorderCaptureMode::XwaylandDirect,
                &settings,
                true,
                false,
                0,
                false
            ),
            None
        );
        assert_eq!(
            recorder_start_blocker_for(
                RecorderCaptureMode::XwaylandDirect,
                &settings,
                false,
                false,
                0,
                false
            ),
            Some("NO XWAYLAND WINDOW OPEN")
        );

        // Direct graphics-API paths always encode their own H.264 SDR stream,
        // so an HDR/HEVC screen configuration cannot block their launch.
        assert_eq!(
            recorder_start_blocker_for(
                RecorderCaptureMode::OpenGlInject,
                &settings,
                false,
                true,
                0,
                false
            ),
            None
        );
        assert_eq!(
            recorder_start_blocker_for(
                RecorderCaptureMode::OpenGlGame,
                &settings,
                false,
                false,
                1,
                true
            ),
            None
        );
        assert_eq!(
            recorder_start_blocker_for(
                RecorderCaptureMode::VulkanGame,
                &settings,
                false,
                false,
                0,
                false
            ),
            Some("NO MATCHING API PROFILE CONFIGURED")
        );

        // A real selection makes every direct method startable.
        for (mode, has_target, profiles, has_profile) in [
            (RecorderCaptureMode::OpenGlInject, true, 0, false),
            (RecorderCaptureMode::OpenGlGame, false, 1, true),
            (RecorderCaptureMode::VulkanGame, false, 2, true),
        ] {
            assert_eq!(
                recorder_start_blocker_for(mode, &settings, true, has_target, profiles, has_profile),
                None,
                "{mode:?}"
            );
        }

        // A disabled recorder is called out before any method detail.
        settings.enabled = false;
        assert_eq!(
            recorder_start_blocker_for(
                RecorderCaptureMode::Screen,
                &settings,
                true,
                true,
                1,
                true
            ),
            Some("RECORDER DISABLED IN SETTINGS")
        );
    }

    #[test]
    fn recorder_panel_and_hit_geometry_stay_panel_sized() {
        let (x, y, panel_w, panel_h) = recorder_panel(1920, 1080);
        assert_eq!((x, y, panel_w, panel_h), (600, 290, 720, 500));
        let buttons = recorder_control_button_rects(panel_w);
        assert_eq!(buttons.len(), 4);
        for (rect, _) in &buttons {
            assert!(rect[0] + rect[2] <= panel_w - 24, "button escapes panel");
        }
        // The last Output row (6 rows) still clears the footer strip.
        let last_row = recorder_settings_row_rect(5, panel_w);
        assert!(last_row[1] + last_row[3] <= 448 - 4);
        // Five category tabs stay stacked above the reset button.
        let last_tab = recorder_settings_tab_rect(RecorderSettingsTab::ALL.len() - 1);
        let reset = recorder_settings_reset_rect();
        assert!(last_tab[1] + last_tab[3] <= reset[1]);
    }

    #[test]
    fn recorder_capture_modes_cycle_without_treating_game_capture_as_screen_capture() {
        assert_eq!(
            RecorderCaptureMode::Screen.cycle(true),
            RecorderCaptureMode::XwaylandDirect
        );
        assert_eq!(
            RecorderCaptureMode::XwaylandDirect.cycle(true),
            RecorderCaptureMode::OpenGlInject
        );
        assert_eq!(
            RecorderCaptureMode::OpenGlInject.cycle(true),
            RecorderCaptureMode::OpenGlGame
        );
        assert_eq!(
            RecorderCaptureMode::OpenGlGame.cycle(true),
            RecorderCaptureMode::VulkanGame
        );
    }

    #[test]
    fn contain_wallpaper_centers_and_preserves_image_pixels() {
        let image = image::RgbaImage::from_raw(2, 1, vec![255, 0, 0, 255, 0, 0, 255, 255]).unwrap();
        let mut canvas = vec![0; 4 * 4 * 4];
        draw_image(&mut canvas, 4, 4, &image, "contain");
        let pixel = |x: usize, y: usize| {
            let offset = (y * 4 + x) * 4;
            u32::from_le_bytes(canvas[offset..offset + 4].try_into().unwrap())
        };
        assert_eq!(pixel(0, 0), 0xff00_0000);
        assert_eq!(pixel(0, 1), 0xffff_0000);
        assert_eq!(pixel(3, 1), 0xff00_00ff);
        assert_eq!(pixel(0, 3), 0xff00_0000);
    }

    #[test]
    fn static_wallpaper_kind_loads_a_configured_image() {
        assert_eq!(wallpaper_byte_len(4, 2), Some(32));
        assert!(wallpaper_byte_len(u32::MAX, u32::MAX).is_none());
        let path = std::env::temp_dir().join(format!(
            "luma-sctk-wallpaper-{}-{}.png",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        image::RgbaImage::from_raw(1, 1, vec![1, 2, 3, 4])
            .unwrap()
            .save(&path)
            .unwrap();
        let alternate = path.with_file_name(format!(
            "luma-sctk-wallpaper-{}-alternate.png",
            std::process::id()
        ));
        image::RgbaImage::from_raw(1, 1, vec![5, 6, 7, 8])
            .unwrap()
            .save(&alternate)
            .unwrap();
        let mut config = Config::default();
        config.wallpaper.kind = "static".into();
        config.wallpaper.path = path.to_string_lossy().into_owned();
        config
            .wallpaper
            .outputs
            .insert("TEST-1".into(), alternate.to_string_lossy().into_owned());
        let wallpapers = load_wallpapers(&config);
        assert_eq!(wallpapers.get("").unwrap().get_pixel(0, 0).0, [1, 2, 3, 4]);
        assert_eq!(
            wallpapers.get("TEST-1").unwrap().get_pixel(0, 0).0,
            [5, 6, 7, 8]
        );
        std::fs::remove_file(path).unwrap();
        std::fs::remove_file(alternate).unwrap();
    }

    #[test]
    fn desktop_entry_parser_keeps_quoted_arguments_and_removes_field_codes() {
        let entry = parse_desktop_entry_text(
            "[Desktop Entry]\nType=Application\nName=Example App\nExec=example --title 'hello world' %U %%\n",
        )
        .unwrap();
        assert_eq!(entry.name, "Example App");
        assert_eq!(entry.exec, ["example", "--title", "hello world", "%"]);
        assert!(
            parse_desktop_entry_text(
                "[Desktop Entry]\nType=Application\nNoDisplay=true\nName=Hidden\nExec=hidden"
            )
            .is_none()
        );
    }

    #[test]
    fn tray_monitor_ignores_non_visual_status_notifier_signals() {
        assert!(tray_signal_needs_refresh(Some("NewIcon")));
        assert!(tray_signal_needs_refresh(Some("PropertiesChanged")));
        assert!(tray_signal_needs_refresh(Some("NewStatus")));
        assert!(!tray_signal_needs_refresh(Some("NewToolTip")));
        assert!(!tray_signal_needs_refresh(Some("Activate")));
        assert!(!tray_signal_needs_refresh(None));
    }

    #[test]
    fn tray_owner_monitor_removes_items_when_their_original_owner_exits() {
        assert!(tray_service_owner_lost(
            "org.example.App",
            ":1.42",
            "",
            "org.example.App",
            ":1.42",
        ));
        assert!(tray_service_owner_lost(
            "org.example.App",
            ":1.42",
            ":1.99",
            "org.example.App",
            ":1.42",
        ));
        assert!(!tray_service_owner_lost(
            "org.example.Other",
            ":1.42",
            "",
            "org.example.App",
            ":1.42",
        ));
        assert!(!tray_service_owner_lost(
            "org.example.App",
            ":1.99",
            "",
            "org.example.App",
            ":1.42",
        ));
    }

    #[test]
    fn native_tray_menu_parses_visible_actions_and_submenus() {
        use zbus::zvariant::{OwnedValue, Str, Structure};

        let child = |id, properties: HashMap<String, OwnedValue>| {
            OwnedValue::try_from(Structure::from((id, properties, Vec::<OwnedValue>::new())))
                .unwrap()
        };
        let rows = parse_tray_menu_layout(
            (
                0,
                HashMap::new(),
                vec![
                    child(
                        1,
                        HashMap::from([
                            ("label".into(), OwnedValue::from(Str::from("_Open"))),
                            ("enabled".into(), OwnedValue::from(true)),
                        ]),
                    ),
                    child(
                        2,
                        HashMap::from([
                            ("label".into(), OwnedValue::from(Str::from("More"))),
                            (
                                "children-display".into(),
                                OwnedValue::from(Str::from("submenu")),
                            ),
                        ]),
                    ),
                    child(
                        3,
                        HashMap::from([("visible".into(), OwnedValue::from(false))]),
                    ),
                ],
            ),
            0,
        )
        .unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].label, "_Open");
        assert!(rows[0].enabled);
        assert!(rows[1].submenu);
    }

    #[test]
    fn fullscreen_windows_suspend_video_wallpaper_decoding() {
        let mut snapshot = Snapshot::default();
        assert!(!video_wallpaper_is_suspended(&snapshot, true, false));
        assert!(video_wallpaper_is_suspended(&snapshot, true, true));
        assert!(!video_wallpaper_is_suspended(&snapshot, false, true));
        snapshot.windows.push(wm_core::WindowInfo {
            id: 1,
            x11_window: None,
            title: "Fullscreen".into(),
            app_id: "test".into(),
            workspace: 1,
            output: "TEST-1".into(),
            floating: false,
            fullscreen: true,
            scratchpad: false,
            geometry: None,
            opacity: 1.0,
            surface_size: None,
        });
        assert!(video_wallpaper_is_suspended(&snapshot, false, false));
    }

    #[test]
    fn bar_snapshot_ignores_animation_only_geometry_and_opacity() {
        let mut previous = Snapshot::default();
        previous.focused = Some(1);
        previous.windows.push(wm_core::WindowInfo {
            id: 1,
            x11_window: None,
            title: "Terminal".into(),
            app_id: "terminal".into(),
            workspace: 1,
            output: "TEST-1".into(),
            floating: false,
            fullscreen: false,
            scratchpad: false,
            geometry: Some(wm_core::Rect {
                x: 0,
                y: 0,
                w: 800,
                h: 600,
            }),
            opacity: 1.0,
            surface_size: None,
        });
        previous.outputs.push(wm_core::OutputInfo {
            name: "TEST-1".into(),
            workspace: 1,
            geometry: wm_core::Rect {
                x: 0,
                y: 0,
                w: 1920,
                h: 1080,
            },
            wallpaper_visible: true,
            active: true,
        });
        assert_eq!(visible_bar_title(&previous, 1, Some("TEST-1")), "Terminal");
        assert_eq!(visible_bar_title(&previous, 2, Some("TEST-1")), "Luma");
        assert_eq!(visible_bar_title(&previous, 1, Some("OTHER")), "Luma");
        let mut animated = previous.clone();
        animated.windows[0].geometry.as_mut().unwrap().x = 64;
        animated.windows[0].opacity = 0.5;
        assert!(!bar_snapshot_changed(&previous, &animated));

        let mut retitled = animated.clone();
        retitled.windows[0].title = "Editor".into();
        assert!(bar_snapshot_changed(&previous, &retitled));

        let mut switched = animated;
        switched.outputs[0].workspace = 2;
        assert!(bar_snapshot_changed(&previous, &switched));
    }

    #[test]
    fn right_modules_keep_their_draw_order_for_pointer_actions() {
        let values = vec![
            ("audio", "AUDIO".to_string()),
            ("media", "MEDIA".to_string()),
            ("clock", "CLOCK".to_string()),
        ];
        // The renderer reverses this list: clock is nearest the right group edge,
        // then media, then audio. Keep hit-testing aligned with those pixels.
        let rects = bar_module_layout(200, 24, None, 12, 0, 0, &values);
        let module_at = |x: f64| {
            rects
                .iter()
                .find(|(_, left, right)| x >= f64::from(*left) && x <= f64::from(*right))
                .map(|(module, _, _)| *module)
        };
        assert_eq!(module_at(150.0), Some("clock"));
        assert_eq!(module_at(100.0), Some("media"));
        assert_eq!(module_at(45.0), Some("audio"));
        assert_eq!(module_at(139.0), None);
    }

    #[test]
    fn network_bar_keeps_connection_names_in_the_control_panel() {
        assert_eq!(compact_network_bar_label("NET docker0"), "NET");
        assert_eq!(compact_network_bar_label("WIFI Home"), "WIFI");
        assert_eq!(compact_network_bar_label("NET OFF"), "NET OFF");
    }

    #[test]
    fn bar_module_layout_sizes_the_tray_by_icon_space() {
        let values = vec![("tray", "TRAY 3".to_string())];
        // With no font the text estimate is 6 chars * 6 = 36, but three 24 px
        // icons need 3 * (12 + 2) + 8 = 50 px, which must win over the text.
        let rects = bar_module_layout(200, 24, None, 12, 3, 0, &values);
        assert_eq!(rects, vec![("tray", 122, 192)]);
    }

    #[test]
    fn narrow_bar_keeps_status_chips_out_of_the_workspace_island() {
        let values = vec![
            ("media", "MEDIA —".to_string()),
            ("audio", "VOL 100%".to_string()),
            ("clock", "Wed 17:45".to_string()),
            ("power", "POWER".to_string()),
        ];
        let left_guard = BAR_WORKSPACE_START + BAR_WORKSPACE_STEP * 9 + 16;
        let rects = bar_module_layout(480, 42, None, 14, 0, left_guard, &values);
        assert!(rects.iter().all(|(_, left, _)| *left >= left_guard));
        assert!(rects.iter().any(|(module, _, _)| *module == "power"));
    }

    #[test]
    fn panels_draw_a_muted_border_inside_the_background_fill() {
        let mut canvas = vec![0; 100 * 60 * 4];
        let colors = Colors {
            background: 0xff11_2233,
            foreground: 0xffee_eeee,
            accent: 0xff88_aaff,
            muted: 0xff88_8899,
            radius: 8,
        };
        panel(&mut canvas, 100, 2, 2, 96, 56, 8, colors);
        let pixel = |x: usize, y: usize| {
            u32::from_le_bytes(canvas[(y * 100 + x) * 4..(y * 100 + x + 1) * 4].try_into().unwrap())
        };
        // The interior keeps the exact background fill.
        assert_eq!(pixel(50, 30), 0xff11_2233);
        // The border ring carries a quiet muted tint on the panel edge pixels.
        assert_ne!(pixel(2, 30), 0xff11_2233);
        assert_ne!(pixel(97, 30), 0xff11_2233);
        assert_eq!(pixel(2, 30) >> 24, 0xff);
    }

    #[test]
    fn launcher_shows_search_categories_and_highlights_the_selected_row() {
        let mut canvas = vec![0; 680 * 420 * 4];
        let colors = Colors {
            background: 0xff11_2233,
            foreground: 0xffee_eeee,
            accent: 0xff88_aaff,
            muted: 0xff88_8899,
            radius: 12,
        };
        draw_launcher(
            &mut canvas,
            680,
            420,
            colors,
            12,
            None,
            13,
            "",
            &["Files".into(), "Editor".into()],
            1,
            true,
        );
        let pixel = |x: usize, y: usize| {
            u32::from_le_bytes(
                canvas[(y * 680 + x) * 4..(y * 680 + x + 1) * 4]
                    .try_into()
                    .unwrap(),
            )
        };
        // The rounded inset field is visibly layered over the panel fill.
        assert_ne!(pixel(640, 90), 0xff11_2233);
        // The first row stays quiet; selection adds a distinct accent surface.
        let unselected = pixel(580, 200);
        let selected = pixel(580, 235);
        assert!(unselected.abs_diff(0xff11_2233) <= 1);
        assert_ne!(selected, 0xff11_2233);
        // The category chips stay inside the fixed panel's top content region.
        assert_ne!(pixel(108, 147), 0xff11_2233);
    }

    #[test]
    fn launcher_pointer_targets_match_drawn_categories_and_visible_result_rows() {
        assert_eq!(launcher_panel_rect(680, 420), [0, 0, 680, 420]);
        for (category, _, rect) in launcher_category_regions(680, 420) {
            let center_x = (rect[0] + rect[2] / 2) as f64;
            let center_y = (rect[1] + rect[3] / 2) as f64;
            assert_eq!(
                launcher_pointer_hit(680, 420, center_x, center_y, 2),
                Some(LauncherHit::Category(category))
            );
        }

        let first_row = launcher_result_row_rect(680, 420, 0).unwrap();
        assert_eq!(first_row, [20, 185, 640, 30]);
        assert_eq!(launcher_result_row_rect(680, 420, 5), None);
        assert_eq!(
            launcher_pointer_hit(
                680,
                420,
                (first_row[0] + first_row[2] / 2) as f64,
                (first_row[1] + first_row[3] / 2) as f64,
                2,
            ),
            Some(LauncherHit::Result(0))
        );

        // The gap between chips and space below the final visible result are
        // intentionally inert, so a click cannot select a neighboring mode.
        assert_eq!(launcher_pointer_hit(680, 420, 142.0, 150.0, 2), None);
        assert_eq!(launcher_pointer_hit(680, 420, 340.0, 270.0, 2), None);
        assert_eq!(launcher_pointer_hit(680, 420, f64::NAN, 200.0, 2), None);
    }

    #[test]
    fn launcher_category_clicks_switch_modes_and_preserve_the_search_term() {
        assert_eq!(launcher_category_for_query("mail"), LauncherCategory::Apps);
        assert_eq!(
            launcher_category_for_query(" @mail"),
            LauncherCategory::Windows
        );
        assert_eq!(
            launcher_category_for_query("> ls"),
            LauncherCategory::Commands
        );
        assert_eq!(
            launcher_category_for_query(":lock"),
            LauncherCategory::Power
        );
        assert_eq!(
            launcher_query_for_category(LauncherCategory::Windows, "mail"),
            "@mail"
        );
        assert_eq!(
            launcher_query_for_category(LauncherCategory::Commands, "@mail"),
            "> mail"
        );
        assert_eq!(
            launcher_query_for_category(LauncherCategory::Power, "> mail"),
            ":mail"
        );
        assert_eq!(
            launcher_query_for_category(LauncherCategory::Apps, ":mail"),
            "mail"
        );
        assert_eq!(
            launcher_query_for_category(LauncherCategory::Commands, ""),
            "> "
        );
    }

    #[test]
    fn control_buttons_tint_active_rows_with_the_accent() {
        let colors = Colors {
            background: 0xff11_2233,
            foreground: 0xffee_eeee,
            accent: 0xff88_aaff,
            muted: 0xff88_8899,
            radius: 10,
        };
        let audio = AudioState {
            label: "VOL 50%".into(),
        };
        let network = NetworkState {
            label: "WIFI Luma".into(),
            networking_enabled: true,
            wireless_enabled: true,
        };
        let media = MediaState {
            label: "MEDIA Track".into(),
        };
        let bluetooth = BluetoothState {
            label: "BT 1".into(),
            powered: Some(true),
        };
        let mut canvas = vec![0; 380 * 300 * 4];
        draw_controls(
            &mut canvas,
            380,
            300,
            colors,
            None,
            13,
            Some(ControlPanel::Power),
            Some(PowerAction::LogOut),
            &audio,
            &network,
            &media,
            &bluetooth,
            false,
            2,
        );
        let pixel = |x: usize, y: usize| {
            u32::from_le_bytes(canvas[(y * 380 + x) * 4..(y * 380 + x + 1) * 4].try_into().unwrap())
        };
        // The active confirm button (76..=110) is accent tinted, while the
        // inactive cancel button (120..=154) keeps a muted chip.
        let active = pixel(190, 93);
        let inactive = pixel(190, 137);
        assert_ne!(active, 0xff11_2233);
        assert_ne!(inactive, 0xff11_2233);
        assert_ne!(active, inactive);
    }

    #[test]
    fn control_slider_paints_a_trough_highlight_and_knob() {
        let colors = Colors {
            background: 0xff11_2233,
            foreground: 0xffee_eeee,
            accent: 0xff88_aaff,
            muted: 0xff88_8899,
            radius: 10,
        };
        let audio = AudioState {
            label: "VOL 50%".into(),
        };
        let network = NetworkState {
            label: "WIFI Luma".into(),
            networking_enabled: true,
            wireless_enabled: true,
        };
        let media = MediaState {
            label: "MEDIA Track".into(),
        };
        let bluetooth = BluetoothState {
            label: "BT 1".into(),
            powered: Some(true),
        };
        let mut canvas = vec![0; 380 * 300 * 4];
        draw_controls(
            &mut canvas,
            380,
            300,
            colors,
            None,
            13,
            Some(ControlPanel::Audio),
            None,
            &audio,
            &network,
            &media,
            &bluetooth,
            false,
            2,
        );
        let pixel = |x: usize, y: usize| {
            u32::from_le_bytes(canvas[(y * 380 + x) * 4..(y * 380 + x + 1) * 4].try_into().unwrap())
        };
        // At 50% volume the trough is accent from 24..214 and muted from
        // 214..332; the knob sits near the middle of the trough.
        let highlighted = pixel(100, 74);
        let remaining = pixel(300, 74);
        assert_ne!(highlighted, 0xff11_2233);
        assert_ne!(remaining, 0xff11_2233);
        assert_ne!(highlighted, remaining);
    }

    #[test]
    fn scroll_steps_prefers_discrete_then_high_resolution_then_touchpad_motion() {
        let mut axis = smithay_client_toolkit::seat::pointer::AxisScroll::default();
        axis.discrete = -2;
        axis.value120 = 240;
        assert_eq!(scroll_steps(axis), -2);
        axis.discrete = 0;
        assert_eq!(scroll_steps(axis), 2);
        axis.value120 = 0;
        axis.absolute = -0.5;
        assert_eq!(scroll_steps(axis), -1);
    }

    #[test]
    fn notification_body_markup_becomes_bounded_plain_text() {
        assert_eq!(
            notification_body_text("<b>Hello</b> &amp; <i>goodbye</i>"),
            "Hello & goodbye"
        );
        assert_eq!(notification_body_text("a &lt; b &gt; c"), "a < b > c");
        assert_eq!(notification_summary_text("<b>Summary</b>"), "Summary");
        assert_eq!(
            notification_body_text(&"x".repeat(3000)).chars().count(),
            2048
        );
    }

    #[test]
    fn config_errors_create_a_persistent_critical_notification() {
        let notification = config_error_notification("invalid binding: Supers+space");
        assert_eq!(notification.id, CONFIG_ERROR_NOTIFICATION_ID);
        assert_eq!(notification.summary, "Configuration warning");
        assert_eq!(notification.body, "invalid binding: Supers+space");
        assert!(notification.critical);
        assert!(notification.expires_at.is_none());
    }

    #[test]
    fn application_notifications_expire_after_five_seconds() {
        let now = Instant::now();
        assert_eq!(
            application_notification_expiry(now).duration_since(now),
            Duration::from_secs(5)
        );
    }

    #[test]
    fn notification_actions_follow_the_icon_adjusted_text_column() {
        let notification = Notification {
            id: 1,
            icon: None,
            summary: String::new(),
            body: String::new(),
            actions: Vec::new(),
            expires_at: None,
            critical: false,
        };
        assert_eq!(notification_text_x(&notification), 18);
        let notification = Notification {
            icon: Some(image::RgbaImage::from_pixel(1, 1, image::Rgba([0; 4]))),
            ..notification
        };
        assert_eq!(notification_text_x(&notification), 60);
    }

    #[test]
    fn unavailable_audio_has_an_explicit_bar_label() {
        assert_eq!(AudioState::default().label, "AUDIO —");
    }

    #[test]
    fn notification_icon_accepts_a_bounded_absolute_image_path() {
        let path = std::env::temp_dir().join(format!(
            "luma-sctk-notification-{}-{}.png",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        image::RgbaImage::from_raw(1, 1, vec![7, 8, 9, 255])
            .unwrap()
            .save(&path)
            .unwrap();
        assert_eq!(
            load_notification_icon(path.to_str().unwrap())
                .unwrap()
                .get_pixel(0, 0)
                .0,
            [7, 8, 9, 255]
        );
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn notification_icons_clip_on_a_small_canvas() {
        let icon = image::RgbaImage::from_raw(1, 1, vec![1, 2, 3, 255]).unwrap();
        let mut canvas = vec![0; 4];
        draw_icon(&mut canvas, 1, 8, 8, 32, &icon);
        assert_eq!(canvas, vec![0; 4]);
    }

    #[test]
    fn icons_are_centered_without_stretching() {
        let icon = image::RgbaImage::from_raw(2, 1, vec![1, 2, 3, 255, 4, 5, 6, 255]).unwrap();
        let mut canvas = vec![0; 4 * 4 * 4];
        draw_icon(&mut canvas, 4, 0, 0, 4, &icon);

        let pixel = |x: usize, y: usize| &canvas[(y * 4 + x) * 4..(y * 4 + x + 1) * 4];
        assert_eq!(pixel(0, 0), [0, 0, 0, 0]);
        assert_eq!(pixel(0, 1), [3, 2, 1, 255]);
        assert_eq!(pixel(3, 1), [6, 5, 4, 255]);
        assert_eq!(pixel(0, 3), [0, 0, 0, 0]);
    }

    #[test]
    fn icon_pixels_are_composited_as_premultiplied_alpha() {
        let mut canvas = 0xff00_00ff_u32.to_le_bytes().to_vec();
        blend_rgba_pixel(&mut canvas, 0, 255, 0, 0, 128);
        assert_eq!(u32::from_le_bytes(canvas.try_into().unwrap()), 0xff80_007f);
    }

    #[test]
    fn bluetooth_label_distinguishes_power_and_connections() {
        assert_eq!(bluetooth_label(Some(false), 2), "BT OFF");
        assert_eq!(bluetooth_label(Some(true), 0), "BT");
        assert_eq!(bluetooth_label(None, 3), "BT 3");
    }

    #[test]
    fn external_bar_labels_are_bounded_and_single_line() {
        assert_eq!(bar_label_text("  Track\nName\t", 64), "Track Name");
        assert_eq!(bar_label_text(&"x".repeat(100), 8), "xxxxxxxx");
        assert_eq!(bar_label_text("text", 0), "");
    }

    #[test]
    fn tray_pixmaps_are_bounded_and_convert_argb_to_rgba() {
        let icon = tray_pixmap(vec![(1, 1, vec![0x80, 0x11, 0x22, 0x33])]).unwrap();
        assert_eq!(icon.get_pixel(0, 0).0, [0x11, 0x22, 0x33, 0x80]);
        assert!(tray_pixmap(vec![(257, 1, vec![0; 257 * 4])]).is_none());
        assert!(tray_pixmap(vec![(2, 2, vec![0; 3])]).is_none());
        assert!(load_named_tray_icon("../../untrusted", None).is_none());

        let directory = std::env::temp_dir().join(format!(
            "luma-sctk-icon-theme-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir(&directory).unwrap();
        let icon_path = directory.join("luma-sctk-fixture.png");
        image::RgbaImage::from_raw(1, 1, vec![8, 16, 32, 255])
            .unwrap()
            .save(&icon_path)
            .unwrap();
        assert_eq!(
            load_named_tray_icon("luma-sctk-fixture", directory.to_str())
                .unwrap()
                .get_pixel(0, 0)
                .0,
            [8, 16, 32, 255]
        );
        assert!(load_named_tray_icon("luma-sctk-fixture", Some("relative-theme")).is_none());
        std::fs::remove_file(icon_path).unwrap();
        std::fs::remove_dir(directory).unwrap();
    }

    #[test]
    fn tray_overlay_is_composited_at_the_bottom_right() {
        let icon = image::RgbaImage::from_raw(2, 2, vec![1, 2, 3, 255].repeat(4)).unwrap();
        let overlay = image::RgbaImage::from_raw(1, 1, vec![9, 8, 7, 255]).unwrap();
        let icon = overlay_tray_icon(icon, Some(overlay));
        assert_eq!(icon.get_pixel(0, 0).0, [1, 2, 3, 255]);
        assert_eq!(icon.get_pixel(1, 1).0, [9, 8, 7, 255]);

        let icon = image::RgbaImage::from_raw(1, 1, vec![0, 0, 255, 255]).unwrap();
        let overlay = image::RgbaImage::from_raw(1, 1, vec![255, 0, 0, 128]).unwrap();
        assert_eq!(
            overlay_tray_icon(icon, Some(overlay)).get_pixel(0, 0).0,
            [128, 0, 127, 255]
        );
    }

    #[test]
    fn bar_paints_a_launcher_island_with_transparent_space_between_groups() {
        let mut canvas = vec![0; 200 * 40 * 4];
        let colors = Colors {
            background: 0xff11_2233,
            foreground: 0xffee_eeee,
            accent: 0xff88_aaff,
            muted: 0xff88_8899,
            radius: 10,
        };
        draw_bar(
            &mut canvas,
            200,
            40,
            colors,
            1,
            "",
            None,
            13,
            &[],
            0,
            None,
            None,
            None,
            None,
            None,
            None,
            &[],
            None,
            None,
            None,
            None,
        );
        let pixel = |x: usize, y: usize| {
            u32::from_le_bytes(
                canvas[(y * 200 + x) * 4..(y * 200 + x + 1) * 4]
                    .try_into()
                    .unwrap(),
            )
        };
        assert_eq!(pixel(0, 0), 0);
        assert_ne!(pixel(20, 20), 0);
        assert_eq!(pixel(100, 20), 0);
    }

    #[test]
    fn native_control_panels_paint_real_controls() {
        let colors = Colors {
            background: 0xff11_2233,
            foreground: 0xffee_eeee,
            accent: 0xff88_aaff,
            muted: 0xff88_8899,
            radius: 10,
        };
        let audio = AudioState {
            label: "VOL 50%".into(),
        };
        let network = NetworkState {
            label: "WIFI Luma".into(),
            networking_enabled: true,
            wireless_enabled: true,
        };
        let media = MediaState {
            label: "MEDIA Track".into(),
        };
        let bluetooth = BluetoothState {
            label: "BT 1".into(),
            powered: Some(true),
        };
        for panel in [
            ControlPanel::Audio,
            ControlPanel::Network,
            ControlPanel::Bluetooth,
            ControlPanel::Media,
            ControlPanel::Notifications,
            ControlPanel::Power,
        ] {
            let mut canvas = vec![0; 380 * 300 * 4];
            draw_controls(
                &mut canvas,
                380,
                300,
                colors,
                None,
                13,
                Some(panel),
                None,
                &audio,
                &network,
                &media,
                &bluetooth,
                false,
                2,
            );
            let center = u32::from_le_bytes(
                canvas[(150 * 380 + 190) * 4..(150 * 380 + 191) * 4]
                    .try_into()
                    .unwrap(),
            );
            assert_ne!(center, 0, "{panel:?} panel should not be blank");
        }
    }

    #[test]
    fn rounded_rect_antialiases_corner_coverage() {
        let mut canvas = vec![0; 10 * 10 * 4];
        rounded_rect(&mut canvas, 10, 0, 0, 10, 10, 4, 0xff11_2233);
        let pixel = |x: usize, y: usize| {
            u32::from_le_bytes(
                canvas[(y * 10 + x) * 4..(y * 10 + x + 1) * 4]
                    .try_into()
                    .unwrap(),
            )
        };
        let edge_alpha = pixel(1, 0) >> 24;
        assert!((1..255).contains(&edge_alpha));
        assert_eq!(pixel(5, 0), 0xff11_2233);
        assert_eq!(pixel(0, 0), 0);
    }

    #[test]
    fn translucent_rect_composites_without_punching_through_its_parent() {
        let mut canvas = 0xff00_00ff_u32.to_le_bytes().to_vec();
        rect(&mut canvas, 1, 0, 0, 1, 1, 0x3300_0000);
        assert_eq!(u32::from_le_bytes(canvas.try_into().unwrap()), 0xff00_00cc);
    }
}
