# Luma Vulkan DMA-BUF game capture

This directory contains Luma's explicit Vulkan loader layer and its external
GPU receiver. It is graphics-API capture: the layer is selected for a launch
profile and intercepts Vulkan presentation regardless of the application name.
It does not inspect application memory or bypass an anti-cheat.

## Pipeline

At `vkQueuePresentKHR`, the layer:

1. waits on those original semaphores;
2. transitions the presented image `PRESENT_SRC_KHR -> TRANSFER_SRC_OPTIMAL`;
3. copies it to a ring of exportable `TRANSFER_DST` images;
4. restores the presented image to `PRESENT_SRC_KHR`;
5. signals a replacement semaphore which the real present consumes; and
6. exports a DRM-modifier DMA-BUF plus `SYNC_FD` fence to the recorder socket.

`luma-vulkan-capture-receiver` verifies the renderer's Unix credentials,
imports the DMA-BUF and fence into a matching EGL device, GPU-copies it into a
bounded NVENC ring, and returns a generation-specific ACK only when reuse is
safe. Only compressed H.264 access units cross to the Hybrid MP4 muxer. Frame
pixels never pass through CPU memory. A full ring drops capture work rather
than waiting on the game's queue.

The layer augments ordinary device and swapchain creation with supported
external-memory and transfer-source capabilities. Unsupported formats or
drivers leave presentation working and produce an explicit capture failure.
Runtime injection after device creation is intentionally unsupported because
the Vulkan loader and application have already cached their dispatch chains.

## Build and run

```sh
make -C native/vulkan-external-capture-prototype
native/vulkan-external-capture-prototype/luma-game-record-vulkan \
  --output /absolute/output.mp4 --fps 240 --quality 20 -- your-vulkan-game
```

The launch wrapper creates private sockets, gates the renderer until the
authenticated receiver and muxer are listening, generates a per-session layer
manifest, and preserves the renderer when recording is stopped externally.

The current implementation is H.264/NVENC-only and video-only. It is validated
on an NVIDIA RTX 3090 Ti with driver 610.57.04 using `vkcube`; AMD and Intel
interop still require live validation even when their advertised extensions
allow the same path.

The live integration check uses a 480 FPS ceiling, stops only the recorder,
verifies that the renderer remains alive, requires a clean full FFmpeg software
decode, and rejects duplicate frames in its decoded sample:

```sh
make -C native/vulkan-external-capture-prototype integration-vkcube
```
