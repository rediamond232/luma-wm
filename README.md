# wm

A Rust/Smithay tiling Wayland desktop in development, with a GTK layer-shell bar,
launcher, notifications and wallpaper process. Smithay is pinned in Cargo.lock;
the compositor backend started from Anvil (see THIRD_PARTY.md).

Build on Linux with Rust, a C toolchain, pkg-config, Wayland, libinput, libseat,
libudev, libxkbcommon, Mesa EGL/GBM, GTK4, gtk4-layer-shell, GStreamer and XWayland
development packages available:

```sh
./tools/build-dev.sh
./tools/run-nested.sh
```

The nested development session runs in an X11 window on the current desktop. Its
private D-Bus and IPC socket keep shell services separate from the host desktop.
The shipped configuration uses Kitty, which is installed on the target machine.

To test on real hardware, log in as your normal user on **TTY3**, then run:

```sh
cd /home/lev/luma
./tools/run-tty.sh
```

For a performance test, use the release-only TTY runner instead:

```bash
./tools/run-tty-release.sh
```

It starts only `target/release/wm`; its log is saved to
`~/.local/state/wm/session-release.log`. Build the release workspace first; the
runner intentionally launches only that prebuilt binary.

`sessions/luma-stable.desktop` is the local-checkout display-manager entry for
the stable release session. The Arch package installs its separate portable
**Luma** entry using `/usr/bin/luma-session`.

### Arch Linux package

Released x86-64 builds include a `luma-wm-bin` Pacman package on GitHub
Releases. Install the downloaded package with `sudo pacman -U <file>`. The AUR
recipe is maintained in `packaging/aur` and can be submitted when an AUR
maintainer account is available. The package installs a portable **Luma**
Wayland session; it does not refer to the maintainer's source checkout. Release
archives are generated with:

```bash
./tools/package-release.sh 0.1.0
```

The TTY script uses `~/.config/wm/config.toml` when present, otherwise the repository
default. `WM_CONFIG` overrides either. It performs a locked offline incremental build,
starts a fresh session log, and prints the final log lines if the compositor exits. Logs are in
`${XDG_STATE_HOME:-~/.local/state}/wm/session.log`; `./tools/logs.sh` follows them.
Luma reserves XWayland displays `:100` through `:132` so starting it beside an
inactive Hyprland session cannot probe or unlink Hyprland's `:0`/`:1` sockets.
Set `WM_XWAYLAND_DISPLAY` to select one exact isolated display when debugging.
`Super+Shift+E` exits. From another terminal, `./tools/stop.sh` requests exit on the
TTY session socket. Nested IPC uses `WM_SOCKET=$XDG_RUNTIME_DIR/wm-nested.sock`.

Generate and validate your configuration:

```sh
mkdir -p ~/.config/wm
target/debug/wmctl config-default > ~/.config/wm/config.toml
target/debug/wmctl config-check
```

Existing configuration files reload automatically. `Super+Shift+R` explicitly
reloads. Invalid values retain the last valid configuration. See
[STATUS.md](STATUS.md) for runtime validation coverage and hardware checks.

### Native SCTK shell

The native SCTK shell is the default. It focuses on a stable, low-overhead core
bar, launcher, wallpaper, notification and tray experience. Set this in your
config to select it explicitly:

```toml
[shell]
backend = "sctk"
do_not_disturb = false
```

It creates native SHM layer surfaces for a per-output workspace/title bar, an
image or fallback-gradient wallpaper, and a UTF-8 `>` command launcher with
Escape dismissal. It uses compositor snapshots and inotify-driven config
reloads, so it has no GTK runtime or constant status-polling wakeup. Theme colors, font,
opacity, bar position and height apply to the native surfaces. The native bar
tracks PulseAudio volume, NetworkManager, BlueZ availability, MPRIS players,
battery state, freedesktop notifications, and StatusNotifier registrations.
It owns `org.freedesktop.Notifications` and `org.kde.StatusNotifierWatcher` on
the compositor session bus; notification replacement, close requests and expiry
are handled natively. Click the audio module to toggle mute and scroll it to
adjust output volume in 5% steps. Click media to play/pause, right-click for
previous, or middle-click for next. Click a notification card to dismiss it or
right-click the notification center to clear its history.
Middle-click the network module to enable or disable Wi-Fi through
NetworkManager, or middle-click Bluetooth to toggle its primary adapter.
Notification action labels are native buttons and emit the standard
`ActionInvoked` signal when selected. Dismissal, expiry, and explicit close
requests emit the standard `NotificationClosed` reason.
Set `do_not_disturb = true` in `[shell]` to suppress notification cards while
still acknowledging notification requests; reload applies it immediately. You
can also middle-click the notifications module to toggle DND for the session.
MPRIS metadata and playback updates follow player property signals, without a
status polling loop.
For `wallpaper.kind = "video"`, the native backend uses installed `ffmpeg` and
`ffprobe`; decoded frames are capped to one queued frame and 60 FPS.

The GTK backend remains available for its tray icons and
menus, interactive network/Bluetooth/audio/media controls, video wallpaper,
and assistive-technology integration. Keep `backend = "gtk"` for those
features.

Run the focused native integration check on a Wayland host:

```sh
cargo build --workspace --features wm-compositor/winit --locked
WM_NESTED_BACKEND=winit python3 tools/check-sctk-shell.py
```

Physical input settings apply on the TTY backend, including after reload and
device hotplug. Nested sessions use the host desktop's device settings:

```toml
[input]
pointer_accel = 0.0 # -1.0 to 1.0
natural_scroll = false
tap_to_click = true
mouse_modifier = "Super" # Super, Alt, Control, or disabled
follow_mouse = true # focus the window below the cursor without raising it
```

DRM outputs select advertised modes at startup and hotplug:

```toml
[outputs.DP-1]
width = 2560
height = 1440
hz = 144.0
vrr = false # opt in with true on a compatible DRM output
scale = 1.0
x = 0
y = 0
```

Set both dimensions to zero to leave resolution automatic; `hz = 0` leaves the
rate automatic. Exact refresh matches win, with up to 0.5 Hz tolerance for
fractional rates. Automatic selection prefers the display's preferred mode, then
the highest matching refresh. Unsupported requests log a warning and fall back
to an advertised default at startup. Reload attempts the requested advertised
mode; unsupported requests retain the current mode and report an error.
Removing a mode setting selects the advertised default again. Reload uses a
coordinated DRM mode test. If the kernel rejects the first atomic test because of
cross-output bandwidth or modifier constraints, Smithay submits fallback frames
for the active CRTCs, retries with implicit modifiers, and restores preferred
modifiers when possible. Nested sessions use the host window's size and presentation timing.
Changes to a connected display's advertised mode list trigger re-evaluation.
The active mode stays available until a replacement is accepted; a late mode
list also retries initially failed output setup. Physical EDID changes still
need hardware testing.
The older `refresh = 144000` millihertz syntax remains supported, but cannot be
combined with a nonzero `hz` value.
VRR requests apply at DRM setup and config reload. Unsupported displays or failed
requests appear in logs and the status error field. Removing the output's VRR
setting requests disabling it. Display activation and frame pacing still require
hardware verification; a successful request is not proof of physical VRR operation.

Rules match app IDs exactly and titles by substring. Later matching rules override
earlier values. Width and height control the initial floating size; oversized
dimensions are constrained to the usable output. For example:

```toml
[[rules]]
app_id = "org.gnome.Calculator"
floating = true
width = 400
height = 500
opacity = 0.95 # 0.0 transparent to 1.0 opaque; fullscreen uses 1.0
blur = true # blur behind this window when theme blur is enabled
```

Opacity rules apply to existing windows on reload. The last matching rule that
specifies opacity wins; windows without a matching opacity use 1.0.
Blur rules also reload live and use the last specified matching value (default
false). They use a cached, quarter-resolution backdrop capture beneath the root
surface. `[theme].blur = false` or `blur_passes = 0` disables blur globally.
Fullscreen bypasses blur. Rotated outputs currently disable these effects.

Floating windows retain their geometry through fullscreen and tiling transitions.
Windows have an exterior rounded border using `[theme].border` for width,
`accent` for the focused window and `muted` for other windows. Set border to zero
to disable it. Border changes reload live; fullscreen bypasses decorations.
`shadow_size` (0–80 logical pixels, default 14) and `shadow_opacity` (0–1,
default 0.22) control the exterior shadow. Either value at zero disables it.
Shadows share the cached decoration shader and need no backdrop capture or
additional blur pass. They reload live and are omitted in fullscreen.
New windows fade in over `[theme].animation_ms` (default 140 ms), including their
border, shadow and backdrop blur. `reduced_motion = true` or `animation_ms = 0`
disables opening, movement, closing and workspace animations. Layout position
changes ease from the current rendered position to the new target, including when
retargeted. Interactive pointer/touch grabs, fullscreen and locked/inactive
sessions bypass movement. Workspace entry and scratchpad restoration replay the
fade; switching workspaces retains the visible outgoing windows on the GPU and
crossfades them over the incoming workspace. Window size changes send one final
configure to the client while the compositor scales the live surface smoothly on
the GPU. A delayed client commit remains scaled at the final visual size until its
new buffer arrives. Animation timers exist only while visible transitions are active.
Hold the configured mouse modifier and drag with the left button to move a
floating window, or the right button to resize from the nearest corner. Clients
can also request moves and resizes through their own title bars. Tiled and
fullscreen windows reject interactive geometry requests.
Clicks in clipped rounded corners pass through to content underneath. When an
output is too small for the requested tiling gaps, gaps shrink; if separate tiles
cannot fit at all, windows share the usable area until space becomes available.

The bar's **Notifications** button opens the most recent 100 notifications, with
Clear history and Do Not Disturb controls. Quiet notifications remain in history.
History and Do Not Disturb last for the shell process's lifetime. Include
`"notifications"` in `[shell].modules` to show the button.
Up to three popups appear at once; older entries remain in history. Popups reflow
automatically, scroll when necessary, and cancel expired or replaced timers.
History text is bounded to 256 summary characters and 8192 body characters per
entry; popups show up to four action buttons.

Repeated launcher shortcuts reuse the same window in that compositor session.
Escape dismisses it even while searching; `@` results update as windows change.

The network module listens to NetworkManager on the system bus and displays
online, connecting, local/site-only, limited, portal sign-in and offline states.
It follows service restarts without polling. Clicking opens controls for networking
and Wi-Fi, with hardware-block status and a shortcut to `nm-connection-editor`.
Changes run asynchronously and display errors in the panel. Nearby Wi-Fi networks
show signal strength, security and connection status; selecting one requests its
saved connection. “Scan for networks” requests a scan on available Wi-Fi adapters;
results follow NetworkManager signals. Update bursts coalesce, and hidden panels
do not read snapshots. If no saved connection works, the panel offers a new-profile
form for open, WPA personal and enhanced-open networks. “Remember this network”
chooses a persistent profile; disabling it uses a temporary profile. Password text
clears on submission and when the menu closes. Enterprise and legacy WEP setup
use connection settings. A nested layer-shell test verifies real password typing,
masked display and keyboard-mode restoration. Real Wi-Fi association and Polkit
authorization still need validation.
Connection progress follows the active connection's state, distinguishing
connecting, connected, authentication failures, timeouts and disconnection.
State meanings follow the [NetworkManager D-Bus API](https://networkmanager.pages.freedesktop.org/NetworkManager/NetworkManager/nm-dbus-types.html).

The `bluetooth` module shows adapter power and connected-device count, with
controls for adapter power and connecting/disconnecting paired devices. It uses
BlueZ object-manager events without polling. Each adapter has Find devices/Stop
discovery controls; closing the menu releases discovery started by this shell.
New devices have a Pair action. An application-owned BlueZ agent displays PIN,
passkey and confirmation prompts in the popover, using a private bus connection
for each attempt. Confirmations require input; requests from another bus owner or
for another device are rejected. Closing the menu or losing the device cancels the attempt and clears
entered credentials. Successful pairing leaves the Connect action available.
The shell does not replace the system default pairing agent. Errors appear in the
menu, with a `blueman-manager` shortcut for advanced settings.
`python3 tools/check-bluetooth-pairing.py` exercises the agent on a private mock bus
with native keyboard input; `WM_CHECK_BLUETOOTH_CONTROLS=1` runs the device-list,
power, discovery and restart integration. Physical Bluetooth hardware validation
remains pending.

The default `tray` module hosts StatusNotifier icons using theme names or bounded
ARGB pixmaps, with half-size status overlays at the icon's lower-right corner.
Unchanged pixmaps reuse cached textures; candidate dimensions and byte counts
are checked before copying, and only the selected image is converted.
Pixmap selection accounts for the widget's display scale and refreshes on scale
changes, while keeping the icon's logical size unchanged.
Attention icons can replace the base icon while retaining the overlay.
Application-provided absolute `IconThemePath` directories are cached and monitored per item
and searched before the system theme and pixmap fallback. Changing or clearing
the path updates the icon without modifying the shell's global icon theme. It
follows item status/property signals without polling and routes
left/middle/right clicks and both scroll axes to item actions. Scroll input uses
GTK discrete steps (positive down/right, negative up/left), matching Waybar's
convention. Pointer actions include the icon's logical output coordinates,
including bars anchored at the bottom of an output. Each bar owns a host registration that
is released when rebuilt or removed. `python3 tools/check-tray.py` verifies the
watcher registry; `python3 tools/check-tray-ui.py` verifies real icon pixels,
pointer actions, exact scroll arguments in all four directions, bar reload and
item lifecycle. Items exporting a D-Bus menu open a themed popover on right click
(or left click for menu-only items). It supports submenu pages with Back,
disabled/hidden entries, separators, toggle indicators, theme/PNG icons and
shortcut labels. PNG input is bounded to 256 KiB and 256×256 pixels and decoded
off the UI thread. Menu preparation and
layout reads are asynchronous and only the visible page follows update signals;
closed menus do not poll. `python3 tools/check-tray-menu.py` verifies nested
keyboard navigation, action arguments, updates, focus restoration and stale
reply rejection. Shortcut labels describe the application's bindings; the shell
does not register them globally. Structured tray tooltips show the supplied title
and description using the configured colors. Descriptions are limited to eight
lines/2048 characters; supported XML content becomes plain text, retaining link
text and image descriptions without loading images. Malformed markup remains
literal text. Menus also honor and monitor their own `IconThemePath` directory list, with bounded absolute
paths, per-menu caching and live property updates while open. Custom icons take
precedence over the system theme and embedded PNG fallback. The global GTK theme
search path is unchanged. Named and structured ARGB tooltip images are covered by
the nested fixture. Images referenced only by paths inside tooltip markup are not
fetched; broader application compatibility remains pending.
`python3 tools/check-tray-vlc.py` verifies the installed VLC's actual tray icon,
exported menu, keyboard Quit action and removal using a separate instance and
temporary settings on the nested session bus. VLC 3.0.23 passed; this does not
test media playback. Bar popups are constrained to their output, including at
the left and right edges.

The `audio` module opens an output-volume slider (0–100%), mute toggle,
output-device selector and Sound settings shortcut. It uses `wpctl` for WirePlumber state/control and
`pactl subscribe` for events from the local Pulse-compatible audio service.
Relevant event bursts coalesce into one refresh; a connected subscription
replaces the old ten-second polling loop. Bars within the shell process share
one subscription and confirmed state; adding a bar uses the cache immediately.
If subscriptions are unavailable,
five-second retries also refresh the state. Commands have a two-second deadline
and failures appear in the popover, with an explicit timeout message for hung
commands. Volume changes during a pending write retain the latest target.
`pavucontrol` remains the advanced settings
shortcut. Output selection uses `pactl set-default-sink`; the selected marker
comes from readback. Device lists refresh on opening, hotplug/default changes
and subscription recovery, only while a popover is open.
`python3 tools/check-audio-input.py` tests isolated fake audio commands
with real nested keyboard input; it does not change the host's volume. Physical
audio device compatibility remains unverified.
The Microphone submenu provides its own 0–100% volume slider, mute toggle and
input-device selector. It shares the event subscription with outputs, keeps input
errors and pending changes separate, and reads microphone state while open.
Closing the submenu restores the Microphone label, avoiding a stale status label;
Escape returns to the output controls before closing the whole audio popover.

The `battery` bar module uses UPower's aggregate display device and property
signals instead of polling individual batteries. It shows percentage, charging
state, low/critical warnings and time estimates in its tooltip. It hides when
UPower is unavailable or reports no display battery. UPower must be running;
there is no sysfs fallback for this bar module. Physical battery/UPS behavior
remains unverified. Wallpaper pause-on-battery also follows UPower events, with
sysfs polling only as a fallback while UPower status is unavailable.
The wallpaper integration check uses a private mock system bus to verify power
events and service restart against actual video pipeline transitions.

The `media` bar module shows track/artist text and previous, play/pause and next
controls for [MPRIS players](https://specifications.freedesktop.org/mpris/latest/Player_Interface.html)
on the compositor's session bus. It prefers a playing player, otherwise the first
available player by bus name, and hides when none exists. Player events update
the UI without polling; unsupported controls are disabled and call failures
appear in tooltips. Discovery is bounded to 32 players and displayed text to 256
characters. With multiple players, the selector lets you choose one or return to
Automatic. Manual selection lasts until that player exits; the menu scrolls for
long lists. The more-controls menu provides backward/forward ten-second seeking,
shuffle and repeat off/track/playlist modes. These controls use reported player
capabilities and state, suppress pending duplicate requests, and show failures
inside the menu. A position slider displays elapsed/total time and sends
track-specific seek requests. While open, it extrapolates playback locally and
follows seek/pause signals; closing stops its update timer. Position reads happen
on opening and state changes, with at most one in flight. Album artwork loads
while the menu is visible, with a 2 MiB input limit and five-second timeout.
Thumbnail decoding runs off the UI thread and preserves aspect ratio within
192×192 pixels. File and local HTTP URLs are tested, including oversized responses
and cancellation during download. HTTP(S) uses the installed GIO backend; HTTPS
and real-player artwork remain unverified. Closing cancels pending loading; unchanged
URLs reuse the current thumbnail or remembered failure. Real-player validation
remains pending.

`python3 tools/check-media-input.py` tests the media popover in an isolated nested
compositor using a private mock player and real pointer clicks. It checks seeking,
playback/pause updates, hidden-menu idling and keyboard-mode restoration. Set
`WM_CHECK_MEDIA_SCREENSHOT=/tmp/media.png` to save a screenshot of the fixture.
Set `WM_CHECK_MEDIA_ART_RACES=1` to also test artwork replacement and menu closure
against delayed local HTTP responses (requires GIO's HTTP backend).
The input fixtures load the configured shell palette. Shell popovers and controls
follow that palette even when a host GTK stylesheet supplies different colors;
this affects the shell process only.

Per-output `transform` accepts `normal`, `90`, `180`, `270`, `flipped`,
`flipped-90`, `flipped-180`, or `flipped-270` in an `[outputs."OUTPUT-NAME"]`
table. Reload applies it and rearranges layer-shell surfaces and tiled windows.
Removing the output override restores normal orientation and 100% scale.
`python3 tools/check-output-transform.py` verifies nested geometry and config
validation. Capture pixels are also tested across all eight transforms;
Nested pointer motion and cursor capture are tested at 150% scaling across all
transforms. Hardware scanout and physical input orientation remain unverified.

Default controls:

| Binding | Action |
| --- | --- |
| Super+Enter / Super+Space | Terminal / launcher |
| Super+1…9 / Super+Shift+1…9 | Workspace / send window |
| Super+arrows / Super+Shift+arrows | Focus / move tiled window |
| Super+F / Super+Shift+Space | Fullscreen / floating |
| Super+M / Super+T | Monocle / master-stack layout |
| Super+Shift+Q | Close window |
| Print | Copy a full-screen PNG screenshot to the clipboard |
| Super+Alt+R | Open the recorder panel |
| Super+F9 / Super+F10 | Toggle recording / pause |

Bindings can also launch applications directly. For example:

```toml
[bindings]
"Super+B" = "launch firefox --new-window"
"Super+Shift+T" = "launch kitty --title 'scratch terminal'"
```

`launch` parses quoted arguments and starts the program directly; it does not
interpret shell operators. The existing `exec ["program", "argument"]` action
remains available for machine-generated command arrays.

`wmctl help` lists IPC commands. `workspace N OUTPUT` selects a workspace on a
specific output. The launcher supports applications, `@` windows, `>` commands
and `:` session actions.

### High-rate recording

Luma exposes four deliberately separate capture modes.

- **Screen (low-lag)** captures the compositor through its image-copy protocol.
  Frames are rendered into a DMA-BUF, imported into GL for GPU scaling and
  color conversion, then encoded with NVIDIA NVENC through GStreamer. It is the
  normal choice for desktop, window, and region recording. Its rate is a
  requested constant-frame-rate output: when the compositor has no new frame,
  it may pad short gaps with duplicate frames. A high setting therefore is not
  evidence that a game supplied that many distinct frames.
- **Xwayland Zero-Copy** is driven by commits from the selected managed X11
  window without loading code into the application. Luma reuses the surface
  texture Smithay has already imported, GPU-copies it directly into one of the
  native recorder's eight persistent DMA-BUFs, and feeds that pool to NVENC.
  The 480 Hz pacing timer only releases Xwayland frame callbacks; it never
  records an unchanged surface. This removes per-frame X Composite naming,
  DRI3 export, and EGL re-import while keeping client-buffer reuse safe. No
  frame pixels cross CPU memory. Each admitted commit occupies exactly one CFR
  slot, so scheduler jitter cannot replace real frames with synthetic padding.
- **OpenGL API capture** hooks GLX/EGL presentation, independent of the game.
  A launch profile loads the hook before the renderer starts; **OpenGL API
  Inject** uses a matching x86-64 helper to load it into an already-running,
  same-user graphics process with remote `dlopen`. It patches resolved GLX/EGL
  present relocations and recognizes LWJGL2's separate writable GLX dispatch
  slot. Accepted presents stay on the GPU and go to
  NVENC without CPU pixel readback. Luma does not hide the hook or bypass an
  anti-cheat or Linux ptrace policy.
- **Vulkan API capture** launches any configured Vulkan renderer through an
  explicit loader layer. At `vkQueuePresentKHR`, the layer submits a GPU copy
  into a four-image export ring and passes a DMA-BUF plus native fence to a
  separate EGL/NVENC receiver. The receiver acknowledges a slot only after its
  GPU copy is complete. A full ring drops capture work without blocking present.

The graphics-API profile's FPS is a ceiling, not a promise or synthetic frame rate.
Only real game presents that reach the next timing slot are accepted; Luma never
pads a direct recording with duplicate frames to reach the requested FPS. The
Vulkan layer drops capture work when its bounded export ring is full, so its
separate receiver cannot back-pressure the game's present thread. The current
OpenGL hook owns NVENC in-process and uses the driver's required synchronous
Linux output contract, so it can add presentation latency when encoding is the
bottleneck. The actual source FPS must be measured from the resulting video.
This is not a claim of Windows Game Capture-equivalent performance.

The Xwayland path performs its surface-to-recorder copy in the compositor, but
NVENC and muxing stay in the separate native recorder process. The persistent
DMA-BUF pool decouples encoder ownership from Xwayland's reusable client
buffers. The OpenGL fast path currently performs the GPU copy and NVENC submission
inside the hooked process, then sends encoded H.264 access units to the muxer.
The Vulkan path uses the more OBS-like cross-process GPU-sharing architecture:
the renderer exports images and synchronization while the recorder process
owns EGL import and NVENC. Neither path transfers frame pixels through CPU RAM.

`recorder.quality` is the H.264 constant-QP value used by both recorder paths:
valid values are 1 through 51, lower is higher quality, and the default is 20.
At 2560x1440 and 480 FPS, QP 20 can create very large files and requires a very
fast local disk for sustained recording. Raise the QP or lower the capture rate
when storage cannot sustain the recording; do not expect the game-present path
to hide disk or encoder overload.

For an application which can be started directly, configure an API launch profile.
It must `exec` the renderer; a launcher that forks a child is rejected by the
exact-PID guard. For a running renderer, open Luma Recorder, choose **OpenGL API
Inject**, select a detected GLX/EGL process, and press Enter. The picker reads
only PID, ownership, mapped graphics-library names, and `comm`; it never reads
the command line because application arguments can contain secrets. Runtime
injection has no JDK or JVM Attach dependency; it always loads the native `.so`.

For a running Xwayland game, open Luma Recorder, choose **Xwayland Zero-Copy**,
select the managed X11 window, and press Enter. The equivalent IPC command is
`wmctl recorder xwayland-start WINDOW_ID`; the window ID is published only for
Xwayland windows in `wmctl status`. Stopping capture terminates only Luma's
native recorder worker, never the selected application.

```toml
[[recorder.game_profiles]]
name = "minecraft-opengl"
api = "opengl"
# This must be the executable that owns the OpenGL presents, not a launcher
# which later forks the game process.
command = ["/absolute/path/to/game-binary", "--game"]
fps = 480
```

For Vulkan, use the same shape with `api = "vulkan"`. Vulkan must be selected
before instance/device creation, so Luma launches the renderer through the
layer instead of injecting into an already-running Vulkan device:

```toml
[[recorder.game_profiles]]
name = "game-vulkan"
api = "vulkan"
command = ["/absolute/path/to/vulkan-game"]
fps = 480
```

```sh
wmctl recorder game-start minecraft-opengl
# Or inject into a detected same-user GLX/EGL process:
wmctl recorder game-attach PID
```

Select **OpenGL Launch Profile** in the native recorder panel to use a matching
profile there, or **Vulkan API Layer** for a Vulkan profile.
The direct path currently requires `recorder.codec = "h264"`; HEVC remains
available for the Screen recorder path.

Direct graphics-API capture is currently **video-only**. It does not add desktop audio
or microphone tracks; those tracks belong to the Screen recorder path.

Stopping a direct recording with the panel's **Stop** control or `wmctl recorder
stop` releases the injected GL/NVENC state on the next present, finalizes the
MP4, and leaves the application running. Re-injection into the same process is
currently unsupported; restart it before a second injected recording.

The **Screen** profile targets 2560x1440 at up to 240 FPS by default, H.264 in
Hybrid MP4, NVENC's performance tune, with separate desktop and microphone
tracks. Encoded tracks stream through a small internal fragmented-MP4 transport
into FFmpeg's `hybrid_fragmented` muxer: completed fragments remain recoverable
after an interruption, while a clean stop finalizes the file as a normal indexed
MP4.
The native engine currently requires an NVIDIA GPU, an FFmpeg build with the
`hybrid_fragmented` MP4 flag, plus GStreamer GL, NVCodec, PulseAudio, Opus,
ISO-MP4, and fd-sink plugins.
`Super+Alt+R` opens the native panel; use Left/Right to choose Screen, OpenGL
API injection, launch-profile, or Vulkan mode. Up/Down selects an output,
window, running GLX/EGL process, or profile; Enter starts or stops and Space pauses Screen
capture. Instant
replay is temporarily unavailable in the native engine. Window capture follows
the client as it moves, resizes, or enters fullscreen and includes client
popups but not Luma's server-side decoration.

The direct OpenGL path remains experimental: launch-time hooking and native
`.so` runtime injection have controlled NVIDIA GLX-to-MP4 coverage, including
an LWJGL2 JVM fixture and cleanup which leaves the application running. Runtime
injection is x86-64 only and can be rejected by
Linux ptrace policy or an anti-cheat; signing the library does not grant trust.
Luma does not weaken or bypass those policies. A detected
captured-surface resize rejects and stops the direct capture; it does not keep
encoding at the old dimensions. Do not rely on it for an important recording
until that end-to-end validation is complete.

The Vulkan path is also experimental. It has live NVIDIA 610-series validation
with `vkcube`: 677 H.264 packets over a 1.85-second timestamp span (about 365
distinct FPS at 1140x1386) with a 480 FPS ceiling, a clean full software decode,
the first 30 decoded frames all distinct, an upright image, and a finalized Hybrid MP4. Drivers must
support external DMA-BUF memory, DRM format modifiers, and native fence export;
unsupported formats fail without replacing the API path with screen capture.

The same controls are available over IPC:

```sh
wmctl recorder start output
wmctl recorder start window 7
wmctl recorder start region 100 80 1920 1080
wmctl recorder pause
wmctl recorder stop
```

The packaged build also installs **Luma Recorder** in application menus. For a
local checkout, install the development desktop entry once with:

```sh
install -Dm644 sessions/luma-recorder.desktop \
  ~/.local/share/applications/luma-recorder.desktop
```

Region coordinates are global logical coordinates. Luma chooses the backing
output, and the selected region is cropped and scaled into the fixed output
canvas entirely on the GPU. If the encoder cannot keep up, recorder capture
falls behind without blocking physical presentation.

Verification:

```sh
cargo test -p wm-core --locked
python3 tools/check-nested.py
python3 tools/check-opacity.py
python3 tools/check-idle.py
python3 tools/check-capture.py
python3 tools/check-network-input.py
```

The opacity check samples captured pixels through reload and fullscreen changes,
then checks backdrop softening and blur toggles against a generated checkerboard.
It additionally requires Pillow, Python GObject/GTK4, xdotool and ImageMagick.

The idle check samples CPU time and main-thread context switches over five
seconds in an empty nested compositor, with shell services disabled. Avoid
interacting with its window during the sample. It reports measurements rather
than imposing a machine-dependent threshold; it does not measure GPU use,
frame latency, DRM behavior or the complete desktop's power consumption.

The nested check creates its own temporary config/socket, launches a compositor,
opens test terminals, checks window policy and repeated bar reloads, then exits.
It requires a running X11/XWayland host display and Kitty. It does not validate
DRM, hardware scanout, secure locking, or performance.

`WM_CHECK_INTERACTION=1 python3 tools/check-nested.py` additionally tests real
Wayland move/resize requests and compositor modifier resizing. It requires
Python GObject/GTK4 bindings and xdotool, activates the test window, and moves the
pointer during the test; avoid interacting with the desktop while it runs.

`python3 tools/check-wallpaper.py` generates a short VP8 video using GStreamer and
checks playback, looping, fullscreen and opaque-coverage pause/resume, reload continuity and recovery
from corrupt media in an isolated nested compositor. It requires gst-launch-1.0
with videotestsrc, VP8 and WebM plugins. Hardware decoding and battery behavior
still require device testing.

Video playback errors show the solid background and stop that pipeline until its
content is changed or rebuilt. Unrelated config edits preserve playback.
Video also pauses when rendered opaque foreground regions cover every output
pixel. Gaps, transparency and rounded corners keep exposed backgrounds playing.
Blur and rotated outputs conservatively keep playback active; fullscreen and
lock still pause it. Complex region sets fall back to playback to bound the cost
of checking visibility.
Set `WM_WALLPAPER_DEBUG=1` when launching to log actual pipeline state transitions
and loops for troubleshooting.

Session locking is still under validation; see STATUS.md before relying on it.
Shared-memory screenshots through `ext-image-copy-capture-v1` now work on transformed
outputs with optional cursor inclusion. Client cursor surfaces retain their
hotspot and colors; default theme images are cached, including output scale.
Named cursor shapes use cached theme images and standard aliases, with an arrow
fallback when a shape is absent. The cursor-shape protocol is available to clients.
The capture check also verifies hidden cursors and custom cursor sizing and
hotspots at 150% output scale.
Screenshots are upright in output coordinates, with transformed dimensions and
a normal frame transform. Tests cover all rotations/reflections plus a rotated
window and custom cursor. Portal screenshots and monitor ScreenCast negotiation through
`xdg-desktop-portal-wlr` work, including opening the returned PipeWire remote and consuming
damage-paced frames. Shared-memory capture uses synchronous GPU readback. Clients can instead
allocate a supported modifier on the advertised render node and receive the rendered frame
directly in a DMA-BUF without CPU mapping. The portal fixture forces DMA-BUF caps through
GStreamer's GL upload path, verifies two damage-paced PipeWire frames, and downloads only at the
PNG test sink; ordinary consumers can retain the buffers on the GPU.
Legacy clients can use version 3 of `zwlr_screencopy_v1`, including region, cursor and
`copy_with_damage` support.
Capture is rejected while locked/inactive,
and locking stops existing sessions. DRM capture and lock isolation still need
hardware/protocol validation. `check-capture.py` builds a Wayland C client using
`cc`, `wayland-scanner`, Wayland development files and `wayland-protocols`, then
checks actual captured pixels and shared-memory row/offset handling with Pillow.
It also verifies incompatible buffers fail without writes and the same session
can subsequently deliver a valid frame, including a frame created before an
output resize and submitted afterward. The resize check uses `xdotool`; on
Hyprland it uses `hyprctl` to make only its own test window floating.
Current output dimensions and SHM bounds
are checked before rendering and again before copying pixels.
The compositor does not currently expose virtual-keyboard or input-method globals
because trusted-helper authentication is not implemented. Ordinary keyboard
layouts and repeat settings remain available.

`python3 tools/check-lock-boundaries.py` verifies that a dedicated nested session
withholds unsupported lock and virtual-input globals and preserves desktop focus
when an unmapped Wayland client connects. This does not validate DRM locking.

Wayland and XWayland windows retain a GPU image for a closing fade on the X11
development and DRM backends. The fade follows `animation_ms`, is bypassed by
`reduced_motion`, and clears on workspace changes. Nested capture checks cover
fade progression, rounded corners, shadows, blur, stacking, abrupt disconnects,
X11 `WM_DELETE_WINDOW`, XWayland resize interpolation and mid-resize closing,
and cleanup; physical DRM validation remains pending.

### Optional Winit development backend

Build the optional backend and select it through the nested-session launcher:

```sh
cargo build --workspace --features wm-compositor/winit --locked
WM_NESTED_BACKEND=winit ./tools/run-nested.sh
```

For the closing/capture checks on a native Wayland host:

```sh
WM_NESTED_BACKEND=winit WM_CHECK_CLOSING_ONLY=1 WM_CAPTURE_NATIVE_WAYLAND=1 python3 tools/check-capture.py
```

The native Wayland fixture covers closing and capture behavior without X11 resize/input controls. Build without the optional feature to restore the standard binary configuration.
