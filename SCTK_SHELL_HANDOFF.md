# SCTK Shell Handoff

Updated: 2026-09-15

## Purpose and current scope

`wm-shell-sctk` is Luma's native Smithay Client Toolkit shell. It is selected
with `[shell] backend = "sctk"` and is intended to be the stable,
low-overhead default shell for a TTY3 session. It owns the bar, launcher,
wallpaper, notifications, StatusNotifier tray, and recorder overlay without
starting GTK.

The source is deliberately in one crate while the feature set settles:

- Entry point and all SCTK surfaces: `crates/shell-sctk/src/main.rs`
- Package/dependencies: `crates/shell-sctk/Cargo.toml`
- Shell selection/config schema: `crates/core/src/lib.rs`
- Compositor layer-surface rendering and blur: `crates/compositor/src/effects.rs`
- Default configuration: `config/default.toml`
- Native integration check: `tools/check-sctk-shell.py`

The GTK shell in `crates/shell` remains the broader compatibility fallback.
Do not move functionality between the two backends incidentally; SCTK is being
optimized for native core behavior, not full GTK feature parity.

## Surface model

The executable selects a mode from its arguments:

- default: bar (`--bar` is implicit)
- `--wallpaper`: wallpaper layer
- `--launcher`: command/application launcher

The SCTK process builds SHM-backed wlr-layer-shell surfaces and uses the
compositor IPC snapshot for workspace/window state. Its namespaces are:

| Namespace | Layer | Role |
| --- | --- | --- |
| `wm-bar` | top | Workspace/title/status bar per output |
| `wm-wallpaper` | background | Static image, video frame, or fallback gradient |
| `wm-launcher` | overlay | Keyboard-focused launcher |
| `wm-notifications` | overlay | Notification cards/history view |

The launcher is intentionally a fixed 680x420 unanchored layer, which
layer-shell centers. It must stay panel-sized: a previous fullscreen
transparent launcher surface caused the compositor to blur the whole overlay
and produced visually fragmented blur. `draw_launcher` keeps the surrounding
pixels transparent and draws the rounded panel itself.

## Launcher state and recent fixes

Launcher state lives in `App`: `apps`, `apps_loaded`, `launcher_query`, and
`launcher_selected`.

- Desktop entries load on a worker thread. Until it completes, the panel says
  `Loading applications…` rather than appearing frozen.
- Font matching and font-file loading (`fc-match`) run on a worker thread; the
  launcher first uses the glyph fallback and redraws after the configured font
  arrives.
- Wallpaper loading only happens in wallpaper mode. Launcher startup must not
  decode the configured wallpaper image or video poster frame.
- Desktop-entry icons were removed from the launcher because the available
  rendering path produced poor-looking icons. Tray and notification icon pixels
  use premultiplied-alpha composition.
- `Escape` dismisses the launcher. `>` starts a direct command query; normal
  text filters applications.

If launcher behavior regresses, inspect `run`, `add_kind_surface`,
`draw_launcher`, `launcher_items`, and the keyboard handler in
`crates/shell-sctk/src/main.rs` before changing compositor effects.

## Rendering and performance rules

- Surfaces are software-rendered into bounded SHM slot pools. Do not add a
  frame-rate polling loop for status state.
- Bar maintenance wakes at the next clock minute, on service signals, and for
  a 30-second battery refresh. Notification expiry uses exact one-shot timers.
- Video wallpaper uses `ffmpeg`/`ffprobe`, one queued decoded frame, a 64 MiB
  decode cap, and a maximum 60 FPS. It pauses when fullscreen content is shown
  and when configured battery behavior requires it.
- The compositor applies blur by namespace in
  `crates/compositor/src/effects.rs`. The current launcher geometry is part of
  that blur contract; do not make it fullscreen again unless blur clipping is
  implemented in the compositor.
- The native shell still does not have complete GTK-equivalent D-BusMenu, IME,
  accessibility, or fractional-scale behavior. Keep new work bounded to native
  stability and performance unless the goal explicitly changes.

## Services and interactions

The SCTK shell owns `org.freedesktop.Notifications` and
`org.kde.StatusNotifierWatcher` on its session bus. It also listens to
PulseAudio/PipeWire-compatible state, NetworkManager, BlueZ, MPRIS, and sysfs
battery state. Relevant updates are event-driven and coalesced.

Native interactions currently include audio click/scroll controls, media
transport controls, notification dismissal/actions/DND, StatusNotifier item
actions, and recorder UI paths. The recorder UI is also rendered by this crate;
avoid treating a recorder-only edit in `main.rs` as a shell regression without
checking its `SurfaceKind` and mode.

The recorder overlay is an OBS-Studio-style two-page app (`RecorderView`):
a Controls page (capture mode/source selection, transport buttons from
`recorder_control_button_rects`) and a Settings page (`draw_recorder_settings`)
with Output/Video/Audio/Replay categories. Settings rows are described by
`recorder_settings_rows` and adjusted by `adjust_recorder_setting`, which
pre-validates values with `wm_core::Recorder::apply` before sending
`recorder set KEY VALUE` over IPC. The UI renders `snapshot.recorder_settings`
(the compositor's effective settings), never the local config copy; responses
arrive on the `recorder_settings_events` channel and surface as footer
feedback. Hit testing uses `recorder_hit_at`/`activate_recorder_hit` with the
same geometry helpers the renderer uses.

Recording method and source memory live in the same settings: `capture_mode`
plus `window_app_id`/`window_title` (matched by substring, case-insensitive,
app_id preferred), `inject_process` (process comm), and `game_profile` are
written through `recorder set` and persisted to `recorder-settings.toml` with
everything else. The Settings page's **Capture** category edits them
(`recorder_identity_edit_seed` seeds the shared inline buffer for the
Identity row; `remember_current_recorder_source` stores the Controls page's
current selection), and every panel open restores the method/selection on the
first snapshot via `restore_recorder_selection`. The compositor resolves
`recorder start remembered` back to a live window ID with the same matcher
(`remembered_recorder_source` in `policy.rs`); setting Start on the panel
uses it whenever the remembered window is open.

## Build, run, and verification

Use the repository configuration unless a user config overrides it:

```sh
cargo build -p wm-compositor --features winit -p wm-shell-sctk --locked --offline
cargo test -p wm-shell-sctk --locked --offline
./tools/run-tty.sh
```

`tools/run-tty.sh` prefers `$XDG_CONFIG_HOME/wm/config.toml` (normally
`~/.config/wm/config.toml`) over `config/default.toml`. If TTY3 still starts
GTK, inspect that user config or set `WM_CONFIG` to a file containing
`[shell] backend = "sctk"`.

The integration check requires a native smoke config and a Wayland-capable
nested backend:

```sh
WM_NESTED_BACKEND=winit python3 tools/check-sctk-shell.py
git diff --check
```

It checks native bar/wallpaper mappings, reload behavior, notifications, tray
D-Bus ownership, video wallpaper, and launcher mapping. The checker also
asserts that launcher mapping completes within two seconds.

## Current checkout notes

At the time of this handoff, the working tree contains unrelated recorder and
DRM/udev work. Preserve it while working on SCTK:

```text
M  README.md
M  config/default.toml
D  config/sctk-smoke.toml
D  config/smoke.toml
M  crates/compositor/src/recorder.rs
M  crates/compositor/src/udev.rs
M  crates/core/src/lib.rs
M  crates/recorder/src/main.rs
M  crates/shell-sctk/src/main.rs
```

`tools/check-sctk-shell.py` reads `config/sctk-smoke.toml`; the recorded
deletion means that check cannot run until the fixture is restored or replaced.
Treat this as a test-fixture issue, not evidence that the SCTK runtime itself
is broken. The SCTK change presently visible in the diff is recorder HDR label
text, unrelated to launcher rendering.

## Design system (shared with the GTK shell's CSS)

The native surfaces mirror the GTK shell's design tokens from
`crates/shell/src/main.rs` (`css()`): panels are the theme background fill with
a one-pixel accent border at 35% opacity; selected rows use accent at 15%;
active/toggled buttons use accent at 22% with accent text; slider troughs and
separators use muted at 25%; disabled labels use muted at 55%. These are
implemented as shared primitives in `crates/shell-sctk/src/main.rs` —
`panel()`, `rounded_rect_outline()`, and `Colors::scaled()`. Use those instead
of hardcoded color literals when touching any draw function, and keep hit-test
geometry tied to `bar_module_layout()` / `control_button()` so hitboxes cannot
drift from the drawn pixels again.

## Recommended next steps

1. Restore or recreate `config/sctk-smoke.toml`, then run the focused check.
2. Test the launcher on the actual TTY3 session with a busy wallpaper and a
   font cache cold enough to validate first-map latency and the centered blur.
3. Capture a screenshot only if the blur is still visually wrong; determine
   whether it is the compositor backdrop shader or the panel's own alpha before
   changing either path.
4. Keep launcher icons disabled until an icon pipeline with correct theme
   lookup, scaling, and premultiplied-alpha output is ready.
