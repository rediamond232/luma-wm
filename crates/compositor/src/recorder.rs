//! Low-overhead native recorder process supervision.
//!
//! The encoder is isolated from the compositor, but it consumes Luma's direct
//! image-copy DMA-BUF stream instead of going through a desktop portal.

use std::{
    io::{Read, Write},
    path::{Path, PathBuf},
    process::{Child, ChildStdin, Command, Stdio},
    time::{Instant, SystemTime, UNIX_EPOCH},
};

use wm_core::{GameCaptureProfile, Recorder, RecorderState, RecorderStatus};

#[derive(Debug, Default)]
pub struct RecorderController {
    child: Option<Child>,
    stdin: Option<ChildStdin>,
    // This is the opt-in game launch/attach wrapper, not the game process. It owns
    // only the direct-capture muxer sidecar and is deliberately signalled by
    // its exact PID on stop; the game itself is never killed by Luma.
    game_launcher: Option<Child>,
    log_path: Option<PathBuf>,
    started: Option<Instant>,
    sample_started: Option<Instant>,
    sample_frames: u64,
    replay_max_mib: Option<u32>,
    pub status: RecorderStatus,
}

impl RecorderController {
    pub fn start(
        &mut self,
        config: &Recorder,
        replay: bool,
        wayland_display: Option<&str>,
        capture_output: &str,
        commit_paced: bool,
    ) -> Result<u32, String> {
        if self.is_running() {
            return Err("recorder is already running".into());
        }
        if replay {
            return Err("instant replay is not implemented by the native recorder yet".into());
        }
        if !config.enabled {
            return Err("recorder is disabled in configuration".into());
        }
        let output_directory = expand_home(&config.output_directory)?;
        std::fs::create_dir_all(&output_directory).map_err(|error| {
            format!(
                "failed to create recorder directory {}: {error}",
                output_directory.display()
            )
        })?;
        let runtime = std::env::var_os("XDG_RUNTIME_DIR")
            .map(PathBuf::from)
            .ok_or("XDG_RUNTIME_DIR is missing")?;
        let log_path = runtime.join("luma-recorder.log");
        let log = std::fs::File::create(&log_path)
            .map_err(|error| format!("failed to create recorder log: {error}"))?;

        let epoch = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|error| error.to_string())?
            .as_secs();
        let output_path = output_directory.join(format!("luma-{epoch}.mp4"));
        let engine = std::env::current_exe()
            .map_err(|error| error.to_string())?
            .with_file_name("luma-recorder-engine");
        if !engine.is_file() {
            return Err(format!(
                "native recorder engine is missing: {}",
                engine.display()
            ));
        }
        let mut command = Command::new(engine);
        command
            .args(["--output", output_path.to_string_lossy().as_ref()])
            .args(["--fps", &config.screen_fps.to_string()])
            .args(["--codec", config.codec.as_str()])
            .args(["--hdr", if config.hdr { "true" } else { "false" }])
            .args(["--quality", &config.quality.to_string()])
            .args(["--width", &config.output_width.to_string()])
            .args(["--height", &config.output_height.to_string()])
            .args(["--output-name", capture_output])
            .args(["--pacing", if commit_paced { "commit" } else { "clock" }])
            .args(["--desktop-audio", config.desktop_audio.as_str()])
            .args(["--microphone", config.microphone.as_str()])
            .env_remove("DISPLAY")
            .env_remove("WAYLAND_SOCKET")
            .env("XDG_SESSION_TYPE", "wayland")
            .env("GST_GL_PLATFORM", "egl")
            .env("GST_GL_WINDOW", "surfaceless")
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped());
        if let Some(display) = wayland_display {
            command.env("WAYLAND_DISPLAY", display);
        }
        if replay {
            command.args(["--replay-seconds", &config.replay_seconds.to_string()]);
        }

        let mut child = command
            .spawn()
            .map_err(|error| format!("failed to start native recorder: {error}"))?;
        let stderr = child
            .stderr
            .take()
            .ok_or("native recorder log pipe is missing")?;
        std::thread::spawn(move || {
            drain_log(stderr, log);
        });
        let stdin = child
            .stdin
            .take()
            .ok_or("native recorder control pipe is missing")?;
        self.status = RecorderStatus {
            state: if replay {
                RecorderState::Replay
            } else {
                RecorderState::Starting
            },
            source: Some("native-dmabuf".into()),
            requested_fps: config.screen_fps,
            output_path: (!replay).then(|| output_path.to_string_lossy().into_owned()),
            ..RecorderStatus::default()
        };
        self.started = Some(Instant::now());
        self.sample_started = self.started;
        self.sample_frames = 0;
        self.replay_max_mib = replay.then_some(config.replay_max_mib);
        self.stdin = Some(stdin);
        self.log_path = Some(log_path);
        self.child = Some(child);
        Ok(config.screen_fps)
    }

    /// Launch one explicitly configured renderer through its graphics-API
    /// capture wrapper. This is intentionally separate from `start`: it
    /// must not request compositor frames or enable the capture boost timer.
    pub fn start_game(
        &mut self,
        config: &Recorder,
        profile: &GameCaptureProfile,
    ) -> Result<u32, String> {
        if self.is_running() {
            return Err("recorder is already running".into());
        }
        if !config.enabled {
            return Err("recorder is disabled in configuration".into());
        }
        if !matches!(profile.api.as_str(), "opengl" | "vulkan") {
            return Err(format!(
                "game capture profile '{}' uses unsupported API {}",
                profile.name, profile.api
            ));
        }
        let output_directory = expand_home(&config.output_directory)?;
        std::fs::create_dir_all(&output_directory).map_err(|error| {
            format!(
                "failed to create recorder directory {}: {error}",
                output_directory.display()
            )
        })?;
        // Both graphics-API launchers intentionally reject relative output paths.
        // Resolve only this game-capture path after creating it, so a config
        // such as `~/Videos/Luma` is handed to the shell wrapper safely.
        let output_directory = std::fs::canonicalize(&output_directory).map_err(|error| {
            format!(
                "failed to resolve game capture directory {}: {error}",
                output_directory.display()
            )
        })?;
        let runtime = std::env::var_os("XDG_RUNTIME_DIR")
            .map(PathBuf::from)
            .ok_or("XDG_RUNTIME_DIR is missing")?;
        let log_path = runtime.join("luma-game-capture.log");
        let log = std::fs::File::create(&log_path)
            .map_err(|error| format!("failed to create game capture log: {error}"))?;
        let epoch = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|error| error.to_string())?
            .as_secs();
        let output_path = output_directory.join(format!("luma-game-{epoch}.mp4"));
        let launcher = game_capture_launcher(&profile.api)?;

        let mut command = Command::new(&launcher);
        command
            .args(["--output", output_path.to_string_lossy().as_ref()])
            .args(["--fps", &profile.fps.to_string()])
            .args(["--quality", &config.quality.to_string()]);
        if !profile.target_process_name.is_empty() {
            if profile.api != "opengl" {
                return Err("only OpenGL game profiles may target a launcher child process".into());
            }
            command.args(["--target-process-name", &profile.target_process_name]);
        }
        command
            .arg("--")
            .args(&profile.command)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped());
        let mut launcher_child = command.spawn().map_err(|error| {
            format!(
                "failed to launch {} capture wrapper {}: {error}",
                profile.api,
                launcher.display()
            )
        })?;
        let stderr = launcher_child
            .stderr
            .take()
            .ok_or("game capture log pipe is missing")?;
        std::thread::spawn(move || drain_log(stderr, log));

        self.status = RecorderStatus {
            state: RecorderState::Starting,
            source: Some(format!("game-{}:{}", profile.api, profile.name)),
            requested_fps: profile.fps,
            output_path: Some(output_path.to_string_lossy().into_owned()),
            ..RecorderStatus::default()
        };
        self.started = Some(Instant::now());
        self.sample_started = None;
        self.sample_frames = 0;
        self.replay_max_mib = None;
        self.log_path = Some(log_path);
        self.game_launcher = Some(launcher_child);
        Ok(profile.fps)
    }

    /// Inject the direct OpenGL hook into one already-running same-user
    /// graphics process. The helper owns the muxer, never the game.
    pub fn start_game_attach(&mut self, config: &Recorder, pid: u32) -> Result<u32, String> {
        if self.is_running() {
            return Err("recorder is already running".into());
        }
        if !config.enabled {
            return Err("recorder is disabled in configuration".into());
        }
        validate_attach_target(pid)?;

        let output_directory = expand_home(&config.output_directory)?;
        std::fs::create_dir_all(&output_directory).map_err(|error| {
            format!(
                "failed to create recorder directory {}: {error}",
                output_directory.display()
            )
        })?;
        let output_directory = std::fs::canonicalize(&output_directory).map_err(|error| {
            format!(
                "failed to resolve game capture directory {}: {error}",
                output_directory.display()
            )
        })?;
        let runtime = std::env::var_os("XDG_RUNTIME_DIR")
            .map(PathBuf::from)
            .ok_or("XDG_RUNTIME_DIR is missing")?;
        let log_path = runtime.join("luma-game-capture.log");
        let log = std::fs::File::create(&log_path)
            .map_err(|error| format!("failed to create game capture log: {error}"))?;
        let epoch = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|error| error.to_string())?
            .as_secs();
        let output_path = output_directory.join(format!("luma-game-{epoch}.mp4"));
        let attacher = game_capture_attacher()?;
        let mut command = Command::new(&attacher);
        command
            .args(["--pid", &pid.to_string()])
            .args(["--output", output_path.to_string_lossy().as_ref()])
            .args(["--fps", &config.fps.to_string()])
            .args(["--quality", &config.quality.to_string()])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped());
        let mut child = command.spawn().map_err(|error| {
            format!(
                "failed to start OpenGL attach helper {}: {error}",
                attacher.display()
            )
        })?;
        let stderr = child
            .stderr
            .take()
            .ok_or("game attach log pipe is missing")?;
        std::thread::spawn(move || drain_log(stderr, log));
        self.status = RecorderStatus {
            state: RecorderState::Starting,
            source: Some(format!("game-opengl-inject:{pid}")),
            requested_fps: config.fps,
            output_path: Some(output_path.to_string_lossy().into_owned()),
            ..RecorderStatus::default()
        };
        self.started = Some(Instant::now());
        self.sample_started = None;
        self.sample_frames = 0;
        self.replay_max_mib = None;
        self.log_path = Some(log_path);
        self.game_launcher = Some(child);
        Ok(config.fps)
    }

    /// Capture one managed Xwayland window without modifying the application.
    /// The compositor copies freshly committed, already-imported textures into
    /// the native recorder's bounded DMA-BUF pool. The copy gives each captured
    /// frame an independent lifetime before Xwayland reuses its client buffer.
    pub fn start_game_xwayland(
        &mut self,
        config: &Recorder,
        wayland_display: Option<&str>,
        capture_output: &str,
        window: u32,
    ) -> Result<u32, String> {
        if window == 0 {
            return Err("Xwayland capture requires a nonzero X11 window ID".into());
        }
        let mut native_config = config.clone();
        native_config.screen_fps = config.fps;
        let fps = self.start(&native_config, false, wayland_display, capture_output, true)?;
        self.status.source = Some(format!("game-xwayland-commit:{window:#x}"));
        Ok(fps)
    }

    pub fn stop(&mut self) -> Result<(), String> {
        if let Some(child) = self.child.take() {
            let mut stdin = self
                .stdin
                .take()
                .ok_or("recorder control pipe is missing")?;
            stdin
                .write_all(b"stop\n")
                .map_err(|error| format!("failed to stop recorder: {error}"))?;
            drop(stdin);
            wait_detached(child);
        } else if let Some(launcher) = self.game_launcher.take() {
            terminate_game_launcher(launcher)?;
        } else {
            return Err("recorder is not running".into());
        }
        self.reset();
        Ok(())
    }

    pub fn toggle_pause(&mut self) -> Result<(), String> {
        if self.status.state == RecorderState::Replay {
            return Err("replay buffering cannot be paused".into());
        }
        self.send("pause")?;
        self.status.state = if self.status.state == RecorderState::Paused {
            RecorderState::Recording
        } else {
            RecorderState::Paused
        };
        Ok(())
    }

    pub fn save_replay(&mut self) -> Result<(), String> {
        if self.status.state != RecorderState::Replay {
            return Err("replay buffer is not running".into());
        }
        self.send("save-replay")?;
        Ok(())
    }

    pub fn poll(&mut self) -> bool {
        if let (Some(child), Some(limit)) = (self.child.as_mut(), self.replay_max_mib) {
            if process_rss_mib(child.id()).is_some_and(|rss| rss > u64::from(limit)) {
                let _ = child.kill();
                let _ = child.wait();
                self.child = None;
                self.status.state = RecorderState::Error;
                self.status.error =
                    Some(format!("replay memory safety limit exceeded ({limit} MiB)"));
                self.replay_max_mib = None;
                return true;
            }
        }
        if self.child.is_some() {
            return self.poll_native();
        }
        if self.game_launcher.is_some() {
            return self.poll_game_launcher();
        }
        false
    }

    fn poll_native(&mut self) -> bool {
        let result = self
            .child
            .as_mut()
            .expect("native recorder checked before polling")
            .try_wait();
        match result {
            Ok(Some(status)) => {
                self.child = None;
                self.status.state = RecorderState::Error;
                let detail = self
                    .log_path
                    .as_deref()
                    .and_then(read_log_tail)
                    .filter(|detail| !detail.is_empty())
                    .map(|detail| format!(": {detail}"))
                    .unwrap_or_default();
                self.status.error = Some(format!("recorder exited with {status}{detail}"));
                self.stdin = None;
                self.log_path = None;
                self.started = None;
                true
            }
            Ok(None) => {
                if self.status.state == RecorderState::Starting
                    && self
                        .started
                        .is_some_and(|started| started.elapsed().as_millis() >= 500)
                {
                    self.status.state = RecorderState::Recording;
                    true
                } else {
                    if let Some(started) = self.started {
                        self.status.elapsed_ms = started.elapsed().as_millis() as u64;
                    }
                    false
                }
            }
            Err(error) => {
                self.status.state = RecorderState::Error;
                self.status.error = Some(format!("failed to inspect recorder: {error}"));
                true
            }
        }
    }

    fn poll_game_launcher(&mut self) -> bool {
        let result = self
            .game_launcher
            .as_mut()
            .expect("game launcher checked before polling")
            .try_wait();
        match result {
            Ok(Some(exit)) => {
                self.game_launcher = None;
                self.stdin = None;
                self.started = None;
                self.sample_started = None;
                self.sample_frames = 0;
                self.replay_max_mib = None;
                if exit.success() {
                    // The launcher has waited for the muxer to finalize its
                    // MP4. Preserve the source and output path so the UI can
                    // show where the completed recording was written.
                    self.status.state = RecorderState::Idle;
                    self.status.error = None;
                } else {
                    self.status.state = RecorderState::Error;
                    let detail = self
                        .log_path
                        .as_deref()
                        .and_then(read_log_tail)
                        .filter(|detail| !detail.is_empty())
                        .map(|detail| format!(": {detail}"))
                        .unwrap_or_default();
                    self.status.error =
                        Some(format!("game capture launcher exited with {exit}{detail}"));
                }
                self.log_path = None;
                true
            }
            Ok(None) => {
                if self.status.state == RecorderState::Starting
                    && self
                        .started
                        .is_some_and(|started| started.elapsed().as_millis() >= 500)
                {
                    self.status.state = RecorderState::Recording;
                    true
                } else {
                    if let Some(started) = self.started {
                        self.status.elapsed_ms = started.elapsed().as_millis() as u64;
                    }
                    false
                }
            }
            Err(error) => {
                self.status.state = RecorderState::Error;
                self.status.error =
                    Some(format!("failed to inspect game capture launcher: {error}"));
                true
            }
        }
    }

    pub fn is_running(&self) -> bool {
        self.child.is_some() || self.game_launcher.is_some()
    }

    pub fn is_game_capture(&self) -> bool {
        self.game_launcher.is_some()
    }

    fn send(&mut self, action: &str) -> Result<(), String> {
        let stdin = self.stdin.as_mut().ok_or("recorder is not running")?;
        writeln!(stdin, "{action}").map_err(|error| format!("recorder control failed: {error}"))?;
        stdin
            .flush()
            .map_err(|error| format!("recorder control failed: {error}"))
    }

    pub fn note_source_frame(&mut self) {
        if self.child.is_none() {
            return;
        }
        self.sample_frames += 1;
        let now = Instant::now();
        let sample_started = self.sample_started.get_or_insert(now);
        let elapsed = now.duration_since(*sample_started).as_secs_f64();
        if elapsed >= 0.5 {
            self.status.source_fps = (self.sample_frames as f64 / elapsed) as f32;
            self.status.encoded_fps = self.status.source_fps.min(self.status.requested_fps as f32);
            self.sample_frames = 0;
            self.sample_started = Some(now);
        }
    }

    pub fn abort(&mut self, reason: &str) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
        if let Some(launcher) = self.game_launcher.take() {
            let _ = terminate_game_launcher(launcher);
        }
        self.stdin = None;
        self.log_path = None;
        self.started = None;
        self.sample_started = None;
        self.sample_frames = 0;
        self.replay_max_mib = None;
        self.status.state = RecorderState::Error;
        self.status.error = Some(reason.into());
    }

    fn reset(&mut self) {
        self.child = None;
        self.stdin = None;
        self.game_launcher = None;
        self.log_path = None;
        self.started = None;
        self.sample_started = None;
        self.sample_frames = 0;
        self.replay_max_mib = None;
        self.status = RecorderStatus::default();
    }
}

impl Drop for RecorderController {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
        if let Some(launcher) = self.game_launcher.take() {
            let _ = terminate_game_launcher(launcher);
        }
    }
}

fn game_capture_launcher(api: &str) -> Result<PathBuf, String> {
    let (environment, executable, development) = match api {
        "opengl" => (
            "LUMA_GAME_CAPTURE_GL_LAUNCHER",
            "luma-game-record-gl",
            "native/game-capture-gl/luma-game-record-gl",
        ),
        "vulkan" => (
            "LUMA_GAME_CAPTURE_VULKAN_LAUNCHER",
            "luma-game-record-vulkan",
            "native/vulkan-external-capture-prototype/luma-game-record-vulkan",
        ),
        _ => return Err(format!("unsupported graphics API {api}")),
    };
    if let Some(path) = std::env::var_os(environment) {
        let path = PathBuf::from(path);
        if executable_file(&path) {
            return Ok(path);
        }
        return Err(format!(
            "{environment} is not an executable file: {}",
            path.display()
        ));
    }

    let current = std::env::current_exe().map_err(|error| error.to_string())?;
    let sibling = current.with_file_name(executable);
    if executable_file(&sibling) {
        return Ok(sibling);
    }

    let development = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join(development);
    if executable_file(&development) {
        return Ok(development);
    }

    Err(format!(
        "Luma {api} capture launcher is missing; install {executable} beside wm or set {environment}"
    ))
}

fn game_capture_attacher() -> Result<PathBuf, String> {
    if let Some(path) = std::env::var_os("LUMA_GAME_CAPTURE_ATTACHER") {
        let path = PathBuf::from(path);
        if executable_file(&path) {
            return Ok(path);
        }
        return Err(format!(
            "LUMA_GAME_CAPTURE_ATTACHER is not an executable file: {}",
            path.display()
        ));
    }
    let current = std::env::current_exe().map_err(|error| error.to_string())?;
    let sibling = current.with_file_name("luma-game-attach");
    if executable_file(&sibling) {
        return Ok(sibling);
    }
    let development = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join("native/game-capture-gl/luma-game-attach");
    if executable_file(&development) {
        return Ok(development);
    }
    Err("Luma generic attach helper is missing; install luma-game-attach beside wm or set LUMA_GAME_CAPTURE_ATTACHER".into())
}

fn validate_attach_target(pid: u32) -> Result<(), String> {
    let proc = PathBuf::from(format!("/proc/{pid}"));
    let status = std::fs::read_to_string(proc.join("status"))
        .map_err(|_| format!("attach target PID {pid} does not exist or is inaccessible"))?;
    let uid = status
        .lines()
        .find_map(|line| line.strip_prefix("Uid:"))
        .and_then(|ids| ids.split_whitespace().next())
        .and_then(|id| id.parse::<u32>().ok())
        .ok_or_else(|| format!("could not determine owner of PID {pid}"))?;
    let own_status = std::fs::read_to_string("/proc/self/status")
        .map_err(|error| format!("could not determine recorder process owner: {error}"))?;
    let own_uid = own_status
        .lines()
        .find_map(|line| line.strip_prefix("Uid:"))
        .and_then(|ids| ids.split_whitespace().next())
        .and_then(|id| id.parse::<u32>().ok())
        .ok_or("could not determine recorder process owner")?;
    if uid != own_uid {
        return Err("Luma only injects into a process owned by the current user".into());
    }
    let maps = std::fs::read_to_string(proc.join("maps"))
        .map_err(|error| format!("cannot inspect attach target PID {pid}: {error}"))?;
    if !maps.lines().any(|line| {
        line.contains("/libGL.so")
            || line.contains("/libGLX.so")
            || line.contains("/libOpenGL.so")
            || line.contains("/libEGL.so")
    }) {
        return Err(format!("PID {pid} has no loaded OpenGL GLX/EGL runtime"));
    }
    Ok(())
}

fn executable_file(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    path.is_file()
        && std::fs::metadata(path)
            .map(|metadata| metadata.permissions().mode() & 0o111 != 0)
            .unwrap_or(false)
}

fn terminate_game_launcher(mut launcher: Child) -> Result<(), String> {
    let pid = launcher.id();
    // Signal only the wrapper PID.  It runs its cleanup trap to stop the
    // muxer; its game child is intentionally neither signalled nor waited on
    // by Luma here.
    Command::new("/bin/kill")
        .args(["-TERM", &pid.to_string()])
        .status()
        .map_err(|error| format!("failed to terminate game capture launcher: {error}"))
        .and_then(|status| {
            status
                .success()
                .then_some(())
                .ok_or_else(|| format!("failed to terminate game capture launcher {pid}: {status}"))
        })?;
    std::thread::spawn(move || {
        let _ = launcher.wait();
    });
    Ok(())
}

fn wait_detached(mut recorder: Child) {
    std::thread::spawn(move || {
        let deadline = Instant::now() + std::time::Duration::from_secs(20);
        while Instant::now() < deadline {
            if recorder.try_wait().ok().flatten().is_some() {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        let _ = recorder.kill();
        let _ = recorder.wait();
    });
}

fn expand_home(value: &str) -> Result<PathBuf, String> {
    if value == "~" || value.starts_with("~/") {
        let home = std::env::var_os("HOME").ok_or("HOME is missing")?;
        return Ok(PathBuf::from(home).join(value.trim_start_matches("~/")));
    }
    Ok(PathBuf::from(value))
}

fn process_rss_mib(pid: u32) -> Option<u64> {
    let status = std::fs::read_to_string(format!("/proc/{pid}/status")).ok()?;
    let kib = status
        .lines()
        .find_map(|line| line.strip_prefix("VmRSS:"))?
        .split_whitespace()
        .next()?
        .parse::<u64>()
        .ok()?;
    Some(kib.div_ceil(1024))
}

fn read_log_tail(path: &Path) -> Option<String> {
    let bytes = std::fs::read(path).ok()?;
    let start = bytes.len().saturating_sub(4096);
    Some(
        String::from_utf8_lossy(&bytes[start..])
            .lines()
            .rev()
            .find(|line| !line.trim().is_empty())?
            .trim()
            .chars()
            .take(400)
            .collect(),
    )
}

const MAX_RECORDER_LOG_BYTES: usize = 1024 * 1024;

fn drain_log(mut input: impl Read, mut output: impl Write) {
    let mut buffer = [0u8; 8192];
    let mut written = 0usize;
    loop {
        let Ok(count) = input.read(&mut buffer) else {
            break;
        };
        if count == 0 {
            break;
        }
        let allowed = count.min(MAX_RECORDER_LOG_BYTES.saturating_sub(written));
        if allowed > 0 && output.write_all(&buffer[..allowed]).is_ok() {
            written += allowed;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recorder_log_is_capped_while_the_pipe_is_fully_drained() {
        let input = vec![b'x'; MAX_RECORDER_LOG_BYTES * 2];
        let mut output = Vec::new();
        drain_log(std::io::Cursor::new(input), &mut output);
        assert_eq!(output.len(), MAX_RECORDER_LOG_BYTES);
    }
}
