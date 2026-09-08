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

Physical input settings apply on the TTY backend, including after reload and
device hotplug. Nested sessions use the host desktop's device settings:

```toml
[input]
pointer_accel = 0.0 # -1.0 to 1.0
natural_scroll = false
tap_to_click = true
mouse_modifier = "Super" # Super, Alt, Control, or disabled
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

`wmctl help` lists IPC commands. `workspace N OUTPUT` selects a workspace on a
specific output. The launcher supports applications, `@` windows, `>` commands
and `:` session actions.

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
