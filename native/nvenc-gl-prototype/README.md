# NVENC OpenGL texture prototype

This is an isolated API probe for Luma's future **opt-in** OpenGL game-capture
launcher. It intentionally contains no injection, process-launch, anti-cheat,
or game-memory code.

It opens an NVENC session with `NV_ENC_DEVICE_TYPE_OPENGL`, registers persistent
`GL_TEXTURE_2D` `GL_RGBA8` capture textures using
`NV_ENC_INPUT_RESOURCE_TYPE_OPENGL_TEX`, and emits H.264 elementary-stream
packets. Pixels never cross the CPU: the producer should GPU-copy the final
game framebuffer into a free ring texture, then call `submit` for that texture.

## Build

```sh
cmake -S native/nvenc-gl-prototype -B /tmp/luma-nvenc-gl-build
cmake --build /tmp/luma-nvenc-gl-build
/tmp/luma-nvenc-gl-build/nvenc_gl_api_smoke
```

`libnvidia-encode.so.1` is loaded dynamically at runtime, so this target does
not link an NVIDIA driver into Luma itself.

## Present-thread contract

* A caller owns `texture_count` preallocated RGBA8 textures and chooses a free
  slot using a modulo counter.
* `submit()` calls `poll()` with `doNotWait=1`; a busy output returns quickly.
  If the next slot remains busy, it drops the capture. It never blocks the game
  present thread waiting for NVENC.
* The packet callback must immediately hand bytes to a non-GL muxing/writer
  queue. It must not block or retain the supplied pointer.
* `shutdown()` is the only blocking cleanup path. Production code must drain
  any queued packets while the original GL context is still current.

## Required integration work

The real hook should copy the final framebuffer to a ring texture with GPU-only
GL operations (FBO blit/copy), invoke this encoder in the presenting GL context,
and send packet copies over a local Unix socket to Luma's muxer. It must be
launched only through a user-selected capture profile, and must not be used to
modify game state or evade anti-cheat rules.

Vulkan cannot reuse this API directly: it needs a Vulkan-layer implementation
around `vkQueuePresentKHR` with swapchain-image synchronization and an
NVENC-compatible export/copy path.
