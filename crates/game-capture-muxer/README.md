# Luma direct-game capture muxer

`luma-game-capture-muxer` is the isolated receiver for Luma's OpenGL launch,
generic runtime-injection, and Vulkan layer paths. The OpenGL hook or external
Vulkan GPU receiver supplies H.264 Annex-B access units and authenticates to
this Unix socket with a per-session token. This process preserves the producer's timestamps in a fragmented MP4
transport before FFmpeg writes a recoverable Hybrid MP4. The result is true
VFR: a game that presents below the requested maximum is not duplicated or
sped up.

The compositor and recorder UI supervise it through an OpenGL/Vulkan launch
profile or a same-user GLX/EGL process. Generic injection loads the hook with a
matching-architecture helper, patches resolved graphics-present relocations,
and recognizes LWJGL2's separately cached GLX dispatch slot. It always loads
the native `.so`; there is no Java Attach compatibility route. Neither route provides an anti-cheat bypass;
use injection only where the application and any anti-cheat service permit it.

## Audio

This first direct-game path is **video only**. It creates no silent or fake
audio track. Desktop/microphone capture needs an explicitly synchronized audio
producer before it can be added.

## Run

```sh
cargo run --release -- \
  --socket "$XDG_RUNTIME_DIR/luma-game-capture.sock" \
  --token "$(openssl rand -hex 32)" \
  --output "$HOME/Videos/Luma/game.mp4" \
  --fps 480 \
  --expected-pid-file "$XDG_RUNTIME_DIR/luma-game-capture.expected-pid"
```

The socket path must not already exist. It is created mode `0600`, accepts one
authenticated peer from the same UID, and is removed when the muxer exits. Its
launcher writes the exact PID of the explicitly launched, execing game to
`--expected-pid-file`; the muxer verifies that PID through kernel-provided
`SO_PEERCRED` before it accepts the protocol token. This prevents an unrelated
child helper that inherited the environment token from claiming the session.
The PID file must be inside a private runtime directory controlled by the
launcher. `--fps` is an upper-bound sanity check, not a CFR target. The H.264
`Start` message must declare the same maximum rate, while every access unit
supplies its real PTS/DTS/duration. The muxer rejects missing durations and
backward PTS rather than inventing timing.

The receiving-to-FFmpeg queue holds at most four access units. When FFmpeg or
storage cannot keep up, the queue stops draining the Unix stream, applying
socket backpressure. A hook must use non-blocking sends and drop its own frame
rather than ever stall a game's present thread.

## Stop semantics

Only an explicit protocol `Stop`, or clean EOF after a valid `Start`, asks the
writer to emit EOS and finalize the MP4 index. Authentication, protocol, and
hook errors abort the pipeline and remove any incomplete output rather than
leaving a partial file that appears to be a completed recording.
