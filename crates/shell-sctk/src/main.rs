//! Low-overhead native Luma shell.
//!
//! This client deliberately has no GTK, GObject, or browser runtime.  It is
//! still opt-in while the feature-complete GTK shell remains the default.

use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
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
use wm_core::{Config, Snapshot};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Mode {
    Bar,
    Wallpaper,
    Launcher,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SurfaceKind {
    Bar,
    Wallpaper,
    Launcher,
    Notifications,
}

impl Mode {
    fn from_args() -> Self {
        if std::env::args().any(|arg| arg == "--wallpaper") {
            Self::Wallpaper
        } else if std::env::args().any(|arg| arg == "--launcher") {
            Self::Launcher
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
    let (icon, visible) = (|| {
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
        Some((icon.map(|icon| overlay_tray_icon(icon, overlay)), visible))
    })()
    .unwrap_or((None, true));
    TrayItem {
        id,
        service,
        path,
        icon,
        visible,
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
        let Ok(signals) = item.receive_all_signals() else {
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
        items
            .lock()
            .expect("tray item list poisoned")
            .retain(|registered| registered != &id);
        let _ = sender.send(TrayEvent::Remove(id));
    });
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
        expire_timeout: i32,
    ) -> u32 {
        let id = if replaces_id == 0 {
            self.next_id
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
                .wrapping_add(1)
                .max(1)
        } else {
            replaces_id
        };
        let expires_at = match expire_timeout {
            timeout if timeout > 0 => {
                Some(std::time::Instant::now() + Duration::from_millis(timeout as u64))
            }
            0 => None,
            _ => Some(std::time::Instant::now() + Duration::from_secs(6)),
        };
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
    tray: Vec<TrayItem>,
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
    let (desktop_entries_sender, desktop_entries_receiver) =
        channel::channel::<Vec<DesktopEntry>>();
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
            wireless_enabled: false,
        },
        media: MediaState {
            label: "MEDIA —".into(),
        },
        bluetooth: BluetoothState {
            label: "BT —".into(),
            powered: None,
        },
        tray: Vec::new(),
        exit: false,
    };

    handle
        .insert_source(desktop_entries_receiver, |event, _, app| {
            if let channel::Event::Msg(entries) = event {
                app.apps = entries;
                app.apps_loaded = true;
                app.launcher_selected = app
                    .launcher_selected
                    .min(app.launcher_items().len().saturating_sub(1));
                if app.mode == Mode::Launcher {
                    app.redraw_all(&qh);
                }
            }
        })
        .map_err(|error| error.to_string())?;
    thread::spawn(move || {
        let _ = desktop_entries_sender.send(load_desktop_entries());
    });
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
                        app.snapshot = snapshot;
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
                        app.redraw_all(&qh);
                    }
                }
                channel::Event::Closed => {}
            })
            .map_err(|error| error.to_string())?;
        spawn_subscription(sender);
    }

    if mode == Mode::Bar {
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
                            let critical = notification.critical;
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
                            if critical || !app.do_not_disturb.load(Ordering::Relaxed) {
                                app.ensure_notification_surface(&notification_timer_qh);
                            }
                            if let Some(expiry) = expiry {
                                let qh = notification_timer_qh.clone();
                                let timer = notification_timer_handle.insert_source(
                                    Timer::from_deadline(expiry),
                                    move |_, _, app| {
                                        app.notification_timers.remove(&id);
                                        if expire_notifications(app, Instant::now()) {
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
                    app.video_frame = Some(frame);
                    app.redraw_all(&qh);
                }
            })
            .map_err(|error| error.to_string())?;
        if app.config.wallpaper.kind == "video" {
            spawn_video_wallpaper(
                app.config.wallpaper.path.clone(),
                app.config.wallpaper.fps,
                video_sender,
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

    if mode == Mode::Launcher {
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
                wireless_enabled: wireless,
            };
        }
    }
    NetworkState {
        label: if wireless { "WIFI" } else { "NET" }.into(),
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
                "-stream_loop",
                "-1",
                "-i",
                &path,
                "-vf",
                &format!("fps={}", fps.clamp(1, 60)),
                "-f",
                "rawvideo",
                "-pix_fmt",
                "rgba",
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
            let mut pixels = vec![0; frame_size];
            if stdout.read_exact(&mut pixels).is_err() {
                let _ = child.wait();
                return;
            }
            if generation.load(Ordering::Relaxed) != expected_generation {
                let _ = child.kill();
                let _ = child.wait();
                return;
            }
            match sender.try_send(
                image::RgbaImage::from_raw(width, height, pixels).expect("validated frame size"),
            ) {
                Ok(()) | Err(std::sync::mpsc::TrySendError::Full(_)) => {}
                Err(std::sync::mpsc::TrySendError::Disconnected(_)) => {
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

    fn run_command(command: String) {
        thread::spawn(move || {
            let _ = wm_core::connect_command(&command);
        });
    }

    fn bar_module_at(&self, surface: &wl_surface::WlSurface, x: f64) -> Option<&'static str> {
        let width = self
            .surfaces
            .iter()
            .find(|candidate| candidate.layer.wl_surface() == surface)
            .map(|surface| surface.width)?;
        let modules = &self.config.shell.modules;
        let values = [
            modules
                .iter()
                .any(|module| module == "clock")
                .then(|| ("clock", self.clock.clone())),
            modules
                .iter()
                .any(|module| module == "battery")
                .then(|| self.battery.clone())
                .flatten()
                .map(|value| ("battery", value)),
            modules
                .iter()
                .any(|module| module == "audio")
                .then(|| ("audio", self.audio.label.clone())),
            modules
                .iter()
                .any(|module| module == "network")
                .then(|| ("network", self.network.label.clone())),
            modules
                .iter()
                .any(|module| module == "media")
                .then(|| ("media", self.media.label.clone())),
            modules
                .iter()
                .any(|module| module == "bluetooth")
                .then(|| ("bluetooth", self.bluetooth.label.clone())),
            modules
                .iter()
                .any(|module| module == "tray")
                .then(|| ("tray", format!("TRAY {}", self.tray.len()))),
            modules
                .iter()
                .any(|module| module == "notifications")
                .then(|| ("notifications", format!("NOT {}", self.notifications.len()))),
        ]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>();
        right_module_at(
            width,
            self.font.as_ref(),
            self.config.theme.font_size,
            &values,
            x,
        )
    }

    fn tray_item_at(&self, surface: &wl_surface::WlSurface, x: f64) -> Option<(String, String)> {
        let surface = self
            .surfaces
            .iter()
            .find(|candidate| candidate.layer.wl_surface() == surface)?;
        let modules = &self.config.shell.modules;
        if !modules.iter().any(|module| module == "tray") {
            return None;
        }
        let values = [
            modules
                .iter()
                .any(|module| module == "clock")
                .then(|| self.clock.clone()),
            modules
                .iter()
                .any(|module| module == "battery")
                .then(|| battery_state().0)
                .flatten(),
            modules
                .iter()
                .any(|module| module == "audio")
                .then(|| self.audio.label.clone()),
            modules
                .iter()
                .any(|module| module == "network")
                .then(|| self.network.label.clone()),
            modules
                .iter()
                .any(|module| module == "media")
                .then(|| self.media.label.clone()),
            modules
                .iter()
                .any(|module| module == "bluetooth")
                .then(|| self.bluetooth.label.clone()),
            Some(format!("TRAY {}", self.tray.len())),
            modules
                .iter()
                .any(|module| module == "notifications")
                .then(|| format!("NOT {}", self.notifications.len())),
        ];
        let mut right = surface.width.saturating_sub(12);
        for (index, value) in values.into_iter().enumerate().rev() {
            let Some(value) = value else {
                continue;
            };
            let icon_size = surface.height.saturating_sub(12).clamp(12, 24);
            let tray_width = self
                .tray
                .iter()
                .filter(|item| item.has_visible_icon())
                .take(6)
                .count() as u32
                * (icon_size + 2)
                + 8;
            let value_width = text_width(self.font.as_ref(), &value, self.config.theme.font_size)
                .max((index == 6).then_some(tray_width).unwrap_or(0))
                .min(right.saturating_sub(12));
            let left = right.saturating_sub(value_width);
            if index == 6 && x >= left as f64 && x < right as f64 {
                let icon_index =
                    ((x as u32).saturating_sub(left.saturating_add(4)) / (icon_size + 2)) as usize;
                return self
                    .tray
                    .iter()
                    .filter(|item| item.has_visible_icon())
                    .take(6)
                    .nth(icon_index)
                    .map(|item| (item.service.clone(), item.path.clone()));
            }
            right = left.saturating_sub(16);
        }
        None
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
        if x < 12.0 {
            if button == 0x110 {
                Self::run_command("launcher".into());
            }
            return;
        }
        if let Some((service, path)) = self.tray_item_at(surface, x) {
            let (action_x, action_y) = self.tray_action_position(surface, x, y);
            match button {
                0x110 => Self::activate_tray_item(service, path, "Activate", action_x, action_y),
                0x111 => Self::activate_tray_item(service, path, "ContextMenu", action_x, action_y),
                0x112 => {
                    Self::activate_tray_item(service, path, "SecondaryActivate", action_x, action_y)
                }
                _ => {}
            }
            return;
        }
        let workspace = ((x as u32).saturating_sub(12) / 25 + 1) as u8;
        if workspace > self.config.layout.workspaces
            || x >= 12.0 + 25.0 * self.config.layout.workspaces as f64
        {
            match (self.bar_module_at(surface, x), button) {
                (Some("audio"), 0x110) => {
                    Self::change_audio(&["set-sink-mute", "@DEFAULT_SINK@", "toggle"]);
                }
                (Some("media"), 0x110) => Self::change_media("PlayPause"),
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
                    let was_enabled = self.do_not_disturb.fetch_xor(true, Ordering::Relaxed);
                    if was_enabled && !self.notifications.is_empty() {
                        self.ensure_notification_surface(qh);
                    }
                    self.redraw_all(qh);
                }
                _ => {}
            }
            return;
        }
        if button != 0x110 {
            return;
        }
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

    fn scroll_bar(&self, surface: &wl_surface::WlSurface, x: f64, steps: i32) {
        if steps == 0 {
            return;
        }
        if let Some((service, path)) = self.tray_item_at(surface, x) {
            Self::scroll_tray_item(service, path, steps, "vertical");
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
                        notification.actions.iter().find_map(|(key, label)| {
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
        let dnd_disabled = self.config.shell.do_not_disturb && !config.shell.do_not_disturb;
        self.config = config;
        self.do_not_disturb
            .store(self.config.shell.do_not_disturb, Ordering::Relaxed);
        self.update_video_wallpaper_playback();
        if dnd_disabled && !self.notifications.is_empty() {
            self.ensure_notification_surface(qh);
        }
        if wallpaper_changed && self.mode == Mode::Wallpaper {
            self.wallpapers = load_wallpapers(&self.config);
            self.video_frame = None;
            let generation = self.video_generation.fetch_add(1, Ordering::Relaxed) + 1;
            if self.mode == Mode::Wallpaper
                && self.config.wallpaper.kind == "video"
                && !self.video_suspended
            {
                spawn_video_wallpaper(
                    self.config.wallpaper.path.clone(),
                    self.config.wallpaper.fps,
                    self.video_sender.clone(),
                    self.video_generation.clone(),
                    generation,
                );
            }
        }
        if font_changed && self.mode != Mode::Wallpaper {
            self.font = load_font(&self.config);
        }
        for surface in &self.surfaces {
            if self.mode == Mode::Bar {
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
        };
        self.add_kind_surface(qh, kind, output);
    }

    fn ensure_notification_surface(&mut self, qh: &QueueHandle<Self>) {
        if !self
            .surfaces
            .iter()
            .any(|surface| surface.kind == SurfaceKind::Notifications)
        {
            self.add_kind_surface(qh, SurfaceKind::Notifications, None);
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
            SurfaceKind::Notifications => (Layer::Overlay, "wm-notifications"),
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
            SurfaceKind::Notifications => {
                layer.set_anchor(Anchor::TOP | Anchor::RIGHT);
                layer.set_margin(48, 12, 0, 0);
                layer.set_size(380, 320);
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
            output,
        });
    }

    fn redraw_all(&mut self, qh: &QueueHandle<Self>) {
        for index in 0..self.surfaces.len() {
            if self.surfaces[index].configured {
                self.draw(index, qh);
            }
        }
    }

    fn draw(&mut self, index: usize, qh: &QueueHandle<Self>) {
        let colors = Colors::from_config(&self.config);
        let title = self
            .snapshot
            .focused
            .and_then(|id| self.snapshot.windows.iter().find(|window| window.id == id))
            .map(|window| bar_label_text(&window.title, 128))
            .filter(|title| !title.is_empty())
            .unwrap_or_else(|| "Luma".into());
        let active_workspace = self.surfaces[index]
            .output
            .as_ref()
            .and_then(|output| self.output_state.info(output))
            .and_then(|info| info.name)
            .and_then(|name| {
                self.snapshot
                    .outputs
                    .iter()
                    .find(|output| output.name == name)
                    .map(|output| output.workspace)
            })
            .unwrap_or(1);
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
            .then(|| self.network.label.as_str());
        let media = modules
            .iter()
            .any(|module| module == "media")
            .then(|| self.media.label.as_str());
        let bluetooth = modules
            .iter()
            .any(|module| module == "bluetooth")
            .then(|| self.bluetooth.label.as_str());
        let tray = modules
            .iter()
            .any(|module| module == "tray")
            .then(|| format!("TRAY {}", self.tray.len()));
        let notifications = modules
            .iter()
            .any(|module| module == "notifications")
            .then(|| {
                if self.do_not_disturb.load(Ordering::Relaxed) {
                    return "DND".into();
                }
                self.notifications.back().map_or_else(
                    || "NOT 0".into(),
                    |notification| {
                        let text = if notification.summary.is_empty() {
                            &notification.body
                        } else {
                            &notification.summary
                        };
                        format!("NOT {} · {}", self.notifications.len(), text)
                    },
                )
            });
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
        let wallpaper = if self.config.wallpaper.kind == "video" {
            self.video_frame.clone()
        } else {
            self.surfaces[index]
                .output
                .as_ref()
                .and_then(|output| self.output_state.info(output))
                .and_then(|info| info.name)
                .and_then(|name| self.wallpapers.get(&name))
                .or_else(|| self.wallpapers.get(""))
                .cloned()
        };
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
            ),
            SurfaceKind::Wallpaper => draw_wallpaper(
                canvas,
                width,
                height,
                colors,
                wallpaper.as_ref(),
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
        }
        surface
            .layer
            .wl_surface()
            .damage_buffer(0, 0, width as i32, height as i32);
        surface
            .layer
            .wl_surface()
            .frame(qh, surface.layer.wl_surface().clone());
        if buffer.attach_to(surface.layer.wl_surface()).is_ok() {
            surface.layer.commit();
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
) {
    fill(canvas, 0);
    let margin = 4.min(height.saturating_sub(1) / 2);
    let panel_height = height.saturating_sub(margin * 2);
    rounded_rect(
        canvas,
        width,
        margin,
        margin,
        width.saturating_sub(margin * 2),
        panel_height,
        colors.radius.min(panel_height / 2),
        colors.background,
    );
    rect(
        canvas,
        width,
        margin.saturating_add(colors.radius.min(width / 2)),
        margin.saturating_add(panel_height.saturating_sub(2)),
        width.saturating_sub(margin * 2 + colors.radius.min(width / 2) * 2),
        2,
        colors.accent,
    );
    let y = height.saturating_sub(14) / 2;
    let has = |name| modules.iter().any(|module| module == name);
    if has("workspaces") {
        for workspace in 1..=workspace_count {
            let x = 12 + (workspace - 1) as u32 * 25;
            let color = if workspace == active_workspace {
                colors.accent
            } else {
                colors.muted
            };
            rounded_rect(canvas, width, x, y, 18, 14, 5, color);
            digit(canvas, width, x + 6, y + 3, workspace, colors.background);
        }
    }
    let workspace_width = if has("workspaces") {
        12 + 25 * workspace_count as u32
    } else {
        12
    };
    text(
        canvas,
        width,
        font,
        workspace_width + 12,
        y + font_size.min(16),
        "Luma",
        font_size,
        colors.accent,
        54,
    );
    let title_x = workspace_width + 82;
    let right_reserved = 220;
    let available = width.saturating_sub(title_x + right_reserved) as usize;
    if has("title") {
        title_text(
            canvas,
            width,
            title_x,
            y + font_size.min(16),
            title,
            available,
            colors.foreground,
            font,
            font_size,
        );
    }
    let mut right = width.saturating_sub(12);
    let mut tray_bounds = None;
    for value in [
        clock,
        battery,
        audio,
        network,
        media,
        bluetooth,
        tray,
        notifications,
    ]
    .into_iter()
    .flatten()
    .rev()
    {
        let icon_width = height.saturating_sub(12).clamp(12, 24) + 2;
        let tray_width = tray_items
            .iter()
            .filter(|item| item.has_visible_icon())
            .take(6)
            .count() as u32
            * icon_width
            + 8;
        let value_width = text_width(font, value, font_size)
            .max(if tray == Some(value) { tray_width } else { 0 })
            .min(right.saturating_sub(12));
        right = right.saturating_sub(value_width);
        if tray == Some(value) {
            tray_bounds = Some((right, value_width));
        }
        if tray != Some(value) || !tray_items.iter().any(TrayItem::has_visible_icon) {
            text(
                canvas,
                width,
                font,
                right,
                y + font_size.min(16),
                value,
                font_size,
                colors.muted,
                value_width,
            );
        }
        right = right.saturating_sub(16);
    }
    if let Some((x, _tray_width)) = tray_bounds {
        draw_tray_icons(canvas, width, height, x, tray_items);
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
    let panel_w = width.min(680);
    let panel_h = height.min(420);
    let x = (width - panel_w) / 2;
    let y = (height - panel_h) / 2;
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
    text(
        canvas,
        width,
        font,
        x + 32,
        y + 34,
        "Luma",
        font_size + 4,
        colors.accent,
        200,
    );
    text(
        canvas,
        width,
        font,
        x + 32,
        y + 64,
        "Launcher",
        font_size,
        colors.foreground,
        300,
    );
    rect(
        canvas,
        width,
        x + 32,
        y + 98,
        panel_w.saturating_sub(64),
        2,
        colors.accent,
    );
    text(
        canvas,
        width,
        font,
        x + 32,
        y + 126,
        "SCTK shell",
        font_size,
        colors.muted,
        300,
    );
    text(
        canvas,
        width,
        font,
        x + 32,
        y + panel_h.saturating_sub(36),
        "ESC TO CLOSE",
        font_size,
        colors.muted,
        300,
    );
    rounded_rect(
        canvas,
        width,
        x + 24,
        y + 156,
        panel_w.saturating_sub(48),
        42,
        8,
        0x3300_0000,
    );
    let prompt = if query.is_empty() {
        "> type a command"
    } else {
        query
    };
    text(
        canvas,
        width,
        font,
        x + 40,
        y + 184,
        prompt,
        font_size,
        if query.is_empty() {
            colors.muted
        } else {
            colors.foreground
        },
        panel_w.saturating_sub(80),
    );
    if rows.is_empty() {
        let message = if apps_loaded || !query.is_empty() {
            "No matching applications"
        } else {
            "Loading applications…"
        };
        text(
            canvas,
            width,
            font,
            x + 40,
            y + 228,
            message,
            font_size,
            colors.muted,
            panel_w.saturating_sub(80),
        );
    }
    for (index, row) in rows.iter().enumerate() {
        let row_y = y + 216 + index as u32 * 28;
        if row_y + 22 > y + panel_h.saturating_sub(52) {
            break;
        }
        if index == selected {
            rounded_rect(
                canvas,
                width,
                x + 24,
                row_y.saturating_sub(16),
                panel_w.saturating_sub(48),
                24,
                6,
                0x334f_8cff,
            );
        }
        text(
            canvas,
            width,
            font,
            x + 40,
            row_y,
            row,
            font_size,
            colors.foreground,
            panel_w.saturating_sub(80),
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
    rounded_rect(
        canvas,
        width,
        0,
        0,
        width,
        panel_height,
        14,
        colors.background,
    );
    if muted && !rows.iter().any(|notification| notification.critical) {
        text(
            canvas,
            width,
            font,
            18,
            54,
            "Do Not Disturb is on",
            font_size,
            colors.muted,
            width.saturating_sub(36),
        );
        return;
    }
    if rows.is_empty() {
        text(
            canvas,
            width,
            font,
            18,
            34,
            "No notifications",
            font_size,
            colors.muted,
            width.saturating_sub(36),
        );
        return;
    }
    text(
        canvas,
        width,
        font,
        18,
        20,
        &format!("Notifications {}/{}", offset + 1, total),
        font_size.saturating_sub(2).max(9),
        colors.muted,
        width.saturating_sub(36),
    );
    for (index, notification) in rows.iter().enumerate() {
        let y = 40 + index as u32 * 88;
        let text_x = notification_text_x(notification);
        if let Some(icon) = notification.icon.as_ref() {
            draw_icon(canvas, width, 18, y.saturating_sub(14), 32, icon);
        }
        text(
            canvas,
            width,
            font,
            text_x,
            y,
            &notification.summary,
            font_size,
            colors.foreground,
            width.saturating_sub(text_x + 18),
        );
        text(
            canvas,
            width,
            font,
            text_x,
            y + 22,
            &notification.body,
            font_size.saturating_sub(2).max(9),
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
                5,
                0x3388_92a5,
            );
            text(
                canvas,
                width,
                font,
                action_x + 6,
                y + 44,
                label,
                font_size.saturating_sub(2).max(9),
                colors.foreground,
                action_width.saturating_sub(12),
            );
            action_x = action_x.saturating_add(action_width + 12);
        }
        if index + 1 < rows.len() {
            rect(
                canvas,
                width,
                18,
                y + 60,
                width.saturating_sub(36),
                1,
                0x3388_92a5,
            );
        }
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
            canvas[index..index + 4].copy_from_slice(&color.to_le_bytes());
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
                if coverage == u8::MAX {
                    canvas[index..index + 4].copy_from_slice(&color.to_le_bytes());
                } else {
                    blend_premultiplied_pixel(canvas, index, color, coverage);
                }
            }
        }
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

const DIGIT_SEGMENTS: [u8; 10] = [
    0b0111111, 0b0000110, 0b1011011, 0b1001111, 0b1100110, 0b1101101, 0b1111101, 0b0000111,
    0b1111111, 0b1101111,
];

fn digit_segments(value: u8) -> u8 {
    DIGIT_SEGMENTS[value.clamp(1, 9) as usize]
}

fn digit(canvas: &mut [u8], width: u32, x: u32, y: u32, value: u8, color: u32) {
    let segments = digit_segments(value);
    let lines = [
        (1, 0, 4, 1),
        (5, 1, 1, 4),
        (5, 6, 1, 4),
        (1, 10, 4, 1),
        (0, 6, 1, 4),
        (0, 1, 1, 4),
        (1, 5, 4, 1),
    ];
    for (index, (dx, dy, w, h)) in lines.into_iter().enumerate() {
        if segments & (1 << index) != 0 {
            rect(canvas, width, x + dx, y + dy, w, h, color);
        }
    }
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

fn right_module_at(
    width: u32,
    font: Option<&FontArc>,
    font_size: u32,
    values: &[(&'static str, String)],
    x: f64,
) -> Option<&'static str> {
    let mut right = width.saturating_sub(12);
    for (module, value) in values.iter().rev() {
        let value_width = text_width(font, value, font_size).min(right.saturating_sub(12));
        let left = right.saturating_sub(value_width);
        if x >= left as f64 && x <= right as f64 {
            return Some(*module);
        }
        right = left.saturating_sub(16);
    }
    None
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
    for ch in text.bytes() {
        if ch == b' ' {
            x += 6;
        } else {
            glyph(canvas, width, x, y, ch, color);
            x += 6;
        }
    }
}

fn glyph(canvas: &mut [u8], width: u32, x: u32, y: u32, ch: u8, color: u32) {
    let rows = match ch.to_ascii_uppercase() {
        b'A' => [
            0b01110, 0b10001, 0b10001, 0b11111, 0b10001, 0b10001, 0b10001,
        ],
        b'C' => [
            0b01111, 0b10000, 0b10000, 0b10000, 0b10000, 0b10000, 0b01111,
        ],
        b'E' => [
            0b11111, 0b10000, 0b10000, 0b11110, 0b10000, 0b10000, 0b11111,
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
        b'O' => [
            0b01110, 0b10001, 0b10001, 0b10001, 0b10001, 0b10001, 0b01110,
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
    fn frame(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_surface::WlSurface, _: u32) {}
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
        if self.mode != Mode::Launcher {
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
                SurfaceKind::Notifications => 320,
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
        if self.mode == Mode::Launcher
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
                        if let Some((service, path)) =
                            self.tray_item_at(&event.surface, event.position.0)
                        {
                            Self::scroll_tray_item(service, path, horizontal_steps, "horizontal");
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
    fn fullscreen_windows_suspend_video_wallpaper_decoding() {
        let mut snapshot = Snapshot::default();
        assert!(!video_wallpaper_is_suspended(&snapshot, true, false));
        assert!(video_wallpaper_is_suspended(&snapshot, true, true));
        assert!(!video_wallpaper_is_suspended(&snapshot, false, true));
        snapshot.windows.push(wm_core::WindowInfo {
            id: 1,
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
    fn right_modules_keep_their_draw_order_for_pointer_actions() {
        let values = vec![
            ("audio", "AUDIO".to_string()),
            ("media", "MEDIA".to_string()),
            ("clock", "CLOCK".to_string()),
        ];
        // The renderer reverses this list: clock is nearest the right edge,
        // then media, then audio. Keep hit-testing aligned with those pixels.
        assert_eq!(
            right_module_at(200, None, 12, &values, 180.0),
            Some("clock")
        );
        assert_eq!(
            right_module_at(200, None, 12, &values, 130.0),
            Some("media")
        );
        assert_eq!(right_module_at(200, None, 12, &values, 80.0), Some("audio"));
        assert_eq!(right_module_at(200, None, 12, &values, 20.0), None);
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
    fn workspace_digits_use_the_standard_seven_segment_layout() {
        assert_eq!(
            DIGIT_SEGMENTS,
            [
                0b0111111, 0b0000110, 0b1011011, 0b1001111, 0b1100110, 0b1101101, 0b1111101,
                0b0000111, 0b1111111, 0b1101111,
            ]
        );
        assert_eq!(digit_segments(1), 0b0000110);
        assert_eq!(digit_segments(9), 0b1101111);
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
    fn bar_uses_transparent_margins_and_a_rounded_panel() {
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
        );
        let pixel = |x: usize, y: usize| {
            u32::from_le_bytes(
                canvas[(y * 200 + x) * 4..(y * 200 + x + 1) * 4]
                    .try_into()
                    .unwrap(),
            )
        };
        assert_eq!(pixel(0, 0), 0);
        assert_ne!(pixel(100, 20), 0);
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
}
