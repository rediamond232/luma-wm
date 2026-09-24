//! Authenticated, bounded timestamped H.264 to Hybrid MP4 muxer.
//!
//! This is deliberately a separate process: the injected present hook must
//! never wait for disk I/O or FFmpeg. Backpressure is bounded here and travels
//! through the Unix stream; a hook is responsible for using non-blocking sends
//! and recording its own dropped-send statistic.

use std::{
    fs, io,
    os::unix::{
        fs::PermissionsExt,
        io::AsRawFd,
        net::{UnixListener, UnixStream},
    },
    path::PathBuf,
    process::{Child, ChildStdin, Command, Stdio},
    sync::mpsc::{self, Receiver},
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use gstreamer as gst;
use gstreamer::prelude::*;
use luma_game_capture_protocol::{
    AccessUnit, Channel, FrameStats, Message, ProtocolError, SessionToken, TOKEN_LEN, VideoConfig,
};

const MAX_QUEUED_ACCESS_UNITS: usize = 4;
const ERROR_PROTOCOL: u32 = 1;
const ERROR_CONFIGURATION: u32 = 2;

#[derive(Debug)]
struct Options {
    socket: PathBuf,
    token: SessionToken,
    output: PathBuf,
    fps: u32,
    /// The launcher writes the PID of the process it explicitly launched
    /// before its hook can complete the hello exchange.  Keeping this in a
    /// private runtime directory avoids placing a process identifier in the
    /// environment inherited by arbitrary helper processes.
    expected_pid_file: PathBuf,
    /// Optional exact Linux task name for a game spawned by a launcher. The
    /// peer must still descend from the PID in `expected_pid_file`.
    expected_process_name: Option<String>,
}

impl Options {
    fn parse_from(args: impl IntoIterator<Item = String>) -> Result<Self, String> {
        let mut args = args.into_iter();
        let mut socket = None;
        let mut token = None;
        let mut output = None;
        let mut fps = None;
        let mut expected_pid_file = None;
        let mut expected_process_name = None;
        while let Some(flag) = args.next() {
            let value = args
                .next()
                .ok_or_else(|| format!("missing value for {flag}"))?;
            match flag.as_str() {
                "--socket" => socket = Some(value.into()),
                "--token" => token = Some(parse_token(&value)?),
                "--output" => output = Some(value.into()),
                "--fps" => fps = Some(value.parse().map_err(|_| "invalid --fps")?),
                "--expected-pid-file" => expected_pid_file = Some(value.into()),
                "--expected-process-name" => expected_process_name = Some(value),
                _ => return Err(format!("unknown option {flag}")),
            }
        }
        let options = Self {
            socket: socket.ok_or("--socket is required")?,
            token: token.ok_or("--token is required")?,
            output: output.ok_or("--output is required")?,
            fps: fps.ok_or("--fps is required")?,
            expected_pid_file: expected_pid_file.ok_or("--expected-pid-file is required")?,
            expected_process_name,
        };
        if options.socket.as_os_str().is_empty()
            || options.output.as_os_str().is_empty()
            || options.expected_pid_file.as_os_str().is_empty()
            || !(1..=480).contains(&options.fps)
        {
            return Err("--socket, --output, and an fps from 1 through 480 are required".into());
        }
        if let Some(name) = &options.expected_process_name
            && (name.is_empty()
                || name.len() > 15
                || !name
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.')))
        {
            return Err("--expected-process-name must be 1 through 15 letters, digits, _, -, or .".into());
        }
        Ok(options)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct PeerCredentials {
    pid: u32,
    uid: u32,
}

/// `SO_PEERCRED` is supplied by the kernel for a connected Linux Unix socket;
/// unlike a protocol PID field it cannot be forged by the connecting process.
fn peer_credentials(stream: &UnixStream) -> Result<PeerCredentials, String> {
    let mut credentials = libc::ucred {
        pid: 0,
        uid: 0,
        gid: 0,
    };
    let mut length = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    // SAFETY: credentials points to initialized writable storage and length
    // accurately describes it. The stream remains open for this call.
    let result = unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            (&mut credentials as *mut libc::ucred).cast(),
            &mut length,
        )
    };
    if result != 0 {
        return Err(format!(
            "failed reading game-capture peer credentials: {}",
            io::Error::last_os_error()
        ));
    }
    if length != std::mem::size_of::<libc::ucred>() as libc::socklen_t || credentials.pid <= 0 {
        return Err("kernel returned incomplete game-capture peer credentials".into());
    }
    Ok(PeerCredentials {
        pid: credentials.pid as u32,
        uid: credentials.uid,
    })
}

fn expected_pid(path: &PathBuf) -> Result<u32, String> {
    // The launcher writes this immediately after spawning its execing game
    // wrapper. The short wait closes the tiny race where a very fast GL game
    // connects before the shell has written the file; it runs on the hook's
    // transport worker, never a present thread.
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        match fs::read_to_string(path) {
            Ok(value) => {
                let value = value.trim();
                let pid = value
                    .parse::<u32>()
                    .map_err(|_| format!("invalid expected game PID in {}", path.display()))?;
                if pid == 0 {
                    return Err(format!("invalid expected game PID in {}", path.display()));
                }
                return Ok(pid);
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound && Instant::now() < deadline => {
                thread::sleep(Duration::from_millis(5));
            }
            Err(error) => {
                return Err(format!(
                    "failed reading expected game PID {}: {error}",
                    path.display()
                ));
            }
        }
    }
}

fn process_name(pid: u32) -> Result<String, String> {
    fs::read_to_string(format!("/proc/{pid}/comm"))
        .map(|name| name.trim_end().to_owned())
        .map_err(|error| format!("could not read game-capture peer process name: {error}"))
}

fn descends_from(mut pid: u32, ancestor: u32) -> bool {
    for _ in 0..128 {
        if pid == ancestor {
            return true;
        }
        let Ok(stat) = fs::read_to_string(format!("/proc/{pid}/stat")) else {
            return false;
        };
        let Some((_, fields)) = stat.rsplit_once(')') else {
            return false;
        };
        let Some(parent) = fields.split_whitespace().nth(1).and_then(|value| value.parse().ok()) else {
            return false;
        };
        if parent == 0 || parent == pid {
            return false;
        }
        pid = parent;
    }
    false
}

fn parse_token(hex: &str) -> Result<SessionToken, String> {
    if hex.len() != TOKEN_LEN * 2 {
        return Err("--token must be exactly 64 hexadecimal characters".into());
    }
    let mut token = [0; TOKEN_LEN];
    for (index, byte) in token.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&hex[index * 2..index * 2 + 2], 16)
            .map_err(|_| "--token must contain only hexadecimal characters")?;
    }
    Ok(token)
}

struct SocketGuard(PathBuf);
impl Drop for SocketGuard {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

fn bind_private_socket(path: &PathBuf) -> Result<(UnixListener, SocketGuard), String> {
    if path.exists() {
        return Err(format!(
            "refusing to replace existing game-capture socket {}",
            path.display()
        ));
    }
    let listener = UnixListener::bind(path).map_err(|error| {
        format!(
            "failed to bind game-capture socket {}: {error}",
            path.display()
        )
    })?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600)).map_err(|error| {
        format!(
            "failed to restrict game-capture socket {}: {error}",
            path.display()
        )
    })?;
    Ok((listener, SocketGuard(path.clone())))
}

#[derive(Clone, Copy, Debug, Default)]
struct MuxerStats {
    received_access_units: u64,
    written_access_units: u64,
    received_bytes: u64,
    first_pts_ns: Option<u64>,
    last_pts_ns: Option<u64>,
    latest_hook_stats: FrameStats,
}

impl MuxerStats {
    fn note_access_unit(&mut self, au: &AccessUnit) -> Result<(), &'static str> {
        if let Some(last) = self.last_pts_ns
            && au.pts_ns < last
        {
            return Err("access-unit PTS moved backwards");
        }
        if au.duration_ns == 0 {
            return Err("access-unit duration is required for true VFR output");
        }
        self.first_pts_ns.get_or_insert(au.pts_ns);
        self.last_pts_ns = Some(au.pts_ns);
        self.received_access_units += 1;
        self.received_bytes += au.data.len() as u64;
        Ok(())
    }

    fn timeline_span_ns(&self) -> u64 {
        self.last_pts_ns
            .zip(self.first_pts_ns)
            .map_or(0, |(last, first)| last.saturating_sub(first))
    }
}

fn validate_start(config: VideoConfig, requested_fps: u32) -> Result<(), String> {
    if config.width == 0
        || config.height == 0
        || config.width > 16_384
        || config.height > 16_384
        || !config.width.is_multiple_of(2)
        || !config.height.is_multiple_of(2)
    {
        return Err("H.264 dimensions must be nonzero, even, and no larger than 16384".into());
    }
    if config.fps_den != 1 || config.fps_num != requested_fps {
        return Err(format!(
            "hook declared {}/{} fps but muxer was launched for {requested_fps} fps",
            config.fps_num, config.fps_den
        ));
    }
    Ok(())
}

fn spawn_ffmpeg(output: &PathBuf) -> Result<(Child, ChildStdin), String> {
    let mut child = Command::new("ffmpeg")
        .args([
            "-nostdin",
            "-hide_banner",
            "-loglevel",
            "warning",
            // Generous probe limits: the transport is paced by live presents,
            // so a slow-starting game must not make the demuxer decide on a
            // partial program map.
            "-probesize",
            "100M",
            "-analyzeduration",
            "100M",
            "-f",
            "mpegts",
            "-i",
            "pipe:0",
            "-map",
            "0:v:0",
            "-c:v",
            "copy",
            "-an",
            "-movflags",
            "+hybrid_fragmented",
            "-frag_duration",
            "1000000",
            "-flush_packets",
            "1",
            "-avoid_negative_ts",
            "make_zero",
            "-y",
            "-f",
            "mp4",
        ])
        .arg(output)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .spawn()
        .map_err(|error| format!("failed to start FFmpeg H.264 muxer: {error}"))?;
    let stdin = child.stdin.take().ok_or_else(|| {
        let _ = child.kill();
        let _ = child.wait();
        "FFmpeg H.264 input pipe is missing".to_string()
    })?;
    Ok((child, stdin))
}

enum WriterInput {
    AccessUnit(AccessUnit),
    /// Only a protocol Stop or a clean peer EOF may request MP4 finalization.
    Finish,
}

fn muxer_writer(
    receiver: Receiver<WriterInput>,
    stdin: ChildStdin,
    config: VideoConfig,
) -> Result<u64, String> {
    gst::init().map_err(|error| format!("failed to initialize GStreamer: {error}"))?;
    // The intermediate stream to FFmpeg is MPEG-TS, not fragmented MP4.
    // fMP4 over a live pipe leaves codec setup to probe timing (empty initial
    // moov, in-band SPS only), which starves ffmpeg's mov demuxer into an
    // uninitialized track and crashes its trailer. TS repeats PAT/PMT and
    // SPS/PPS in-band by design, so the demuxer always initializes no matter
    // when it starts reading. FFmpeg still finalizes the artifact as an
    // indexed hybrid MP4.
    let pipeline = gst::parse::launch(
        "appsrc name=video is-live=true format=time do-timestamp=false block=true max-buffers=4 \
         ! h264parse config-interval=-1 ! video/x-h264,stream-format=byte-stream,alignment=au \
         ! mpegtsmux name=mux alignment=7 \
         ! fdsink name=transport_sink sync=false",
    )
    .map_err(|error| format!("failed to construct timestamped H.264 muxer: {error}"))?
    .downcast::<gst::Pipeline>()
    .map_err(|_| "timestamped H.264 muxer is not a pipeline".to_string())?;
    let appsrc = pipeline
        .by_name("video")
        .ok_or("timestamped H.264 appsrc is missing")?;
    let transport_sink = pipeline
        .by_name("transport_sink")
        .ok_or("timestamped MP4 transport sink is missing")?;
    transport_sink.set_property("fd", std::os::fd::AsRawFd::as_raw_fd(&stdin));
    let caps = gst::Caps::builder("video/x-h264")
        .field("stream-format", "byte-stream")
        .field("alignment", "au")
        .field("width", config.width as i32)
        .field("height", config.height as i32)
        .field(
            "framerate",
            gst::Fraction::new(config.fps_num as i32, config.fps_den as i32),
        )
        .build();
    appsrc.set_property("caps", caps);
    pipeline
        .set_state(gst::State::Playing)
        .map_err(|error| format!("failed to start timestamped H.264 muxer: {error:?}"))?;

    let mut written = 0_u64;
    let mut first_pts_ns = None;
    let mut first_dts_ns = None;
    let mut finish_requested = false;
    while let Ok(input) = receiver.recv() {
        let au = match input {
            WriterInput::AccessUnit(au) => au,
            WriterInput::Finish => {
                finish_requested = true;
                break;
            }
        };
        let origin_pts_ns = *first_pts_ns.get_or_insert(au.pts_ns);
        let origin_dts_ns = *first_dts_ns.get_or_insert(au.dts_ns);
        let mut buffer = gst::Buffer::with_size(au.data.len())
            .map_err(|error| format!("failed to allocate H.264 GStreamer buffer: {error}"))?;
        {
            let writable = buffer
                .get_mut()
                .ok_or("new H.264 GStreamer buffer is not writable")?;
            writable
                .map_writable()
                .map_err(|error| format!("failed to map H.264 GStreamer buffer: {error}"))?
                .as_mut_slice()
                .copy_from_slice(&au.data);
            writable.set_pts(gst::ClockTime::from_nseconds(
                au.pts_ns.saturating_sub(origin_pts_ns),
            ));
            writable.set_dts(gst::ClockTime::from_nseconds(
                au.dts_ns.saturating_sub(origin_dts_ns),
            ));
            writable.set_duration(gst::ClockTime::from_nseconds(au.duration_ns));
        }
        let result = appsrc.emit_by_name::<gst::FlowReturn>("push-buffer", &[&buffer]);
        if result != gst::FlowReturn::Ok {
            let _ = pipeline.set_state(gst::State::Null);
            return Err(format!(
                "timestamped H.264 muxer rejected access unit: {result:?}"
            ));
        }
        written += 1;
    }
    if !finish_requested {
        let _ = pipeline.set_state(gst::State::Null);
        return Err("H.264 writer was aborted without protocol Stop or clean EOF".into());
    }
    let eos = appsrc.emit_by_name::<gst::FlowReturn>("end-of-stream", &[]);
    if eos != gst::FlowReturn::Ok {
        let _ = pipeline.set_state(gst::State::Null);
        return Err(format!(
            "timestamped H.264 muxer rejected end-of-stream: {eos:?}"
        ));
    }
    let bus = pipeline
        .bus()
        .ok_or("timestamped H.264 muxer bus is missing")?;
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut finalized = false;
    while Instant::now() < deadline {
        if let Some(message) = bus.timed_pop(gst::ClockTime::from_mseconds(100)) {
            use gst::MessageView;
            match message.view() {
                MessageView::Eos(..) => {
                    finalized = true;
                    break;
                }
                MessageView::Error(error) => {
                    let _ = pipeline.set_state(gst::State::Null);
                    return Err(format!("timestamped H.264 muxer failed: {}", error.error()));
                }
                _ => {}
            }
        }
    }
    pipeline
        .set_state(gst::State::Null)
        .map_err(|error| format!("failed to stop timestamped H.264 muxer: {error:?}"))?;
    if !finalized {
        return Err(
            "timestamped H.264 muxer timed out while finalizing the recording index".into(),
        );
    }
    drop(stdin);
    Ok(written)
}

fn send_error(channel: &mut Channel, code: u32, message: impl Into<String>) {
    let _ = channel.send(&Message::Error {
        code,
        message: message.into(),
    });
}

fn wait_cleanly(child: &mut Child) -> Result<(), String> {
    let status = child
        .wait()
        .map_err(|error| format!("failed waiting for FFmpeg: {error}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("FFmpeg exited with {status}"))
    }
}

fn accept_expected_peer(listener: &UnixListener, options: &Options) -> Result<UnixStream, String> {
    // SAFETY: `geteuid` has no preconditions and does not dereference memory.
    let muxer_uid = unsafe { libc::geteuid() };
    loop {
        let (stream, _) = listener
            .accept()
            .map_err(|error| format!("failed accepting game-capture hook: {error}"))?;
        let credentials = match peer_credentials(&stream) {
            Ok(credentials) => credentials,
            Err(error) => {
                eprintln!("warning: rejected game-capture peer: {error}");
                continue;
            }
        };
        if credentials.uid != muxer_uid {
            eprintln!(
                "warning: rejected game-capture peer from UID {} (muxer UID is {})",
                credentials.uid, muxer_uid
            );
            continue;
        }
        let expected = expected_pid(&options.expected_pid_file)?;
        if let Some(name) = &options.expected_process_name {
            if !descends_from(credentials.pid, expected) {
                eprintln!(
                    "warning: rejected game-capture peer PID {} (not descended from launcher PID {})",
                    credentials.pid, expected
                );
                continue;
            }
            if process_name(credentials.pid).as_deref() != Ok(name.as_str()) {
                eprintln!(
                    "warning: rejected game-capture peer PID {} (expected process name {})",
                    credentials.pid, name
                );
                continue;
            }
        } else if credentials.pid != expected {
            eprintln!(
                "warning: rejected game-capture peer PID {} (expected launcher game PID {})",
                credentials.pid, expected
            );
            continue;
        }
        return Ok(stream);
    }
}

fn run_session(stream: UnixStream, options: &Options) -> Result<MuxerStats, String> {
    let mut channel = Channel::new(stream);
    let session_id = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_nanos() as u64)
        ^ ((std::process::id() as u64) << 32);
    channel
        .server_handshake(&options.token, session_id)
        .map_err(|error| format!("hook authentication failed: {error}"))?;

    let config = match channel.recv() {
        Ok(Message::Start(config)) => config,
        Ok(other) => {
            send_error(
                &mut channel,
                ERROR_PROTOCOL,
                "expected Start after authenticated hello",
            );
            return Err(format!("expected Start after hello, received {other:?}"));
        }
        Err(error) => return Err(format!("failed receiving Start: {error}")),
    };
    if let Err(error) = validate_start(config, options.fps) {
        send_error(&mut channel, ERROR_CONFIGURATION, &error);
        return Err(error);
    }

    let (mut child, stdin) = spawn_ffmpeg(&options.output)?;
    let (sender, receiver) = mpsc::sync_channel::<WriterInput>(MAX_QUEUED_ACCESS_UNITS);
    let writer = thread::spawn(move || muxer_writer(receiver, stdin, config));
    let mut stats = MuxerStats::default();
    let mut outcome: Result<(), String> = Ok(());
    let mut graceful_end = false;

    loop {
        match channel.recv() {
            Ok(Message::AccessUnit(au)) => {
                if let Err(error) = stats.note_access_unit(&au) {
                    send_error(&mut channel, ERROR_PROTOCOL, error);
                    outcome = Err(error.into());
                    break;
                }
                // This is intentionally blocking only in the muxer process. Once the
                // fixed queue fills, socket receive stops and backpressure reaches a
                // correctly implemented non-blocking hook instead of its present thread.
                if let Err(error) = sender.send(WriterInput::AccessUnit(au)) {
                    outcome = Err(format!(
                        "FFmpeg writer stopped before receiving access unit: {error}"
                    ));
                    break;
                }
            }
            Ok(Message::Stats(hook_stats)) => stats.latest_hook_stats = hook_stats,
            Ok(Message::Stop) => {
                graceful_end = true;
                break;
            }
            Ok(Message::Error { code, message }) => {
                outcome = Err(format!("hook reported error {code}: {message}"));
                break;
            }
            Ok(other) => {
                send_error(
                    &mut channel,
                    ERROR_PROTOCOL,
                    "unexpected message after Start",
                );
                outcome = Err(format!("unexpected message after Start: {other:?}"));
                break;
            }
            Err(ProtocolError::Io(error)) if error.kind() == io::ErrorKind::UnexpectedEof => {
                // A hook process can exit before emitting Stop. EOF after a
                // valid Start is still a graceful producer end: all complete
                // stream packets have been read and may be indexed safely.
                graceful_end = true;
                break;
            }
            Err(error) => {
                send_error(&mut channel, ERROR_PROTOCOL, error.to_string());
                outcome = Err(format!("game-capture protocol error: {error}"));
                break;
            }
        }
    }
    if graceful_end && outcome.is_ok() {
        if let Err(error) = sender.send(WriterInput::Finish) {
            outcome = Err(format!(
                "FFmpeg writer stopped before finalization: {error}"
            ));
        }
    }
    drop(sender);
    match writer.join() {
        Ok(Ok(written)) => stats.written_access_units = written,
        Ok(Err(error)) => outcome = Err(error),
        Err(_) => outcome = Err("FFmpeg writer thread panicked".into()),
    }
    if outcome.is_ok() && graceful_end {
        if let Err(error) = wait_cleanly(&mut child) {
            outcome = Err(error);
        }
    } else {
        let _ = child.kill();
        let _ = child.wait();
        // A failed session must not masquerade as a finalized recording. The
        // requested output is owned by this muxer invocation (`ffmpeg -y`), so
        // remove any incomplete transport output it may have created.
        let _ = fs::remove_file(&options.output);
    }
    outcome?;
    Ok(stats)
}

fn main() {
    let options = match Options::parse_from(std::env::args().skip(1)) {
        Ok(options) => options,
        Err(error) => {
            eprintln!(
                "error: {error}\nusage: luma-game-capture-muxer --socket PATH --token HEX64 --output FILE.mp4 --fps FPS --expected-pid-file PATH"
            );
            std::process::exit(2);
        }
    };
    let (listener, _socket_guard) = match bind_private_socket(&options.socket) {
        Ok(listener) => listener,
        Err(error) => {
            eprintln!("error: {error}");
            std::process::exit(1);
        }
    };
    eprintln!(
        "luma game capture muxer waiting on {} (video only; no audio track)",
        options.socket.display()
    );
    let stream = match accept_expected_peer(&listener, &options) {
        Ok(stream) => stream,
        Err(error) => {
            eprintln!("error: {error}");
            std::process::exit(1);
        }
    };
    match run_session(stream, &options) {
        Ok(stats) => eprintln!(
            "direct game capture received {} H.264 access units ({} bytes), wrote {}, PTS span {} ms; hook submitted {}, encoded {}, pre-encode drops {}, send-backpressure drops {}, encode latency {} us",
            stats.received_access_units,
            stats.received_bytes,
            stats.written_access_units,
            stats.timeline_span_ns() / 1_000_000,
            stats.latest_hook_stats.submitted,
            stats.latest_hook_stats.encoded,
            stats.latest_hook_stats.dropped_before_encode,
            stats.latest_hook_stats.dropped_send_backpressure,
            stats.latest_hook_stats.encode_latency_us,
        ),
        Err(error) => {
            eprintln!("error: direct game capture failed: {error}");
            std::process::exit(1);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use luma_game_capture_protocol::{AccessUnitFlags, Codec};
    use std::{fs::File, io::Read};

    #[test]
    fn parses_exactly_one_256_bit_token() {
        let expected = [0xab; TOKEN_LEN];
        assert_eq!(parse_token(&"ab".repeat(TOKEN_LEN)).unwrap(), expected);
        assert!(parse_token("ab").is_err());
        assert!(parse_token(&format!("{}gg", "ab".repeat(TOKEN_LEN - 1))).is_err());
    }

    #[test]
    fn requires_all_explicit_cli_options() {
        assert!(Options::parse_from(["--fps".into(), "480".into()]).is_err());
        let parsed = Options::parse_from([
            "--socket".into(),
            "/tmp/luma.sock".into(),
            "--token".into(),
            "00".repeat(TOKEN_LEN),
            "--output".into(),
            "/tmp/luma.mp4".into(),
            "--fps".into(),
            "480".into(),
            "--expected-pid-file".into(),
            "/tmp/luma-game.pid".into(),
        ])
        .unwrap();
        assert_eq!(parsed.fps, 480);
        let parsed = Options::parse_from([
            "--socket".into(),
            "/tmp/luma.sock".into(),
            "--token".into(),
            "00".repeat(TOKEN_LEN),
            "--output".into(),
            "/tmp/luma.mp4".into(),
            "--fps".into(),
            "480".into(),
            "--expected-pid-file".into(),
            "/tmp/luma-game.pid".into(),
            "--expected-process-name".into(),
            "java".into(),
        ])
        .unwrap();
        assert_eq!(parsed.expected_process_name.as_deref(), Some("java"));
        assert!(Options::parse_from([
            "--socket".into(),
            "/tmp/luma.sock".into(),
            "--token".into(),
            "00".repeat(TOKEN_LEN),
            "--output".into(),
            "/tmp/luma.mp4".into(),
            "--fps".into(),
            "480".into(),
            "--expected-pid-file".into(),
            "/tmp/luma-game.pid".into(),
            "--expected-process-name".into(),
            "not valid".into(),
        ])
        .is_err());
    }

    #[test]
    fn reads_kernel_peer_identity_not_a_protocol_claim() {
        let (local, _peer) = UnixStream::pair().unwrap();
        let credentials = peer_credentials(&local).unwrap();
        assert_eq!(credentials.pid, std::process::id());
        // SAFETY: `geteuid` has no preconditions and does not dereference memory.
        assert_eq!(credentials.uid, unsafe { libc::geteuid() });
    }

    #[test]
    fn expected_pid_file_requires_one_positive_pid() {
        let path = std::env::temp_dir().join(format!(
            "luma-game-muxer-pid-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::write(&path, format!("{}\n", std::process::id())).unwrap();
        assert_eq!(expected_pid(&path).unwrap(), std::process::id());
        fs::write(&path, "0\n").unwrap();
        assert!(expected_pid(&path).is_err());
        let _ = fs::remove_file(path);
    }

    #[test]
    fn refuses_a_false_cfr_declaration() {
        let config = VideoConfig {
            codec: Codec::H264,
            width: 1920,
            height: 1080,
            fps_num: 360,
            fps_den: 1,
        };
        assert!(validate_start(config, 480).is_err());
        assert!(
            validate_start(
                VideoConfig {
                    fps_num: 480,
                    ..config
                },
                480
            )
            .is_ok()
        );
    }

    #[test]
    fn stats_track_received_data_and_reject_time_reversal() {
        let mut stats = MuxerStats::default();
        let first = AccessUnit {
            pts_ns: 10,
            dts_ns: 10,
            duration_ns: 1,
            flags: AccessUnitFlags::KEYFRAME,
            data: vec![1, 2],
        };
        let second = AccessUnit {
            pts_ns: 20,
            dts_ns: 20,
            duration_ns: 1,
            flags: AccessUnitFlags::EMPTY,
            data: vec![3],
        };
        stats.note_access_unit(&first).unwrap();
        stats.note_access_unit(&second).unwrap();
        assert_eq!(stats.received_access_units, 2);
        assert_eq!(stats.received_bytes, 3);
        assert_eq!(stats.timeline_span_ns(), 10);
        assert!(
            stats
                .note_access_unit(&AccessUnit {
                    pts_ns: 19,
                    ..second
                })
                .is_err()
        );
    }

    #[test]
    fn ffmpeg_finalizes_a_video_only_hybrid_mp4() {
        let stem = format!(
            "luma-game-muxer-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        let h264 = std::env::temp_dir().join(format!("{stem}.h264"));
        let mp4 = std::env::temp_dir().join(format!("{stem}.mp4"));
        let generated = Command::new("ffmpeg")
            .args([
                "-nostdin",
                "-hide_banner",
                "-loglevel",
                "error",
                "-f",
                "lavfi",
                "-i",
                "color=c=black:s=32x32:r=60",
                "-frames:v",
                "3",
                "-c:v",
                "libx264",
                "-g",
                "1",
                "-x264-params",
                "aud=1:repeat-headers=1",
                "-f",
                "h264",
                "-y",
            ])
            .arg(&h264)
            .status()
            .expect("ffmpeg must be installed for the direct-game muxer");
        assert!(generated.success());
        let mut bytes = Vec::new();
        File::open(&h264).unwrap().read_to_end(&mut bytes).unwrap();
        let boundaries = bytes
            .windows(5)
            .enumerate()
            .filter_map(|(index, window)| (window == [0, 0, 0, 1, 9]).then_some(index))
            .collect::<Vec<_>>();
        assert_eq!(boundaries.len(), 3, "one H.264 AU per generated frame");
        let (mut child, stdin) = spawn_ffmpeg(&mp4).unwrap();
        let (sender, receiver) = mpsc::sync_channel(4);
        let config = VideoConfig {
            codec: Codec::H264,
            width: 32,
            height: 32,
            fps_num: 60,
            fps_den: 1,
        };
        let writer = thread::spawn(move || muxer_writer(receiver, stdin, config));
        let timing = [
            (0, 16_666_667),
            (16_666_667, 100_000_000),
            (116_666_667, 33_333_333),
        ];
        for (index, (pts_ns, duration_ns)) in timing.into_iter().enumerate() {
            let end = boundaries.get(index + 1).copied().unwrap_or(bytes.len());
            sender
                .send(WriterInput::AccessUnit(AccessUnit {
                    pts_ns,
                    dts_ns: pts_ns,
                    duration_ns,
                    flags: AccessUnitFlags::KEYFRAME,
                    data: bytes[boundaries[index]..end].to_vec(),
                }))
                .unwrap();
        }
        sender.send(WriterInput::Finish).unwrap();
        drop(sender);
        assert_eq!(writer.join().unwrap().unwrap(), 3);
        wait_cleanly(&mut child).unwrap();
        let inspected = Command::new("ffprobe")
            .args([
                "-v",
                "error",
                "-show_entries",
                "stream=codec_type",
                "-of",
                "csv=p=0",
            ])
            .arg(&mp4)
            .output()
            .expect("ffprobe must be installed for the direct-game muxer");
        assert!(inspected.status.success());
        assert_eq!(String::from_utf8(inspected.stdout).unwrap().trim(), "video");
        assert!(mp4.metadata().unwrap().len() > 0);
        let duration = Command::new("ffprobe")
            .args([
                "-v",
                "error",
                "-show_entries",
                "format=duration",
                "-of",
                "csv=p=0",
            ])
            .arg(&mp4)
            .output()
            .unwrap();
        assert!(duration.status.success());
        let seconds = String::from_utf8(duration.stdout)
            .unwrap()
            .trim()
            .parse::<f64>()
            .unwrap();
        assert!(
            // MPEG-TS carries PTS but no per-sample durations, so the MP4 is
            // timed purely by presentation timestamps: 0, 1/60, then the
            // 100 ms VFR gap, with the trailing frame defaulting to one
            // frame interval. The packet-PTS assertions below are the true
            // VFR proof; this only guards against CFR collapse.
            seconds > 0.12,
            "irregular 150 ms source timeline must not become 3/60 s: {seconds}"
        );
        let packets = Command::new("ffprobe")
            .args([
                "-v",
                "error",
                "-select_streams",
                "v:0",
                "-show_entries",
                "packet=pts_time",
                "-of",
                "csv=p=0",
            ])
            .arg(&mp4)
            .output()
            .unwrap();
        assert!(packets.status.success());
        let pts = String::from_utf8(packets.stdout)
            .unwrap()
            .lines()
            .map(|line| line.parse::<f64>().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(pts.len(), 3);
        assert!(
            pts[2] - pts[1] > 0.09,
            "ffprobe must retain the 100 ms VFR gap: {pts:?}"
        );
        let _ = fs::remove_file(h264);
        let _ = fs::remove_file(mp4);
    }

    #[test]
    fn writer_requires_explicit_finish_before_finalizing() {
        let output = std::env::temp_dir().join(format!(
            "luma-game-muxer-abort-{}-{}.mp4",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let (mut child, stdin) = spawn_ffmpeg(&output).unwrap();
        let (sender, receiver) = mpsc::sync_channel(1);
        drop(sender);
        let result = muxer_writer(
            receiver,
            stdin,
            VideoConfig {
                codec: Codec::H264,
                width: 32,
                height: 32,
                fps_num: 60,
                fps_den: 1,
            },
        );
        assert!(
            result
                .unwrap_err()
                .contains("without protocol Stop or clean EOF")
        );
        let _ = child.kill();
        let _ = child.wait();
        let _ = fs::remove_file(output);
    }
}
