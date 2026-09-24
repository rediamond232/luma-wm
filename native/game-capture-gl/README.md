# OpenGL shared-VRAM capture

Both launch-time preload and late attach now run the same transport:

`game framebuffer -> shared VRAM pool -> recorder staging pool -> NVENC -> muxer`

The hook does not link the NVENC implementation. It copies on the owning GL
thread into four persistent external-memory textures. Vulkan is used only to
allocate exportable device-local memory and semaphores on the matching GPU
UUID; the application remains OpenGL. This does not depend on Java or LWJGL.
The receiver owns NVENC and its private encoder textures. No raw pixels are
read back to system memory.

Every handoff uses an external GPU semaphore and a zero-timeout fence probe.
The receiver acknowledges a slot only after its GPU read is complete. A full
pool drops capture opportunities; it never waits for the encoder from the game.
Admission advances a persistent capture clock, so non-integer ratios between
game FPS and capture FPS do not accumulate timing drift. The receiver does not
pace those frames again. The import thread copies into a bounded,
resolution-aware reservoir of private staging textures and acknowledges the
game after that copy, independently of encoding.
A submission worker and output collector each own a shared EGL context. The
collector keeps a bounded, measured submission lead before blocking for
bitstreams and unmaps completed encoder inputs itself; resource recycling never
runs on the import or submission thread. GPU fences protect staging reuse across
contexts.
The preallocated FIFO absorbs short encoder overloads and submits retained
frames later with their original timestamps. Its default budget is 250 ms and
1024 MiB (whichever is smaller, 4..256 frames); `LUMA_GAME_CAPTURE_BUFFER_MS`
and `LUMA_GAME_CAPTURE_BUFFER_MIB` override those bounded limits. All queues
remain bounded, prolonged overload drops before encoding, and stop drains queued
frames before GPU teardown.
The receiver is authenticated by process credentials; the muxer accepts only
the receiver PID. Named launcher descendants remain supported.

Requirements: desktop OpenGL with `GL_EXT_memory_object_fd` and
`GL_EXT_semaphore_fd`, a matching Vulkan device supporting opaque FD exports,
and NVENC. Unsupported devices fail closed, without reverting to in-game
encoding or CPU readback. This is a generic GLX/EGL implementation, not a claim
that every OpenGL driver, anti-cheat, or injection target is supported. Resizing
the capture drawable ends its current export session; restart recording at the
new size. An already-loaded older hook requires a game restart after rebuilding.

Build: `make -C native/game-capture-gl all`. Distribution must include both
`libluma-game-capture-gl.so` and `luma-opengl-capture-receiver` beside the wrappers.
The Vulkan export protocol also changed; rebuild its layer and receiver together.

Validation targets:

- `test`: fractional-rate pacing, launcher and stop ownership tests.
- `visual-integration-glx`: decoded color/orientation check.
- `generic-inject-integration-glx`: non-LWJGL late attach, game survives stop.
- `rerecord-integration-glx`: two sessions without reinjection.
- `launcher-child-integration-glx`: named-descendant authentication.
- `shared-vram-integration-egl`: 2560x1440 desktop EGL pixels, distinct decoded
  frames, NVENC absent from game mappings, and a paused receiver/full-pool test.

These fixtures prove transport and lifecycle behavior, not Minecraft FPS parity
with Windows OBS. Measure a matched in-game baseline and recording after restart.
