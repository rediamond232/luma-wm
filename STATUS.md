# Implementation status

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
  Unsupported requests retain the current mode and report errors. This compiles
  but needs physical mode-switch and rejection/recovery testing; cross-output
  bandwidth fallback remains outstanding.
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
  method at coordinates 0,0. The nested test
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
  shortcut label, and its screenshot was inspected. Tooltip images,
  fallback ContextMenu coordinates
  and broader real-application compatibility remain pending.
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
  missing/relative-path fallback. Directory monitoring and menu-specific icon
  paths still need broader compatibility validation.
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

Remaining work toward the requested desktop:

- Test TTY3 on the NVIDIA hardware, VT suspend/resume, multi-monitor hotplug,
  fullscreen games and frame pacing. Measure idle CPU/GPU, latency and effect cost.
- Audit and test lock input routing, grabs, capture isolation and presentation
  acknowledgement before relying on the lock for security.
- Finish efficient DMA-BUF streaming and portal integration.
- Finish output mode recovery testing and resizing/closing/workspace animations.
  Extend interaction
  testing to touch/XWayland and measure the opaque-region optimization on hardware.
- Finish tray menus/remaining interactions, real Bluetooth pairing compatibility, and richer media UI.
- Validate video decode acceleration and battery handling; extend background
  occlusion checks to blur/rotated outputs, and benchmark multi-GPU effect imports.

The full original feature set and the TTY3 acceptance gate are still outstanding.
