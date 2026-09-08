# Implementation status

### XWayland display isolation (2026-09-08)

- Smithay's automatic X11 display search starts at `:0`. A missing or stale
  lock file can let its probe remove an existing compositor's filesystem socket
  before the abstract-socket bind reports `Address already in use`; the TTY log
  showed this exact collision against Hyprland's display.
- Luma now reserves its XWayland server from the separate `:100`-`:132` range,
  skips existing lock/socket paths, retries collisions, and continues without
  XWayland instead of crashing if the range is exhausted. An explicit
  `WM_XWAYLAND_DISPLAY` override is available for debugging.

Last verified: 2026-09-06. This is a development compositor, not a finished desktop.

Implemented and exercised in a nested X11 session:

- Master-stack and monocle tiling, focus, workspace transfers, floating state,
  fullscreen transitions, scratchpad and close requests.
- GTK bar, app/window launcher, static background layer and rounded window rendering.
- TOML theme/binding reload, bar geometry/module rebuild, bounded local IPC and
  subscriptions. Reload and monitor rebuild remove old bar polling timers.
- Floating geometry survives fullscreen and tiling transitions. Floating window
  rules apply width/height and fit oversized windows to the usable output area.
- Nine core layout/config/mode-selection/animation tests; workspace compilation and nested policy checks,
  including floating geometry restoration and window-rule sizing.
- Client-initiated Wayland pointer move/resize now updates persistent floating
  geometry. A GTK client and real pointer events verify that the changes persist.
- Configurable modifier plus left-drag moves floating windows; right-drag resizes
  from the nearest corner. Modifier resizing and live disabling are tested.
- Interactive requests reject tiled/fullscreen/hidden windows; XWayland resize
  requests without an active grab no longer panic. Touch and XWayland interaction
  use the same geometry tracking but still need runtime coverage.
- Rounded windows preserve opaque interior regions instead of treating their
  entire area as transparent. Two renderer tests verify shader-mask safety at
  fractional positions and greater than 99% opaque coverage for a 1000x700 window
  with 10px corners; this is geometry evidence, not a GPU benchmark.
- Floating windows remain above tiled content after bar/config rebuilds. Verified
  with real pointer interaction after reload and a nested-window screenshot.
- Launcher requests reuse one per-session window; repeated activation, Escape
  dismissal and reopening pass interaction checks. Window search refreshes live.
- Notification history UI, clear-history control and session-local Do Not Disturb
  are implemented. Delivery, panel opening and popup suppression pass interaction
  checks; history is bounded to 100 entries and does not persist across restarts.
- NetworkManager status is event-driven over the system bus, with no polling
  subprocesses. A real GTK button/private D-Bus test verifies property changes,
  service disappearance and recovery. State mapping covers limited and portal
  access. Non-NetworkManager backends remain outstanding.
- The network bar menu now contains networking and Wi-Fi switches, hardware-block
  status and a connection-settings shortcut. Changes are asynchronous, disable
  duplicate requests while pending and restore actual state on errors. A private
  mock NetworkManager test verifies Wi-Fi writes, networking changes, permission
  denial recovery, no writes during initial synchronization and service restart.
  Real Polkit behavior and the rendered popover still need interactive validation.
- Wi-Fi rows show SSID, strength, security and active status from NetworkManager's
  object snapshot. Matching SSID/security entries prefer the connected AP, then
  stronger signal; rows are bounded to 64 and updates coalesce while loading.
  Open panels follow access-point/property signals; closed panels do not poll.
  Selecting a row requests saved-profile activation and shows errors. Unit/private
  D-Bus tests verify deduplication, exact activation arguments, error recovery and
  AP removal. Real Wi-Fi connection validation remains pending.
- Explicit Wi-Fi scanning sends asynchronous RequestScan calls to up to 16
  adapters. Pending requests suppress duplicate clicks; scan errors re-enable
  controls and display the failure. Signals coalesce over 100 ms before refreshing.
  Private D-Bus tests verify request arguments, rate-limit recovery, a 20-signal
  burst producing one snapshot, and no snapshot reads for a hidden panel.
- New-profile forms support open, WPA-PSK, SAE and OWE networks after saved-profile
  activation fails. Remembering chooses disk persistence; otherwise profiles are
  volatile with autoconnect disabled. Passwords are validated and entry text clears
  on submission, panel close, network selection or service changes. Private D-Bus
  tests verify settings/paths/persistence, failed-request recovery, retry and entry
  clearing. The network popover temporarily enables on-demand keyboard focus.
  Polkit and association failures remain unverified;
  enterprise, WEP and hidden-SSID setup still use external connection settings.
- `check-network-input.py` opens a real layer-shell network popover in an isolated
  nested compositor and mock service. It clicks the menu and password entry,
  types through compositor keyboard routing, verifies profile/retry requests and
  checks keyboard mode restoration on close. The rendered masked entry/form was
  inspected. The test waits for the bar's first frame before injecting input.
- Wi-Fi requests track the returned active-connection object and read its initial
  state, then follow state/removal signals without polling. Progress distinguishes
  connecting, connected, authentication/credential/address/timeout errors and
  disconnection. The mock nested test covers delayed authentication failure,
  activation, result retention across list refreshes and a later disconnect while
  hidden with no snapshot reads. Actual radio association is still unverified.
- Event-driven MPRIS track/artist display and previous/play-pause/next controls
  are included in the default bar modules. The module prefers a playing player
  and respects control capabilities. A GTK/private D-Bus test verifies discovery,
  actual button method calls, metadata/capability updates, removal and recovery.
  A bounded, scrollable selector supports manual choice and automatic fallback
  when the selected player exits; the private-bus test verifies this behavior.
  A more-controls popover adds ten-second seeking, shuffle and repeat modes with
  capability checks, pending-request suppression and inline errors. The private
  D-Bus test verifies signed seek offsets, property writes and signal-driven
  labels, all repeat modes, duplicate suppression, denied-seek recovery and
  capability revocation. A track-specific position slider extrapolates playback
  locally while mapped, follows seek signals and pauses, and stops its timer on
  close. Position reads are bounded to one in flight and stale replies are
  discarded. The private GTK/D-Bus test verifies actual track/timestamp requests,
  elapsed/total display, playback without polling, pause stability, signal updates,
  missing-metadata disablement and no timer updates or reads while unmapped.
  The position checks also run in a real layer-shell popover inside the nested
  compositor via tools/check-media-input.py. Actual pointer clicks open the menu
  and seek; the test verifies continued playback/pause updates while mapped and
  keyboard-mode restoration on close. Screenshot review shows controls and time
  labels without clipping. Media and Wi-Fi input fixtures now load the shell's
  configured theme; both interaction tests pass with it. Popovers, slider tracks,
  checkboxes and switches have explicit palette rules. The shell stylesheet takes
  precedence over host GTK styles within its own process, including button text
  and selected/disabled states. Real-player behavior remains unverified.
  Album artwork now loads while mapped with a 2 MiB input cap, five-second
  cancellation timeout and off-thread 192-pixel thumbnail decoding. Changed URLs
  discard old artwork; unchanged successes/failures avoid repeat reads. Unmapping
  cancels pending loading. File tests verify oversized/corrupt rejection and
  aspect-ratio/color preservation; the real nested popover test verifies loaded
  texture dimensions and pixels, with screenshot review. HTTP(S) relies on GIO's
  installed backend. A loopback HTTP test verifies decoded pixels, oversized
  response rejection and cancellation after response headers arrive while the
  body is pending. Live popover tests also hold an HTTP body while changing the
  artwork URL or closing the menu, then release it and verify stale pixels never
  appear. These tests found synchronous GIO stream destruction could stall the
  replacement until timeout; stream cleanup now waits for pending cancellation
  and closes asynchronously. HTTPS and real-player testing remain unverified.
- Real GStreamer video playback, looping and fullscreen pause/resume pass a
  generated-media integration check. Corrupt media stays stopped during later
  compositor state updates, and selecting valid media recovers playback.
- Unrelated config edits preserve the wallpaper pipeline; playback requests are
  deduplicated while asynchronous state changes are pending.
- Rendered opaque foreground coverage pauses video even without fullscreen.
  Real pipeline transitions verify pause under full coverage and resume for
  transparency, rounded corners and gaps. Coverage work has bounded region and
  fragmentation counts; blur and rotated outputs conservatively retain playback.
- Notification bursts use one layout-managed stack with at most three popups,
  bounded text/action counts, and scrolling for oversized content. Replacement,
  dismissal and eviction cancel obsolete timers. A 12-notification burst test
  verifies replacement IDs, old-timeout cancellation and expiry; a screenshot
  confirms that the newest three entries are visible without overlap.
- Crowded layouts reduce gaps before exhausting available pixels; impossible
  partitions use monocle placement instead of extending outside the usable area.
  Boundary tests cover one-pixel outputs, large gaps and crowded stacks.
- Rounded window hit testing matches the clipped corners on normal outputs.
  Real pointer tests verify click-through at a corner and focus inside the edge.
- Nested X11 rendering now requires scene damage as well as an available present
  buffer; presentation completion alone no longer starts another frame. Event
  dispatch sleeps without a 16ms poll. An empty five-second nested sample dropped
  from 3.6% of one CPU/723 main-thread context switches per second to below CPU
  tick resolution/about one switch per second. This excludes shell/client work
  and is not a DRM/GPU or full-desktop benchmark. Surface destruction explicitly
  requests repaint so disconnects do not rely on continuous rendering.
- Window-rule opacity applies on reload, with the last matching opacity winning.
  Captured nested-session pixels verify opaque, half-transparent and transparent
  windows against a known wallpaper color. Fullscreen forces opacity to one and
  restores the rule value on exit; hardware scanout remains unverified.
- Exterior rounded window borders use configured width and focused/unfocused
  colors, preserving client content. Stable render IDs and commits retain damage
  tracking; fullscreen bypasses decorations. Captured pixels verify placement,
  focus colors and live disable/restore. Rotated outputs currently omit borders;
  fractional-scale and multi-GPU decoration costs still need runtime coverage.
- Configurable exterior shadows share the cached border element and use an
  analytic falloff without framebuffer capture or a separate blur pass. Pixel
  tests verify falloff, unaffected client content and live disabling. Configuration
  validates size/opacity limits. Fullscreen and rotated outputs omit shadows;
  GPU cost and fractional-scale appearance remain unmeasured.
- Opening fades apply to client content, borders, shadows and backdrop blur.
  Configured duration and reduced motion are wired; fullscreen, lock and inactive
  sessions bypass fades. A shared refresh-paced timer exists only while visible
  transitions animate. Captured pixels verify intermediate/final frames and reduced
  motion; easing tests cover endpoints and bounds. Resizing/closing/workspace
  transitions and hardware pacing remain outstanding.
- Per-window blur rules use the cached quarter-resolution GLES backdrop pass.
  A generated checkerboard and captured pixels verify backdrop softening, live
  rule disable, global disable, zero strength and restoration. Effect-setting
  changes invalidate retained output buffers even without new client content.
  Notification history joins the shell blur surfaces. Rotated outputs disable
  these effects; subsurface/pop-up arrangements still need dedicated coverage.

Implemented but requiring further runtime validation:

- DRM/libseat backend, output hotplug/scale/position and XWayland applications.
- DRM startup/hotplug selects configured advertised resolution and refresh,
  publishes advertised modes, and safely handles empty mode lists. Unsupported
  requests warn and fall back. Selection tests cover exact/fractional rates,
  defaults and unsupported requests; actual modesetting needs hardware testing.
- Config reload now tests and requests advertised DRM resolution/refresh changes,
  updates output state after acceptance, and resets buffers/presentation timing.
  Unsupported requests retain the current mode and report errors. Mode changes
  use Smithay's locked output-manager recovery, which retries atomic-test failures
  after submitting fallback frames across active CRTCs and temporarily selecting
  implicit modifiers. This compiles but needs physical mode-switch and
  rejection/recovery testing.
- Connected-display EDID mode updates refresh the mode list and reconsider the
  configured mode. Failed/empty initial setup can retry when EDID changes;
  outputs are published only after successful DRM initialization. An output-state
  test verifies preserving the active mode until replacement and removing stale
  alternatives afterward. Physical EDID/hotplug behavior remains unverified.
- VRR requests are wired through Smithay at DRM startup/hotplug and reload,
  including disabling when configuration is removed. Capability and request
  failures appear in logs/status. This path compiles; physical activation,
  modeset-required displays and VRR frame pacing remain unverified.
- Libinput pointer acceleration, natural scrolling and tap-to-click apply to
  connected devices on config reload and to newly connected devices.
- Downsampled GLES backdrop blur, battery pausing,
  notification actions, process supervision and session locking.
- Wallpaper reload applies its new battery policy and playback state immediately.
- Lock startup now clears keyboard/pointer/touch grabs, held keys, drag icons and
  client cursors. Locked keyboard dispatch rejects ordinary window targets;
  XWayland cannot request keyboard focus, and clipboard focus stays empty.
- Lock startup also cancels tablet grabs and ends active proximity sequences,
  clearing retained tip/button state. Tablet motion, axes, presses and proximity
  entry reject non-lock targets while locked; release/out cleanup remains allowed.
  These paths compile but still need tablet protocol/device testing across lock.
- Lock surfaces receive frame callbacks; VT shortcuts remain available. DRM lock
  acknowledgement is scheduled only after a successful submitted-frame result.
  Submitted frames now carry their lock generation, so older desktop/lock frames
  cannot advance a newer lock's confirmation. A state test verifies generation
  rejection, the two-presentation requirement and fresh hotplug state.
  New DRM outputs receive lock metadata before initialization/publication, closing
  the gap before the next desktop-maintenance pass.
  These changes compile and pass ordinary nested regression checks, but lock
  behavior still needs dedicated protocol and DRM presentation tests.
- Unrestricted input-method/virtual-keyboard globals are no longer advertised.
  Authenticated helper support is required before exposing input injection again.
- Capture sessions are now retained instead of immediately stopped on creation,
  with a 32-session bound and cleanup on client destruction. Lock/inactive or
  removed-output sources are rejected; lock stops existing sessions immediately.
  Output-size changes refresh buffer constraints. Lock lifecycle still needs
  dedicated protocol tests.
- Shared-memory ext-image-copy-capture frame delivery is implemented for transformed
  outputs with optional cursor inclusion. A real Wayland C client verifies screenshot
  delivery, channel order, orientation, window pixels, padded strides and nonzero
  buffer offsets in a nested session. The same GLES path is wired for DRM but is
  hardware-unverified. It uses synchronous GPU readback and a 256 MiB frame cap;
  DMA-BUF streaming and portals remain outstanding.
- Cursor-inclusive capture uses existing pointer render elements. Real nested
  pixel tests verify default theme cursor inclusion/exclusion and a client cursor
  surface's exact bounds, hotspot and color. Theme buffers are cached by image
  and scale. Named shapes load lazily from the theme using standard aliases,
  caching absent shapes too. The cursor-shape protocol is exposed; nested tests
  confirm a real client set_shape request and distinct text/arrow pixels.
  X11 now renders the themed pointer itself. DRM uses named images with corrected
  color format and hotspot positioning. DRM theme image selection and cache keys
  now include output scale, retain fractional hotspot coordinates, and cap image
  retention at 128 entries. Nested pixel tests verify hidden cursors add no pixels
  and a custom cursor at 150% has the expected dimensions, hotspot and color.
  DRM and multi-output cursor behavior still await hardware verification.
- Capture buffers are checked against current output dimensions and checked SHM
  bounds before GPU rendering, then checked again within the write guard. Unit
  coverage includes truncated pools, invalid layouts and the frame size cap.
  The real capture client verifies incompatible-buffer failure without writes
  and successful recovery on the same session. It also creates a frame before
  a real nested output resize, receives updated constraints, submits the stale
  frame, verifies rejection without writes, and captures successfully with a
  replacement buffer on that same session. Physical DRM hotplug is unverified.

- Window position changes now ease using the existing bounded animation timer.
  Retargeting starts from the current mapped position; hit testing follows that
  position. Pointer/touch grabs, fullscreen, hidden windows and reduced motion
  bypass movement. Real capture pixels verify intermediate/final positions when
  toggling floating and immediate placement with reduced motion. Client resizing
  remains immediate, and closing/outgoing workspace transitions are still pending.
- Workspace entry and scratchpad restoration reset the visibility fade. Outgoing
  windows unmap immediately and stop animating; reduced motion bypasses the fade.
  Capture tests verify outgoing content disappears, incoming intermediate/final
  pixels and reduced-motion restoration. Pointer focus now refreshes after scene
  changes, including windows appearing beneath a stationary pointer; the real
  cursor-shape client test verifies this without injecting a new motion event.

- Bluetooth is included as a configurable default bar module. BlueZ's object
  manager supplies adapter power, paired devices and connection state without
  polling. Async power/connect/disconnect requests suppress duplicates, recover
  from errors and reject stale completions after service restart. Controls bound
  displayed adapters/devices to 16/64 and coalesce UI updates. A private D-Bus test
  verifies actual request arguments/state changes, errors, cache updates without
  new snapshots, and service restart. Integrated pairing and rendered interaction
  were added below; real hardware remains unverified.
- Bluetooth discovery can be started/stopped per adapter and tracks only sessions
  acquired by this shell. Menu close and widget destruction release ownership;
  late start replies after closing are immediately stopped. New unpaired devices
  appear through object-manager events. A private GTK/D-Bus test verifies retained
  discovery while open, added/removed devices, close cleanup and late-start cleanup.
  Cleanup also resumes after an in-flight adapter request completes if the menu
  closed while that request occupied the adapter. The private test covers closing
  immediately after requesting power-off during an owned discovery session.

- The battery bar module now consumes UPower's aggregate display device through
  property signals, replacing first-battery sysfs polling. Percentage, charge
  state, low/critical warnings and time estimates are displayed. Private GTK/D-Bus
  coverage verifies state changes without fresh snapshots, invalid-value handling,
  disappearance and service restart. The nested shell smoke check passes. It
  requires running UPower; physical battery behavior remains to be validated.
- Wallpaper power handling now follows UPower OnBattery signals. A sysfs fallback
  timer exists only while UPower status is unavailable, and is removed on recovery
  or application shutdown. The private D-Bus test verifies power changes, loss and
  restart. Real decoded-video playback, looping, fullscreen/coverage pause and
  corrupt-file recovery still pass. The decoded-video test now uses an isolated
  mock system bus to verify battery/AC pause/resume, live pause-policy reload and
  UPower restart followed by a fresh power event. Physical unplugging remains
  unverified.

- Per-output transforms now support normal, quarter turns and reflected variants
  through validated config. Live changes rearrange layer-shell surfaces and
  refresh output geometry before layout. A nested test cycles all eight settings
  and checks output, bar and tiled-window bounds plus invalid-value rejection.
  Removing an output override now restores normal orientation and 100% scale;
  the nested test verifies output, bar and client geometry after removal of a
  combined 90-degree/150% override.
  Physical scanout and physical-device input mapping remain unverified.
- Rotated/reflected capture now uses an upright offscreen framebuffer sized to
  the transformed output. Constraints, SHM validation and writes use those same
  dimensions; frames advertise a normal transform. Real capture pixels verify
  dimensions and orientation for all eight output transforms and window/custom
  cursor pixels at 90 degrees. Existing resize, cursor, animation and reduced-motion
  capture checks pass. Combined rotation/reflection and 150% scaling now pass
  dimension, window-pixel and custom-cursor size/hotspot checks for every transform.
  Nested absolute pointer motion previously omitted the output transform; it now
  maps native window coordinates through rotation/reflection before hit testing.
  Real pointer events verify the resulting cursor position in captured pixels.
  DRM capture and physical-device mapping remain unverified.

- The shell now exports an org.kde.StatusNotifierWatcher registration service.
  Item/host registrations and pending lookups are bounded to 64 each; duplicate
  requests are idempotent, caller service ownership is checked, and ownership
  changes remove registrations and invalidate pending requests. A nested private
  D-Bus test verifies properties, path registrations, invalid/foreign rejection,
  disconnect cleanup and the registry limit.
- The configurable default tray module now creates per-bar host registrations,
  follows item registrations/status changes and displays theme icons or bounded
  ARGB pixmaps. Async property refreshes suppress concurrent duplicate reads.
  Left/middle/right clicks and discrete horizontal/vertical scrolls invoke item
  actions; items without an exported menu use the application's ContextMenu
  method at the icon's output coordinates. The nested test
  verifies actual pixmap colors/size, all three pointer actions, exact signed
  scroll arguments in all four directions, passive/active
  transitions, host/bar recreation on height reload and disconnect removal.
  Exported com.canonical.dbusmenu menus now open an anchored themed popover,
  with bounded on-demand page reads, AboutToShow preparation, submenus/Back,
  separators, disabled/hidden rows and toggle indicators. Visible update signals
  refresh the current page while preserving the selected action; closed menus
  do not read updates. Request generations reject late replies after closing.
  A real nested layer-shell test verifies keyboard navigation, exact action
  arguments, submenu preparation, layout updates, width, focus
  restoration, hidden read suppression and close/reopen delayed-reply isolation.
  A screenshot exposed truncated labels; explicit minimum content width fixed
  them and the corrected rendering was inspected. Menu rows now prefer theme
  icons and fall back to bounded PNG data (256 KiB, 256×256), decoded in one worker
  per visible page; pages without PNG data do not start a worker. Shortcut
  sequences are displayed as bounded labels without global key registrations.
  Unit checks cover decoded RGBA pixels, invalid/oversized PNGs and shortcut
  validation; the nested fixture verifies theme and PNG fallback icons and the
  shortcut label, and its screenshot was inspected. Structured tooltip pixmaps
  are covered below; broader real-application compatibility remains pending.
- A separate VLC 3.0.23 instance now passes an actual tray compatibility check:
  registration, rendered cone icon, exported menu, keyboard Quit and removal,
  with temporary VLC settings and the private nested session bus. Its screenshot
  exposed an off-screen right-edge menu. Popup unconstraining previously only
  handled normal windows; it now handles layer-shell roots using output-local
  bounds and runs again when get_popup assigns a layer parent. The nested menu
  fixture verifies popup geometry at both output edges, and the corrected VLC
  screenshot was inspected. Media playback, other tray implementations and
  physical multi-output popup placement remain unverified.
- Tray overlays now use theme names or the same bounded ARGB pixmap loader as
  normal/attention icons. A 9-pixel non-interactive overlay sits at the lower-right
  of the centered 18-pixel base icon without expanding the button. The nested
  pixel test verifies exact overlay color, size, placement, removal and retention
  through an attention-icon transition. It caught and fixed alignment to the
  button height instead of the icon height; cursor occlusion is excluded from
  these pixel measurements. Status and overlay changes remain signal-driven.
- Structured tray tooltips now show the supplied title and bounded description,
  falling back to the item title when needed. XML formatting becomes plain text;
  entities, link text and image alt text are preserved without file/network
  loads. Malformed markup stays literal. Parsing input is bounded to 8192
  characters and description output to eight lines/2048 characters. Tests cover
  fallback/type checks, malformed markup, entities, alt text, control characters
  and Unicode/line bounds. A real hovered tooltip was inspected; tooltip CSS now
  uses the WM colors instead of inheriting the host desktop's tooltip palette.
- Tray item IconThemePath directories now use a separate cached GtkIconTheme per
  item, with local theme/file lookup before system icons and pixmap fallback.
  Only absolute paths up to 4096 bytes without control characters are accepted.
  Path changes/clearing replace the cache without altering the display theme;
  normal, attention and overlay icon lookup share this path handling. Nested
  pixel checks verify two different icons under the same name in separate flat
  directories, custom-over-pixmap precedence, path replacement, clearing, and
  missing/relative-path fallback. Directory changes are monitored and coalesced
  through one GTK idle refresh; menu-specific paths use the same monitor cache.
  More application-specific icon layouts still need compatibility validation.
- Tray normal/attention/overlay pixmaps now retain separate texture caches keyed
  by supplied pixmaps and requested icon size. Equivalent property refreshes
  reuse the same paintable instead of copying/converting/replacing its texture.
  Selection inspects at most sixteen candidates, validates dimensions and byte
  counts before copying, and converts only the selected valid candidate. A GTK
  check verifies texture identity across 100 equivalent updates, replacement
  after pixel/size changes, clearing, malformed/oversized candidates and the
  candidate-count limit. Rendered overlay/attention/custom-path tests still pass.
  This establishes avoided texture rebuilds, not a measured frame-time or GPU
  power improvement; hardware performance measurement remains pending.
- Tray pixmap selection now includes the GTK display scale in its requested
  source resolution/cache key, and scale-factor notifications refresh normal,
  attention and overlay images from cached properties without a D-Bus read.
  Custom theme lookups also refresh on scale changes. The nested pixel test
  supplies differently colored 18- and 36-pixel sources and cycles output scale
  through 2x, 1x, 1.5x and 1x, verifying the selected source and physical icon
  dimensions while retaining an 18-pixel logical size. Physical mixed-DPI monitor
  movement remains unverified.

- The audio module now has an integrated 0–100% output slider, mute toggle and
  advanced pavucontrol shortcut. wpctl reads/writes are asynchronous with a
  two-second command deadline; pending volume writes retain the latest target.
  A shared pactl subscription replaces the unconditional ten-second timer,
  coalescing relevant sink/server/card events over 100 ms and ignoring unrelated
  client/sink-input churn. Disconnected subscriptions retry every five seconds
  with a fallback state read. Destruction stops the monitor and removes timers.
  An isolated mock-command test on a real nested layer-shell surface verifies
  keyboard volume/mute actions and exact arguments, slider focus, popup focus
  restoration, event coalescing/idle read suppression, read failure recovery,
  monitor reconnect and process cleanup. The themed popover was inspected.
  Hardware volume writes and physical default-device changes were not exercised.
  Microphone controls and output selection were added in the follow-up entries below.
- The audio mock integration now tests denied writes separately from reads,
  confirms that the last readable volume remains usable, hangs a write past
  its deadline and verifies the child is killed/reaped, and checks recovery
  afterward. Timeouts now have an explicit error message. Three requested
  volume targets during a slow write produce only the active and latest writes,
  and the latest target is read back. These cases use isolated fake commands;
  they do not change host audio settings.
- Audio views in one shell process now share the backend state, command queue,
  subscription, retry timer and event coalescing. New views render cached data
  without another state read; volume, mute, availability and errors propagate
  across views. Views are weakly held, and only removing the final view stops
  the subscription. The nested integration verifies one monitor for two views,
  no read on second attachment, synchronized labels through writes/events/errors,
  survival after removing one view, final-view process cleanup and fresh setup
  after all views have been removed. Physical multi-monitor use remains untested.

- The audio popover now lists available outputs and selects the default through
  structured `pactl` arguments. It confirms the selected marker from readback,
  retains keyboard focus across list refreshes, and reports rejected selections.
  Device discovery is limited to visible popovers and refreshes on hotplug,
  default changes and subscription recovery. The isolated nested test verifies
  native keyboard selection, exact command arguments, denied selection without
  changing the marker, device arrival/removal, restart recovery and no hidden
  device-list reads. Parser tests cover malformed/oversized lists and invalid or
  duplicate names. The rendered popover was inspected. Physical output switching
  remains unverified; microphone controls are covered by the following entry.

- Microphone controls now reuse the audio control implementation in a submenu:
  independent input volume/mute, default input selection, read/write errors and
  coalesced writes, with one event subscriber shared with output controls.
  Source events refresh visible input controls without querying output volume;
  hidden input controls do not poll. The nested test verifies real keyboard
  input, exact source targets, unchanged output state, input selection, denied
  writes, read failure/recovery, and Escape preserving the parent popup's keyboard
  mode. The microphone popup was visually inspected. Physical microphone capture,
  hardware mute behavior and real device switching remain unverified.

- New Bluetooth devices now expose Pair, backed by a private BlueZ Agent1
  connection per attempt. The popover supports PIN/passkey entry, zero-padded
  confirmation and display codes, typed-digit progress, authorization prompts,
  cancellation and inline errors. Agent callbacks must come from the captured
  BlueZ unique owner and reference the selected device. The shell does not become
  the default system agent. Closing the popover, losing BlueZ or destroying the
  operation cancels pending pairing and clears credentials; completion closes
  the private bus connection. Pairing has a two-minute command deadline.
  The isolated nested tests verify registration capability, native keyboard
  confirmation/PIN/passkey replies, display progress, wrong sender/device
  rejection, prompt cancellation distinct from Pair completion, user cancellation,
  destruction cleanup and bus-connection release. The device-list integration
  verifies Pair becomes Connect and rechecks power/discovery/connect/restart.
  The pairing popup was visually inspected. Physical devices, pairing modes not
  explicitly covered by these fixtures, and multiple simultaneous hardware
  attempts remain unverified.

- Bluetooth pairing cancellation now has a device-list integration check with
  a deliberately pending Pair reply. Native Escape sends CancelPairing, clears
  the pairing panel and releases discovery; reopening can start a fresh attempt.
  Removing the active device now cancels immediately with an explanatory message,
  instead of waiting for the two-minute Pair deadline. The fixture verifies the
  remote cancellation, panel cleanup and continued power/connect/restart behavior.

- Exported tray menus now read their own IconThemePath list with a bounded
  two-second property request. A per-menu GTK icon theme searches at most 16
  validated absolute directories, with custom icons ahead of the system theme
  and embedded PNGs. A scoped PropertiesChanged subscription refreshes visible
  menus and is released on close; late replies cannot start another layout read
  after closing. The nested menu test verifies custom precedence, directory
  replacement, clearing/relative-path fallback, unchanged global theme paths and
  no closed-menu reads. Shared tray icon/scale tests and real VLC interaction
  still pass. Changes to files inside an unchanged directory remain untested.

- The animation timer now derives its cadence only from outputs with unfinished
  opening or movement transitions. An idle faster monitor no longer increases
  the timer frequency for transitions on slower outputs. The existing 30–240 Hz
  bounds and removal of the timer when animations finish remain in place.
  Formatting, workspace build/tests and the nested compositor check pass.
  Physical mixed-refresh pacing and frame-time measurements remain unverified;
  closing, resizing and outgoing-workspace transitions remain unfinished.

- The DRM backend now schedules a one-shot wakeup at the next visible named
  cursor frame boundary, so animated themes can advance while the pointer and
  desktop are idle. Static themes, all-zero-delay cycles and cycles containing
  only one visible frame do not retain an animation timer. Hiding the cursor or
  switching to a static/client cursor cancels the timer on redraw; pausing the
  session removes it immediately. Existing earlier deadlines survive intervening
  repaints. Frame selection now uses the full elapsed clock and overflow-safe
  delay sums, and retains sub-millisecond precision for the next deadline.
  Unit tests cover cycle boundaries, scale variants, long uptime, zero delays,
  large delays and static fallback. Build and workspace tests pass. The complete
  capture fixture also passes after adding a normal-frame geometry barrier and
  fresh pointer motion to its input setup. Physical DRM idle-cursor animation,
  cursor-plane presentation timing and VT resume remain unverified.

- Tray tooltips now show their named icon or ARGB pixmap beside sanitized text.
  Widgets and the bounded pixmap decoder are initialized on tooltip queries and
  reused, so ordinary status updates do not decode tooltip images. Lookup prefers
  the item's custom icon theme, then the system theme, then the tooltip pixmap;
  missing images preserve text, and image-only tooltips can still open. The
  nested tray test checks exact image color/size, continued visibility while
  hovering and dismissal on pointer exit, alongside existing actions, overlays,
  icon paths and scale coverage. The rendered tooltip was inspected. Broader
  real-application tooltip formats and tooltip scaling across outputs remain
  unverified.

- Lock surfaces now use transformed, scaled logical output dimensions for their
  initial configure. Output-policy updates reconfigure retained live lock surfaces
  after mode, rotation or scale changes, while identical sizes and position-only
  changes do not send duplicate configures. Unlock clears the retained surface
  and configured size with the rest of the output lock state. Tests cover all
  eight transforms, fractional scale, output relocation and mode/scale changes.
  Build, workspace tests and nested compositor regressions pass. Actual lock
  protocol resize exchanges, DRM presentation and security auditing remain
  unverified; nested success does not establish secure locking.

- Added a standalone Wayland protocol client and `tools/check-lock-boundaries.py`
  for the nested backend. The live check confirms that the backend withholds
  ext-session-lock and untrusted virtual-keyboard/pointer globals, an unmapped
  client receives no keyboard focus, and the existing application's focus and
  window state survive client creation/destruction. Workspace commands still
  work afterward. The C fixture compiles with warnings treated as errors and the
  isolated check passes. This verifies the unsupported-backend boundary only;
  it does not exercise accepted DRM locks, pipelined requests to an advertised
  lock manager, presentation guarantees or lock-surface resize exchanges.

- Split output rendering into `capture::render_texture`, which retains a GLES
  texture without CPU readback, and the existing SHM readback path that now uses
  it. This provides a GPU-only capture primitive for retained transitions while
  preserving output transforms, cursor composition and the 256 MiB capture bound.
  Smithay clears renderer surface state during destruction, so closing transitions
  still require capture before cleanup plus per-window lifetime/render handling;
  merely retaining a dead window is insufficient. Build and workspace tests pass.
  The full capture suite passed on the diagnostic run, but two preceding runs
  failed at fractional-scale custom-cursor input placement. That intermittent
  integration result remains unresolved. `WM_CAPTURE_DEBUG` can preserve that
  cursor frame and the assertion now reports its pixel/bounds/dimensions.
  Closing animations themselves remain unfinished.

- The fractional-scale custom-cursor capture check now explicitly reloads and
  waits for scaled output geometry before launching its client. The GTK fixture
  can atomically report pointer enter/motion/leave coordinates and its actual
  configured size through an opt-in receipt. Capture waits for the expected
  logical pointer position and matching client geometry instead of relying on
  two fixed sleeps. Two consecutive complete capture runs pass with this
  synchronization, covering the previously intermittent scaled-cursor assertion
  as well as rotations, resize, hiding, stationary focus and existing fades.
  The receipt code runs only when the fixture environment requests it.

- Added GPU-retained closing fades for non-fullscreen Wayland windows on the
  X11 development and DRM backends. Capture occurs before a null-buffer commit
  clears renderer state, or before destruction cleanup when a client disconnects
  directly. The retained image has no client input role and fades after the live
  window is removed. Per-output retention is bounded to eight images and 128 MiB
  of pixel data, with a 64 MiB per-window capture limit. Expired images, workspace
  changes, locking/inactive state and reduced motion clear the retained list;
  new buffers cancel a snapshot for a remapped surface. Ordinary new-buffer
  commits only scan retained images when that surface was captured. DRM snapshots
  use the primary GLES context used by MultiRenderer's drawing stage.
  Build and workspace tests pass. The full capture suite verifies intermediate
  closing pixels after client destruction, monotonic fade, expiry, workspace
  isolation and reduced-motion bypass. Physical DRM/multi-GPU behavior, remapping,
  mass-close limits and direct-disconnect paths still need dedicated validation.
  Rounded-corner/shadow parity, fullscreen closing, XWayland closing and the Winit
  fallback remain incomplete. Resizing and outgoing-workspace transitions also
  remain outstanding; this is not completion of the full animation plan.

Historical remaining-work checkpoint (superseded by the dated entries below):

- Test TTY3 on the NVIDIA hardware, VT suspend/resume, multi-monitor hotplug,
  fullscreen games and frame pacing. Measure idle CPU/GPU, latency and effect cost.
- Audit and test lock input routing, grabs, capture isolation and presentation
  acknowledgement before relying on the lock for security.
- Validate capture on the physical GPU, across GPUs, and with explicit-sync consumers;
  direct and portal-negotiated DMA-BUF capture are integrated.
- Finish output mode recovery testing. Extend interaction
  testing to touch/XWayland and measure the opaque-region optimization on hardware.
- Test more real tray implementations, finish real Bluetooth pairing compatibility,
  and enrich the media UI.
- Validate video decode acceleration and battery handling; extend background
  occlusion checks to blur/rotated outputs, and benchmark multi-GPU effect imports.

The dated entries below resolve the software items from this checkpoint. Physical
DRM behavior remains the TTY3 acceptance gate.

### Same-surface remapping and closing cleanup (2026-09-07)

- Fixed the initial configure handshake for an xdg toplevel that commits a null buffer and later remaps the same surface. The lookup uses the xdg-shell registry, including windows absent from Space, and waits for the next bufferless commit after unmapping.
- The controlled Wayland client in `tools/remap-client.c` verifies a new buffer after remapping and disconnects without explicitly destroying its protocol objects. Capture checks verify snapshot cancellation on remap, closing fades after abrupt disconnect, expiry, workspace isolation, and reduced-motion bypass. `WM_CHECK_CLOSING_ONLY=1 python3 tools/check-capture.py` runs the focused lifecycle checks; the full capture suite also includes them.
- Validation: workspace tests (28 passed), workspace build without warnings, nested desktop checks, and the full capture suite passed. Logs: `/tmp/luma-remap-tests.log`, `/tmp/luma-remap-build.log`, `/tmp/luma-remap-nested.log`, `/tmp/luma-remap-full-capture.log`.
- These nested results do not establish physical DRM/TTY3, multi-GPU, or XWayland closing behavior. The full plan remains incomplete.

### Rounded closing images (2026-09-07)

- Window snapshots now reuse the live scene's rounded-corner shader and window geometry while rendering the snapshot. The resulting texture retains the mask throughout its closing fade without repeating the mask pass on each animation frame. Output theme radius and fractional scale are used; transformed outputs retain the existing live-scene effect bypass.
- The capture lifecycle checks explicitly configure a 24-pixel radius without border or shadow and verify that corner pixels keep revealing the wallpaper during and after closing. Remap cancellation and abrupt-disconnect checks remain included.
- Validation: build without warnings, all 28 workspace tests, formatting, Python syntax, focused closing capture, and the full capture suite passed. Logs: `/tmp/luma-rounded-closing-build.log`, `/tmp/luma-rounded-closing-tests.log`, `/tmp/luma-rounded-closing-capture.log`, `/tmp/luma-rounded-closing-full.log`.
- Closing shadow/border and blur parity, popup-specific behavior, fractional-scale closing pixel checks, and physical DRM/TTY3 validation remain outstanding. This change does not complete the full plan.

### Closing border and shadow preservation (2026-09-07)

- Closing snapshots now include border and shadow pixels. A shared border-style function supplies both the live scene and snapshot rendering, including focus color and opening opacity. Snapshot bounds expand to include decorations before the existing allocation limit is checked. Decorations are rendered once into the snapshot texture.
- The capture lifecycle fixture checks a floating window with a six-pixel border and sixteen-pixel shadow. Interior, border, and shadow pixels must fade together within a three-channel-value tolerance and return to the original wallpaper after expiration.
- Validation: warning-free workspace build, all 28 workspace tests, formatting, Python syntax, focused closing capture, and full capture suite passed. Logs: `/tmp/luma-closing-decoration-build.log`, `/tmp/luma-closing-decoration-tests.log`, `/tmp/luma-closing-decoration-capture.log`, `/tmp/luma-closing-decoration-full.log`.
- Closing blur parity, popup-specific behavior, fractional-scale decoration pixel checks, and physical DRM/TTY3 validation remain outstanding. The full plan remains active.

### Fractional closing validation and fresh capture receipts (2026-09-07)

- Closing decoration checks now run at output scales 1.0 and 1.5, sampling window content, border, shadow, and rounded-edge pixels. They compare the live scene against the closing fade and confirm that the wallpaper is restored after expiry. The focused run passed without a compositor change.
- Fixed a capture-fixture defect: reusing a descriptive capture name could consume an old image and completion receipt. Every capture request now has a unique serial prefix, including polling retries. Per-scale decoration images also have distinct names.
- Validation: Python syntax and focused capture passed. The first full run failed the pre-existing rotated-cursor bounds check at flipped-270 (23 by 35 pixels); the full rerun passed with fresh capture receipts. Logs: `/tmp/luma-closing-fractional.log`, `/tmp/luma-closing-fractional-full.log`, `/tmp/luma-closing-fractional-full-recheck.log`. The intermittent cursor check is not considered resolved.
- Closing blur parity, popup-specific behavior, and physical DRM/TTY3 validation remain outstanding. The full plan remains active.

### Cursor capture comparison stability (2026-09-07)

- Reproduced a cursor-comparison failure where the entire 960-by-640 scene changed between plain and cursor captures. This is evidence of an invalid comparison baseline, rather than evidence of a cursor-sized rendering defect.
- Cursor comparisons now bracket each cursor capture with two cursor-free captures. They require equal dimensions and an unchanged underlying image, retrying scene changes for up to five seconds. Position, shape, scale, and hiding assertions remain unchanged; a stable but incorrect cursor still fails. Unique capture receipts remain in use.
- Added failure images and state diagnostics for the fractional transformed-cursor bounds assertion, plus before/after diagnostics if the underlying scene never settles.
- Validation: Python syntax and two successive full capture runs passed (`/tmp/luma-cursor-stable-pairs.log`, `/tmp/luma-cursor-stable-pairs-repeat.log`). This improves the test baseline; it does not prove that every intermittent cursor issue or physical cursor-plane issue is resolved.
- Closing blur parity, popup-specific behavior, and physical DRM/TTY3 validation remain outstanding. The full plan remains active.

### Snapshot allocation boundaries (2026-09-07)

- Added explicit snapshot size validation before conversion and GPU texture allocation: dimensions must be positive and representable, scale must be finite and positive, and the rounded physical RGBA allocation must stay within 64 MiB. Decoration padding is included before this check.
- Retention independently rejects an individual texture larger than its 128 MiB per-output budget, protecting the budget if a future backend bypasses the current capture limit. Existing eight-image eviction behavior is unchanged.
- Boundary tests cover the exact 64 MiB limit, the first oversized width, decoration growth, fractional scaling, zero/negative dimensions, non-finite/extreme scales, and dimensions that round to zero.
- Validation: all 30 workspace tests, warning-free build, formatting, and focused closing capture at 1x/1.5x passed. Logs: `/tmp/luma-snapshot-bounds-tests.log`, `/tmp/luma-snapshot-bounds-build.log`, `/tmp/luma-snapshot-bounds-capture.log`. Physical GPU memory measurements and mass-close behavior remain unmeasured.
- The full plan remains active; closing blur parity and other unfinished features are not completed by this validation work.

### Popups outside rounded parent windows (2026-09-07)

- Reproduced a GTK popover being clipped outside its parent by the compositor's rounded-window mask. Toplevel surface IDs and popup surface IDs are now handled separately: popup trees do not inherit the parent's corner mask. Other window elements retain their previous mask behavior. Popup enumeration visits the popup trees directly.
- Snapshot bounds now also include the window's popup bounding box, and snapshot masks similarly exempt popup trees. Preserving a popup through parent destruction has not been validated; clients may destroy popups before the parent.
- Added a controlled non-autohiding popover to the interaction fixture and a capture assertion for green popup content beyond the parent's right edge at 1.5x scale. It failed before the renderer fix and passed afterward.
- Validation: all 30 workspace tests, warning-free build, formatting, Python syntax, and final focused capture passed. Logs: `/tmp/luma-popover-before.log`, `/tmp/luma-popover-tests.log`, `/tmp/luma-popover-build.log`, `/tmp/luma-popover-final-focused.log`.
- Two full capture attempts failed at different earlier gates: a fractional transformed-window pixel assertion at 90 degrees, then a cursor-location assertion. Logs: `/tmp/luma-popover-full.log`, `/tmp/luma-popover-full-recheck.log`. These failures remain unresolved; the full suite is not claimed green for this change.
- Closing blur parity and physical DRM/TTY3 validation remain outstanding. The full plan remains active.

### Input ordering and capture-readiness investigation (2026-09-07)

- Added client pointer/configure receipts to each fractional output-transform check, replacing its fixed pointer delay. The fixture now waits for expected local coordinates and client dimensions. Added host X11 pointer-destination verification before continuing after warps. These changes have not yet produced a complete passing full run.
- Added failure diagnostics in `/tmp/luma-capture-failure.json`: test configuration, compositor snapshot/log, and only the test window's host/X11 geometry. Added a debug-level windowed-input trace of raw coordinates, output transform, and mapped coordinates. Build, formatting, and Python syntax passed.
- A failure trace proved that the requested (200,200) motion reached the compositor, then later input events replaced it before capture. In that run, raw and mapped coordinates matched under the normal transform, and the host/output geometry both measured 960x640. This supports input interference/order as the cause of that particular cursor assertion; it does not resolve all capture failures.
- Tried a separate rootful Xwayland server. Added `WM_CAPTURE_PRIVATE_X11=1` to use direct X11 focus without requiring an EWMH window manager. The private run failed earlier at a rotated-wallpaper pixel assertion, so cursor isolation was not established. The private server was shut down afterward.
- Logs: `/tmp/luma-transform-client-receipt.log`, `/tmp/luma-transform-client-receipt-recheck.log`, `/tmp/luma-verified-pointer.log`, `/tmp/luma-pointer-trace-build.log`, `/tmp/luma-pointer-trace.log`, `/tmp/luma-private-capture.log`. Full-suite failures remain unresolved. Next investigation: ensure client buffers reflect output changes before judging transformed captures, then verify the private-input path.
- The full implementation goal remains active and incomplete.

### Committed layer sizes and private capture runner (2026-09-07)

- Layer IPC diagnostics now include optional `surface_size`, the logical size of the latest committed renderer surface. The field defaults to absent when deserializing older snapshots. Capture requests wait until the wallpaper has committed the output's logical size, independently of the pixel assertions.
- Added `tools/check-capture-private.py`: starts a separate rootful Xwayland display, runs the capture suite with direct X11 focus, and terminates its owned test process group/server on timeout or interruption. Unexpected server exits preserve `/tmp/luma-private-xwayland-failure.log`. The root window can still receive host input; this is not complete input isolation.
- Validation: all 30 workspace tests, warning-free build, formatting, and Python syntax passed. One complete private capture run passed (`/tmp/luma-private-layer-readiness.log`); the repeat still failed a cursor-position assertion (`/tmp/luma-private-layer-readiness-repeat.log`). Repeatability remains unresolved.
- An experiment disabling host-fed input devices inside the private server caused Xwayland crashes. That device modification was removed from the runner. All private servers from these runs were shut down.
- Build/test logs: `/tmp/luma-layer-readiness-tests.log`, `/tmp/luma-layer-readiness-build.log`. The full implementation goal remains active; this work does not complete blur parity or physical DRM/TTY3 validation.

### Retained background blur during closing (2026-09-07)

- Animated blurred windows retain a reference to their existing quarter-resolution framebuffer cache. The closing snapshot samples that retained texture with the live blur shader and strength, instead of capturing a new empty offscreen background. The final closing image therefore includes the blurred background and only needs ordinary alpha fading on later frames.
- Frozen snapshot effects are not marked as framebuffer effects, so snapshot creation does not overwrite the retained background. Popups remain exempt from the parent mask, and the retained blur is attached only to the root surface. References are cleared when blur/animations are disabled or the window is unmapped from Space.
- Added a striped-wallpaper regression: it first proves that blur visibly changes a translucent live window, then checks that multiple closing frames follow the same fade across contrasting background stripes, and that expiration restores the original wallpaper.
- Validation: all 30 workspace tests, warning-free build, formatting, Python syntax, focused private capture, and the full private capture suite passed. Logs: `/tmp/luma-closing-blur-tests.log`, `/tmp/luma-closing-blur-build.log`, `/tmp/luma-closing-blur-focused.log`, `/tmp/luma-closing-blur-full.log`.
- This validates a static background at scale 1.0. Moving backgrounds, multi-GPU context behavior, closing behind occluding windows, and physical DRM/TTY3 remain unverified. The full implementation goal remains active.

### Closing-window stacking and fullscreen bypass (2026-09-07)

- Closing images now record render identities of windows below them, including client surface trees and compositor header elements. The renderer inserts each closing image before the surviving lower content instead of placing every closing image above the entire scene. Newer snapshots are inserted first so an older snapshot can anchor to a lower window that has also closed. Background/bottom layers provide the fallback position; custom/preview elements remain outside this insertion region.
- Fullscreen rendering suppresses closing images, and policy discards those hidden animations rather than retaining their timer.
- Window IPC diagnostics now expose optional committed `surface_size`, separately from target geometry, with a serde default for older snapshots. The fullscreen fixture waits for the client's resized buffer before validating pixels.
- Added functional checks for closing a partly covered window without focusing it (overlap remains covered while exposed pixels fade), closing the foreground window (it remains above its lower window), and entering fullscreen during a close.
- Validation: all 30 workspace tests, warning-free build, formatting, Python syntax, focused capture, and full private capture passed. Logs: `/tmp/luma-closing-stack-tests.log`, `/tmp/luma-closing-stack-build.log`, `/tmp/luma-closing-stack-ready.log`, `/tmp/luma-closing-stack-full.log`. The first fullscreen check sampled before the client resize; the committed-size gate addresses that test ordering.
- Explicit restacking during an active close, simultaneous multi-window closes, SSD-specific pixel behavior, and physical DRM/TTY3 remain unverified. The full implementation goal remains active.

### Explicit focus during a closing animation (2026-09-07)

- User-triggered focus removes the raised window's surface/header identities from closing-image lower anchors, allowing that live window to appear above existing fades. The same update is wired into pointer/touch focus raising. Identity collection is deferred until an output actually has closing images.
- Automatic focus recovery after a close does not perform this adjustment, preserving the foreground closing animation instead of immediately covering it with the newly focused lower window.
- The fixture checks that explicit IPC focus covers the overlapping part of a closing image while exposed pixels continue fading. It then repeats the foreground close and fullscreen check to preserve both behaviors.
- Validation: all 30 workspace tests, warning-free build, formatting, Python syntax, focused private capture, and full private capture passed. Logs: `/tmp/luma-closing-raise-tests.log`, `/tmp/luma-closing-raise-build.log`, `/tmp/luma-closing-raise-focused.log`, `/tmp/luma-closing-raise-full.log`.
- The capture fixture now exercises both explicit IPC focus and a real pointer click through a foreground closing snapshot. Both raise the live underlying window without cancelling the still-visible exposed part of the snapshot. Touch-down uses the same `update_keyboard_focus` path; physical touch injection remains part of hardware validation. Simultaneous closes, SSD-specific pixels, and physical DRM/TTY3 remain unverified. The full implementation goal remains active.

### Winit capture and closing support (2026-09-07)

- The optional Winit backend now implements shared output capture and window snapshots using its owned GLES renderer. It also renders cursors through the shared CaptureCursor path and hides the host cursor, aligning cursor rendering with compositor capture. The nested test title is supported through WM_NESTED_TITLE.
- `tools/run-nested.sh` accepts WM_NESTED_BACKEND=x11 or winit. The optional Winit feature remains opt-in. Added a native-Wayland closing-only fixture mode that skips X11 resize controls, while retaining the closing, decoration, blur, popup, stacking and explicit-focus assertions.
- Validation: optional-feature build and all 30 feature-enabled workspace tests passed without warnings. The native Wayland closing suite passed (`/tmp/luma-winit-wayland-closing.log`). Formatting, shell/Python syntax, and restoration of the standard default-feature build passed. Logs: `/tmp/luma-winit-capture-build.log`, `/tmp/luma-winit-tests.log`, `/tmp/luma-winit-default-build.log`.
- Winit initialization on the private Xwayland server failed with EGL_BAD_CONFIG before rendering (`/tmp/luma-winit-closing.log`); that platform path remains unresolved. Native Wayland input/resize and cursor-specific pixel checks were not exercised by the closing-only fixture. Physical DRM/TTY3 and the rest of the full implementation plan remain outstanding.

### Fullscreen window closing (2026-09-07)

- Fullscreen windows now participate in bounded closing snapshots. Their state is passed explicitly through each backend, selecting the live fullscreen clear color and bypassing rounded masks, border/shadow decoration, and blur. The fullscreen holder drops dead or empty windows so ordinary scene rendering can resume after unmapping/destruction.
- Outgoing fullscreen snapshots anchor above all other ordinary windows, matching fullscreen's override of normal z-order. Active fullscreen content still suppresses unrelated closing images.
- Added checks for fullscreen edge pixels during fade and complete expiry, plus a fullscreen window closing above a newly created, previously hidden window.
- Validation: all 30 workspace tests, warning-free default build, optional Winit feature check, formatting, and Python syntax passed. The full private suite passed before the final fullscreen-underlying-window case was added; the final focused suite includes that case and passed. Logs: `/tmp/luma-fullscreen-close-tests.log`, `/tmp/luma-fullscreen-close-build.log`, `/tmp/luma-fullscreen-close-winit.log`, `/tmp/luma-fullscreen-close-full.log`, `/tmp/luma-fullscreen-close-final.log`.
- Fullscreen unmap/remap state restoration, translucent fullscreen clients, oversized snapshot fallback, and physical DRM/TTY3 behavior remain unverified. The full implementation goal remains active.

### Fullscreen unmap and same-surface remap (2026-09-07)

- A null-buffer commit now resets the compositor's managed fullscreen state after capturing the outgoing image. It clears the output's fullscreen holder, resets cached configure size and movement/opening state, and forces backend buffer reevaluation. The next xdg initial-configure sequence therefore starts as a normal mapping unless the client requests fullscreen again.
- The remap fixture now resubmits its title and app-id before remapping, as required because xdg-shell discards those role attributes on unmap. A remap receives a fresh opening fade; its pixel path is checked against the wallpaper-to-green blend so a retained orange closing snapshot still fails the test.
- Added a focused regression that enters fullscreen, unmaps, verifies fullscreen state clears, remaps the same surface, waits for a 320-by-200 committed buffer, and confirms normal floating geometry and content. The full capture suite includes this regression.
- The fractional transformed-window check now waits for the rendered window pixel to reach the geometry reported by IPC. This closes a separate asynchronous test gap exposed during the full verification run without weakening its expected pixel or cursor assertions.
- Validation: warning-free default build, all 30 workspace tests, optional Winit feature check, formatting, Python syntax, focused fullscreen-remap check, focused closing suite, and the final full private capture suite passed. Logs: `/tmp/luma-fullscreen-remap-build.log`, `/tmp/luma-fullscreen-remap-workspace-tests.log`, `/tmp/luma-fullscreen-remap-winit-final.log`, `/tmp/luma-fullscreen-remap-after.log`, `/tmp/luma-fullscreen-remap-closing-recheck.log`, `/tmp/luma-fullscreen-remap-full-final.log`.
- Client-driven fullscreen re-request during remap, physical DRM/TTY3 behavior, and the remaining full implementation plan are still unverified.

### Simultaneous closing snapshots (2026-09-07)

- The controlled remap client accepts test-only app-id and initial/remap colors, while retaining its bounded defaults. This lets integration checks distinguish multiple independent snapshots and apply size rules to each one. The client still compiles with warnings treated as errors as part of every capture run.
- Added an eight-window simultaneous-disconnect scenario with nested floating sizes and distinct colors. The test reaches the configured image-count capacity, finds a visible point for each z-order layer, then models back-to-front alpha composition so lower closing images showing through higher fading images are verified rather than mistaken for an error.
- The scenario first disables animations for a policy tick and installs a solid wallpaper, preventing a preceding fade from contaminating its baseline. Expiry uses a bounded three-second poll because the eight clients observe their command files independently and therefore receive slightly different close timestamps.
- Validation: the focused closing suite and the full private capture suite passed with all eight snapshots present, correctly ordered, and fully expired. The full suite also includes the fullscreen same-surface remap regression and all existing transform/cursor/blur/decoration checks. The warning-free build, all 33 workspace tests, optional Winit feature check, formatting, Python syntax, and diff checks passed. Logs: `/tmp/luma-eight-close-focused.log`, `/tmp/luma-eight-close-full.log`, `/tmp/luma-eight-close-build.log`, `/tmp/luma-eight-close-tests.log`, `/tmp/luma-eight-close-winit.log`.
- The configured eight-image count boundary now has live GPU coverage. The 128 MiB boundary has unit-level arithmetic coverage but has not been exercised under equivalent real GPU texture pressure. Physical DRM/TTY3 and the rest of the implementation plan remain outstanding.

### Closing retention boundary tests (2026-09-07)

- Extracted the closing-cache eviction decision from GPU texture ownership into a pure bounded calculation. Runtime retention now drains all required oldest entries in one operation instead of repeatedly removing index zero.
- Tests cover seven images fitting without eviction, the eighth-slot eviction, exact 128 MiB occupancy, minimal and multi-entry byte-budget eviction, rejection above 128 MiB, and saturating accounting for impossible overflow-sized inputs.
- Validation: all 33 workspace tests, warning-free default build, optional Winit feature check, formatting, and diff checks passed. Logs: `/tmp/luma-retention-tests.log`, `/tmp/luma-retention-workspace-tests.log`, `/tmp/luma-retention-build.log`, `/tmp/luma-retention-winit.log`.
- Runtime coverage reaches eight simultaneous real GPU snapshots, while equivalent pressure at the 128 MiB byte boundary and physical DRM/TTY3 behavior remain unverified. The full implementation plan remains active.

### Outgoing workspace crossfade (2026-09-07)

- Workspace switches now snapshot the visible outgoing windows before changing the output assignment and fade those GPU-retained images over the incoming workspace. The existing per-output eight-image and 128 MiB limits bound the added retention.
- Snapshot placement preserves the outgoing stack. A fullscreen workspace captures only its visible fullscreen window instead of retaining obscured ordinary windows. When a numbered workspace swaps between two outputs, each output captures its own outgoing workspace before the swap.
- Reduced motion, zero-duration animation, session inactivity, and the lock bypass capture. Re-entering windows retain their existing opening fade, producing a complete crossfade while the animation timer remains active only for visible work.
- The private GPU capture test now verifies intermediate outgoing pixels, complete expiry, incoming progression, and immediate outgoing/incoming changes with reduced motion. The full capture suite passed alongside all eight-image close, blur, decoration, transform, cursor, fullscreen, and remap checks. The warning-free build, all 33 workspace tests, optional Winit feature check, formatting, Python syntax, and diff checks passed. Logs: `/tmp/luma-workspace-outgoing-build.log`, `/tmp/luma-workspace-outgoing-tests.log`, `/tmp/luma-workspace-outgoing-winit.log`, `/tmp/luma-workspace-outgoing-second.log`.
- Physical DRM/TTY3, multi-output swap pixels, XWayland-specific workspace snapshots, and the remaining implementation plan are still unverified.

### GPU resize interpolation (2026-09-07)

- Layout and floating-state size changes now interpolate the live surface tree on the GPU while sending only the final configure size to the client. This avoids repeated application redraws during a compositor animation. Position and size share the same easing duration and can progress together.
- The current rendered size is retained in window user data, so retargeting starts from the visible geometry. If a client has not committed its final buffer when interpolation ends, that buffer stays scaled to the target instead of jumping back to its old dimensions. Interactive pointer/touch grabs, fullscreen, lock/inactive state, reduced motion, and zero-duration animation retain their existing immediate behavior.
- Root subsurfaces scale around the animated window rectangle, while independent popups keep their protocol-managed geometry. Rounded clipping, blur, borders, and shadows use the animated dimensions. Resized elements conservatively stop advertising opaque regions during interpolation so damage/occlusion cannot hide pixels outside stale client geometry.
- Closing capture reads the same animated geometry. A dedicated real-GPU regression disconnects a client midway through a 320-by-200 floating-to-tiled resize, verifies that the retained image stays within 25 pixels of the last live bounds despite asynchronous command delivery, then verifies complete expiry. The main capture test also proves intermediate width and height, final target geometry, and reduced-motion changes without interpolated client sizes.
- Validation: the focused closing suite and final full private capture suite passed, including outgoing workspaces, eight simultaneous closes, blur, decorations, fractional transforms, cursors, fullscreen, and same-surface remapping. The warning-free build, all 33 workspace tests, optional Winit feature check, formatting, Python syntax, and diff checks passed. Logs: `/tmp/luma-resize-close-focused5.log`, `/tmp/luma-resize-build-verified.log`, `/tmp/luma-resize-full-verified.log`, `/tmp/luma-resize-tests-verified.log`, `/tmp/luma-resize-winit-verified.log`.
- Physical DRM/TTY3 and hardware frame-time/cost measurements remain unverified. The full implementation plan remains active.

### XWayland closing snapshots (2026-09-07)

- The X11 unmap handler now captures the associated Wayland surface tree before removing the XWayland window from Space. It reuses the same bounded GPU retention, decoration/effect rendering, stacking, animation timer, reduced-motion behavior, and remap cancellation as native Wayland closing.
- Added a deterministic XCB fixture with a solid 360-by-220 surface, explicit `WM_CLASS`, and `WM_DELETE_WINDOW` support. The fixture compiles with all warnings treated as errors and avoids dependencies on terminal fonts, themes, or toolkit timing.
- The XCB fixture now toggles from its 360-by-220 floating rule into tiled geometry, verifies intermediate GPU-scaled width and height, and closes through `WM_DELETE_WINDOW` during that resize. The focused and full suites verify the retained blended pixel after X11 unmap and exact expiry to the background. The full suite still passes native Wayland closing, eight simultaneous snapshots, mid-resize disconnect, outgoing workspaces, fullscreen/remap, blur, decorations, transforms, and cursors. All 33 workspace tests, optional Winit feature check, formatting, Python syntax, C warning checks, and diff checks passed. Logs: `/tmp/luma-xwayland-close-build.log`, `/tmp/luma-xwayland-resize-focused2.log`, `/tmp/luma-xwayland-resize-full6.log`, `/tmp/luma-xwayland-resize-tests.log`, `/tmp/luma-xwayland-resize-winit.log`.
- Physical DRM/TTY3 XWayland behavior, override-redirect windows, transformed-output XWayland resize, and broader application compatibility remain unverified. The full implementation plan remains active.

### Tray action coordinates (2026-09-07)

- StatusNotifier `Activate`, `SecondaryActivate`, and fallback `ContextMenu` calls now receive the clicked icon's logical output coordinates instead of `(0, 0)`. The shell walks GTK widget coordinates to the bar root, adds the monitor origin, and accounts for a bottom bar's offset using the allocated bar height.
- Each per-output tray host retains its monitor geometry, so recreated bars and multiple outputs do not share an implicit origin. Coordinate addition is saturating at the D-Bus `i32` boundary.
- The real nested X11/Wayland fixture records D-Bus arguments for all three actions and checks them against the captured icon center. It recreates the bar at the bottom of the output and verifies the resulting global y coordinate. The focused coordinate test and the complete tray UI fixture pass on an isolated rootful Xwayland server, including tooltip hover/dismissal, ARGB and overlay pixels, icon changes, scale changes, four scroll directions, state changes and disconnect removal. Logs: `/tmp/luma-tray-host-debug.log`, `/tmp/luma-tray-ui-private-final.log`.
- The live D-Bus menu fixture also passes there, covering keyboard navigation, submenu preparation, updates, action arguments, output-edge placement, focus restoration and delayed-reply isolation (`/tmp/luma-tray-menu-private-final.log`). The private runner starts AT-SPI directly because its isolated D-Bus session has no user-systemd instance; this prevents GTK test applications from exiting during accessibility registration.
- The final warning-free workspace build, all 33 tests, optional Winit feature check, formatting, Python and shell syntax, C fixture warning check, and diff check passed after the fixture reliability changes. Logs: `/tmp/luma-tray-final-build.log`, `/tmp/luma-tray-final-tests.log`, `/tmp/luma-tray-final-winit.log`. Physical multi-output coordinates and more real applications that consume the fallback position remain unverified. The full implementation plan remains active.

### Portal screenshots and damage-paced ScreenCast (2026-09-07)

- The compositor now advertises the version-3 `zwlr_screencopy_manager_v1` compatibility protocol alongside `ext-image-copy-capture-v1`. The compatibility path supports output regions, optional cursor composition, ARGB shared-memory buffers, damage events, timestamps, transforms, one-use frames, and bounded pending damage requests.
- Later `ext-image-copy-capture-v1` frames are queued per session until their output is redrawn, while the first successful frame remains immediate. Queues are bounded, aborted or destroyed frames are removed, invalid sessions fail, and completed frames conservatively report full-buffer damage. X11 and Winit release frames only after renderer damage; DRM releases them after an active scheduled repaint.
- `XDG_CURRENT_DESKTOP=wm:wlr` lets the system portal select `xdg-desktop-portal-wlr` while preserving the desktop's `wm` identity. A real isolated nested test verifies whole-output, exact clipped-region, cursor-inclusive and portal screenshots. It negotiates a monitor ScreenCast, opens the returned PipeWire file descriptor, proves a second frame does not arrive while idle, changes the visible background, and consumes two complete 1280-by-800 PNG frames with GStreamer.
- A forced legacy client now captures an initial SHM frame, reuses the manager and buffer for `copy_with_damage`, and emits a synchronization marker only after the compositor has queued the request. The fixture proves no second image arrives during an idle interval, changes the visible background, then validates the released frame's dimensions and pixels. This covers the compatibility dispatch and its damage queue instead of inferring support from advertised globals.
- Validation: warning-free workspace build, all 35 workspace tests, optional Winit feature check, formatting, Python and C warning checks, and two consecutive complete portal/screencopy fixtures passed. Log: `/tmp/luma-screencopy.log`. Physical DRM/TTY3 capture remains unverified. The full implementation plan remains active.

### Direct DMA-BUF capture (2026-09-07)

- Capture constraints now advertise formats and modifiers that the active GLES context can render, grouped by FourCC, together with its DRM render node. X11 and Winit use their EGL render node; DRM uses the primary GPU renderer. Backends with no usable render node continue to advertise shared memory only.
- A DMA-BUF frame is rendered directly into the client allocation with the same transformed output, cursor and effect composition as shared-memory capture. It avoids the offscreen-texture mapping and full CPU copy. Size and 256 MiB limits are checked before binding; Smithay validates the advertised node-independent format, modifier and dimensions before dispatch.
- Added a strict C protocol fixture that reads the advertised node and modifier constraints, opens the matching render node, allocates a GBM buffer, imports it through `zwp_linux_dmabuf_v1`, attaches it to `ext-image-copy-capture-v1`, and requires a successful frame. The isolated capture suite confirms that this reaches the DMA-BUF renderer path before continuing through SHM pixels and portal streaming.
- The portal fixture additionally forces `video/x-raw(memory:DMABuf)` into GStreamer's GL upload path. The real `xdg-desktop-portal-wlr` stream negotiates two DMA-BUF frames; the compositor log proves both use the direct renderer, while the downstream PNGs prove the visible background change and damage pacing. CPU download occurs only at the test's PNG sink.
- Validation: the direct fixture compiles with all warnings treated as errors and the full isolated screencopy/portal test passes. The direct client's tested tiled modifier cannot itself be CPU-mapped by GBM, while the independent portal GL path verifies the frame contents after importing that class of buffer. Cross-GPU allocations, explicit-sync consumers, and physical DRM/TTY3 remain unverified. The full implementation plan remains active.

### Coordinated DRM mode recovery (2026-09-07)

- Output reloads now call the locked `DrmOutputManager::use_mode` path with a renderer and fallback frame set. The manager first tests the requested mode directly. For atomic-test failures caused by total CRTC bandwidth or modifier combinations, it submits fallback frames on the other active outputs, retries the mode, retries all CRTCs with implicit modifiers when required, and then attempts to restore preferred modifiers.
- Output protocol state, swapchain reset and presentation-timing reset still update only after the manager accepts the change. Unsupported configuration requests never enter DRM recovery, and any failed recovery retains the compositor's previously published mode while reporting the error through status.
- Validation: warning-free default compilation and existing mode-selection/advertised-mode state tests pass. Real atomic bandwidth exhaustion, flicker behavior during fallback frames, rollback on a kernel rejection, VRR interaction, and physical multi-monitor mode changes require the TTY3 hardware pass. The full implementation plan remains active.

### Live tray icon-directory monitoring (2026-09-07)

- Per-item and exported-menu `IconThemePath` directories now use GIO file monitors. Creates, replacements, moves and removals mark the isolated GTK icon theme dirty, rebuild it on the GTK main loop, and refresh the owning item or visible menu without requiring a StatusNotifier or D-BusMenu property signal.
- Bursts of filesystem events coalesce into one idle callback. Path changes release old monitors, empty or invalid paths retain fallback behavior, and item callbacks hold only a weak reference to the update closure so destroying a tray button does not create a monitor/update reference cycle.
- The real tray fixture overwrites the currently displayed custom PNG twice without changing item state or emitting `NewIcon`, and validates both pixel-color transitions. The live menu test also passes with its monitored path cache.
- Bar recreation exposed a race where the watcher's first registered-item read could be lost while output scale changed, leaving a fallback-only bar until another signal. Each host registration now schedules one bounded 250 ms reconciliation, and every new item button performs one property refresh. The fixture establishes its multi-resolution state before rebuilding bars.
- A failed asynchronous item-proxy construction no longer leaves that item permanently marked pending. The host removes the failed generation and schedules at most three short event-loop retries; a success clears the retry count. Shell config notifications now coalesce for 75 ms before rebuilding bars, allowing the compositor to publish matching output scale/transform state and collapsing editor write/replace bursts into one recreation.
- The private Xwayland fixture now sizes the nested compositor window to the host-controlled root bounds and waits for compositor IPC to report the matching output size before checking right-aligned pixels. Failure diagnostics include compositor layer state and X11 display/window geometry. This prevents a clipped host surface from being reported as a missing tray icon.
- Validation: warning-free shell and workspace builds, Python syntax, three consecutive clean isolated tray UI fixtures, and four consecutive live D-Bus menu fixtures pass after the final changes. Physical multi-output scale moves and more application-specific icon directory layouts remain unverified. The full implementation plan remains active.

### Input raising and transformed popup resize coverage (2026-09-07)

- The private visual fixture now closes a foreground window, moves the real nested X11 pointer onto the exposed lower client and sends a primary-button click. Captured pixels prove the live client rises above the overlapping closing snapshot while the snapshot's exposed remainder continues fading. This exercises the compositor's pointer-driven `update_keyboard_focus` and `raise_above_closing` path; touch-down calls the same focus routine.
- At a 90-degree output transform, the fixture changes a live client from tiled to floating with a two-second animation. Exact client-color bounds are captured before, through eight intermediate frames and after completion. The bounds must remain present, reach a distinct final size and contain a size strictly between the endpoints.
- A real GTK popover is kept open while its parent changes from floating to tiled at 150% output scale. Every intermediate capture must retain the popup, and its captured bounds must move between distinct starting and final anchors.
- Validation: Python compilation and diff checks pass; the focused closing suite passes with pointer and popup-resize coverage, and the complete private capture suite passes with pointer raising and transformed resize coverage. Two consecutive complete private suites also passed immediately before these additions, resolving the earlier transformed-window/cursor repeatability concern. Logs: `/tmp/luma-pointer-test-output.txt`, `/tmp/luma-pointer-full-output.txt`, `/tmp/luma-transformed-resize-output.txt`, `/tmp/luma-popup-resize-output.txt`. Physical touch injection, DRM/TTY3 rendering and hardware frame-time measurements remain unverified. The full implementation plan remains active.

### Settled tray reloads and richer launcher rows (2026-09-07)

- Tray items now reconcile their cached properties and icon source once their button is mapped. This closes the lifecycle gap where the initial asynchronous proxy refresh could finish before a recreated bar supplied the button's monitor scale.
- The tray scale fixture returns the exact observed bounds instead of taking a second racy screenshot, and requires four consecutive correctly sized frames after every 2x, 1x, 1.5x and 1x transition. Ten consecutive private runs pass. Shared private-host fitting keeps right-aligned controls inside a host-constrained rootful Xwayland display; six consecutive live D-Bus menu runs and the installed VLC icon/menu/Quit path pass with reachable coordinates.
- Launcher application rows now use native desktop icons, bold titles and muted descriptions. Window, command and session-action modes use matching symbolic icons and contextual subtitles. Filtering, selection, activation and singleton behavior remain unchanged. A focused private fixture resizes the live output and captures the complete launcher; `/tmp/luma-launcher-final.png` was inspected at 943 by 800 pixels.
- Warning-free shell compilation, all 19 shell tests, Python syntax, launcher lifecycle, real Wayland pointer/resize interaction, rendered opacity reload and the focused launcher receipt pass. Physical DRM/TTY3 appearance, input and performance remain the acceptance gate.

### Pre-TTY3 software readiness audit (2026-09-08)

- The requested Smithay tiling desktop scope is present: master-stack and monocle layouts, nine per-output workspaces, focus/move/send/ratio controls, floating, fullscreen, scratchpad, rules, mouse/touch interaction, native Wayland, XWayland and hot-reloaded TOML bindings.
- The integrated desktop shell is present: configurable per-output bars and module order, application/window/command launcher, notifications/history, tray and D-BusMenu, NetworkManager/Wi-Fi, BlueZ pairing, audio/microphone, battery, MPRIS controls/artwork and session actions. Colors, font, size, opacity and geometry come from the same live configuration.
- The requested visual and wallpaper scope is present: rounded client masks, borders, shadows, per-window opacity and blur, downsampled cached backdrop blur, opening/closing/movement/resize/workspace animations, reduced motion, image wallpaper and decoded looping video wallpaper with battery, fullscreen and opaque-coverage suspension.
- The performance and interoperability scope is present in software: damage-based redraws, refresh-paced timers, bounded GPU closing images, direct DMA-BUF and SHM capture, `ext-image-copy-capture-v1`, version-3 `zwlr_screencopy_v1`, portal screenshots, damage-paced PipeWire ScreenCast, DRM/libseat, mode selection and recovery, scale/transform/position/hotplug, VRR requests and libinput configuration.
- XWayland configure notifications now keep override-redirect menus and tooltips above managed windows. New X11 clipboard and primary selections are accepted only while an X11 window from that Xwayland instance owns keyboard focus.
- GTK focus styling now uses the supported `button:focus` and `entry:focus-within` states, restoring the accent outline. Luma's own stylesheet parses cleanly; the remaining `gtk.css:312` warning in local fixture logs comes from the user's global GTK4 theme (`.boxed-list row:insensitive`), outside this repository. The shipped default terminal is Kitty, matching the installed TTY3 target environment.
- Read-only `status` and process-only `exec`, terminal, launcher, lock and quit IPC commands no longer request a scene redraw. Surface commits and state-changing commands still schedule frames. This removes needless renderer wakeups and preserves damage-paced capture semantics.
- The final capture fixtures normalize numeric X11 window IDs, scope closing-expiry masks to the former window region, hide the portal cursor for the idle-damage assertion, make private-host sizing idempotent, wait for bottom-bar tray bounds after reload, and settle VLC menu focus before activation. The legacy screencopy fixture drains earlier real damage and then proves a subsequent `copy_with_damage` stays pending through an idle interval before a second visual change. These changes remove host-input, clock-text, queued-reload and stale-geometry false failures without weakening compositor assertions.
- Current-worktree validation passes: all workspace tests, default workspace build, optional Winit feature check, formatting, shell/Python syntax and diff checks; full and focused private GPU capture; real Wayland interaction and opacity reload; SHM and direct DMA-BUF capture; legacy screencopy, portal screenshot and two-frame damage-paced PipeWire ScreenCast; network, audio/microphone, media/artwork, Bluetooth pairing, video wallpaper, idle, lock-boundary and output-transform fixtures; generic tray menus, real VLC tray action and repeated tray scale/reload checks. Receipts are under `/tmp/luma-final-*.log`.
- The implementation is ready for the user's TTY3 acceptance pass. That pass must still establish NVIDIA DRM startup, real monitor modes and VRR, VT leave/resume, hotplug, physical input, fullscreen applications and subjective appearance/frame pacing; nested software results cannot prove those hardware properties.
- Final exact-worktree receipts: locked offline build and all 35 workspace tests, optional winit feature check, formatting, shell/Python syntax, default-config validation, shared-library resolution and diff checks passed; the full private GPU/XWayland capture passed in `/tmp/luma-final-post-ipc-capture.log`; five consecutive SHM/DMA-BUF/legacy/portal/PipeWire capture runs passed in `/tmp/luma-final-exact-screencopy-1.log` through `-5.log`; and the inspected launcher receipt is `/tmp/luma-launcher-final-exact.png`.

### TTY3 configurable key binding dispatch fix (2026-09-08)

- The first physical TTY3 run showed the compositor panicking at `input_handler.rs:956` as soon as a configured binding produced `KeyAction::Desktop`. Both backend dispatch tables omitted that common action even though `process_common_key_action` already implemented it. The DRM table reached `unreachable!()` and the nested backend rejected the action as unsupported.
- `KeyAction::Desktop` now enters the common command dispatcher on both paths. The DRM match is exhaustive at compile time instead of ending in `unreachable!()`, so adding another action cannot recreate the same unchecked crash. A private real-keyboard regression sends `Super+2` and `Super+1` through XTest and verifies workspace changes through compositor IPC; `/tmp/luma-keybind-regression-final.log` passes along with all 35 workspace tests and the optional winit build.
- `tools/run-tty.sh` now performs a locked offline incremental build on every launch, truncates the prior session log, prints the active config and log path, and shows the final 30 log lines when the compositor exits. This prevents stale debug binaries and makes future TTY failures immediately visible.
### Human-readable output refresh configuration (2026-09-08)

- Output configuration accepts `hz` as a human-readable floating-point refresh
  rate, for example `hz = 360.0`, while retaining the legacy millihertz
  `refresh` field. Setting both is rejected as ambiguous.
- Mode selection converts Hz once and keeps the existing 0.5 Hz tolerance, so
  integer requests match advertised fractional modes such as 359.98 Hz.
- The default configuration now includes a commented connector example. Core
  mode-selection tests and the workspace build pass; applying a new mode still
  requires a physical TTY3 check.
