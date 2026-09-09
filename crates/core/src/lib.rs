//! Shared configuration, layout mathematics and local control protocol.
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, path::PathBuf};
pub const PROTOCOL_VERSION: u32 = 1;
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub theme: Theme,
    pub layout: Layout,
    pub input: Input,
    pub terminal: Vec<String>,
    pub bindings: BTreeMap<String, String>,
    pub startup: Vec<Vec<String>>,
    pub wallpaper: Wallpaper,
    pub outputs: BTreeMap<String, OutputConfig>,
    pub rules: Vec<WindowRule>,
    pub shell: Shell,
}
impl Default for Config {
    fn default() -> Self {
        let mut bindings = BTreeMap::from([
            ("Super+Return".into(), "terminal".into()),
            ("Super+space".into(), "launcher".into()),
            ("Super+Shift+Q".into(), "close".into()),
            ("Super+q".into(), "close".into()),
            ("Super+f".into(), "fullscreen".into()),
            ("Super+Shift+space".into(), "floating".into()),
            ("Super+m".into(), "layout monocle".into()),
            ("Super+t".into(), "layout master".into()),
            ("Super+Shift+R".into(), "reload".into()),
            ("Super+minus".into(), "ratio -0.05".into()),
            ("Super+equal".into(), "ratio 0.05".into()),
            ("Super+grave".into(), "scratchpad show".into()),
            ("Super+Shift+asciitilde".into(), "scratchpad send".into()),
            ("Super+Escape".into(), "lock".into()),
            ("Super+Shift+E".into(), "quit".into()),
        ]);
        for (key, dir) in [
            ("Left", "left"),
            ("Right", "right"),
            ("Up", "up"),
            ("Down", "down"),
        ] {
            bindings.insert(format!("Super+{key}"), format!("focus {dir}"));
            bindings.insert(format!("Super+Shift+{key}"), format!("move {dir}"));
        }
        for n in 1..=9 {
            bindings.insert(format!("Super+{n}"), format!("workspace {n}"));
        }
        // Shift changes the keysym of digits; bindings are resolved against the base symbol too.
        for n in 1..=9 {
            bindings.insert(format!("Super+Shift+{n}"), format!("send {n}"));
        }
        Self {
            theme: Theme::default(),
            layout: Layout::default(),
            input: Input::default(),
            terminal: vec!["kitty".into()],
            bindings,
            startup: vec![],
            wallpaper: Wallpaper::default(),
            outputs: BTreeMap::new(),
            rules: vec![],
            shell: Shell::default(),
        }
    }
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Theme {
    pub background: String,
    pub foreground: String,
    pub accent: String,
    pub muted: String,
    pub font: String,
    pub font_size: u32,
    pub gap: i32,
    pub border: i32,
    pub shadow_size: i32,
    pub shadow_opacity: f32,
    pub radius: f32,
    pub opacity: f32,
    pub blur: bool,
    pub blur_passes: u32,
    pub animation_ms: u32,
    pub reduced_motion: bool,
}
impl Default for Theme {
    fn default() -> Self {
        Self {
            background: "#161a22".into(),
            foreground: "#e2e8f0".into(),
            accent: "#89b4fa".into(),
            muted: "#8892a5".into(),
            font: "sans-serif".into(),
            font_size: 13,
            gap: 8,
            border: 2,
            shadow_size: 14,
            shadow_opacity: 0.22,
            radius: 10.,
            opacity: 0.94,
            blur: true,
            blur_passes: 3,
            animation_ms: 140,
            reduced_motion: false,
        }
    }
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Layout {
    pub mode: String,
    pub master_ratio: f64,
    pub workspaces: u8,
}
impl Default for Layout {
    fn default() -> Self {
        Self {
            mode: "master".into(),
            master_ratio: 0.55,
            workspaces: 9,
        }
    }
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Input {
    pub layout: String,
    pub variant: String,
    pub options: String,
    pub repeat_delay: i32,
    pub repeat_rate: i32,
    pub natural_scroll: bool,
    pub tap_to_click: bool,
    pub pointer_accel: f64,
    pub mouse_modifier: String,
}
impl Default for Input {
    fn default() -> Self {
        Self {
            layout: "us".into(),
            variant: String::new(),
            options: String::new(),
            repeat_delay: 200,
            repeat_rate: 25,
            natural_scroll: false,
            tap_to_click: true,
            pointer_accel: 0.,
            mouse_modifier: "Super".into(),
        }
    }
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Shell {
    pub enabled: bool,
    /// `gtk` keeps the established feature-complete shell. `sctk` enables the
    /// low-overhead Smithay Client Toolkit implementation.
    pub backend: String,
    pub position: String,
    pub height: i32,
    pub modules: Vec<String>,
    pub do_not_disturb: bool,
}
impl Default for Shell {
    fn default() -> Self {
        Self {
            enabled: true,
            backend: "gtk".into(),
            position: "top".into(),
            height: 36,
            do_not_disturb: false,
            modules: vec![
                "workspaces",
                "title",
                "audio",
                "network",
                "bluetooth",
                "media",
                "tray",
                "battery",
                "notifications",
                "clock",
            ]
            .into_iter()
            .map(str::to_string)
            .collect(),
        }
    }
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Wallpaper {
    pub path: String,
    pub kind: String,
    pub fit: String,
    pub fps: u32,
    pub pause_on_battery: bool,
    pub outputs: BTreeMap<String, String>,
}
impl Default for Wallpaper {
    fn default() -> Self {
        Self {
            path: String::new(),
            kind: "static".into(),
            fit: "fill".into(),
            fps: 30,
            pause_on_battery: true,
            outputs: BTreeMap::new(),
        }
    }
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct OutputConfig {
    pub scale: f64,
    pub transform: String,
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
    /// Requested refresh rate in hertz. Zero leaves it automatic.
    pub hz: f64,
    /// Legacy refresh-rate setting in millihertz.
    pub refresh: i32,
    pub vrr: bool,
}
impl Default for OutputConfig {
    fn default() -> Self {
        Self {
            scale: 1.,
            transform: "normal".into(),
            x: 0,
            y: 0,
            width: 0,
            height: 0,
            hz: 0.,
            refresh: 0,
            vrr: false,
        }
    }
}

/// Return the requested refresh rate in millihertz. `hz` is the preferred,
/// human-readable setting; `refresh` keeps older configurations compatible.
pub fn output_refresh_millihz(config: &OutputConfig) -> i64 {
    if config.hz > 0.0 {
        (config.hz * 1000.0).round() as i64
    } else {
        i64::from(config.refresh)
    }
}

/// Choose only advertised modes. Advertised refresh values are millihertz;
/// allow common fractional rates (59.94/143.98 Hz) when an integer rate was
/// requested.
pub fn select_output_mode(modes: &[(i32, i32, i32, bool)], config: &OutputConfig) -> Option<usize> {
    let requested_refresh = output_refresh_millihz(config);
    modes
        .iter()
        .enumerate()
        .filter(|(_, (w, h, refresh, _))| {
            (config.width == 0 || (*w == config.width && *h == config.height))
                && (requested_refresh == 0
                    || (i64::from(*refresh) - requested_refresh).abs() <= 500)
        })
        .max_by_key(|(index, (_, _, refresh, preferred))| {
            let distance = if requested_refresh == 0 {
                0
            } else {
                -(i64::from(*refresh) - requested_refresh).abs()
            };
            (distance, *preferred, *refresh, std::cmp::Reverse(*index))
        })
        .map(|(index, _)| index)
}
pub fn opening_opacity(
    elapsed: std::time::Duration,
    duration_ms: u32,
    reduced_motion: bool,
) -> f32 {
    if reduced_motion || duration_ms == 0 {
        return 1.0;
    }
    let progress = (elapsed.as_secs_f64() * 1000.0 / f64::from(duration_ms)).clamp(0.0, 1.0);
    (1.0 - (1.0 - progress).powi(3)) as f32
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct WindowRule {
    pub app_id: Option<String>,
    pub title: Option<String>,
    pub workspace: Option<u8>,
    pub output: Option<String>,
    pub floating: Option<bool>,
    pub width: Option<i32>,
    pub height: Option<i32>,
    pub opacity: Option<f32>,
    pub blur: Option<bool>,
}
pub fn config_path() -> PathBuf {
    std::env::var_os("WM_CONFIG")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            std::env::var_os("XDG_CONFIG_HOME")
                .map(PathBuf::from)
                .unwrap_or_else(|| {
                    PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".config")
                })
                .join("wm/config.toml")
        })
}
pub fn socket_path() -> Result<PathBuf, String> {
    if let Some(p) = std::env::var_os("WM_SOCKET") {
        return Ok(p.into());
    }
    let dir = std::env::var_os("XDG_RUNTIME_DIR").ok_or("XDG_RUNTIME_DIR is missing")?;
    Ok(PathBuf::from(dir).join("wm.sock"))
}
impl Config {
    pub fn load() -> Result<Self, String> {
        let p = config_path();
        match std::fs::read_to_string(&p) {
            Ok(s) => Self::parse(&s),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(format!("{}: {e}", p.display())),
        }
    }
    pub fn parse(s: &str) -> Result<Self, String> {
        let c: Self = toml::from_str(s).map_err(|e| e.to_string())?;
        c.validate()?;
        Ok(c)
    }
    pub fn validate(&self) -> Result<(), String> {
        if !(0.1..=0.9).contains(&self.layout.master_ratio)
            || !(1..=9).contains(&self.layout.workspaces)
        {
            return Err("master_ratio must be 0.1..0.9; workspaces 1..9".into());
        }
        if !["master", "monocle"].contains(&self.layout.mode.as_str()) {
            return Err("layout mode must be master or monocle".into());
        }
        if !(0..=100).contains(&self.theme.gap)
            || !(0..=20).contains(&self.theme.border)
            || !(0..=80).contains(&self.theme.shadow_size)
            || !(0.0..=1.0).contains(&self.theme.shadow_opacity)
            || !(0.0..=100.0).contains(&self.theme.radius)
            || !(0.0..=1.0).contains(&self.theme.opacity)
            || self.theme.blur_passes > 8
            || self.theme.animation_ms > 2000
            || !(6..=72).contains(&self.theme.font_size)
        {
            return Err("invalid theme dimensions/effect limits".into());
        }
        for c in [
            &self.theme.background,
            &self.theme.foreground,
            &self.theme.accent,
            &self.theme.muted,
        ] {
            if color(c).is_none() {
                return Err(format!("invalid RGB color: {c}"));
            }
        }
        if self.terminal.is_empty() || self.startup.iter().any(Vec::is_empty) {
            return Err("commands must contain an executable".into());
        }
        if !(1..=240).contains(&self.wallpaper.fps)
            || !["static", "video"].contains(&self.wallpaper.kind.as_str())
            || !["fit", "fill"].contains(&self.wallpaper.fit.as_str())
        {
            return Err("invalid wallpaper kind, fit, or fps".into());
        }
        if !["gtk", "sctk"].contains(&self.shell.backend.as_str())
            || !["top", "bottom"].contains(&self.shell.position.as_str())
            || !(20..=100).contains(&self.shell.height)
        {
            return Err("invalid shell backend, bar position, or height".into());
        }
        for o in self.outputs.values() {
            if !(0.5..=4.0).contains(&o.scale)
                || ![
                    "normal",
                    "90",
                    "180",
                    "270",
                    "flipped",
                    "flipped-90",
                    "flipped-180",
                    "flipped-270",
                ]
                .contains(&o.transform.as_str())
                || o.width < 0
                || o.height < 0
                || !o.hz.is_finite()
                || !(0.0..=1000.0).contains(&o.hz)
                || o.refresh < 0
                || (o.hz > 0.0 && o.refresh > 0)
                || (o.width == 0) != (o.height == 0)
            {
                return Err("invalid output settings".into());
            }
        }
        if !(0..=100).contains(&self.input.repeat_rate)
            || !(0..=5000).contains(&self.input.repeat_delay)
            || !(-1.0..=1.0).contains(&self.input.pointer_accel)
            || !["Super", "Alt", "Control", "disabled"]
                .contains(&self.input.mouse_modifier.as_str())
        {
            return Err("invalid input settings".into());
        }
        for binding in self.bindings.keys() {
            let mut parts = binding.split('+');
            let Some(key) = parts.next_back() else {
                return Err("invalid binding".into());
            };
            if key.is_empty()
                || parts.any(|modifier| !["Super", "Ctrl", "Alt", "Shift"].contains(&modifier))
            {
                return Err(format!("invalid binding: {binding}"));
            }
        }
        for r in &self.rules {
            if r.workspace
                .is_some_and(|w| w < 1 || w > self.layout.workspaces)
                || r.width.is_some_and(|w| w < 1)
                || r.height.is_some_and(|h| h < 1)
                || r.opacity.is_some_and(|v| !(0.0..=1.0).contains(&v))
            {
                return Err("invalid window rule".into());
            }
        }
        Ok(())
    }
}
pub fn color(s: &str) -> Option<[f32; 4]> {
    let h = s.strip_prefix('#')?;
    if h.len() != 6 {
        return None;
    }
    let v = u32::from_str_radix(h, 16).ok()?;
    Some([
        ((v >> 16) & 255) as f32 / 255.,
        ((v >> 8) & 255) as f32 / 255.,
        (v & 255) as f32 / 255.,
        1.,
    ])
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
}
/// Preserve floating geometry across layout/fullscreen changes, fitting it to the usable output.
pub fn floating_rect(
    area: Rect,
    saved: Option<Rect>,
    requested: (Option<i32>, Option<i32>),
) -> Rect {
    let width = area.w.max(1);
    let height = area.h.max(1);
    let w = saved
        .map(|r| r.w)
        .or(requested.0)
        .unwrap_or(width / 2)
        .clamp(1, width);
    let h = saved
        .map(|r| r.h)
        .or(requested.1)
        .unwrap_or(height / 2)
        .clamp(1, height);
    Rect {
        x: saved
            .map(|r| r.x)
            .unwrap_or(area.x + (width - w) / 2)
            .clamp(area.x, area.x + width - w),
        y: saved
            .map(|r| r.y)
            .unwrap_or(area.y + (height - h) / 2)
            .clamp(area.y, area.y + height - h),
        w,
        h,
    }
}
pub fn rounded_contains(rect: Rect, x: f64, y: f64, radius: f64) -> bool {
    if x < rect.x as f64
        || y < rect.y as f64
        || x >= (rect.x + rect.w) as f64
        || y >= (rect.y + rect.h) as f64
    {
        return false;
    }
    let r = radius
        .max(0.)
        .min(rect.w as f64 / 2.)
        .min(rect.h as f64 / 2.);
    let cx = x.clamp(rect.x as f64 + r, (rect.x + rect.w) as f64 - r);
    let cy = y.clamp(rect.y as f64 + r, (rect.y + rect.h) as f64 - r);
    (x - cx).hypot(y - cy) <= r
}
/// Exact partitioning: integer remainder pixels go to the earliest stack cells.
pub fn tile(area: Rect, count: usize, gap: i32, ratio: f64, monocle: bool) -> Vec<Rect> {
    if count == 0 {
        return vec![];
    }
    let mut g = gap.max(0).min((area.w.min(area.h) - 1).max(0) / 2);
    if count > 1 && !monocle && count - 1 <= area.h.max(0) as usize && area.w >= 2 {
        // Give pixels back to clients before the outer gap can exhaust the stack.
        g = g
            .min((area.h - (count - 1) as i32) / 2)
            .min((area.w - 2) / 2);
    }
    let a = Rect {
        x: area.x + g,
        y: area.y + g,
        w: (area.w - 2 * g).max(1),
        h: (area.h - 2 * g).max(1),
    };
    if count == 1 || monocle || a.w < 2 || count - 1 > a.h as usize {
        return vec![a; count];
    }
    let split_gap = g.min((a.w - 2).max(0));
    let available = (a.w - split_gap).max(2);
    let mw = ((available as f64 * ratio.clamp(0.1, 0.9)).round() as i32).clamp(1, available - 1);
    let mut out = vec![Rect { w: mw, ..a }];
    let n = (count - 1) as i32;
    let sg = g.min(((a.h - n) / (n - 1).max(1)).max(0));
    let usable = (a.h - sg * (n - 1)).max(n);
    let mut y = a.y;
    for i in 0..n {
        let h = usable / n + i32::from(i < usable % n);
        out.push(Rect {
            x: a.x + mw + split_gap,
            y,
            w: (a.w - mw - split_gap).max(1),
            h,
        });
        y += h + sg;
    }
    out
}
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
pub struct Snapshot {
    pub version: u32,
    pub windows: Vec<WindowInfo>,
    pub outputs: Vec<OutputInfo>,
    pub focused: Option<u64>,
    pub error: Option<String>,
    #[serde(default)]
    pub layers: Vec<LayerInfo>,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct LayerInfo {
    pub namespace: String,
    pub output: String,
    pub geometry: Rect,
    /// Logical size of the most recently committed surface buffer, if mapped.
    #[serde(default)]
    pub surface_size: Option<[i32; 2]>,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct WindowInfo {
    pub id: u64,
    pub title: String,
    pub app_id: String,
    pub workspace: u8,
    pub output: String,
    pub floating: bool,
    pub fullscreen: bool,
    pub scratchpad: bool,
    #[serde(default)]
    pub geometry: Option<Rect>,
    #[serde(default = "opaque")]
    pub opacity: f32,
    /// Logical size of the current client buffer, independent of target geometry.
    #[serde(default)]
    pub surface_size: Option<[i32; 2]>,
}
fn opaque() -> f32 {
    1.0
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct OutputInfo {
    pub name: String,
    pub workspace: u8,
    pub geometry: Rect,
    pub wallpaper_visible: bool,
    pub active: bool,
}
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    pub version: u32,
    pub command: String,
}
#[derive(Debug, Serialize, Deserialize)]
pub struct Response {
    pub version: u32,
    pub ok: bool,
    pub error: Option<String>,
    pub state: Snapshot,
}
pub fn connect_command(command: &str) -> Result<Response, String> {
    use std::io::{BufRead, BufReader, Write};
    use std::os::unix::net::UnixStream;
    let mut s = UnixStream::connect(socket_path()?).map_err(|e| e.to_string())?;
    s.set_read_timeout(Some(std::time::Duration::from_secs(3)))
        .map_err(|e| e.to_string())?;
    serde_json::to_writer(
        &mut s,
        &Request {
            version: PROTOCOL_VERSION,
            command: command.into(),
        },
    )
    .map_err(|e| e.to_string())?;
    s.write_all(b"\n").map_err(|e| e.to_string())?;
    let mut line = String::new();
    BufReader::new(s)
        .read_line(&mut line)
        .map_err(|e| e.to_string())?;
    serde_json::from_str(&line).map_err(|e| e.to_string())
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn opening_fade_is_bounded_and_respects_reduced_motion() {
        use std::time::Duration;
        assert_eq!(opening_opacity(Duration::ZERO, 140, false), 0.0);
        assert_eq!(
            opening_opacity(Duration::from_millis(70), 140, false),
            0.875
        );
        assert_eq!(opening_opacity(Duration::from_secs(1), 140, false), 1.0);
        assert_eq!(opening_opacity(Duration::ZERO, 0, false), 1.0);
        assert_eq!(opening_opacity(Duration::ZERO, 140, true), 1.0);
        let mut previous = 0.0;
        for ms in 0..=200 {
            let value = opening_opacity(Duration::from_millis(ms), 140, false);
            assert!(value >= previous && value <= 1.0);
            previous = value;
        }
    }
    #[test]
    fn advertised_output_modes_match_resolution_and_fractional_refresh() {
        let modes = [
            (1920, 1080, 60000, true),
            (2560, 1440, 59940, false),
            (2560, 1440, 143980, false),
            (2560, 1440, 144000, false),
        ];
        let mut config = OutputConfig::default();
        assert_eq!(select_output_mode(&modes, &config), Some(0));
        config.width = 2560;
        config.height = 1440;
        assert_eq!(select_output_mode(&modes, &config), Some(3));
        config.refresh = 60000;
        assert_eq!(select_output_mode(&modes, &config), Some(1));
        config.refresh = 144000;
        assert_eq!(select_output_mode(&modes, &config), Some(3));
        assert_eq!(select_output_mode(&modes[..3], &config), Some(2));
        config.refresh = 120000;
        assert_eq!(select_output_mode(&modes, &config), None);
        config.refresh = i32::MAX;
        assert_eq!(select_output_mode(&modes, &config), None);
        assert_eq!(select_output_mode(&[], &config), None);

        config.refresh = 0;
        config.hz = 144.0;
        assert_eq!(select_output_mode(&modes, &config), Some(3));
        config.hz = 143.98;
        assert_eq!(select_output_mode(&modes, &config), Some(2));

        let mut full = Config::default();
        full.outputs.insert(
            "DP-1".into(),
            OutputConfig {
                width: 1920,
                ..Default::default()
            },
        );
        assert!(full.validate().is_err());

        let mut conflicting = Config::default();
        conflicting.outputs.insert(
            "DP-1".into(),
            OutputConfig {
                hz: 144.0,
                refresh: 144_000,
                ..Default::default()
            },
        );
        assert!(conflicting.validate().is_err());
    }
    #[test]
    fn config_roundtrip() {
        let c = Config::default();
        let text = toml::to_string_pretty(&c).unwrap();
        Config::parse(&text).unwrap();
    }
    #[test]
    fn invalid_config_rejected() {
        for s in [
            "[layout]\nmaster_ratio=1.1",
            "[wallpaper]\nfps=0",
            "[theme]\nbackground='red'",
            "[theme]\nshadow_size=81",
            "[theme]\nshadow_size=-1",
            "[theme]\nshadow_opacity=1.1",
            "[theme]\nshadow_opacity=nan",
            "[shell]\nbackend='other'",
            "[bindings]\n'Supers+space'='launcher'",
            "typo=1",
        ] {
            assert!(Config::parse(s).is_err(), "{s}");
        }
    }
    #[test]
    fn layouts_partition_without_overlap() {
        for n in 1..30 {
            let a = Rect {
                x: -1920,
                y: 40,
                w: 1920,
                h: 1040,
            };
            let r = tile(a, n, 8, 0.55, false);
            assert_eq!(r.len(), n);
            for (i, p) in r.iter().enumerate() {
                assert!(
                    p.w > 0
                        && p.h > 0
                        && p.x >= a.x
                        && p.y >= a.y
                        && p.x + p.w <= a.x + a.w
                        && p.y + p.h <= a.y + a.h
                );
                for q in &r[i + 1..] {
                    assert!(
                        p.x + p.w <= q.x
                            || q.x + q.w <= p.x
                            || p.y + p.h <= q.y
                            || q.y + q.h <= p.y
                    );
                }
            }
        }
    }
    #[test]
    fn monocle_and_empty() {
        let a = Rect {
            x: 0,
            y: 0,
            w: 100,
            h: 100,
        };
        assert!(tile(a, 0, 8, 0.55, false).is_empty());
        let r = tile(a, 4, 8, 0.55, true);
        assert!(r.iter().all(|x| *x == r[0]));
    }
    #[test]
    fn floating_geometry_restores_and_fits_changed_outputs() {
        let area = Rect {
            x: -1000,
            y: 40,
            w: 1000,
            h: 700,
        };
        let initial = floating_rect(area, None, (Some(640), Some(480)));
        assert_eq!(
            initial,
            Rect {
                x: -820,
                y: 150,
                w: 640,
                h: 480
            }
        );
        assert_eq!(floating_rect(area, Some(initial), (None, None)), initial);
        let smaller = Rect {
            x: 0,
            y: 50,
            w: 400,
            h: 300,
        };
        assert_eq!(floating_rect(smaller, Some(initial), (None, None)), smaller);
        let oversize = floating_rect(area, None, (Some(2000), Some(2000)));
        assert_eq!(oversize, area);
    }
    #[test]
    fn crowded_layouts_reduce_gaps_and_never_escape_the_output() {
        for width in [1, 2, 8, 30] {
            for height in [1, 2, 10, 30] {
                for count in 1..40 {
                    for gap in [0, 8, 100] {
                        let area = Rect {
                            x: -30,
                            y: 51,
                            w: width,
                            h: height,
                        };
                        let rects = tile(area, count, gap, 0.55, false);
                        assert_eq!(rects.len(), count);
                        for (i, rect) in rects.iter().enumerate() {
                            assert!(rect.w > 0 && rect.h > 0);
                            assert!(rect.x >= area.x && rect.y >= area.y);
                            assert!(rect.x + rect.w <= area.x + area.w);
                            assert!(rect.y + rect.h <= area.y + area.h);
                            if width >= 2 && count - 1 <= height as usize {
                                for other in &rects[i + 1..] {
                                    assert!(
                                        rect.x + rect.w <= other.x
                                            || other.x + other.w <= rect.x
                                            || rect.y + rect.h <= other.y
                                            || other.y + other.h <= rect.y
                                    );
                                }
                            }
                        }
                    }
                }
            }
        }
    }
    #[test]
    fn rounded_hit_regions_exclude_only_clipped_corners() {
        let rect = Rect {
            x: -20,
            y: 30,
            w: 100,
            h: 60,
        };
        assert!(!rounded_contains(rect, -19.5, 30.5, 10.));
        assert!(rounded_contains(rect, -15., 35., 10.));
        assert!(rounded_contains(rect, 30., 30.5, 10.));
        assert!(!rounded_contains(rect, 79.5, 89.5, 10.));
        assert!(rounded_contains(rect, -19.5, 30.5, 0.));
        assert!(!rounded_contains(rect, 80., 50., 0.));
        assert!(rounded_contains(rect, 30., 60., 1000.));
    }
}
