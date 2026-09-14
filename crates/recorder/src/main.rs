//! Native Luma capture engine.
//!
//! Frames arrive directly from Luma's image-copy protocol into DMA-BUFs. The
//! encoder path imports those buffers into GL and NVENC without a portal or a
//! GPU-to-CPU readback.

use std::{
    collections::VecDeque,
    fs::{File, OpenOptions},
    io::BufRead,
    os::{
        fd::{AsFd, AsRawFd},
        unix::fs::MetadataExt,
    },
    path::{Path, PathBuf},
    process::{Child, ChildStdin, Command, Stdio},
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
        mpsc,
    },
    thread,
    time::{Duration, Instant},
};

use gbm::{BufferObject, BufferObjectFlags, Device, Format, Modifier};
use gstreamer as gst;
use gstreamer::prelude::*;
use gstreamer_allocators::{DmaBufAllocator, prelude::DmaBufAllocatorExtManual};
use gstreamer_video::{VideoFormat, VideoInfo, VideoInfoDmaDrm, VideoMeta};

use wayland_client::{
    Connection, Dispatch, Proxy, QueueHandle,
    globals::{GlobalListContents, registry_queue_init},
    protocol::{wl_buffer, wl_output, wl_registry},
};
use wayland_protocols::ext::{
    image_capture_source::v1::client::{
        ext_image_capture_source_v1, ext_output_image_capture_source_manager_v1,
    },
    image_copy_capture::v1::client::{
        ext_image_copy_capture_frame_v1, ext_image_copy_capture_manager_v1,
        ext_image_copy_capture_session_v1,
    },
};
use wayland_protocols::wp::linux_dmabuf::zv1::client::{
    zwp_linux_buffer_params_v1, zwp_linux_dmabuf_v1,
};

#[derive(Debug)]
struct Options {
    output: PathBuf,
    fps: u32,
    codec: String,
    quality: u8,
    desktop_audio: String,
    microphone: String,
    output_width: u32,
    output_height: u32,
    output_name: Option<String>,
    replay_seconds: Option<u32>,
    commit_paced: bool,
}

impl Options {
    fn parse() -> Result<Self, String> {
        let mut args = std::env::args().skip(1);
        let mut options = Self {
            output: PathBuf::new(),
            fps: 480,
            codec: "h264".into(),
            quality: 20,
            desktop_audio: "disabled".into(),
            microphone: "disabled".into(),
            output_width: 0,
            output_height: 0,
            output_name: None,
            replay_seconds: None,
            commit_paced: false,
        };
        while let Some(flag) = args.next() {
            let value = args
                .next()
                .ok_or_else(|| format!("missing value for {flag}"))?;
            match flag.as_str() {
                "--output" => options.output = value.into(),
                "--fps" => options.fps = value.parse().map_err(|_| "invalid fps")?,
                "--codec" => options.codec = value,
                "--quality" => options.quality = value.parse().map_err(|_| "invalid quality")?,
                "--desktop-audio" => options.desktop_audio = value,
                "--microphone" => options.microphone = value,
                "--width" => options.output_width = value.parse().map_err(|_| "invalid width")?,
                "--height" => {
                    options.output_height = value.parse().map_err(|_| "invalid height")?
                }
                "--output-name" => options.output_name = Some(value),
                "--pacing" => {
                    options.commit_paced = match value.as_str() {
                        "clock" => false,
                        "commit" => true,
                        _ => return Err("pacing must be clock or commit".into()),
                    }
                }
                "--replay-seconds" => {
                    options.replay_seconds = Some(
                        value
                            .parse()
                            .map_err(|_| "invalid replay buffer duration")?,
                    )
                }
                _ => return Err(format!("unknown option {flag}")),
            }
        }
        if options.output.as_os_str().is_empty() || !(30..=480).contains(&options.fps) {
            return Err("--output and an fps from 30 through 480 are required".into());
        }
        if options.replay_seconds.is_some() {
            return Err("native replay buffering is not implemented yet".into());
        }
        Ok(options)
    }
}

#[derive(Debug, Default)]
struct CaptureState {
    width: u32,
    height: u32,
    drm_device: Option<u64>,
    formats: Vec<(u32, Vec<u64>)>,
    constraints_done: bool,
    stopped: bool,
    frame_ready: bool,
    frame_failed: Option<String>,
    presentation_ns: Option<u64>,
    ready_slot: Option<usize>,
    output_names: Vec<Option<String>>,
}

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for CaptureState {
    fn event(
        _state: &mut Self,
        _proxy: &wl_registry::WlRegistry,
        _event: wl_registry::Event,
        _data: &GlobalListContents,
        _connection: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<wl_output::WlOutput, usize> for CaptureState {
    fn event(
        state: &mut Self,
        _proxy: &wl_output::WlOutput,
        event: wl_output::Event,
        index: &usize,
        _connection: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        if let wl_output::Event::Name { name } = event
            && let Some(slot) = state.output_names.get_mut(*index)
        {
            *slot = Some(name);
        }
    }
}

macro_rules! empty_dispatch {
    ($ty:path) => {
        impl Dispatch<$ty, ()> for CaptureState {
            fn event(
                _state: &mut Self,
                _proxy: &$ty,
                _event: <$ty as wayland_client::Proxy>::Event,
                _data: &(),
                _connection: &Connection,
                _qh: &QueueHandle<Self>,
            ) {
            }
        }
    };
}

empty_dispatch!(ext_output_image_capture_source_manager_v1::ExtOutputImageCaptureSourceManagerV1);
empty_dispatch!(ext_image_capture_source_v1::ExtImageCaptureSourceV1);
empty_dispatch!(ext_image_copy_capture_manager_v1::ExtImageCopyCaptureManagerV1);
empty_dispatch!(wl_buffer::WlBuffer);
empty_dispatch!(zwp_linux_dmabuf_v1::ZwpLinuxDmabufV1);
empty_dispatch!(zwp_linux_buffer_params_v1::ZwpLinuxBufferParamsV1);

impl Dispatch<ext_image_copy_capture_session_v1::ExtImageCopyCaptureSessionV1, ()>
    for CaptureState
{
    fn event(
        state: &mut Self,
        _proxy: &ext_image_copy_capture_session_v1::ExtImageCopyCaptureSessionV1,
        event: ext_image_copy_capture_session_v1::Event,
        _data: &(),
        _connection: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        use ext_image_copy_capture_session_v1::Event;
        match event {
            Event::BufferSize { width, height } => {
                state.width = width;
                state.height = height;
            }
            Event::DmabufDevice { device } => {
                if device.len() == std::mem::size_of::<u64>() {
                    state.drm_device = Some(u64::from_ne_bytes(device.try_into().unwrap()));
                }
            }
            Event::DmabufFormat { format, modifiers } => {
                let modifiers = modifiers
                    .chunks_exact(8)
                    .map(|bytes| u64::from_ne_bytes(bytes.try_into().unwrap()))
                    .collect();
                state.formats.push((format, modifiers));
            }
            Event::Done => state.constraints_done = true,
            Event::Stopped => state.stopped = true,
            _ => {}
        }
    }
}

impl Dispatch<ext_image_copy_capture_frame_v1::ExtImageCopyCaptureFrameV1, usize> for CaptureState {
    fn event(
        state: &mut Self,
        _proxy: &ext_image_copy_capture_frame_v1::ExtImageCopyCaptureFrameV1,
        event: ext_image_copy_capture_frame_v1::Event,
        slot: &usize,
        _connection: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        use ext_image_copy_capture_frame_v1::Event;
        match event {
            Event::PresentationTime {
                tv_sec_hi,
                tv_sec_lo,
                tv_nsec,
            } => {
                let seconds = (u64::from(tv_sec_hi) << 32) | u64::from(tv_sec_lo);
                state.presentation_ns = Some(seconds * 1_000_000_000 + u64::from(tv_nsec));
            }
            Event::Ready => {
                state.frame_ready = true;
                state.ready_slot = Some(*slot);
            }
            Event::Failed { reason } => state.frame_failed = Some(format!("{reason:?}")),
            _ => {}
        }
    }
}

struct CaptureBuffer {
    bo: BufferObject<()>,
    wl_buffer: wl_buffer::WlBuffer,
}

struct BufferRelease {
    remaining: AtomicUsize,
    sender: mpsc::Sender<usize>,
    slot: usize,
}

#[derive(Debug, Default)]
struct PushOutcome {
    written: u64,
    duplicated: u64,
    unfilled: u64,
    maximum_gap: u64,
    skipped: bool,
}

unsafe extern "C" fn encoder_buffer_finalized(
    data: *mut libc::c_void,
    _mini_object: *mut gst::ffi::GstMiniObject,
) {
    let release = unsafe { Arc::from_raw(data.cast::<BufferRelease>()) };
    if release.remaining.fetch_sub(1, Ordering::AcqRel) == 1 {
        let _ = release.sender.send(release.slot);
    }
}

struct Encoder {
    pipeline: gst::Pipeline,
    appsrc: gst::Element,
    audio_sources: Vec<gst::Element>,
    muxer: Child,
    muxer_stdin: Option<ChildStdin>,
    allocator: DmaBufAllocator,
    presentation_origin_ns: Option<u64>,
    paused_ns: u64,
    paused_at: Option<Instant>,
    fps: u32,
    last_frame_index: Option<u64>,
}

impl Encoder {
    fn new(
        options: &Options,
        fourcc: u32,
        modifier: u64,
        width: u32,
        height: u32,
    ) -> Result<Self, String> {
        gst::init().map_err(|error| error.to_string())?;
        let encoder = match options.codec.as_str() {
            "h264" => "nvh264enc",
            "hevc" => "nvh265enc",
            codec => return Err(format!("unsupported codec {codec}")),
        };
        let parser = if options.codec == "h264" {
            "h264parse"
        } else {
            "h265parse"
        };
        let mut pipeline = format!(
            "appsrc name=video is-live=true format=time do-timestamp=false block=true max-buffers=4 \
             ! glupload ! glcolorscale \
             ! video/x-raw(memory:GLMemory),format=RGBA,width={},height={},framerate={}/1 \
             ! glcolorconvert \
             ! video/x-raw(memory:GLMemory),format=NV12,width={},height={},framerate={}/1 \
             ! {} preset=p1 tune=ultra-low-latency rc-mode=constqp qp-const-i={} qp-const-p={} qp-const-b={} bframes=0 rc-lookahead=0 zerolatency=true \
             ! {} ! queue ! mux.video_0 mp4mux name=mux fragment-duration=1000 streamable=true movie-timescale=48000 trak-timescale=48000 \
             ! fdsink name=transport_sink sync=false",
            options_width(options, width),
            options_height(options, height),
            options.fps,
            options_width(options, width),
            options_height(options, height),
            options.fps,
            encoder,
            options.quality,
            options.quality,
            options.quality,
            parser,
        );
        for (index, source) in [&options.desktop_audio, &options.microphone]
            .into_iter()
            .enumerate()
        {
            if source == "disabled" || source.is_empty() {
                continue;
            }
            let device = match source.as_str() {
                "default_output" => "@DEFAULT_MONITOR@",
                "default_input" => "@DEFAULT_SOURCE@",
                other => other,
            };
            pipeline.push_str(&format!(
                " pulsesrc name=audio_source_{} device={} do-timestamp=true ! queue ! audioconvert ! audioresample \
                 ! audio/x-raw,rate=48000,channels=2 ! opusenc bitrate=256000 audio-type=restricted-lowdelay \
                 ! queue ! mux.audio_{}",
                index,
                gst_quote(Path::new(device)),
                index,
            ));
        }
        let pipeline = gst::parse::launch(&pipeline)
            .map_err(|error| format!("failed to build encoder pipeline: {error}"))?
            .downcast::<gst::Pipeline>()
            .map_err(|_| "encoder pipeline is not a pipeline")?;
        let appsrc = pipeline
            .by_name("video")
            .ok_or("encoder appsrc is missing")?;
        let audio_sources = (0..2)
            .filter_map(|index| pipeline.by_name(&format!("audio_source_{index}")))
            .collect();
        let transport_sink = pipeline
            .by_name("transport_sink")
            .ok_or("hybrid MP4 transport sink is missing")?;
        let mut muxer = Command::new("ffmpeg")
            .args([
                "-nostdin",
                "-hide_banner",
                "-loglevel",
                "warning",
                "-fflags",
                "+genpts",
                "-f",
                "mp4",
                "-i",
                "pipe:0",
                "-map",
                "0:v:0",
                "-map",
                "0:a?",
                "-c",
                "copy",
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
            .arg(&options.output)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .spawn()
            .map_err(|error| format!("failed to start Hybrid MP4 muxer: {error}"))?;
        let muxer_stdin = muxer.stdin.take().ok_or_else(|| {
            let _ = muxer.kill();
            let _ = muxer.wait();
            "Hybrid MP4 muxer input is missing".to_string()
        })?;
        transport_sink.set_property("fd", muxer_stdin.as_raw_fd());
        let info = VideoInfo::builder(VideoFormat::Bgra, width, height)
            .fps(gst::Fraction::new(options.fps as i32, 1))
            .build()
            .map_err(|error| error.to_string())?;
        let caps = VideoInfoDmaDrm::new(info, fourcc, modifier)
            .to_caps()
            .map_err(|error| error.to_string())?;
        appsrc.set_property("caps", caps);
        if let Err(error) = pipeline.set_state(gst::State::Playing) {
            drop(muxer_stdin);
            let _ = muxer.kill();
            let _ = muxer.wait();
            return Err(format!("failed to start encoder: {error:?}"));
        }
        Ok(Self {
            pipeline,
            appsrc,
            audio_sources,
            muxer,
            muxer_stdin: Some(muxer_stdin),
            allocator: DmaBufAllocator::new(),
            presentation_origin_ns: None,
            paused_ns: 0,
            paused_at: None,
            fps: options.fps,
            last_frame_index: None,
        })
    }

    fn push_latest(
        &mut self,
        buffer: &CaptureBuffer,
        slot: usize,
        release_sender: &mpsc::Sender<usize>,
        presentation_ns: u64,
    ) -> Result<PushOutcome, String> {
        let origin_ns = *self.presentation_origin_ns.get_or_insert(presentation_ns);
        let elapsed_ns = presentation_ns
            .saturating_sub(origin_ns)
            .saturating_sub(self.paused_ns);
        let frame_index = frame_index_at(u128::from(elapsed_ns), self.fps);
        if self
            .last_frame_index
            .is_some_and(|previous| frame_index <= previous)
        {
            return Ok(PushOutcome {
                skipped: true,
                ..PushOutcome::default()
            });
        }

        // Smoothie and other frame-indexed consumers require a real CFR stream.
        // Fill short scheduling misses with the newest completed image. Large
        // stalls remain timestamp gaps so a stopped GPU cannot create an
        // unbounded encode backlog.
        // One second is enough to absorb encoder warm-up and ordinary scheduler
        // stalls while still bounding the amount of catch-up work.
        let maximum_duplicates = u64::from(self.fps);
        let previous = self.last_frame_index;
        let (first_index, duplicates, unfilled) =
            cfr_span(previous, frame_index, maximum_duplicates);
        let push_count = duplicates + 1;
        let release = Arc::new(BufferRelease {
            remaining: AtomicUsize::new(push_count as usize),
            sender: release_sender.clone(),
            slot,
        });
        for index in first_index..=frame_index {
            self.push_one(buffer, index, &release)?;
        }
        self.last_frame_index = Some(frame_index);
        Ok(PushOutcome {
            written: push_count,
            duplicated: duplicates,
            unfilled,
            maximum_gap: duplicates.saturating_add(unfilled),
            skipped: false,
        })
    }

    fn push_one(
        &mut self,
        buffer: &CaptureBuffer,
        frame_index: u64,
        release: &Arc<BufferRelease>,
    ) -> Result<(), String> {
        let fd = buffer
            .bo
            .fd()
            .map_err(|_| "failed to export capture DMA-BUF")?;
        let size = usize::try_from(buffer.bo.stride())
            .ok()
            .and_then(|stride| stride.checked_mul(buffer.bo.height() as usize))
            .ok_or("capture DMA-BUF size overflow")?;
        let memory =
            unsafe { self.allocator.alloc_dmabuf(fd, size) }.map_err(|error| error.to_string())?;
        let mut gst_buffer = gst::Buffer::new();
        {
            let writable = gst_buffer
                .get_mut()
                .ok_or("new encoder buffer is not writable")?;
            writable.append_memory(memory);
            VideoMeta::add_full(
                writable,
                gstreamer_video::VideoFrameFlags::empty(),
                VideoFormat::DmaDrm,
                buffer.bo.width(),
                buffer.bo.height(),
                &[buffer.bo.offset(0) as usize],
                &[buffer.bo.stride_for_plane(0) as i32],
            )
            .map_err(|error| error.to_string())?;
            let (pts, next_pts) = frame_times(frame_index, self.fps);
            writable.set_pts(gst::ClockTime::from_nseconds(pts));
            writable.set_duration(gst::ClockTime::from_nseconds(next_pts - pts));
        }
        unsafe {
            gst::ffi::gst_mini_object_weak_ref(
                gst_buffer.make_mut().upcast_mut().as_mut_ptr(),
                Some(encoder_buffer_finalized),
                Arc::into_raw(release.clone()).cast_mut().cast(),
            );
        }
        let result = self
            .appsrc
            .emit_by_name::<gst::FlowReturn>("push-buffer", &[&gst_buffer]);
        if result != gst::FlowReturn::Ok {
            return Err(format!("encoder rejected frame: {result:?}"));
        }
        Ok(())
    }

    fn set_paused(&mut self, paused: bool) -> Result<(), String> {
        if paused {
            if self.paused_at.is_none() {
                self.pipeline
                    .set_state(gst::State::Paused)
                    .map_err(|error| format!("failed to pause encoder: {error:?}"))?;
                self.paused_at = Some(Instant::now());
            }
        } else if let Some(paused_at) = self.paused_at.take() {
            self.paused_ns = self
                .paused_ns
                .saturating_add(paused_at.elapsed().as_nanos().min(u128::from(u64::MAX)) as u64);
            self.pipeline
                .set_state(gst::State::Playing)
                .map_err(|error| format!("failed to resume encoder: {error:?}"))?;
        }
        Ok(())
    }

    fn stop(mut self) -> Result<(), String> {
        let video_eos = self
            .appsrc
            .emit_by_name::<gst::FlowReturn>("end-of-stream", &[]);
        if video_eos != gst::FlowReturn::Ok {
            return Err(format!(
                "video encoder rejected end-of-stream: {video_eos:?}"
            ));
        }
        let audio_eos = self
            .audio_sources
            .iter()
            .map(|source| {
                source
                    .static_pad("src")
                    .ok_or("audio source pad is missing".to_string())
            })
            .collect::<Result<Vec<_>, _>>()?
            .into_iter()
            .map(|pad| thread::spawn(move || pad.push_event(gst::event::Eos::new())))
            .collect::<Vec<_>>();
        for result in audio_eos {
            if !result.join().unwrap_or(false) {
                return Err("audio encoder rejected end-of-stream".into());
            }
        }
        let mut finalized = false;
        if let Some(bus) = self.pipeline.bus() {
            let deadline = Instant::now() + Duration::from_secs(10);
            while Instant::now() < deadline {
                if let Some(message) = bus.timed_pop(gst::ClockTime::from_mseconds(100)) {
                    use gst::MessageView;
                    match message.view() {
                        MessageView::Eos(..) => {
                            finalized = true;
                            break;
                        }
                        MessageView::Error(error) => {
                            return Err(format!("encoder failed: {}", error.error()));
                        }
                        _ => {}
                    }
                }
            }
        }
        if !finalized {
            let _ = self.pipeline.set_state(gst::State::Null);
            return Err("encoder timed out while finalizing the recording index".into());
        }
        self.pipeline
            .set_state(gst::State::Null)
            .map_err(|error| format!("failed to stop encoder: {error:?}"))?;
        drop(self.muxer_stdin.take());
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            match self.muxer.try_wait() {
                Ok(Some(status)) if status.success() => break,
                Ok(Some(status)) => {
                    return Err(format!("Hybrid MP4 muxer exited with {status}"));
                }
                Ok(None) if Instant::now() < deadline => {
                    thread::sleep(Duration::from_millis(20));
                }
                Ok(None) => {
                    let _ = self.muxer.kill();
                    let _ = self.muxer.wait();
                    return Err("Hybrid MP4 muxer timed out while finalizing".into());
                }
                Err(error) => {
                    return Err(format!("failed to inspect Hybrid MP4 muxer: {error}"));
                }
            }
        }
        Ok(())
    }
}

impl Drop for Encoder {
    fn drop(&mut self) {
        let _ = self.pipeline.set_state(gst::State::Null);
        drop(self.muxer_stdin.take());
        if self.muxer.try_wait().ok().flatten().is_none() {
            let _ = self.muxer.kill();
            let _ = self.muxer.wait();
        }
    }
}

fn options_width(options: &Options, source: u32) -> u32 {
    if options.output_width == 0 {
        source
    } else {
        options.output_width
    }
}

fn options_height(options: &Options, source: u32) -> u32 {
    if options.output_height == 0 {
        source
    } else {
        options.output_height
    }
}

fn gst_quote(path: &Path) -> String {
    format!(
        "\"{}\"",
        path.to_string_lossy()
            .replace('\\', "\\\\")
            .replace('\"', "\\\"")
    )
}

fn open_render_node(device: u64) -> Result<File, String> {
    for minor in 128..256 {
        let path = PathBuf::from(format!("/dev/dri/renderD{minor}"));
        let Ok(metadata) = path.metadata() else {
            continue;
        };
        if metadata.rdev() == device {
            return OpenOptions::new()
                .read(true)
                .write(true)
                .open(&path)
                .map_err(|error| format!("failed to open {}: {error}", path.display()));
        }
    }
    Err("capture render node was not found".into())
}

fn select_format(formats: &[(u32, Vec<u64>)]) -> Result<(Format, Modifier), String> {
    for preferred in [Format::Argb8888, Format::Xrgb8888] {
        if let Some((_, modifiers)) = formats
            .iter()
            .find(|(format, _)| *format == preferred as u32)
            && let Some(modifier) = modifiers
                .iter()
                .copied()
                .find(|modifier| *modifier == u64::from(Modifier::Linear))
                .or_else(|| modifiers.first().copied())
        {
            return Ok((preferred, Modifier::from(modifier)));
        }
    }
    Err("compositor did not offer ARGB/XRGB DMA-BUF capture".into())
}

fn create_capture_buffer(
    device: &Device<File>,
    dmabuf: &zwp_linux_dmabuf_v1::ZwpLinuxDmabufV1,
    qh: &QueueHandle<CaptureState>,
    width: u32,
    height: u32,
    format: Format,
    modifier: Modifier,
) -> Result<CaptureBuffer, String> {
    let mut flags = BufferObjectFlags::RENDERING;
    if modifier == Modifier::Linear {
        flags |= BufferObjectFlags::LINEAR;
    }
    let bo = device
        .create_buffer_object_with_modifiers2::<()>(
            width,
            height,
            format,
            std::iter::once(modifier),
            flags,
        )
        .map_err(|error| format!("failed to allocate capture DMA-BUF: {error}"))?;
    let params = dmabuf.create_params(qh, ());
    let raw_modifier = u64::from(bo.modifier());
    for plane in 0..bo.plane_count() {
        let fd = bo
            .fd_for_plane(plane as i32)
            .map_err(|_| "failed to export DMA-BUF plane")?;
        params.add(
            fd.as_fd(),
            plane,
            bo.offset(plane as i32),
            bo.stride_for_plane(plane as i32),
            (raw_modifier >> 32) as u32,
            raw_modifier as u32,
        );
    }
    let wl_buffer = params.create_immed(
        width as i32,
        height as i32,
        format as u32,
        zwp_linux_buffer_params_v1::Flags::empty(),
        qh,
        (),
    );
    params.destroy();
    Ok(CaptureBuffer { bo, wl_buffer })
}

fn main() {
    #[cfg(target_os = "linux")]
    unsafe {
        // The recorder is a compositor-owned worker. Never survive a compositor
        // crash, forced logout, or session restart as an orphaned GPU process.
        libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL);
    }
    if std::env::var_os("GST_GL_PLATFORM").is_none() {
        unsafe { std::env::set_var("GST_GL_PLATFORM", "egl") };
    }
    if std::env::var_os("GST_GL_WINDOW").is_none() {
        unsafe { std::env::set_var("GST_GL_WINDOW", "surfaceless") };
    }
    if let Err(error) = run() {
        eprintln!("luma-recorder-engine: {error}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), String> {
    let options = Options::parse()?;
    let connection = Connection::connect_to_env().map_err(|error| error.to_string())?;
    let (globals, mut queue) =
        registry_queue_init::<CaptureState>(&connection).map_err(|error| error.to_string())?;
    let qh = queue.handle();
    let output_globals: Vec<_> = globals
        .contents()
        .clone_list()
        .into_iter()
        .filter(|global| global.interface == wl_output::WlOutput::interface().name)
        .collect();
    if output_globals.is_empty() {
        return Err("compositor did not advertise an output".into());
    }
    let mut state = CaptureState {
        output_names: vec![None; output_globals.len()],
        ..CaptureState::default()
    };
    let outputs: Vec<_> = output_globals
        .iter()
        .enumerate()
        .map(|(index, global)| {
            globals.registry().bind::<wl_output::WlOutput, _, _>(
                global.name,
                global.version.min(4),
                &qh,
                index,
            )
        })
        .collect();
    queue
        .roundtrip(&mut state)
        .map_err(|error| error.to_string())?;
    let output_index = if let Some(requested) = options.output_name.as_deref() {
        state
            .output_names
            .iter()
            .position(|name| name.as_deref() == Some(requested))
            .ok_or_else(|| format!("capture output {requested:?} is unavailable"))?
    } else {
        0
    };
    let output = outputs[output_index].clone();
    let sources = globals
        .bind::<ext_output_image_capture_source_manager_v1::ExtOutputImageCaptureSourceManagerV1, _, _>(
            &qh,
            1..=1,
            (),
        )
        .map_err(|error| error.to_string())?;
    let manager = globals
        .bind::<ext_image_copy_capture_manager_v1::ExtImageCopyCaptureManagerV1, _, _>(
            &qh,
            1..=1,
            (),
        )
        .map_err(|error| error.to_string())?;
    let linux_dmabuf = globals
        .bind::<zwp_linux_dmabuf_v1::ZwpLinuxDmabufV1, _, _>(&qh, 4..=5, ())
        .map_err(|error| error.to_string())?;
    let source = sources.create_source(&output, &qh, ());
    let session = manager.create_session(
        &source,
        ext_image_copy_capture_manager_v1::Options::empty(),
        &qh,
        (),
    );
    while !state.constraints_done && !state.stopped {
        queue
            .blocking_dispatch(&mut state)
            .map_err(|error| error.to_string())?;
    }
    if state.stopped || state.width == 0 || state.height == 0 || state.formats.is_empty() {
        return Err("compositor did not offer usable DMA-BUF capture constraints".into());
    }

    let render_node = open_render_node(
        state
            .drm_device
            .ok_or("compositor omitted DMA-BUF device")?,
    )?;
    let gbm =
        Device::new(render_node).map_err(|error| format!("failed to open GBM device: {error}"))?;
    let (format, modifier) = select_format(&state.formats)?;
    let (command_tx, command_rx) = mpsc::channel();
    thread::spawn(move || {
        for line in std::io::stdin().lock().lines().map_while(Result::ok) {
            let _ = command_tx.send(line);
        }
        let _ = command_tx.send("stop".into());
    });
    const CAPTURE_BUFFER_POOL_SIZE: usize = 8;
    let capture_buffers = (0..CAPTURE_BUFFER_POOL_SIZE)
        .map(|_| {
            create_capture_buffer(
                &gbm,
                &linux_dmabuf,
                &qh,
                state.width,
                state.height,
                format,
                modifier,
            )
        })
        .collect::<Result<Vec<_>, _>>()?;
    // Allocate every capture target before starting audio or the recording
    // clock. GBM allocation can otherwise create a visible startup timestamp
    // hole in an otherwise healthy high-rate recording.
    let mut encoder = Encoder::new(
        &options,
        format as u32,
        u64::from(modifier),
        state.width,
        state.height,
    )?;
    let (release_sender, release_receiver) = mpsc::channel();
    let mut available_slots: VecDeque<_> = (0..CAPTURE_BUFFER_POOL_SIZE).collect();
    let interval = Duration::from_secs_f64(1.0 / f64::from(options.fps));
    let mut next_frame = Instant::now();
    let mut captured_frames = 0u64;
    let mut encoded_frames = 0u64;
    let mut duplicated_frames = 0u64;
    let mut unfilled_frames = 0u64;
    let mut skipped_captures = 0u64;
    let mut maximum_gap = 0u64;
    let mut paused = false;
    let mut commit_pacing_origin = None;
    'recording: loop {
        let (stop, toggle_pause) = drain_commands(&command_rx);
        if stop {
            break;
        }
        if toggle_pause {
            paused = !paused;
            encoder.set_paused(paused)?;
            next_frame = Instant::now();
        }
        if paused {
            match command_rx.recv_timeout(Duration::from_millis(100)) {
                Ok(command) if command.trim() == "stop" => break,
                Ok(command) if command.trim() == "pause" => {
                    paused = false;
                    encoder.set_paused(false)?;
                    next_frame = Instant::now();
                }
                Ok(command) => eprintln!("ignored recorder command: {}", command.trim()),
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
                Err(mpsc::RecvTimeoutError::Timeout) => {}
            }
            continue;
        }
        available_slots.extend(release_receiver.try_iter());
        let Some(slot) = available_slots.pop_front() else {
            thread::sleep(Duration::from_micros(250));
            continue;
        };
        // Limit capture requests to the configured rate while allowing either
        // the next real output repaint or the capture boost to satisfy them.
        // This preserves useful game commits without submitting hundreds of
        // redundant requests above the requested FPS.
        let now = Instant::now();
        if now < next_frame {
            thread::sleep(next_frame - now);
        }
        next_frame += interval;
        if Instant::now().saturating_duration_since(next_frame) > interval.saturating_mul(4) {
            next_frame = Instant::now();
        }
        let capture_buffer = &capture_buffers[slot];
        state.frame_ready = false;
        state.frame_failed = None;
        state.presentation_ns = None;
        state.ready_slot = None;
        let frame = session.create_frame(&qh, 0);
        frame.attach_buffer(&capture_buffer.wl_buffer);
        frame.damage_buffer(0, 0, state.width as i32, state.height as i32);
        frame.capture();
        while !state.frame_ready && state.frame_failed.is_none() && !state.stopped {
            queue
                .blocking_dispatch(&mut state)
                .map_err(|error| error.to_string())?;
            let (stop, toggle_pause) = drain_commands(&command_rx);
            if toggle_pause {
                paused = !paused;
            }
            if stop {
                frame.destroy();
                break 'recording;
            }
        }
        frame.destroy();
        if state.stopped {
            return Err("capture session was stopped by the compositor".into());
        }
        if let Some(reason) = state.frame_failed.take() {
            return Err(format!("capture frame failed: {reason}"));
        }
        let source_presentation_ns = state
            .presentation_ns
            .ok_or("capture frame omitted its presentation timestamp")?;
        // A commit-paced source has already admitted exactly one fresh surface
        // for this request. Number those admitted frames consecutively so
        // sub-millisecond scheduler jitter cannot turn a real frame into a
        // skip followed by a synthetic duplicate. This intentionally favors a
        // fully real CFR stream for frame-indexed interpolation tools.
        let presentation_ns = if options.commit_paced {
            let origin = *commit_pacing_origin.get_or_insert(source_presentation_ns);
            origin
                .saturating_add(encoder.paused_ns)
                .saturating_add(sequence_elapsed_ns(captured_frames, options.fps))
        } else {
            source_presentation_ns
        };
        let outcome =
            encoder.push_latest(capture_buffer, slot, &release_sender, presentation_ns)?;
        if outcome.written == 0 {
            available_slots.push_back(slot);
        }
        captured_frames = captured_frames.saturating_add(1);
        encoded_frames = encoded_frames.saturating_add(outcome.written);
        duplicated_frames = duplicated_frames.saturating_add(outcome.duplicated);
        unfilled_frames = unfilled_frames.saturating_add(outcome.unfilled);
        skipped_captures = skipped_captures.saturating_add(u64::from(outcome.skipped));
        maximum_gap = maximum_gap.max(outcome.maximum_gap);
        if paused {
            encoder.set_paused(true)?;
        }
    }
    encoder.stop()?;
    for buffer in capture_buffers {
        buffer.wl_buffer.destroy();
    }
    session.destroy();
    source.destroy();
    eprintln!(
        "direct capture received {captured_frames} frames and wrote {encoded_frames} frames (duplicated {duplicated_frames}, skipped {skipped_captures}, unfilled {unfilled_frames}, maximum gap {maximum_gap}) at {}x{} and up to {} fps to {} ({})",
        state.width,
        state.height,
        options.fps,
        options.output.display(),
        options.codec
    );
    Ok(())
}

fn drain_commands(receiver: &mpsc::Receiver<String>) -> (bool, bool) {
    let mut stop = false;
    let mut toggle_pause = false;
    for command in receiver.try_iter() {
        match command.trim() {
            "stop" => stop = true,
            "pause" => toggle_pause = !toggle_pause,
            "" => {}
            other => eprintln!("ignored recorder command: {other}"),
        }
    }
    (stop, toggle_pause)
}

fn frame_times(frame: u64, fps: u32) -> (u64, u64) {
    let denominator = u64::from(fps);
    let pts = frame.saturating_mul(1_000_000_000) / denominator;
    let next = frame.saturating_add(1).saturating_mul(1_000_000_000) / denominator;
    (pts, next)
}

fn frame_index_at(elapsed_ns: u128, fps: u32) -> u64 {
    let index = elapsed_ns.saturating_mul(u128::from(fps)) / 1_000_000_000;
    u64::try_from(index).unwrap_or(u64::MAX)
}

fn sequence_elapsed_ns(frame: u64, fps: u32) -> u64 {
    let numerator = u128::from(frame).saturating_mul(1_000_000_000);
    let denominator = u128::from(fps);
    let elapsed = numerator.div_ceil(denominator);
    u64::try_from(elapsed).unwrap_or(u64::MAX)
}

fn cfr_span(previous: Option<u64>, target: u64, maximum_duplicates: u64) -> (u64, u64, u64) {
    let missing = previous
        .map(|previous| target.saturating_sub(previous).saturating_sub(1))
        .unwrap_or(0);
    let duplicates = missing.min(maximum_duplicates);
    (
        target.saturating_sub(duplicates),
        duplicates,
        missing.saturating_sub(duplicates),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn constant_rate_timestamps_are_monotonic_and_land_on_one_second() {
        for fps in [30, 60, 120, 240, 480] {
            let mut previous = 0;
            for frame in 0..u64::from(fps) {
                let (pts, next) = frame_times(frame, fps);
                assert!(next > pts);
                assert!(frame == 0 || pts > previous);
                previous = pts;
            }
            assert_eq!(frame_times(u64::from(fps), fps).0, 1_000_000_000);
        }
    }

    #[test]
    fn elapsed_time_selects_the_latest_slot_without_a_catch_up_count() {
        assert_eq!(frame_index_at(0, 480), 0);
        assert_eq!(frame_index_at(2_083_333, 480), 0);
        assert_eq!(frame_index_at(2_083_334, 480), 1);
        assert_eq!(frame_index_at(1_000_000_000, 480), 480);
        assert_eq!(
            frame_times(frame_index_at(10_000_000_000, 480), 480).0,
            10_000_000_000
        );
    }

    #[test]
    fn commit_sequence_maps_every_real_frame_to_one_cfr_slot() {
        for fps in [30, 60, 120, 240, 480] {
            for frame in 0..u64::from(fps) * 10 {
                assert_eq!(
                    frame_index_at(u128::from(sequence_elapsed_ns(frame, fps)), fps),
                    frame
                );
            }
        }
    }

    #[test]
    fn cfr_span_fills_short_gaps_and_bounds_long_stalls() {
        assert_eq!(cfr_span(None, 0, 480), (0, 0, 0));
        assert_eq!(cfr_span(Some(9), 10, 480), (10, 0, 0));
        assert_eq!(cfr_span(Some(9), 13, 480), (10, 3, 0));
        assert_eq!(cfr_span(Some(9), 600, 480), (120, 480, 110));
    }
}
