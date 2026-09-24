# Luma Direct Game Capture Handoff

Updated: 2026-09-20

## Resume here — September 19 capture-performance work

This section supersedes the September 17 historical notes below. The latest
request was to fix intermittent, wave-like stutter in **saved video playback**,
not merely reduce Minecraft render-thread overhead. The user watches with mpv
and wants genuinely distinct high-FPS frames, not nominal CFR or duplicated
frames. Latest tested settings were **2560x1440, 360 FPS, H.264 QP 18**.

### Current outcome and remaining problem

Implemented and rebuilt a generic GLX/EGL receiver synchronization fix. No
Minecraft/LWJGL-specific capture dependency was introduced. It removes encoder
input-resource recycling from both capture reception and frame submission.

On September 20, receiver scheduling was extended to absorb transient encoder
overload instead of dropping as soon as the former four-frame staging pool
filled. The receiver now preallocates a FIFO of full-resolution GPU textures,
bounded by both time and memory: 250 ms and 1024 MiB by default, whichever is
smaller (4..256 frames). `LUMA_GAME_CAPTURE_BUFFER_MS` and
`LUMA_GAME_CAPTURE_BUFFER_MIB` can override those bounded limits. At
2560x1440/360 the default is 72 frames, about 200 ms and 1012 MiB. Accepted
frames wait only on the receiver's private encode worker, remain FIFO, and keep
their original source PTS; the game/export import thread still never waits for
NVENC. The NVENC input ring is now twelve frames, the compressed transport ring
is sixteen, and the output collector is event-driven instead of polling every
100 microseconds.

This improves burst tolerance, not sustained throughput. Once the bounded GPU
reservoir fills, new frames still drop rather than allowing unbounded VRAM and
latency. No fresh Minecraft moving-camera sample was run for this scheduler
change, so the earlier heavy-scene result below remains the latest live-game
evidence and 360 FPS must not yet be claimed.

Last session's live Minecraft 1.21.11/Sodium tests on an RTX 3090 Ti showed:

| Test | Measured delivery | Interpretation |
| --- | --- | --- |
| Before collector-context fix, focused movement | 231–273 frames per full second | Decoupling reception alone was insufficient; recycling still cost about 3 ms on submission. |
| After fix, focused low-motion scene | 355–357 frames per full second | Near requested 360 FPS after startup. Submission admission/reclaim benchmark about 0.1 microseconds. |
| After fix, focused moving camera | 268–326 frames per full second | Improved, but NOT stable 360 FPS; NVENC mostly 98–100% utilized. Admission benchmark 0.2–0.5 microseconds. |

The moving test wrote 4,253 frames over 14.925 seconds including startup,
with 54 staging drops and 532 pre-encode submission drops. The hook reported
4,840 copied frames and 52 full-pool drops. One interior PTS gap reached
31.72 ms; most per-second maximum gaps were about 7–10 ms. Do not claim the
wave-like playback issue is completely solved. Heavy-scene encoder capacity
or encoder scheduling still needs work; utilization alone does not establish
an immutable hardware ceiling.

Resolution, quality, and target FPS were not reduced. No system installation,
release publication, or commit was performed. Receiver-only changes do not
require restarting the game. Rebuilding/replacing the loaded hook does.

**Evidence availability:** the measurements above are recorded session results,
not rerun on September 20. On September 20 the source and receiver artifacts
were checked, but the `/tmp` recordings/profiles listed below were already gone.
Do not offer those old paths as downloadable files or pretend to re-inspect them.

### Architecture and ownership

`game present -> shared VRAM textures -> receiver staging textures -> NVENC input textures -> compressed muxer stream`

- `native/game-capture-gl/src/luma_game_capture_gl.c`: generic GLX/EGL present
  interception and resident-hook re-arming; late attachment remains riskier
  than launch-time preload. Earlier user crash was in `injected_hook_worker`
  (`libluma-game-capture-gl.so+0x68bd`), not the missing narrator/flite library.
- `src/luma_gl_export.cpp`: game-side GPU copy and transport. Does not link
  NVENC. Four shared textures; full pool drops opportunities without waiting
  for encoding. External GPU semaphores/fences protect ownership.
- `src/luma_gl_vram.h`: Vulkan allocates exportable device-local memory and
  semaphores on the matching GPU UUID; the game still renders with OpenGL.
- `src/luma_capture_pacing.h`: phase-preserving pacing. Previous `due = now +
  interval` incorrectly undersampled noninteger source/target ratios. Tests
  cover 500->360, 774->360, 1000->480, 500->240, and 220->360 without duplicates.
- `native/vulkan-external-capture-prototype/luma_vk_capture_receiver.cpp`:
  shared source for OpenGL and Vulkan receivers. `EncodeWorker` owns the
  resolution-aware, preallocated staging FIFO. Import thread blits and
  asynchronously ACKs the game's texture after GPU completion; it does not
  call NVENC. Submission worker owns a shared EGL context and context-local
  FBO. Cross-context fences protect copy completion and staging reuse. Mutex
  protects only the bounded work queue, never a driver wait. Stop drains it.
- `native/game-capture-gl/src/luma_nvenc_direct.cpp`: `poll_harvest(...,
  release_inputs=true)` unmaps completed inputs on the output collector's own
  shared EGL context, then marks slots reusable. `start_output_worker` creates
  that context (around line 1172). Blocking bitstream collection and unmapping
  no longer run on the submission context. Keeps a small pipeline lead and
  drains on stop; legacy harvest behavior remains separate.
- Input textures must not be reused before unmap finishes; staging textures
  must not be overwritten before the submission worker's GPU read completes.
  FBOs are context-local, textures/syncs shared. Preserve these boundaries.
- Wrapper authentication remains PID/ancestry plus private sockets/token;
  readiness is signaled after the first accepted encoder submission. Direct
  capture remains video-only. Do not silently change audio/codec settings.

### Investigation that motivated the fix

A temporary diagnostic receiver timed `nvEncUnmapInputResource` at mean
3.46 ms and max 6.82 ms, versus a 2.78 ms budget at 360 FPS. Another focused
profile only delivered about 235 FPS at 48–50% NVENC utilization, disproving
the earlier blanket claim that all poor throughput was hardware saturation.
Moving unmap to the output collector's shared context removed the measured
submission-stage stall. Merely adding staging/another thread did not suffice.

Several samples fell to exactly 15 FPS after Minecraft lost focus. Background
throttling is a separate confounder, not an explanation for all active-scene
waves. Always log focus and avoid running other GPU fixtures during profiling.

### Build and validation

Receiver artifacts:

- `native/game-capture-gl/build/luma-opengl-capture-receiver`
- `native/vulkan-external-capture-prototype/luma-vulkan-capture-receiver`

Both receiver artifacts were rebuilt again on September 20 after the scheduler
change.

Build receivers without unnecessarily replacing the game-loaded hook:

```sh
make -C native/game-capture-gl build/luma-opengl-capture-receiver
make -C native/vulkan-external-capture-prototype luma-vulkan-capture-receiver
```

Passed in the last session:

```sh
bash native/game-capture-gl/tests/shared-vram-egl.sh
LUMA_GAME_CAPTURE_RUNTIME_SECONDS=6 make -C native/game-capture-gl \
  test visual-integration-glx rerecord-integration-glx
git diff --check
```

Passed after the September 20 scheduler change:

```sh
make -C native/game-capture-gl -j2 test
make -C native/game-capture-gl shared-vram-integration-egl
LUMA_GAME_CAPTURE_RUNTIME_SECONDS=6 make -C native/game-capture-gl visual-integration-glx
LUMA_KEEP_RERECORD_TEST=1 make -C native/game-capture-gl rerecord-integration-glx
make -C native/vulkan-external-capture-prototype luma-vulkan-capture-receiver
git diff --check
```

The final 1440p/240 EGL saturation fixture used 60 staging frames, reached a
43-frame high-water mark, and reported 948 submitted frames, 948 written
frames, zero submission drops, zero staging drops, and 69 distinct hashes in
the first 100 decoded frames. Packet PTS were monotonic with the original
5.526-second span. Its deliberate stopped-receiver interval created a
1.49-second source gap, so that fixture is lifecycle/backpressure and timeline
evidence, not a smoothness benchmark.

A final synthetic 2560x1440/360, QP 18 run exercised the shipped 72-frame
reservoir under concurrent desktop/game GPU load. It processed 2,839 exports,
submitted and wrote 2,736 frames over 8.009 seconds, preserved monotonic PTS,
decoded 91 distinct hashes in the first 200 frames, and had zero submission
drops. The reservoir reached 71/72 and then dropped 103 frames, demonstrating
that it absorbs a long burst but cannot hide sustained encoder underspeed.
Earlier same-fixture tuning improved delivery from 2,646 to 2,820 frames by
deepening the collector lead; twelve NVENC surfaces was retained, while a
sixteen-surface experiment regressed to 2,680 frames and was reverted.

EGL regression now compares submitted-frame count with MP4 frame count on
stop, catching incomplete worker drain. It also verifies decoded colors and
orientation, NVENC absent from game mappings, game survival after recorder
stop, and continued game rendering while the receiver is paused. GLX visual
and two-recordings-on-one-process tests passed. A three-second GLX run failed
because startup left less than one second of video for its `-ss 1` pixel
check; six-second rerun passed. Do not misreport that initial test as passing.

Decoded 200 consecutive frames from both low-motion and moving Minecraft
samples: 200 distinct hashes each. Inspected a decoded moving-game image:
correct colors/orientation, game HUD visible (433 FPS in that sampled image).
This is pixel/uniqueness evidence, not proof of consistently smooth playback.

Historical temporary artifacts, **missing on September 20**:

- `/tmp/luma-decoupled-test.ovXZBg/`: `minecraft-new.mp4` (staging-only),
  `collector.mp4` (low motion), `final-moving.mp4` (final heavy test), associated
  logs/GPU/focus CSVs. `movement.mp4` was a background-throttled invalid comparison.
- `/tmp/luma-receiver-diagnostic.nWf5ng/`: temporary instrumented receiver/source.
- `/tmp/luma-wave-profile.V07R3I/`: pre-fix focused profile.
- `/tmp/luma-shared-vram.2qWN2I/`: final EGL regression artifacts.

### Next-session workflow

1. Read current source and `native/game-capture-gl/README.md`; preserve the
   extensively dirty worktree, including unrelated compositor/SCTK changes.
   Do not reset or overwrite it. No agents were delegated in the last session.
2. Re-discover running Minecraft PID/window and active receiver path; historical
   PID 70054/window 25 are stale identifiers, not safe action targets. Two JVMs
   existed earlier; never kill a second JVM merely to improve benchmark numbers.
3. If asked to investigate more, obtain a fresh focused moving-camera sample
   at unchanged settings with per-second PTS gaps, pre-encode drop counters,
   GPU/NVENC utilization, focus, and decoded uniqueness. Save evidence under a
   durable user-approved location if it needs to survive a reboot.
4. Distinguish source starvation, input-pool pressure, encoder saturation, and
   playback pacing. Do not call a build or average FPS a completed fix. The
   current heavy-motion ceiling remains unresolved; avoid promising 360 FPS
   or silently reducing resolution/FPS/quality to make a test pass.
5. The user expects implementation when asking "fix", but diagnosis requests
   alone do not authorize unrelated edits/settings changes. No desktop/game
   restart is currently required just to use these receiver changes.

---

## Historical September 17 handoff (superseded, not current instructions)

The following retains earlier rationale and Lunar-profile work. Its live
process/config status, architecture descriptions, test list, and "Next action"
are historical; do not automatically execute its restart/profile steps.

## Goal

Provide an OBS-style, high-FPS GPU capture path for Minecraft clients. The
target is real rendered frames at up to 480 FPS on NVIDIA, without compositor
frame copies or CPU pixel readback.

## Current conclusion

There are two distinct OpenGL capture modes. Do not conflate them.

| Mode | How the native hook enters the game | Status / boundary |
| --- | --- | --- |
| Launch-time injection | `LD_PRELOAD` loads `libluma-game-capture-gl.so` before the launched renderer starts. | Preferred production path. The hook runs at the actual GL present call after normal process startup. |
| Runtime attach | `luma-game-inject` ptrace-attaches to an existing process, forces remote `dlopen`, then asks the injected library to install late hooks. | Controlled fixtures pass, but it is an experimental live-process path. It can be denied by ptrace policy and is inherently riskier for a running HotSpot/LWJGL process. |

The native `.so` is not the problem by itself. A worker thread can be created
inside an already-running process after a successful `dlopen`. The unsafe part
is the bootstrap: an external injector must make an arbitrary live target
thread execute `dlopen`, and late setup modifies already-active GLX/EGL/LWJGL
dispatch state. A new worker cannot read Minecraft's final framebuffer on its
own because the GL context belongs to the render thread. The hook must copy the
final image from the present/render thread; a worker handles non-GL transport
and muxing work.

Do not weaken Yama, bypass anti-cheat, or replace the native injector with GDB.
GDB uses the same ptrace/live-thread mechanism.

## Launch-time Lunar support

`GameCaptureProfile.target_process_name` was added for launchers that fork the
actual renderer instead of `exec`ing it. With `target_process_name = "java"`:

1. Luma launches Lunar with the hook in `LD_PRELOAD`.
2. Lunar's Java child inherits the library.
3. The hook checks `/proc/self/comm` and remains inert in every non-`java`
   launcher helper.
4. The muxer accepts only a same-user peer named `java` that descends from the
   launched root PID.

This avoids runtime ptrace entirely while retaining a strict private capture
socket/token boundary.

The intended profile is already documented, but remains commented in
`config/default.toml` because the compositor that was live during this session
predated the new `target_process_name` field. Adding an active profile caused a
safe config-reload parse error, which was immediately reversed. The live config
is valid again.

After restarting into the freshly built compositor, uncomment this profile:

```toml
[[recorder.game_profiles]]
name = "lunar-opengl"
api = "opengl"
command = ["/usr/bin/env", "DESKTOPINTEGRATION=false", "/usr/bin/lunarclient", "--no-sandbox"]
fps = 480
target_process_name = "java"
```

Then launch a fresh Lunar session through:

```sh
wmctl recorder game-start lunar-opengl
```

Do not attach this profile to an already-running Lunar JVM; launch-time
inheritance is the point of the profile.

## Implementation map

- `native/game-capture-gl/luma-game-capture-gl`
  - exports stream configuration and either an exact target PID or an exact
    target process name before `exec`.
- `native/game-capture-gl/luma-game-record-gl`
  - starts the private direct-video muxer and supports
    `--target-process-name`.
- `native/game-capture-gl/src/luma_game_capture_gl.c`
  - interposes `glXSwapBuffers`/`eglSwapBuffers`; target selection supports
    `LUMA_GAME_CAPTURE_TARGET_COMM`.
  - also interposes GLX/EGL proc-address lookup, covering modern GLFW/LWJGL3
    clients (including Sodium-based Minecraft) without a game-specific path.
  - late attach remains separately implemented here. It loads config and
    patches resolved GLX/EGL relocations or LWJGL2's writable swap dispatch
    slot on a worker created after remote loading.
- `native/game-capture-inject/luma_game_inject.c`
  - native x86-64 ptrace/remote-`dlopen` helper used only by
    `luma-game-attach`.
- `crates/game-capture-muxer/src/main.rs`
  - verifies kernel `SO_PEERCRED`, target ancestry, and optional exact
    process name before accepting the token handshake.
- `crates/core/src/lib.rs`
  - defines and validates `GameCaptureProfile.target_process_name`.
- `crates/compositor/src/recorder.rs`
  - passes the child process name to OpenGL launch profiles.

## Codec and audio behavior

Direct graphics-API capture always writes SDR H.264. It is now independent of
the Screen/Xwayland recorder's selected codec, so HDR/HEVC screen settings do
not block a direct Lunar recording. Direct capture is still video-only;
desktop and microphone tracks remain on the Screen/Xwayland recorder path.

## Verification completed

Passed in this checkout:

```sh
cargo test -p wm-core --locked --offline
cargo test -p wm-shell-sctk --locked --offline
cargo test -p luma-game-capture-muxer --locked --offline
cargo build --release -p wm-compositor --locked --offline
cargo build --release -p luma-game-capture-muxer --locked --offline
make -C native/game-capture-gl test
LUMA_GAME_CAPTURE_RUNTIME_SECONDS=3 LUMA_GAME_CAPTURE_TEST_FPS=480 \
  make -C native/game-capture-gl launcher-child-integration-glx
git diff --check
```

The 480-FPS child-launch fixture used the NVIDIA RTX 3090 Ti, verified decoded
pattern orientation/non-black pixels, and exercised preload inheritance,
named-child selection, peer ancestry authentication, direct NVENC, and MP4
finalization. This is not proof of an actual Lunar Minecraft recording.

The generic runtime-injection and LWJGL2 fixtures also pass only in controlled
processes that explicitly permit ptrace. Treat that as mechanism coverage, not
as a guarantee for a production game JVM.

## Next action

1. Restart into `target/release/wm` when it is safe to interrupt the desktop.
2. Uncomment the Lunar profile above and reload config.
3. Start Lunar with `wmctl recorder game-start lunar-opengl`, enter a world,
   record at least ten seconds, and inspect the actual MP4 plus
   `$XDG_RUNTIME_DIR/luma-game-capture.log`.
4. Only after that live test, decide whether the runtime `game-attach` UI path
   should remain an explicit experimental option or be hidden in favor of the
   launch-time path.
