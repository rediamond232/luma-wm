# Luma OpenGL game-capture protocols

`libluma-game-capture-gl.so` is an explicit graphics-API hook loadable through
`LD_PRELOAD`, generic same-user process injection, or a HotSpot compatibility
loader. It intercepts `glXSwapBuffers` and `eglSwapBuffers`,
passes the call through unchanged, and can send one best-effort Unix datagram
to the path in `LUMA_GAME_CAPTURE_SOCKET`.

The metadata-only mode does **not** capture pixels. The direct mode below uses
a GPU framebuffer copy and never calls `glReadPixels`. Neither mode hides the
library, modifies unrelated application state, or attempts an anti-cheat
bypass. A missing or full socket drops work instead of delaying the game.

The native-endian v1 datagram is exactly 64 bytes:

| Offset | Type | Field |
| --- | --- | --- |
| 0 | `u64` | magic, `0x0031504c414d554c` (`LUMALP1`) |
| 8 | `u16` | protocol version (`1`) |
| 10 | `u16` | message length (`64`) |
| 12 | `u32` | API: `1` GLX, `2` EGL |
| 16 | `u64` | monotonically increasing present sequence |
| 24 | `u64` | `CLOCK_MONOTONIC` timestamp in ns |
| 32 | `u64` | opaque native display handle |
| 40 | `u64` | opaque drawable/surface handle |
| 48 | `u32` | surface width, or zero if unavailable |
| 52 | `u32` | surface height, or zero if unavailable |
| 56 | `u64` | reserved; currently zero |

The final eight bytes are padding in the v1 C ABI and receivers must ignore
them. This protocol is local-only: handles are process-local identifiers, not
portable graphics resources.

## Experimental NVIDIA direct stream

When all three settings below are supplied, the hook attempts an OpenGL/NVENC
direct path. It can be enabled for a newly launched process or explicitly
injected into a same-user GLX/EGL process. Neither establishes compatibility
with any game or anti-cheat. The configured launch command must `exec` the actual GL
renderer/game process. A launcher that forks a renderer or game child is
deliberately rejected by the PID-ownership guard; use the final renderer/game
executable rather than a forking launcher.

```bash
native/game-capture-gl/luma-game-capture-gl \
  --stream-socket /run/user/$UID/luma-game-capture.sock \
  --token 0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef \
  --fps 480 -- game-command
```

Immediately before a GLX/EGL present, it copies the current default framebuffer
with a GPU-only `glBlitFramebuffer` into a four-texture `GL_RGBA8` ring. The
reversed destination rectangle accounts for OpenGL's bottom-left origin so the
recording has the normal top-left video orientation. Those textures are
registered once as `NV_ENC_INPUT_RESOURCE_TYPE_OPENGL_TEX` and encoded by NVENC
with `NV_ENC_DEVICE_TYPE_OPENGL` and a **NULL device**, while the game context
remains current. No CPU pixel readback occurs. A busy NVENC slot, full bounded
output queue, or unavailable stream drops the capture frame instead of waiting
in the presenting game thread.

Encoded H.264 Annex-B access units use the local stream protocol in
`crates/game-capture-protocol/src/lib.rs`: 16-byte `LGCP` framed messages,
big-endian fields, client hello (the 32-byte supplied token), server hello,
start, then complete access units. A dedicated non-game thread owns blocking
socket connection and writes. The game thread only does a try-lock queue copy;
when contended or full, it drops output.

`--fps` is a hard ceiling, not a synthetic frame rate: the hook admits only a
real game present whose monotonic timestamp reaches the next `1/fps` slot.
Early presents are skipped and late intervals advance to the next future slot;
it never duplicates a frame to meet the requested rate. A busy encoder slot,
queue contention, or stream failure is also a dropped capture frame, not a
reason to block or invent a frame. Consequently the resulting source rate can
be below the configured ceiling and must be measured from the recording.

The direct stream carries H.264 video only. It does not create desktop-audio or
microphone tracks. `LUMA_GAME_CAPTURE_QUALITY` is an H.264 constant-QP value:
1 through 51 are accepted, lower values mean higher quality, and the default is
QP 20. At 2560x1440 and 480 FPS, QP 20 can require very fast sustained local
storage and produce very large files; it is not a bounded-bitrate setting.

The launched target is placed in its own session. Stopping the recorder ends
the recorder and muxer, but deliberately leaves that game process running.

## Runtime injection

`luma-game-attach --pid PID --output /absolute/file.mp4 --fps N` loads the
same hook with the x86-64 `luma-game-inject` helper and patches already-resolved
GLX/EGL ELF relocations. It is process-generic: selection depends on mapped
graphics libraries, not a game name. It does not patch arbitrary instructions.
Linux ptrace policy or an anti-cheat may
reject the load, and Luma does not weaken or bypass those policies.

LWJGL2 is supported by the same native `.so` injection route. After remote
`dlopen`, the hook locates the exported LWJGL2 GLX JNI bridge and atomically
replaces its writable resolved-swap slot. It does not use `jcmd`, JVM Attach,
scan arbitrary instructions, or depend on a JDK. Stop requests are observed on
the next present so GL textures and NVENC resources are destroyed on the owning
GL thread; the JVM itself is not signalled. Re-injection is currently
unsupported until that process is restarted.

After each accepted GPU copy it calls `glFlush`, which submits GL work to the
shared GPU context but does not wait for completion on the CPU. NVENC's OpenGL
resource mapping then orders the encoder behind that submitted copy. There is
no portable NVENC field for a GLsync fence; inserting `glFinish` or a CPU fence
wait would defeat the present-path guarantee and is deliberately not done.

## Metadata build and launch

```bash
make -C native/game-capture-gl
native/game-capture-gl/luma-game-capture-gl \
  --socket /tmp/luma-game-events.sock -- your-game-command
```

The receiver must bind an `AF_UNIX`, `SOCK_DGRAM` socket before starting the
game. The launcher refuses relative socket paths and is visibly opt-in.

## Current boundary

Metadata mode is runtime-tested by the local fake-GLX integration test. The
direct video path also has generic-injection and opt-in NVIDIA/GLX fixtures,
protocol, and muxer coverage. That demonstrates only controlled GLX-to-MP4 paths; it
does not establish compatibility with individual games or anti-cheat. Resize
handling stops the direct capture when the captured surface dimensions change;
it does not continue encoding at the old dimensions. Clean-stop recovery across
arbitrary games, EGL validation, and game compatibility remain pending.

This stream protocol is also used by the external Vulkan DMA-BUF receiver after
it imports and GPU-copies a presented image. Vulkan capture is launch-time only
because a loader layer must participate before instance/device dispatch chains
are created.

The authenticated stream runs on a dedicated writer thread. If connect, authentication,
or write fails, the direct path atomically disables itself and all later
submits become no-ops; metadata events continue normally. Runtime-attach stop
joins the writer and destroys GL/NVENC state from the next present call while
the original context is current.

## Live GLX integration fixture

On a logged-in NVIDIA/X11 session, after building the release muxer, run:

```bash
cargo build --release -p luma-game-capture-muxer
make -C native/game-capture-gl integration-glx
make -C native/game-capture-gl runtime-inject-lwjgl2-glx
# Optional pixel/orientation check at the requested ceiling:
LUMA_GAME_CAPTURE_RUNTIME_SECONDS=2 LUMA_GAME_CAPTURE_TEST_FPS=480 \
  make -C native/game-capture-gl visual-integration-glx
```

The fixture launches only `glxgears` through the explicit launcher for four
seconds, then directly stops that target (it does not wrap it in `timeout`). It
checks the finalized file with `ffprobe` for an H.264 MP4 video stream and a
duration of at least half the controlled runtime. It exits `77` (skip) rather than
claiming success when the active session has no NVIDIA GLX renderer, NVENC
toolchain, X11 access, or required command. Set
`LUMA_GAME_CAPTURE_RUNTIME_SECONDS`, `LUMA_GAME_CAPTURE_TEST_FPS`, or
`LUMA_GAME_CAPTURE_MUXER` to override its controlled test inputs;
`LUMA_GAME_CAPTURE_MIN_DURATION_SECONDS` controls the duration assertion. On a real
failure it retains the temporary directory and prints its path plus
`launcher.log` for diagnosis.
The visual fixture uses a deterministic four-quadrant GLX surface and decodes a
frame to verify that the GPU copy is upright and non-black; it uses the same
launcher, PID guard, NVENC path, and muxer as a real profile.
