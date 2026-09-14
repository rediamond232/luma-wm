# Luma Vulkan game-capture layer (scaffold)

This is an explicit, opt-in Vulkan layer for games launched through
`run-vulkan-game-capture.sh`. It observes `vkQueuePresentKHR` and sends a
fixed-size, non-blocking local Unix datagram after each present. The event has
the `LGC1` magic number, version, monotonic timestamp, queue handle,
swapchain count, and Vulkan result.

It deliberately does **not** export image pixels yet. It therefore is not a
working video capture path and cannot improve recording performance by itself.
Correct zero-copy image export needs a capture allocation, queue ownership and
layout transitions, synchronization with the application's semaphores, and an
NVENC-compatible Vulkan interop path. Those are intentionally deferred rather
than risking a layer that stalls or corrupts a game.

## Build

```sh
make -C native/game-capture-vulkan
```

## Run (development)

Start a local Unix datagram listener at the path supplied to `--socket`, then:

```sh
native/game-capture-vulkan/run-vulkan-game-capture.sh \
  --socket "$XDG_RUNTIME_DIR/luma-game-events.sock" -- your-vulkan-game
```

The layer is not globally installed or enabled. It uses `VK_LAYER_PATH` and
`VK_INSTANCE_LAYERS` only for the process launched by the helper. Do not use it
in anti-cheat protected sessions unless the game and anti-cheat provider
explicitly permit the integration.
