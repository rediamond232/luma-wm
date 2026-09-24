// Allow in this module because of existing usage
#![allow(clippy::uninlined_format_args)]
use std::{
    collections::hash_map::HashMap,
    io,
    ops::Not,
    path::Path,
    sync::{Mutex, Once, atomic::Ordering},
    time::{Duration, Instant},
};

use crate::{
    drawing::*,
    render::*,
    shell::WindowElement,
    state::{AnvilState, Backend, take_presentation_feedback, update_primary_scanout_output},
};
use crate::{
    shell::WindowRenderElement,
    state::{DndIcon, SurfaceDmabufFeedback},
};
use smithay::backend::drm::compositor::PrimaryPlaneElement;
#[cfg(feature = "egl")]
use smithay::backend::renderer::ImportEgl;
#[cfg(feature = "debug")]
use smithay::backend::renderer::{ImportMem, multigpu::MultiTexture};
use smithay::{
    backend::{
        SwapBuffersError,
        allocator::{
            Fourcc, Modifier,
            dmabuf::Dmabuf,
            format::FormatSet,
            gbm::{GbmAllocator, GbmBufferFlags, GbmDevice},
        },
        drm::{
            CreateDrmNodeError, DrmAccessError, DrmDevice, DrmDeviceFd, DrmError, DrmEvent,
            DrmEventMetadata, DrmEventTime, DrmNode, DrmSurface, GbmBufferedSurface, NodeType,
            compositor::{DrmCompositor, FrameFlags},
            exporter::gbm::GbmFramebufferExporter,
            output::{DrmOutput, DrmOutputManager, DrmOutputRenderElements},
        },
        egl::{self, EGLContext, EGLDevice, EGLDisplay, context::ContextPriority},
        input::InputEvent,
        libinput::{LibinputInputBackend, LibinputSessionInterface},
        renderer::{
            DebugFlags, ImportDma, ImportMemWl,
            damage::Error as OutputDamageTrackerError,
            element::{AsRenderElements, RenderElementStates, memory::MemoryRenderBuffer},
            gles::{Capability, GlesRenderer},
            multigpu::{GpuManager, MultiRenderer, gbm::GbmGlesBackend},
        },
        session::{
            Event as SessionEvent, Session,
            libseat::{self, LibSeatSession},
        },
        udev::{UdevBackend, UdevEvent, all_gpus, primary_gpu},
    },
    desktop::{
        space::{Space, SurfaceTree},
        utils::OutputPresentationFeedback,
    },
    input::{
        keyboard::LedState,
        pointer::{CursorImageAttributes, CursorImageStatus},
    },
    output::{Mode as WlMode, Output, PhysicalProperties},
    reexports::{
        calloop::{
            EventLoop, RegistrationToken,
            timer::{TimeoutAction, Timer},
        },
        drm::{
            Device as _,
            control::{
                AtomicCommitFlags, Device, Mode as DrmMode, ModeTypeFlags, atomic, connector, crtc,
                property,
            },
        },
        input::{DeviceCapability, Libinput},
        rustix::fs::OFlags,
        wayland_protocols::wp::{
            linux_dmabuf::zv1::server::zwp_linux_dmabuf_feedback_v1,
            presentation_time::server::wp_presentation_feedback,
        },
        wayland_server::{Display, DisplayHandle, backend::GlobalId, protocol::wl_surface},
    },
    utils::{DeviceFd, IsAlive, Logical, Monotonic, Point, Scale, Time, Transform},
    wayland::{
        compositor,
        dmabuf::{DmabufFeedbackBuilder, DmabufGlobal, DmabufHandler, DmabufState, ImportNotifier},
        drm_lease::{
            DrmLease, DrmLeaseBuilder, DrmLeaseHandler, DrmLeaseRequest, DrmLeaseState,
            LeaseRejected,
        },
        drm_syncobj::{DrmSyncobjHandler, DrmSyncobjState, supports_syncobj_eventfd},
        presentation::Refresh,
    },
};
use smithay_drm_extras::{
    display_info,
    drm_scanner::{DrmScanEvent, DrmScanner},
};
use tracing::{debug, error, info, trace, warn};

// we cannot simply pick the first supported format of the intersection of *all* formats, because:
// - we do not want something like Abgr4444, which looses color information, if something better is available
// - some formats might perform terribly
// - we might need some work-arounds, if one supports modifiers, but the other does not
//
// So lets just pick `ARGB2101010` (10-bit) or `ARGB8888` (8-bit) for now, they are widely supported.
const SUPPORTED_FORMATS: &[Fourcc] = &[
    Fourcc::Abgr2101010,
    Fourcc::Argb2101010,
    Fourcc::Abgr8888,
    Fourcc::Argb8888,
];
const SUPPORTED_FORMATS_8BIT_ONLY: &[Fourcc] = &[Fourcc::Abgr8888, Fourcc::Argb8888];

pub(crate) type UdevRenderer<'a> = MultiRenderer<
    'a,
    'a,
    GbmGlesBackend<GlesRenderer, DrmDeviceFd>,
    GbmGlesBackend<GlesRenderer, DrmDeviceFd>,
>;

#[derive(Debug, PartialEq)]
struct UdevOutputId {
    device_id: DrmNode,
    crtc: crtc::Handle,
}

pub struct UdevData {
    pub session: LibSeatSession,
    dh: DisplayHandle,
    dmabuf_state: Option<(DmabufState, DmabufGlobal)>,
    syncobj_state: Option<DrmSyncobjState>,
    primary_gpu: DrmNode,
    gpus: GpuManager<GbmGlesBackend<GlesRenderer, DrmDeviceFd>>,
    backends: HashMap<DrmNode, BackendData>,
    pointer_images: Vec<(xcursor::parser::Image, i32, MemoryRenderBuffer)>,
    pointer_element: PointerElement,
    #[cfg(feature = "debug")]
    fps_texture: Option<MultiTexture>,
    pointer_image: crate::cursor::Cursor,
    cursor_timer: Option<(smithay::reexports::calloop::RegistrationToken, Instant)>,
    debug_flags: DebugFlags,
    keyboards: Vec<smithay::reexports::input::Device>,
    input_devices: Vec<smithay::reexports::input::Device>,
    last_input_at: Option<Instant>,
    input_generation: u64,
}

const PERFORMANCE_SAMPLE_COUNT: usize = 256;

#[derive(Debug)]
struct DurationSamples {
    values: [u32; PERFORMANCE_SAMPLE_COUNT],
    len: usize,
    next: usize,
}

impl Default for DurationSamples {
    fn default() -> Self {
        Self {
            values: [0; PERFORMANCE_SAMPLE_COUNT],
            len: 0,
            next: 0,
        }
    }
}

impl DurationSamples {
    fn record(&mut self, duration: Duration) {
        self.values[self.next] = duration.as_micros().min(u128::from(u32::MAX)) as u32;
        self.next = (self.next + 1) % PERFORMANCE_SAMPLE_COUNT;
        self.len = (self.len + 1).min(PERFORMANCE_SAMPLE_COUNT);
    }

    fn percentile(&self, percentile: usize) -> u32 {
        if self.len == 0 {
            return 0;
        }
        let mut values = self.values[..self.len].to_vec();
        values.sort_unstable();
        let index = (values.len() - 1) * percentile / 100;
        values[index]
    }
}

#[derive(Debug, Default)]
struct OutputPerformanceMetrics {
    direct_scanout_frames: u64,
    composed_frames: u64,
    empty_frames: u64,
    missed_deadlines: u64,
    render: DurationSamples,
    input_to_submit: DurationSamples,
}

impl OutputPerformanceMetrics {
    fn clear(&mut self) {
        *self = Self::default();
    }

    fn repaint_budget(&self, frame_duration: Duration) -> Duration {
        // Keep enough room for the measured slow tail plus a small driver/KMS
        // margin. The clamps prevent both zero-delay busy rendering and an
        // over-optimistic deadline at very high refresh rates.
        if self.render.len < 8 {
            return frame_duration * 2 / 5;
        }
        let measured = Duration::from_micros(u64::from(self.render.percentile(95)));
        let safety = Duration::from_micros(250);
        (measured + safety).clamp(frame_duration / 5, frame_duration * 4 / 5)
    }
}

#[cfg(test)]
mod performance_metric_tests {
    use super::*;

    #[test]
    fn duration_samples_keep_recent_percentiles() {
        let mut samples = DurationSamples::default();
        for value in 1..=100 {
            samples.record(Duration::from_micros(value));
        }
        assert_eq!(samples.percentile(50), 50);
        assert_eq!(samples.percentile(95), 95);
        assert_eq!(samples.percentile(99), 99);
    }

    #[test]
    fn repaint_budget_starts_safe_and_adapts_to_slow_tail() {
        let frame = Duration::from_micros(2_778);
        let mut metrics = OutputPerformanceMetrics::default();
        assert_eq!(metrics.repaint_budget(frame), frame * 2 / 5);
        for _ in 0..32 {
            metrics.render.record(Duration::from_micros(800));
        }
        assert_eq!(metrics.repaint_budget(frame), Duration::from_micros(1_050));

        for _ in 0..256 {
            metrics.render.record(Duration::from_micros(10_000));
        }
        assert_eq!(metrics.repaint_budget(frame), frame * 4 / 5);
    }
}

fn configure_input_device(device: &mut smithay::reexports::input::Device, config: &wm_core::Input) {
    if device.config_accel_is_available() {
        if let Err(error) = device.config_accel_set_speed(config.pointer_accel) {
            warn!(
                device = %device.name(),
                ?error,
                "Could not set pointer acceleration"
            );
        }
    }
    if device.config_scroll_has_natural_scroll() {
        if let Err(error) = device.config_scroll_set_natural_scroll_enabled(config.natural_scroll) {
            warn!(
                device = %device.name(),
                ?error,
                "Could not set natural scrolling"
            );
        }
    }
    if device.config_tap_finger_count() > 0 {
        if let Err(error) = device.config_tap_set_enabled(config.tap_to_click) {
            warn!(device = %device.name(), ?error, "Could not set tap to click");
        }
    }
}

impl UdevData {
    pub fn set_debug_flags(&mut self, flags: DebugFlags) {
        if self.debug_flags != flags {
            self.debug_flags = flags;

            for backend in self.backends.values_mut() {
                for surface in backend.surfaces.values_mut() {
                    surface.drm_output.set_debug_flags(flags);
                }
            }
        }
    }

    pub fn debug_flags(&self) -> DebugFlags {
        self.debug_flags
    }
}

impl DmabufHandler for AnvilState<UdevData> {
    fn dmabuf_state(&mut self) -> &mut DmabufState {
        &mut self.backend_data.dmabuf_state.as_mut().unwrap().0
    }

    fn dmabuf_imported(
        &mut self,
        _global: &DmabufGlobal,
        dmabuf: Dmabuf,
        notifier: ImportNotifier,
    ) {
        if self
            .backend_data
            .gpus
            .single_renderer(&self.backend_data.primary_gpu)
            .and_then(|mut renderer| renderer.import_dmabuf(&dmabuf, None))
            .is_ok()
        {
            if dmabuf.node().is_none() {
                dmabuf.set_node(self.backend_data.primary_gpu);
            }
            let _ = notifier.successful::<AnvilState<UdevData>>();
        } else {
            notifier.failed();
        }
    }
}

impl Backend for UdevData {
    fn apply_performance_policy(
        &mut self,
        performance: &wm_core::Performance,
        gaming_outputs: &[String],
        outputs: &std::collections::BTreeMap<String, wm_core::OutputConfig>,
    ) -> Vec<String> {
        let mut errors = Vec::new();
        for backend in self.backends.values_mut() {
            for surface in backend.surfaces.values_mut() {
                let configured = outputs
                    .get(&surface.output.name())
                    .is_some_and(|output| output.vrr);
                let automatic =
                    performance.fullscreen_vrr && gaming_outputs.contains(&surface.output.name());
                if let Err(error) = surface.configure_vrr(configured || automatic) {
                    errors.push(error);
                }
            }
        }
        errors
    }

    fn performance_status(&self) -> Vec<wm_core::OutputPerformanceStatus> {
        self.backends
            .values()
            .flat_map(|backend| backend.surfaces.values())
            .map(|surface| wm_core::OutputPerformanceStatus {
                name: surface.output.name(),
                refresh_millihz: surface
                    .output
                    .current_mode()
                    .map(|mode| mode.refresh)
                    .unwrap_or_default(),
                vrr_active: surface
                    .drm_output
                    .with_compositor(|compositor| compositor.vrr_enabled()),
                direct_scanout_frames: surface.performance.direct_scanout_frames,
                composed_frames: surface.performance.composed_frames,
                empty_frames: surface.performance.empty_frames,
                missed_deadlines: surface.performance.missed_deadlines,
                render_us_p50: surface.performance.render.percentile(50),
                render_us_p95: surface.performance.render.percentile(95),
                render_us_p99: surface.performance.render.percentile(99),
                input_to_submit_us_p50: surface.performance.input_to_submit.percentile(50),
                input_to_submit_us_p95: surface.performance.input_to_submit.percentile(95),
                input_to_submit_us_p99: surface.performance.input_to_submit.percentile(99),
            })
            .collect()
    }

    fn reset_performance_metrics(&mut self) {
        for backend in self.backends.values_mut() {
            for surface in backend.surfaces.values_mut() {
                surface.performance.clear();
            }
        }
    }

    fn snapshot_window(
        &mut self,
        window: &crate::shell::WindowElement,
        output: &Output,
        fullscreen: bool,
    ) -> Result<
        (
            smithay::backend::renderer::gles::GlesTexture,
            smithay::utils::Rectangle<i32, smithay::utils::Logical>,
        ),
        String,
    > {
        // MultiRenderer draws GLES elements on the primary/render GPU before
        // copying to an output GPU. Retained textures must use that context too.
        let mut renderer = self
            .gpus
            .single_renderer(&self.primary_gpu)
            .map_err(|e| e.to_string())?;
        crate::capture::render_window_texture(renderer.as_mut(), window, output, fullscreen)
    }

    fn capture_output(
        &mut self,
        space: &Space<crate::shell::WindowElement>,
        output: &Output,
        cursor: Option<&crate::capture::CaptureCursor>,
    ) -> Result<Vec<u8>, String> {
        let id = output
            .user_data()
            .get::<UdevOutputId>()
            .ok_or("output has no DRM device")?;
        let node = self
            .backends
            .get(&id.device_id)
            .ok_or("output device removed")?
            .render_node
            .unwrap_or(self.primary_gpu);
        let mut renderer = self
            .gpus
            .single_renderer(&node)
            .map_err(|e| e.to_string())?;
        crate::capture::render(renderer.as_mut(), space, output, cursor)
    }
    const SUPPORTS_SESSION_LOCK: bool = true;
    const HAS_RELATIVE_MOTION: bool = true;
    const HAS_GESTURES: bool = true;

    fn apply_device_config(&mut self, config: &wm_core::Input) {
        for device in &mut self.input_devices {
            configure_input_device(device, config);
        }
    }

    fn capture_dmabuf_constraints(
        &mut self,
        _output: &Output,
    ) -> Option<smithay::wayland::image_copy_capture::DmabufConstraints> {
        let node = self.primary_gpu;
        let renderer = self.gpus.single_renderer(&node).ok()?;
        crate::capture::dmabuf_constraints(renderer.as_ref(), node)
    }

    fn capture_output_dmabuf(
        &mut self,
        space: &Space<crate::shell::WindowElement>,
        output: &Output,
        cursor: Option<&crate::capture::CaptureCursor>,
        mut dmabuf: Dmabuf,
    ) -> Result<(), String> {
        let node = self.primary_gpu;
        let mut renderer = self
            .gpus
            .single_renderer(&node)
            .map_err(|error| error.to_string())?;
        crate::capture::render_dmabuf(renderer.as_mut(), space, output, cursor, &mut dmabuf)
    }

    fn capture_window_dmabuf(
        &mut self,
        window: &crate::shell::WindowElement,
        output: &Output,
        mut dmabuf: Dmabuf,
    ) -> Result<(), String> {
        let node = self.primary_gpu;
        let mut renderer = self
            .gpus
            .single_renderer(&node)
            .map_err(|error| error.to_string())?;
        crate::capture::render_window_dmabuf(renderer.as_mut(), window, output, &mut dmabuf)
    }

    fn capture_region_dmabuf(
        &mut self,
        space: &Space<crate::shell::WindowElement>,
        output: &Output,
        cursor: Option<&crate::capture::CaptureCursor>,
        region: wm_core::Rect,
        mut dmabuf: Dmabuf,
    ) -> Result<(), String> {
        let node = self.primary_gpu;
        let mut renderer = self
            .gpus
            .single_renderer(&node)
            .map_err(|error| error.to_string())?;
        crate::capture::render_region_dmabuf(
            renderer.as_mut(),
            space,
            output,
            cursor,
            region,
            &mut dmabuf,
        )
    }

    fn seat_name(&self) -> String {
        self.session.seat()
    }

    fn apply_output_config(
        &mut self,
        config: &std::collections::BTreeMap<String, wm_core::OutputConfig>,
    ) -> Vec<String> {
        let mut errors = Vec::new();
        let primary_gpu = self.primary_gpu;
        let gpus = &mut self.gpus;
        for device in self.backends.values_mut() {
            for (crtc, surface) in device.surfaces.iter_mut() {
                let requested = config
                    .get(&surface.output.name())
                    .cloned()
                    .unwrap_or_default();
                let modes = advertised_modes(&surface.modes);
                match wm_core::select_output_mode(&modes, &requested) {
                    Some(index) => {
                        let mode = surface.modes[index];
                        let wl_mode = WlMode::from(mode);
                        if surface.output.current_mode() != Some(wl_mode) {
                            let render_node = device.render_node.unwrap_or(primary_gpu);
                            let result = gpus
                                .single_renderer(&render_node)
                                .map_err(|error| error.to_string())
                                .and_then(|mut renderer| {
                                    device
                                        .drm_output_manager
                                        .lock()
                                        .use_mode::<_, WindowRenderElement<GlesRenderer>>(
                                            crtc,
                                            mode,
                                            renderer.as_mut(),
                                            &DrmOutputRenderElements::default(),
                                        )
                                        .map_err(|error| error.to_string())
                                })
                                .map_err(|error| {
                                    format!(
                                        "{}: mode change failed: {error}",
                                        surface.output.name()
                                    )
                                });
                            match result {
                                Ok(()) => {
                                    surface.output.change_current_state(
                                        Some(wl_mode),
                                        None,
                                        None,
                                        None,
                                    );
                                    surface.drm_output.reset_buffers();
                                    surface.last_presentation_time = None;
                                    surface.refresh_advertised_modes();
                                    info!(output = %surface.output.name(), ?wl_mode, "Output mode changed");
                                }
                                Err(error) => {
                                    warn!("{error}");
                                    errors.push(error);
                                }
                            }
                        }
                    }
                    None => {
                        let error = format!(
                            "{}: requested {}x{} at {} mHz is not advertised; keeping current mode",
                            surface.output.name(),
                            requested.width,
                            requested.height,
                            wm_core::output_refresh_millihz(&requested)
                        );
                        warn!("{error}");
                        errors.push(error);
                    }
                }
                if let Err(error) = surface.configure_vrr(requested.vrr) {
                    warn!("{error}");
                    errors.push(error);
                }
                if requested.hdr != surface.hdr_enabled {
                    match configure_hdr(
                        device.drm_output_manager.device(),
                        surface.connector,
                        *crtc,
                        requested.hdr,
                    ) {
                        Ok(()) => {
                            surface.hdr_enabled = requested.hdr;
                            surface.drm_output.reset_buffers();
                            info!(
                                output = %surface.output.name(),
                                enabled = requested.hdr,
                                "HDR10 output state applied"
                            );
                        }
                        Err(error) => {
                            let error = format!("{}: {error}", surface.output.name());
                            warn!("{error}");
                            errors.push(error);
                        }
                    }
                }
            }
        }
        errors
    }

    fn reset_buffers(&mut self, output: &Output) {
        if let Some(id) = output.user_data().get::<UdevOutputId>() {
            if let Some(gpu) = self.backends.get_mut(&id.device_id) {
                if let Some(surface) = gpu.surfaces.get_mut(&id.crtc) {
                    surface.drm_output.reset_buffers();
                }
            }
        }
    }

    fn early_import(&mut self, surface: &wl_surface::WlSurface) {
        if let Err(err) = self.gpus.early_import(self.primary_gpu, surface) {
            warn!("Early buffer import failed: {}", err);
        }
    }

    fn update_led_state(&mut self, led_state: LedState) {
        for keyboard in self.keyboards.iter_mut() {
            keyboard.led_update(led_state.into());
        }
    }
}

pub fn run_udev() {
    let mut event_loop = EventLoop::try_new().unwrap();
    let display = Display::new().unwrap();
    let mut display_handle = display.handle();

    /*
     * Initialize session
     */
    let (session, notifier) = match LibSeatSession::new() {
        Ok(ret) => ret,
        Err(err) => {
            error!("Could not initialize a session: {}", err);
            return;
        }
    };

    /*
     * Initialize the compositor
     */
    let primary_gpu = if let Ok(var) = std::env::var("ANVIL_DRM_DEVICE") {
        DrmNode::from_path(var).expect("Invalid drm device path")
    } else {
        primary_gpu(session.seat())
            .unwrap()
            .and_then(|x| {
                DrmNode::from_path(x)
                    .ok()?
                    .node_with_type(NodeType::Render)?
                    .ok()
            })
            .unwrap_or_else(|| {
                all_gpus(session.seat())
                    .unwrap()
                    .into_iter()
                    .find_map(|x| DrmNode::from_path(x).ok())
                    .expect("No GPU!")
            })
    };
    info!("Using {} as primary gpu.", primary_gpu);

    let gpus = GpuManager::new(GbmGlesBackend::with_factory(|display| {
        let context = EGLContext::new_with_priority(display, ContextPriority::High)?;
        let mut capabilities = unsafe { GlesRenderer::supported_capabilities(&context)? };
        if std::env::var("ANVIL_GLES_DISABLE_INSTANCING").is_ok() {
            capabilities.retain(|capability| *capability != Capability::Instancing);
        }
        Ok(unsafe { GlesRenderer::with_capabilities(context, capabilities)? })
    }))
    .unwrap();

    let data = UdevData {
        dh: display_handle.clone(),
        dmabuf_state: None,
        syncobj_state: None,
        session,
        primary_gpu,
        gpus,
        backends: HashMap::new(),
        pointer_image: crate::cursor::Cursor::load(),
        cursor_timer: None,
        pointer_images: Vec::new(),
        pointer_element: PointerElement::default(),
        #[cfg(feature = "debug")]
        fps_texture: None,
        debug_flags: DebugFlags::empty(),
        keyboards: Vec::new(),
        input_devices: Vec::new(),
        last_input_at: None,
        input_generation: 0,
    };
    let mut state = AnvilState::init(display, event_loop.handle(), data, true);

    /*
     * Initialize the udev backend
     */
    let udev_backend = match UdevBackend::new(&state.seat_name) {
        Ok(ret) => ret,
        Err(err) => {
            error!(error = ?err, "Failed to initialize udev backend");
            return;
        }
    };

    /*
     * Initialize libinput backend
     */
    let mut libinput_context = Libinput::new_with_udev::<LibinputSessionInterface<LibSeatSession>>(
        state.backend_data.session.clone().into(),
    );
    libinput_context.udev_assign_seat(&state.seat_name).unwrap();
    let libinput_backend = LibinputInputBackend::new(libinput_context.clone());

    /*
     * Bind all our objects that get driven by the event loop
     */
    event_loop
        .handle()
        .insert_source(libinput_backend, move |mut event, _, data| {
            let dh = data.backend_data.dh.clone();
            let pointer_before = data.pointer.current_location();
            if !matches!(
                &event,
                InputEvent::DeviceAdded { .. } | InputEvent::DeviceRemoved { .. }
            ) {
                data.backend_data.last_input_at = Some(Instant::now());
                data.backend_data.input_generation =
                    data.backend_data.input_generation.wrapping_add(1).max(1);
            }
            if let InputEvent::DeviceAdded { device } = &mut event {
                configure_input_device(device, &data.desktop.config.input);
                data.backend_data.input_devices.push(device.clone());
                if device.has_capability(DeviceCapability::Keyboard) {
                    if let Some(led_state) = data
                        .seat
                        .get_keyboard()
                        .map(|keyboard| keyboard.led_state())
                    {
                        device.led_update(led_state.into());
                    }
                    data.backend_data.keyboards.push(device.clone());
                }
            } else if let InputEvent::DeviceRemoved { ref device } = event {
                data.backend_data
                    .input_devices
                    .retain(|item| item != device);
                if device.has_capability(DeviceCapability::Keyboard) {
                    data.backend_data.keyboards.retain(|item| item != device);
                }
            }

            data.process_input_event(&dh, event);
            // Relative motion for a locked pointer must reach the client but
            // does not move a compositor cursor or damage the output. Client
            // commits, focus/layout changes, and real cursor motion schedule
            // their own redraws.
            if data.pointer.current_location() != pointer_before {
                data.desktop.redraw = true;
            }
        })
        .unwrap();

    event_loop
        .handle()
        .insert_source(notifier, move |event, &mut (), data| match event {
            SessionEvent::PauseSession => {
                data.desktop.active = false;
                data.stop_capture_boost();
                if data.desktop.recorder.is_running() {
                    data.desktop
                        .recorder
                        .abort("recording stopped because the session became inactive");
                }
                if let Some((timer, _)) = data.backend_data.cursor_timer.take() {
                    data.handle.remove(timer);
                }
                libinput_context.suspend();
                info!("pausing session");

                for backend in data.backend_data.backends.values_mut() {
                    backend.drm_output_manager.pause();
                    backend.active_leases.clear();
                    if let Some(lease_global) = backend.leasing_global.as_mut() {
                        lease_global.suspend();
                    }
                }
            }
            SessionEvent::ActivateSession => {
                data.desktop.active = true;
                data.desktop.redraw = true;
                info!("resuming session");

                if let Err(err) = libinput_context.resume() {
                    error!("Failed to resume libinput context: {:?}", err);
                }
                for (node, backend) in data
                    .backend_data
                    .backends
                    .iter_mut()
                    .map(|(handle, backend)| (*handle, backend))
                {
                    // if we do not care about flicking (caused by modesetting) we could just
                    // pass true for disable connectors here. this would make sure our drm
                    // device is in a known state (all connectors and planes disabled).
                    // but for demonstration we choose a more optimistic path by leaving the
                    // state as is and assume it will just work. If this assumption fails
                    // we will try to reset the state when trying to queue a frame.
                    backend
                        .drm_output_manager
                        .lock()
                        .activate(false)
                        .expect("failed to activate drm backend");
                    if let Some(lease_global) = backend.leasing_global.as_mut() {
                        lease_global.resume::<AnvilState<UdevData>>();
                    }
                    data.handle
                        .insert_idle(move |data| data.render(node, None, data.clock.now()));
                }
            }
        })
        .unwrap();

    // We try to initialize the primary node before others to make sure
    // any display only node can fall back to the primary node for rendering
    let primary_node = primary_gpu
        .node_with_type(NodeType::Primary)
        .and_then(|node| node.ok());
    let primary_device = udev_backend.device_list().find(|(device_id, _)| {
        primary_node
            .map(|primary_node| *device_id == primary_node.dev_id())
            .unwrap_or(false)
            || *device_id == primary_gpu.dev_id()
    });

    if let Some((device_id, path)) = primary_device {
        let node = DrmNode::from_dev_id(device_id).expect("failed to get primary node");
        state
            .device_added(node, path)
            .expect("failed to initialize primary node");
    }

    let primary_device_id = primary_device.map(|(device_id, _)| device_id);
    for (device_id, path) in udev_backend.device_list() {
        if Some(device_id) == primary_device_id {
            continue;
        }

        if let Err(err) = DrmNode::from_dev_id(device_id)
            .map_err(DeviceAddError::DrmNode)
            .and_then(|node| state.device_added(node, path))
        {
            error!("Skipping device {device_id}: {err}");
        }
    }
    state.shm_state.update_formats(
        state
            .backend_data
            .gpus
            .single_renderer(&primary_gpu)
            .unwrap()
            .shm_formats(),
    );

    #[cfg_attr(not(feature = "egl"), allow(unused_mut))]
    let mut renderer = state
        .backend_data
        .gpus
        .single_renderer(&primary_gpu)
        .unwrap();

    #[cfg(feature = "debug")]
    {
        #[allow(deprecated)]
        let fps_image = image::io::Reader::with_format(
            std::io::Cursor::new(FPS_NUMBERS_PNG),
            image::ImageFormat::Png,
        )
        .decode()
        .unwrap();
        let fps_texture = renderer
            .import_memory(
                &fps_image.to_rgba8(),
                Fourcc::Abgr8888,
                (fps_image.width() as i32, fps_image.height() as i32).into(),
                false,
            )
            .expect("Unable to upload FPS texture");

        for backend in state.backend_data.backends.values_mut() {
            for surface in backend.surfaces.values_mut() {
                surface.fps_element = Some(FpsElement::new(fps_texture.clone()));
            }
        }
        state.backend_data.fps_texture = Some(fps_texture);
    }

    #[cfg(feature = "egl")]
    {
        info!(
            ?primary_gpu,
            "Trying to initialize EGL Hardware Acceleration",
        );
        match renderer.bind_wl_display(&display_handle) {
            Ok(_) => info!("EGL hardware-acceleration enabled"),
            Err(err) => info!(?err, "Failed to initialize EGL hardware-acceleration"),
        }
    }

    // init dmabuf support with format list from our primary gpu
    let dmabuf_formats = renderer.dmabuf_formats();
    let default_feedback = DmabufFeedbackBuilder::new(primary_gpu.dev_id(), dmabuf_formats)
        .build()
        .unwrap();
    let mut dmabuf_state = DmabufState::new();
    let global = dmabuf_state.create_global_with_default_feedback::<AnvilState<UdevData>>(
        &display_handle,
        &default_feedback,
    );
    state.backend_data.dmabuf_state = Some((dmabuf_state, global));

    let gpus = &mut state.backend_data.gpus;
    state
        .backend_data
        .backends
        .iter_mut()
        .for_each(|(node, backend_data)| {
            // Update the per drm surface dmabuf feedback
            backend_data.surfaces.values_mut().for_each(|surface_data| {
                surface_data.dmabuf_feedback = surface_data.dmabuf_feedback.take().or_else(|| {
                    surface_data.drm_output.with_compositor(|compositor| {
                        get_surface_dmabuf_feedback(
                            primary_gpu,
                            surface_data.render_node,
                            *node,
                            gpus,
                            compositor.surface(),
                        )
                    })
                });
            });
        });

    // Expose syncobj protocol if supported by primary GPU
    if let Some(primary_node) = state
        .backend_data
        .primary_gpu
        .node_with_type(NodeType::Primary)
        .and_then(|x| x.ok())
    {
        if let Some(backend) = state.backend_data.backends.get(&primary_node) {
            let import_device = backend.drm_output_manager.device().device_fd().clone();
            if supports_syncobj_eventfd(&import_device) {
                let syncobj_state =
                    DrmSyncobjState::new::<AnvilState<UdevData>>(&display_handle, import_device);
                state.backend_data.syncobj_state = Some(syncobj_state);
            }
        }
    }

    event_loop
        .handle()
        .insert_source(udev_backend, move |event, _, data| match event {
            UdevEvent::Added { device_id, path } => {
                if let Err(err) = DrmNode::from_dev_id(device_id)
                    .map_err(DeviceAddError::DrmNode)
                    .and_then(|node| data.device_added(node, &path))
                {
                    error!("Skipping device {device_id}: {err}");
                }
            }
            UdevEvent::Changed { device_id } => {
                if let Ok(node) = DrmNode::from_dev_id(device_id) {
                    data.device_changed(node)
                }
            }
            UdevEvent::Removed { device_id } => {
                if let Ok(node) = DrmNode::from_dev_id(device_id) {
                    data.device_removed(node)
                }
            }
        })
        .unwrap();

    /*
     * Start XWayland if supported
     */
    #[cfg(feature = "xwayland")]
    state.start_xwayland();

    #[cfg(feature = "libei")]
    crate::libei::listen_eis(&event_loop.handle());

    /*
     * And run our loop
     */

    state.install_desktop();
    while state.running.load(Ordering::SeqCst) {
        let result = event_loop.dispatch(None, &mut state);
        if result.is_err() {
            state.running.store(false, Ordering::SeqCst);
        } else {
            state.space.refresh();
            state.maintain_desktop();
            if state.desktop.redraw && state.desktop.active {
                state.desktop.redraw = false;
                let nodes: Vec<_> = state.backend_data.backends.keys().copied().collect();
                for node in nodes {
                    state.render(node, None, state.clock.now());
                }
                crate::screencopy::process_pending(&mut state);
                let outputs: Vec<_> = state.space.outputs().cloned().collect();
                state.capture_generation = state.capture_generation.wrapping_add(1).max(1);
                for output in outputs {
                    state.process_pending_capture_frames(&output);
                }
            }
            state.popups.cleanup();
            display_handle.flush_clients().unwrap();
        }
    }
}

impl DrmLeaseHandler for AnvilState<UdevData> {
    fn drm_lease_state(&mut self, node: DrmNode) -> &mut DrmLeaseState {
        self.backend_data
            .backends
            .get_mut(&node)
            .unwrap()
            .leasing_global
            .as_mut()
            .unwrap()
    }

    fn lease_request(
        &mut self,
        node: DrmNode,
        request: DrmLeaseRequest,
    ) -> Result<DrmLeaseBuilder, LeaseRejected> {
        let backend = self
            .backend_data
            .backends
            .get(&node)
            .ok_or(LeaseRejected::default())?;

        let drm_device = backend.drm_output_manager.device();
        let mut builder = DrmLeaseBuilder::new(drm_device);
        for conn in request.connectors {
            if let Some((_, crtc)) = backend
                .non_desktop_connectors
                .iter()
                .find(|(handle, _)| *handle == conn)
            {
                builder.add_connector(conn);
                builder.add_crtc(*crtc);
                let planes = drm_device.planes(crtc).map_err(LeaseRejected::with_cause)?;
                let (primary_plane, primary_plane_claim) = planes
                    .primary
                    .iter()
                    .find_map(|plane| {
                        drm_device
                            .claim_plane(plane.handle, *crtc)
                            .map(|claim| (plane, claim))
                    })
                    .ok_or_else(LeaseRejected::default)?;
                builder.add_plane(primary_plane.handle, primary_plane_claim);
                if let Some((cursor, claim)) = planes.cursor.iter().find_map(|plane| {
                    drm_device
                        .claim_plane(plane.handle, *crtc)
                        .map(|claim| (plane, claim))
                }) {
                    builder.add_plane(cursor.handle, claim);
                }
            } else {
                tracing::warn!(
                    ?conn,
                    "Lease requested for desktop connector, denying request"
                );
                return Err(LeaseRejected::default());
            }
        }

        Ok(builder)
    }

    fn new_active_lease(&mut self, node: DrmNode, lease: DrmLease) {
        let backend = self.backend_data.backends.get_mut(&node).unwrap();
        backend.active_leases.push(lease);
    }

    fn lease_destroyed(&mut self, node: DrmNode, lease: u32) {
        let backend = self.backend_data.backends.get_mut(&node).unwrap();
        backend.active_leases.retain(|l| l.id() != lease);
    }
}

impl DrmSyncobjHandler for AnvilState<UdevData> {
    fn drm_syncobj_state(&mut self) -> Option<&mut DrmSyncobjState> {
        self.backend_data.syncobj_state.as_mut()
    }
}

pub type RenderSurface = GbmBufferedSurface<GbmAllocator<DrmDeviceFd>, PresentedFrame>;

pub type GbmDrmCompositor =
    DrmCompositor<GbmAllocator<DrmDeviceFd>, GbmDevice<DrmDeviceFd>, PresentedFrame, DrmDeviceFd>;

#[derive(Debug)]
pub struct PresentedFrame {
    feedback: OutputPresentationFeedback,
    lock_generation: Option<u64>,
}

struct SurfaceData {
    dh: DisplayHandle,
    device_id: DrmNode,
    connector: connector::Handle,
    modes: Vec<DrmMode>,
    render_node: Option<DrmNode>,
    output: Output,
    global: Option<GlobalId>,
    drm_output: DrmOutput<
        GbmAllocator<DrmDeviceFd>,
        GbmFramebufferExporter<DrmDeviceFd>,
        PresentedFrame,
        DrmDeviceFd,
    >,
    disable_direct_scanout: bool,
    #[cfg(feature = "debug")]
    fps: fps_ticker::Fps,
    #[cfg(feature = "debug")]
    fps_element: Option<FpsElement<MultiTexture>>,
    dmabuf_feedback: Option<SurfaceDmabufFeedback>,
    last_presentation_time: Option<Time<Monotonic>>,
    vblank_throttle_timer: Option<RegistrationToken>,
    repaint_timer: Option<RegistrationToken>,
    repaint_target: Option<Time<Monotonic>>,
    performance: OutputPerformanceMetrics,
    last_input_generation: u64,
    hdr_enabled: bool,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
struct HdrChromaticity {
    x: u16,
    y: u16,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
struct HdrMetadataInfoframe {
    eotf: u8,
    metadata_type: u8,
    display_primaries: [HdrChromaticity; 3],
    white_point: HdrChromaticity,
    max_display_mastering_luminance: u16,
    min_display_mastering_luminance: u16,
    max_cll: u16,
    max_fall: u16,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
struct HdrOutputMetadata {
    metadata_type: u32,
    hdmi_metadata_type1: HdrMetadataInfoframe,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
struct DrmColorLut {
    red: u16,
    green: u16,
    blue: u16,
    reserved: u16,
}

#[repr(C)]
#[derive(Clone, Copy, Debug)]
struct DrmColorCtm {
    matrix: [u64; 9],
}

const HDR_LUT_SIZE: usize = 1024;
const HDR_MAX_LUMINANCE: u16 = 1000;
const HDR_SDR_WHITE_LUMINANCE: f64 = 203.0;

fn hdr10_metadata() -> HdrOutputMetadata {
    HdrOutputMetadata {
        metadata_type: 0,
        hdmi_metadata_type1: HdrMetadataInfoframe {
            // CTA-861 SMPTE ST 2084 and static metadata type 1.
            eotf: 2,
            metadata_type: 0,
            // BT.2020 primaries and D65, in CTA units of 0.00002.
            display_primaries: [
                HdrChromaticity {
                    x: 35_400,
                    y: 14_600,
                },
                HdrChromaticity {
                    x: 8_500,
                    y: 39_850,
                },
                HdrChromaticity { x: 6_550, y: 2_300 },
            ],
            white_point: HdrChromaticity {
                x: 15_635,
                y: 16_450,
            },
            max_display_mastering_luminance: HDR_MAX_LUMINANCE,
            min_display_mastering_luminance: 1,
            max_cll: HDR_MAX_LUMINANCE,
            max_fall: 400,
        },
    }
}

fn property_named<D: Device, H: smithay::reexports::drm::control::ResourceHandle>(
    device: &D,
    handle: H,
    name: &str,
) -> Result<(property::Info, u64), String> {
    device
        .get_properties(handle)
        .map_err(|error| format!("cannot read DRM properties: {error}"))?
        .into_iter()
        .find_map(|(property, value)| {
            let info = device.get_property(property).ok()?;
            (info.name().to_str() == Ok(name)).then_some((info, value))
        })
        .ok_or_else(|| format!("DRM property {name} is unavailable"))
}

fn enum_value(info: &property::Info, name: &str) -> Result<u64, String> {
    let property::ValueType::Enum(values) = info.value_type() else {
        return Err(format!("DRM property {:?} is not an enum", info.name()));
    };
    values
        .values()
        .1
        .iter()
        .find(|value| value.name().to_str() == Ok(name))
        .map(|value| value.value())
        .ok_or_else(|| format!("DRM enum value {name} is unavailable"))
}

fn srgb_to_linear(value: f64) -> f64 {
    if value <= 0.04045 {
        value / 12.92
    } else {
        ((value + 0.055) / 1.055).powf(2.4)
    }
}

fn linear_to_pq(value: f64) -> f64 {
    let normalized_luminance = value.clamp(0.0, 1.0) * HDR_SDR_WHITE_LUMINANCE / 10_000.0;
    let m1 = 2610.0 / 16384.0;
    let m2 = 2523.0 / 32.0;
    let c1 = 3424.0 / 4096.0;
    let c2 = 2413.0 / 128.0;
    let c3 = 2392.0 / 128.0;
    let powered = normalized_luminance.powf(m1);
    ((c1 + c2 * powered) / (1.0 + c3 * powered)).powf(m2)
}

fn color_lut(transform: fn(f64) -> f64) -> [DrmColorLut; HDR_LUT_SIZE] {
    std::array::from_fn(|index| {
        let input = index as f64 / (HDR_LUT_SIZE - 1) as f64;
        let value = (transform(input).clamp(0.0, 1.0) * u16::MAX as f64).round() as u16;
        DrmColorLut {
            red: value,
            green: value,
            blue: value,
            reserved: 0,
        }
    })
}

fn ctm_value(value: f64) -> u64 {
    (value * (1u64 << 32) as f64).round() as u64
}

fn srgb_to_bt2020_ctm() -> DrmColorCtm {
    DrmColorCtm {
        matrix: [
            ctm_value(0.627_404),
            ctm_value(0.329_283),
            ctm_value(0.043_313),
            ctm_value(0.069_097),
            ctm_value(0.919_540),
            ctm_value(0.011_362),
            ctm_value(0.016_391),
            ctm_value(0.088_013),
            ctm_value(0.895_595),
        ],
    }
}

fn configure_hdr<D: Device>(
    device: &D,
    connector: connector::Handle,
    crtc: crtc::Handle,
    enabled: bool,
) -> Result<(), String> {
    if enabled && std::env::var_os("ANVIL_DISABLE_10BIT").is_some() {
        return Err("HDR cannot be enabled while ANVIL_DISABLE_10BIT is set".into());
    }
    let (colorspace, _) = property_named(device, connector, "Colorspace")?;
    let (hdr_metadata, _) = property_named(device, connector, "HDR_OUTPUT_METADATA")?;
    let (degamma, _) = property_named(device, crtc, "DEGAMMA_LUT")?;
    let (_, degamma_size) = property_named(device, crtc, "DEGAMMA_LUT_SIZE")?;
    let (ctm, _) = property_named(device, crtc, "CTM")?;
    let (gamma, _) = property_named(device, crtc, "GAMMA_LUT")?;
    let (_, gamma_size) = property_named(device, crtc, "GAMMA_LUT_SIZE")?;
    if enabled && (degamma_size != HDR_LUT_SIZE as u64 || gamma_size != HDR_LUT_SIZE as u64) {
        return Err(format!(
            "HDR requires {HDR_LUT_SIZE}-entry KMS LUTs, found degamma={degamma_size} gamma={gamma_size}"
        ));
    }

    let mut blobs = Vec::new();
    let mut request = atomic::AtomicModeReq::new();
    let colorspace_value = enum_value(&colorspace, if enabled { "BT2020_RGB" } else { "Default" })?;
    request.add_raw_property(connector.into(), colorspace.handle(), colorspace_value);

    if enabled {
        for (info, value) in [
            (
                &hdr_metadata,
                device.create_property_blob(&hdr10_metadata()),
            ),
            (
                &degamma,
                device.create_property_blob(&color_lut(srgb_to_linear)),
            ),
            (&ctm, device.create_property_blob(&srgb_to_bt2020_ctm())),
            (
                &gamma,
                device.create_property_blob(&color_lut(linear_to_pq)),
            ),
        ] {
            let blob = value
                .map_err(|error| {
                    format!(
                        "cannot create {} blob: {error}",
                        info.name().to_string_lossy()
                    )
                })?
                .as_blob()
                .ok_or("DRM returned a non-blob property value")?;
            blobs.push(blob);
            let target = if info.handle() == hdr_metadata.handle() {
                connector.into()
            } else {
                crtc.into()
            };
            request.add_raw_property(target, info.handle(), blob);
        }
    } else {
        request.add_raw_property(connector.into(), hdr_metadata.handle(), 0);
        request.add_raw_property(crtc.into(), degamma.handle(), 0);
        request.add_raw_property(crtc.into(), ctm.handle(), 0);
        request.add_raw_property(crtc.into(), gamma.handle(), 0);
    }

    let result = device
        .atomic_commit(
            AtomicCommitFlags::ALLOW_MODESET | AtomicCommitFlags::TEST_ONLY,
            request.clone(),
        )
        .and_then(|()| device.atomic_commit(AtomicCommitFlags::ALLOW_MODESET, request))
        .map_err(|error| format!("cannot apply HDR KMS state: {error}"));
    for blob in blobs {
        if let Err(error) = device.destroy_property_blob(blob) {
            debug!(
                blob,
                ?error,
                "Could not release userspace HDR property blob handle"
            );
        }
    }
    result
}

fn advertised_modes(modes: &[DrmMode]) -> Vec<(i32, i32, i32, bool)> {
    modes
        .iter()
        .map(|mode| {
            let wl = WlMode::from(*mode);
            (
                wl.size.w,
                wl.size.h,
                wl.refresh,
                mode.mode_type().contains(ModeTypeFlags::PREFERRED),
            )
        })
        .collect()
}

impl SurfaceData {
    fn refresh_advertised_modes(&self) {
        let modes: Vec<_> = self.modes.iter().copied().map(WlMode::from).collect();
        let preferred =
            wm_core::select_output_mode(&advertised_modes(&self.modes), &Default::default());
        synchronize_output_modes(&self.output, &modes, preferred);
    }
    fn configure_vrr(&mut self, enabled: bool) -> Result<(), String> {
        let name = self.output.name();
        self.drm_output.with_compositor(|compositor| {
            if compositor.vrr_enabled() == enabled {
                return Ok(());
            }
            if enabled {
                let support = compositor
                    .vrr_supported(self.connector)
                    .map_err(|error| format!("{name}: cannot query VRR: {error}"))?;
                if support == smithay::backend::drm::VrrSupport::NotSupported {
                    return Err(format!("{name}: VRR requested but not supported"));
                }
            }
            compositor
                .use_vrr(enabled)
                .map_err(|error| format!("{name}: cannot configure VRR: {error}"))?;
            compositor.reset_buffers();
            info!(output = %name, enabled, "VRR state requested for next presentation");
            Ok(())
        })
    }
}

fn synchronize_output_modes(output: &Output, modes: &[WlMode], preferred: Option<usize>) {
    for mode in output.modes() {
        // Keep the active mode until its replacement passes the DRM test.
        // wl_output cannot retract old modes from already-connected clients.
        if !modes.contains(&mode) && output.current_mode() != Some(mode) {
            output.delete_mode(mode);
        }
    }
    for mode in modes {
        output.add_mode(*mode);
    }
    if let Some(mode) = preferred.and_then(|index| modes.get(index)) {
        output.set_preferred(*mode);
    }
}

#[cfg(test)]
mod mode_update_tests {
    use super::*;
    #[test]
    fn edid_update_preserves_active_mode_until_replacement() {
        let output = Output::new(
            "test".into(),
            PhysicalProperties {
                size: (0, 0).into(),
                subpixel: smithay::output::Subpixel::Unknown,
                make: "test".into(),
                model: "test".into(),
                serial_number: "test".into(),
            },
        );
        let old = WlMode {
            size: (1920, 1080).into(),
            refresh: 60000,
        };
        let stale = WlMode {
            size: (1280, 720).into(),
            refresh: 60000,
        };
        let new = WlMode {
            size: (2560, 1440).into(),
            refresh: 144000,
        };
        output.change_current_state(Some(old), None, None, None);
        output.add_mode(stale);
        synchronize_output_modes(&output, &[new], Some(0));
        assert_eq!(output.current_mode(), Some(old));
        assert!(output.modes().contains(&old));
        assert!(output.modes().contains(&new));
        assert!(!output.modes().contains(&stale));
        output.change_current_state(Some(new), None, None, None);
        synchronize_output_modes(&output, &[new], Some(0));
        assert_eq!(output.modes(), vec![new]);
        synchronize_output_modes(&output, &[], None);
        assert_eq!(output.current_mode(), Some(new));
    }

    #[test]
    fn hdr_metadata_matches_the_kernel_uapi_layout_and_bt2020_values() {
        assert_eq!(std::mem::size_of::<HdrOutputMetadata>(), 32);
        let metadata = hdr10_metadata();
        assert_eq!(metadata.hdmi_metadata_type1.eotf, 2);
        assert_eq!(metadata.hdmi_metadata_type1.display_primaries[0].x, 35_400);
        assert_eq!(metadata.hdmi_metadata_type1.max_cll, HDR_MAX_LUMINANCE);
    }

    #[test]
    fn hdr_luts_decode_srgb_and_encode_reference_white_as_pq() {
        let degamma = color_lut(srgb_to_linear);
        let gamma = color_lut(linear_to_pq);
        assert_eq!(degamma[0].red, 0);
        assert_eq!(degamma[HDR_LUT_SIZE - 1].red, u16::MAX);
        assert_eq!(gamma[0].red, 0);
        assert!(gamma[HDR_LUT_SIZE - 1].red > 37_000);
        assert!(gamma[HDR_LUT_SIZE - 1].red < 39_000);
    }
}

impl Drop for SurfaceData {
    fn drop(&mut self) {
        self.output.leave_all();
        if let Some(global) = self.global.take() {
            self.dh.remove_global::<AnvilState<UdevData>>(global);
        }
    }
}

struct BackendData {
    surfaces: HashMap<crtc::Handle, SurfaceData>,
    non_desktop_connectors: Vec<(connector::Handle, crtc::Handle)>,
    leasing_global: Option<DrmLeaseState>,
    active_leases: Vec<DrmLease>,
    drm_output_manager: DrmOutputManager<
        GbmAllocator<DrmDeviceFd>,
        GbmFramebufferExporter<DrmDeviceFd>,
        PresentedFrame,
        DrmDeviceFd,
    >,
    drm_scanner: DrmScanner,
    render_node: Option<DrmNode>,
    registration_token: RegistrationToken,
}

#[derive(Debug, thiserror::Error)]
enum DeviceAddError {
    #[error("Failed to open device using libseat: {0}")]
    DeviceOpen(libseat::Error),
    #[error("Failed to initialize drm device: {0}")]
    DrmDevice(DrmError),
    #[error("Failed to initialize gbm device: {0}")]
    GbmDevice(std::io::Error),
    #[error("Failed to access drm node: {0}")]
    DrmNode(CreateDrmNodeError),
    #[error("Failed to add device to GpuManager: {0}")]
    AddNode(egl::Error),
    #[error("The device has no render node")]
    NoRenderNode,
    #[error("Primary GPU is missing")]
    PrimaryGpuMissing,
}

fn get_surface_dmabuf_feedback(
    primary_gpu: DrmNode,
    render_node: Option<DrmNode>,
    scanout_node: DrmNode,
    gpus: &mut GpuManager<GbmGlesBackend<GlesRenderer, DrmDeviceFd>>,
    surface: &DrmSurface,
) -> Option<SurfaceDmabufFeedback> {
    let primary_formats = gpus.single_renderer(&primary_gpu).ok()?.dmabuf_formats();
    let render_formats = if let Some(render_node) = render_node {
        gpus.single_renderer(&render_node).ok()?.dmabuf_formats()
    } else {
        FormatSet::default()
    };

    let all_render_formats = primary_formats
        .iter()
        .chain(render_formats.iter())
        .copied()
        .collect::<FormatSet>();

    let planes = surface.planes().clone();

    // We limit the scan-out tranche to formats we can also render from
    // so that there is always a fallback render path available in case
    // the supplied buffer can not be scanned out directly
    let planes_formats = surface
        .plane_info()
        .formats
        .iter()
        .copied()
        .chain(planes.overlay.into_iter().flat_map(|p| p.formats))
        .collect::<FormatSet>()
        .intersection(&all_render_formats)
        .copied()
        .collect::<FormatSet>();

    let builder = DmabufFeedbackBuilder::new(primary_gpu.dev_id(), primary_formats);
    let render_feedback = if let Some(render_node) = render_node {
        builder
            .clone()
            .add_preference_tranche(
                render_node.dev_id(),
                zwp_linux_dmabuf_feedback_v1::TrancheFlags::Sampling,
                render_formats.clone(),
                3u32..=6,
            )
            .build()
            .unwrap()
    } else {
        builder.clone().build().unwrap()
    };

    let scanout_feedback = builder
        .add_preference_tranche(
            surface.device_fd().dev_id().unwrap(),
            zwp_linux_dmabuf_feedback_v1::TrancheFlags::Scanout,
            planes_formats,
            4u32..=6,
        )
        .add_preference_tranche(
            scanout_node.dev_id(),
            zwp_linux_dmabuf_feedback_v1::TrancheFlags::Sampling,
            render_formats,
            4u32..=6,
        )
        .build()
        .unwrap();

    Some(SurfaceDmabufFeedback {
        render_feedback,
        scanout_feedback,
    })
}

impl AnvilState<UdevData> {
    fn device_added(&mut self, node: DrmNode, path: &Path) -> Result<(), DeviceAddError> {
        // Try to open the device
        let fd = self
            .backend_data
            .session
            .open(
                path,
                OFlags::RDWR | OFlags::CLOEXEC | OFlags::NOCTTY | OFlags::NONBLOCK,
            )
            .map_err(DeviceAddError::DeviceOpen)?;

        let fd = DrmDeviceFd::new(DeviceFd::from(fd));

        let (drm, notifier) =
            DrmDevice::new(fd.clone(), true).map_err(DeviceAddError::DrmDevice)?;
        let gbm = GbmDevice::new(fd).map_err(DeviceAddError::GbmDevice)?;

        let registration_token = self
            .handle
            .insert_source(
                notifier,
                move |event, metadata, data: &mut AnvilState<_>| match event {
                    DrmEvent::VBlank(crtc) => {
                        profiling::scope!("vblank", &format!("{crtc:?}"));
                        data.frame_finish(node, crtc, metadata);
                    }
                    DrmEvent::Error(error) => {
                        error!("{:?}", error);
                    }
                },
            )
            .unwrap();

        let mut try_initialize_gpu = || {
            let display = unsafe { EGLDisplay::new(gbm.clone()).map_err(DeviceAddError::AddNode)? };
            let egl_device =
                EGLDevice::device_for_display(&display).map_err(DeviceAddError::AddNode)?;

            if egl_device.is_software() {
                return Err(DeviceAddError::NoRenderNode);
            }

            let render_node = egl_device
                .try_get_render_node()
                .ok()
                .flatten()
                .unwrap_or(node);
            self.backend_data
                .gpus
                .as_mut()
                .add_node(render_node, gbm.clone())
                .map_err(DeviceAddError::AddNode)?;

            std::result::Result::<DrmNode, DeviceAddError>::Ok(render_node)
        };

        let render_node = try_initialize_gpu()
            .inspect_err(|err| {
                warn!(?err, "failed to initialize gpu");
            })
            .ok();

        let allocator = render_node
            .is_some()
            .then(|| {
                GbmAllocator::new(
                    gbm.clone(),
                    GbmBufferFlags::RENDERING | GbmBufferFlags::SCANOUT,
                )
            })
            .or_else(|| {
                self.backend_data
                    .backends
                    .get(&self.backend_data.primary_gpu)
                    .or_else(|| {
                        self.backend_data.backends.values().find(|backend| {
                            backend.render_node == Some(self.backend_data.primary_gpu)
                        })
                    })
                    .map(|backend| backend.drm_output_manager.allocator().clone())
            })
            .ok_or(DeviceAddError::PrimaryGpuMissing)?;

        let framebuffer_exporter = GbmFramebufferExporter::new(gbm.clone(), render_node.into());

        let color_formats = if std::env::var("ANVIL_DISABLE_10BIT").is_ok() {
            SUPPORTED_FORMATS_8BIT_ONLY
        } else {
            SUPPORTED_FORMATS
        };
        let mut renderer = self
            .backend_data
            .gpus
            .single_renderer(&render_node.unwrap_or(self.backend_data.primary_gpu))
            .unwrap();
        let render_formats = renderer
            .as_mut()
            .egl_context()
            .dmabuf_render_formats()
            .iter()
            .filter(|format| render_node.is_some() || format.modifier == Modifier::Linear)
            .copied()
            .collect::<FormatSet>();

        let drm_output_manager = DrmOutputManager::new(
            drm,
            allocator,
            framebuffer_exporter,
            Some(gbm),
            color_formats.iter().copied(),
            render_formats,
        );

        self.backend_data.backends.insert(
            node,
            BackendData {
                registration_token,
                drm_output_manager,
                drm_scanner: DrmScanner::new(),
                non_desktop_connectors: Vec::new(),
                render_node,
                surfaces: HashMap::new(),
                leasing_global: DrmLeaseState::new::<AnvilState<UdevData>>(
                    &self.display_handle,
                    &node,
                )
                .inspect_err(|err| {
                    warn!(?err, "Failed to initialize drm lease global for: {}", node);
                })
                .ok(),
                active_leases: Vec::new(),
            },
        );

        self.device_changed(node);

        Ok(())
    }

    fn connector_connected(
        &mut self,
        node: DrmNode,
        connector: connector::Info,
        crtc: crtc::Handle,
    ) {
        let device = if let Some(device) = self.backend_data.backends.get_mut(&node) {
            device
        } else {
            return;
        };

        let render_node = device.render_node.unwrap_or(self.backend_data.primary_gpu);
        let mut renderer = self
            .backend_data
            .gpus
            .single_renderer(&render_node)
            .unwrap();

        let output_name = format!(
            "{}-{}",
            connector.interface().as_str(),
            connector.interface_id()
        );
        info!(?crtc, "Trying to setup connector {}", output_name,);

        let drm_device = device.drm_output_manager.device();

        let non_desktop = drm_device
            .get_properties(connector.handle())
            .ok()
            .and_then(|props| {
                let (info, value) = props
                    .into_iter()
                    .filter_map(|(handle, value)| {
                        let info = drm_device.get_property(handle).ok()?;

                        Some((info, value))
                    })
                    .find(|(info, _)| info.name().to_str() == Ok("non-desktop"))?;

                info.value_type().convert_value(value).as_boolean()
            })
            .unwrap_or(false);

        let display_info = display_info::for_connector(drm_device, connector.handle());

        let make = display_info
            .as_ref()
            .and_then(|info| info.make())
            .unwrap_or_else(|| "Unknown".into());

        let model = display_info
            .as_ref()
            .and_then(|info| info.model())
            .unwrap_or_else(|| "Unknown".into());

        let serial_number = display_info
            .as_ref()
            .and_then(|info| info.serial())
            .unwrap_or_else(|| "Unknown".into());

        if non_desktop {
            info!(
                "Connector {} is non-desktop, setting up for leasing",
                output_name
            );
            device
                .non_desktop_connectors
                .push((connector.handle(), crtc));
            if let Some(lease_state) = device.leasing_global.as_mut() {
                lease_state.add_connector::<AnvilState<UdevData>>(
                    connector.handle(),
                    output_name,
                    format!("{make} {model}"),
                );
            }
        } else {
            let advertised = advertised_modes(connector.modes());
            let requested = self
                .desktop
                .config
                .outputs
                .get(&output_name)
                .cloned()
                .unwrap_or_default();
            let mode_id = wm_core::select_output_mode(&advertised, &requested).or_else(|| {
                warn!(output = %output_name, width = requested.width, height = requested.height,
                    refresh = wm_core::output_refresh_millihz(&requested),
                    "Requested mode unavailable; using advertised default");
                wm_core::select_output_mode(&advertised, &Default::default())
            });
            let Some(mode_id) = mode_id else {
                warn!(output = %output_name, "Connector has no usable advertised modes");
                return;
            };

            let drm_mode = connector.modes()[mode_id];
            let wl_mode = WlMode::from(drm_mode);

            let (phys_w, phys_h) = connector.size().unwrap_or((0, 0));
            let output = Output::new(
                output_name,
                PhysicalProperties {
                    size: (phys_w as i32, phys_h as i32).into(),
                    subpixel: connector.subpixel().into(),
                    make,
                    model,
                    serial_number,
                },
            );
            if self.lock.locked {
                output.user_data().insert_if_missing(|| {
                    std::sync::Mutex::new(crate::lock::LockOutput {
                        locked: true,
                        generation: self.lock.generation,
                        ..Default::default()
                    })
                });
            }

            let x = self.space.outputs().fold(0, |acc, o| {
                acc + self.space.output_geometry(o).unwrap().size.w
            });
            let position = (x, 0).into();

            for (mode, details) in connector.modes().iter().zip(&advertised) {
                let mode = WlMode::from(*mode);
                output.add_mode(mode);
                if details.3 {
                    output.set_preferred(mode);
                }
            }
            output.change_current_state(Some(wl_mode), None, None, Some(position));

            output.user_data().insert_if_missing(|| UdevOutputId {
                crtc,
                device_id: node,
            });

            #[cfg(feature = "debug")]
            let fps_element = self.backend_data.fps_texture.clone().map(FpsElement::new);

            let driver = match drm_device.get_driver() {
                Ok(driver) => driver,
                Err(err) => {
                    warn!("Failed to query drm driver: {}", err);
                    return;
                }
            };

            let mut planes = match drm_device.planes(&crtc) {
                Ok(planes) => planes,
                Err(err) => {
                    warn!("Failed to query crtc planes: {}", err);
                    return;
                }
            };

            // Using an overlay plane on a nvidia card breaks
            if driver
                .name()
                .to_string_lossy()
                .to_lowercase()
                .contains("nvidia")
                || driver
                    .description()
                    .to_string_lossy()
                    .to_lowercase()
                    .contains("nvidia")
            {
                planes.overlay = vec![];
            }

            let drm_output = match device
                .drm_output_manager
                .lock()
                .initialize_output::<_, OutputRenderElements<UdevRenderer<'_>, WindowRenderElement<UdevRenderer<'_>>>>(
                    crtc,
                    drm_mode,
                    &[connector.handle()],
                    &output,
                    Some(planes),
                    &mut renderer,
                    &DrmOutputRenderElements::default(),
                ) {
                Ok(drm_output) => drm_output,
                Err(err) => {
                    warn!("Failed to initialize drm output: {}", err);
                    return;
                }
            };

            let disable_direct_scanout = std::env::var("ANVIL_DISABLE_DIRECT_SCANOUT").is_ok();
            // Publish only after DRM initialization succeeds; a later EDID
            // update can retry failed setup without leaving a phantom output.
            let global = output.create_global::<AnvilState<UdevData>>(&self.display_handle);
            self.space.map_output(&output, position);

            let dmabuf_feedback = drm_output.with_compositor(|compositor| {
                compositor.set_debug_flags(self.backend_data.debug_flags);

                get_surface_dmabuf_feedback(
                    self.backend_data.primary_gpu,
                    device.render_node,
                    node,
                    &mut self.backend_data.gpus,
                    compositor.surface(),
                )
            });

            let mut surface = SurfaceData {
                dh: self.display_handle.clone(),
                device_id: node,
                connector: connector.handle(),
                modes: connector.modes().to_vec(),
                render_node: device.render_node,
                output,
                global: Some(global),
                drm_output,
                disable_direct_scanout,
                #[cfg(feature = "debug")]
                fps: fps_ticker::Fps::default(),
                #[cfg(feature = "debug")]
                fps_element,
                dmabuf_feedback,
                last_presentation_time: None,
                vblank_throttle_timer: None,
                repaint_timer: None,
                repaint_target: None,
                performance: OutputPerformanceMetrics::default(),
                last_input_generation: 0,
                hdr_enabled: false,
            };

            if let Err(error) = surface.configure_vrr(requested.vrr) {
                warn!("{error}");
                self.desktop.error = Some(error);
            }
            if requested.hdr {
                match configure_hdr(
                    device.drm_output_manager.device(),
                    connector.handle(),
                    crtc,
                    true,
                ) {
                    Ok(()) => {
                        surface.hdr_enabled = true;
                        surface.drm_output.reset_buffers();
                        info!(output = %surface.output.name(), "HDR10 output enabled");
                    }
                    Err(error) => {
                        let error = format!("{}: {error}", surface.output.name());
                        warn!("{error}");
                        self.desktop.error = Some(error);
                    }
                }
            }

            device.surfaces.insert(crtc, surface);

            // kick-off rendering
            self.handle.insert_idle(move |state| {
                state.render_surface(node, crtc, state.clock.now());
            });
        }
    }

    fn connector_disconnected(
        &mut self,
        node: DrmNode,
        connector: connector::Info,
        crtc: crtc::Handle,
    ) {
        let device = if let Some(device) = self.backend_data.backends.get_mut(&node) {
            device
        } else {
            return;
        };

        if let Some(pos) = device
            .non_desktop_connectors
            .iter()
            .position(|(handle, _)| *handle == connector.handle())
        {
            let _ = device.non_desktop_connectors.remove(pos);
            if let Some(leasing_state) = device.leasing_global.as_mut() {
                leasing_state.withdraw_connector(connector.handle());
            }
        } else if let Some(surface) = device.surfaces.remove(&crtc) {
            self.space.unmap_output(&surface.output);
            self.space.refresh();
        }

        let render_node = device.render_node.unwrap_or(self.backend_data.primary_gpu);
        let mut renderer = self
            .backend_data
            .gpus
            .single_renderer(&render_node)
            .unwrap();
        let _ = device.drm_output_manager.lock().try_to_restore_modifiers::<_, OutputRenderElements<
            UdevRenderer<'_>,
            WindowRenderElement<UdevRenderer<'_>>,
        >>(
            &mut renderer,
            // FIXME: For a flicker free operation we should return the actual elements for this output..
            // Instead we just use black to "simulate" a modeset :)
            &DrmOutputRenderElements::default(),
        );
    }

    fn device_changed(&mut self, node: DrmNode) {
        let device = if let Some(device) = self.backend_data.backends.get_mut(&node) {
            device
        } else {
            return;
        };

        let scan_result = match device
            .drm_scanner
            .scan_connectors(device.drm_output_manager.device())
        {
            Ok(scan_result) => scan_result,
            Err(err) => {
                tracing::warn!(?err, "Failed to scan connectors");
                return;
            }
        };

        for event in scan_result {
            match event {
                DrmScanEvent::Connected {
                    connector,
                    crtc: Some(crtc),
                } => {
                    self.connector_connected(node, connector, crtc);
                }
                DrmScanEvent::Disconnected {
                    connector,
                    crtc: Some(crtc),
                } => {
                    self.connector_disconnected(node, connector, crtc);
                }
                DrmScanEvent::Changed {
                    connector,
                    crtc: Some(crtc),
                } => {
                    let device = self.backend_data.backends.get_mut(&node).unwrap();
                    if let Some(surface) = device.surfaces.get_mut(&crtc) {
                        surface.modes = connector.modes().to_vec();
                        surface.refresh_advertised_modes();
                        let errors = self
                            .backend_data
                            .apply_output_config(&self.desktop.config.outputs);
                        if !errors.is_empty() {
                            self.desktop.error = Some(errors.join("; "));
                        }
                        self.desktop.redraw = true;
                    } else if !device
                        .non_desktop_connectors
                        .iter()
                        .any(|(handle, _)| *handle == connector.handle())
                    {
                        // Initial EDID may have contained no modes, or setup
                        // may have failed. Retry when the connector changes.
                        self.connector_connected(node, connector, crtc);
                    }
                }
                _ => {}
            }
        }

        // fixup window coordinates
        crate::shell::fixup_positions(&mut self.space, self.pointer.current_location());
    }

    fn device_removed(&mut self, node: DrmNode) {
        let device = if let Some(device) = self.backend_data.backends.get_mut(&node) {
            device
        } else {
            return;
        };

        let crtcs: Vec<_> = device
            .drm_scanner
            .crtcs()
            .map(|(info, crtc)| (info.clone(), crtc))
            .collect();

        for (connector, crtc) in crtcs {
            self.connector_disconnected(node, connector, crtc);
        }

        debug!("Surfaces dropped");

        // drop the backends on this side
        if let Some(mut backend_data) = self.backend_data.backends.remove(&node) {
            if let Some(mut leasing_global) = backend_data.leasing_global.take() {
                leasing_global.disable_global::<AnvilState<UdevData>>();
            }

            if let Some(render_node) = backend_data.render_node {
                self.backend_data.gpus.as_mut().remove_node(&render_node);
            }

            self.handle.remove(backend_data.registration_token);

            debug!("Dropping device");
        }

        crate::shell::fixup_positions(&mut self.space, self.pointer.current_location());
    }

    fn frame_finish(
        &mut self,
        dev_id: DrmNode,
        crtc: crtc::Handle,
        metadata: &mut Option<DrmEventMetadata>,
    ) {
        profiling::scope!("frame_finish", &format!("{crtc:?}"));

        let device_backend = match self.backend_data.backends.get_mut(&dev_id) {
            Some(backend) => backend,
            None => {
                error!("Trying to finish frame on non-existent backend {}", dev_id);
                return;
            }
        };

        let surface = match device_backend.surfaces.get_mut(&crtc) {
            Some(surface) => surface,
            None => {
                error!("Trying to finish frame on non-existent crtc {:?}", crtc);
                return;
            }
        };

        if let Some(timer_token) = surface.vblank_throttle_timer.take() {
            self.handle.remove(timer_token);
        }
        if let Some(timer_token) = surface.repaint_timer.take() {
            self.handle.remove(timer_token);
        }
        surface.repaint_target = None;

        let output = if let Some(output) = self.space.outputs().find(|o| {
            o.user_data().get::<UdevOutputId>()
                == Some(&UdevOutputId {
                    device_id: surface.device_id,
                    crtc,
                })
        }) {
            output.clone()
        } else {
            // somehow we got called with an invalid output
            return;
        };

        let Some(frame_duration) = output
            .current_mode()
            .map(|mode| Duration::from_secs_f64(1_000f64 / mode.refresh as f64))
        else {
            return;
        };

        let tp = metadata.as_ref().and_then(|metadata| match metadata.time {
            smithay::backend::drm::DrmEventTime::Monotonic(tp) => tp.is_zero().not().then_some(tp),
            smithay::backend::drm::DrmEventTime::Realtime(_) => None,
        });

        let seq = metadata
            .as_ref()
            .map(|metadata| metadata.sequence)
            .unwrap_or(0);

        let (clock, flags) = if let Some(tp) = tp {
            (
                tp.into(),
                wp_presentation_feedback::Kind::Vsync
                    | wp_presentation_feedback::Kind::HwClock
                    | wp_presentation_feedback::Kind::HwCompletion,
            )
        } else {
            (self.clock.now(), wp_presentation_feedback::Kind::Vsync)
        };

        let vblank_remaining_time = surface
            .last_presentation_time
            .map(|last_presentation_time| {
                frame_duration.saturating_sub(Time::elapsed(&last_presentation_time, clock))
            });

        if let Some(vblank_remaining_time) = vblank_remaining_time {
            if vblank_remaining_time > frame_duration / 2 {
                static WARN_ONCE: Once = Once::new();
                WARN_ONCE.call_once(|| {
                    warn!("display running faster than expected, throttling vblanks and disabling HwClock")
                });
                let throttled_time = tp
                    .map(|tp| tp.saturating_add(vblank_remaining_time))
                    .unwrap_or(Duration::ZERO);
                let throttled_metadata = DrmEventMetadata {
                    sequence: seq,
                    time: DrmEventTime::Monotonic(throttled_time),
                };
                let timer_token = self
                    .handle
                    .insert_source(
                        Timer::from_duration(vblank_remaining_time),
                        move |_, _, data| {
                            data.frame_finish(dev_id, crtc, &mut Some(throttled_metadata));
                            TimeoutAction::Drop
                        },
                    )
                    .expect("failed to register vblank throttle timer");
                surface.vblank_throttle_timer = Some(timer_token);
                return;
            }
        }
        surface.last_presentation_time = Some(clock);
        let submit_result = surface
            .drm_output
            .frame_submitted()
            .map_err(Into::<SwapBuffersError>::into);

        let schedule_render = match submit_result {
            Ok(user_data) => {
                if let Some(mut frame) = user_data {
                    if let Some(generation) = frame.lock_generation {
                        let lock_output = output.clone();
                        self.handle.insert_idle(move |state| {
                            state.lock_presented(&lock_output, generation)
                        });
                    }
                    frame.feedback.presented(
                        clock,
                        Refresh::fixed(frame_duration),
                        seq as u64,
                        flags,
                    );
                }

                true
            }
            Err(err) => {
                warn!("Error during rendering: {:?}", err);
                match err {
                    SwapBuffersError::AlreadySwapped => true,
                    // If the device has been deactivated do not reschedule, this will be done
                    // by session resume
                    SwapBuffersError::TemporaryFailure(err)
                        if matches!(
                            err.downcast_ref::<DrmError>(),
                            Some(&DrmError::DeviceInactive)
                        ) =>
                    {
                        false
                    }
                    SwapBuffersError::TemporaryFailure(err) => matches!(
                        err.downcast_ref::<DrmError>(),
                        Some(DrmError::Access(DrmAccessError {
                            source,
                            ..
                        })) if source.kind() == io::ErrorKind::PermissionDenied
                    ),
                    SwapBuffersError::ContextLost(err) => panic!("Rendering loop lost: {err}"),
                }
            }
        };

        if schedule_render {
            let next_frame_target = clock + frame_duration;

            // What are we trying to solve by introducing a delay here:
            //
            // Basically it is all about latency of client provided buffers.
            // A client driven by frame callbacks will wait for a frame callback
            // to repaint and submit a new buffer. As we send frame callbacks
            // as part of the repaint in the compositor the latency would always
            // be approx. 2 frames. By introducing a delay before we repaint in
            // the compositor we can reduce the latency to approx. 1 frame + the
            // remaining duration from the repaint to the next VBlank.
            //
            // With the delay it is also possible to further reduce latency if
            // the client is driven by presentation feedback. As the presentation
            // feedback is directly sent after a VBlank the client can submit a
            // new buffer during the repaint delay that can hit the very next
            // VBlank, thus reducing the potential latency to below one frame.
            //
            // Schedule from measured work instead of a fixed fraction. At
            // 360 Hz, the former 60% delay left only about 1.1 ms regardless
            // of actual compositor and KMS cost.
            let repaint_budget = surface.performance.repaint_budget(frame_duration);
            let repaint_delay = frame_duration.saturating_sub(repaint_budget);

            let vrr_active = surface
                .drm_output
                .with_compositor(|compositor| compositor.vrr_enabled());
            let timer = if vrr_active
                || surface
                    .render_node
                    .map(|render_node| render_node != self.backend_data.primary_gpu)
                    .unwrap_or(true)
            {
                // However, if we need to do a copy, that might not be enough.
                // (And without actual comparison to previous frames we cannot really know.)
                // So lets ignore that in those cases to avoid thrashing performance.
                trace!("scheduling repaint timer immediately on {:?}", crtc);
                Timer::immediate()
            } else {
                trace!(
                    "scheduling repaint timer with delay {:?} on {:?}",
                    repaint_delay, crtc
                );
                Timer::from_duration(repaint_delay)
            };

            let repaint_token = self
                .handle
                .insert_source(timer, move |_, _, data| {
                    if let Some(backend) = data.backend_data.backends.get_mut(&dev_id)
                        && let Some(surface) = backend.surfaces.get_mut(&crtc)
                    {
                        surface.repaint_timer = None;
                    }
                    data.render(dev_id, Some(crtc), next_frame_target);
                    TimeoutAction::Drop
                })
                .expect("failed to schedule frame timer");
            surface.repaint_timer = Some(repaint_token);
            surface.repaint_target = Some(next_frame_target);
        }
    }

    // If crtc is `Some()`, render it, else render all crtcs
    fn render(&mut self, node: DrmNode, crtc: Option<crtc::Handle>, frame_target: Time<Monotonic>) {
        let device_backend = match self.backend_data.backends.get_mut(&node) {
            Some(backend) => backend,
            None => {
                error!("Trying to render on non-existent backend {}", node);
                return;
            }
        };

        if let Some(crtc) = crtc {
            self.render_surface(node, crtc, frame_target);
        } else {
            let crtcs: Vec<_> = device_backend.surfaces.keys().copied().collect();
            for crtc in crtcs {
                self.render_surface(node, crtc, frame_target);
            }
        };
    }

    fn render_surface(&mut self, node: DrmNode, crtc: crtc::Handle, frame_target: Time<Monotonic>) {
        profiling::scope!("render_surface", &format!("{crtc:?}"));

        let output = if let Some(output) = self.space.outputs().find(|o| {
            o.user_data().get::<UdevOutputId>()
                == Some(&UdevOutputId {
                    device_id: node,
                    crtc,
                })
        }) {
            output.clone()
        } else {
            // somehow we got called with an invalid output
            return;
        };

        self.pre_repaint(&output, frame_target);

        let input_generation = self.backend_data.input_generation;
        let input_elapsed = self.backend_data.last_input_at.map(|input| input.elapsed());

        let device = if let Some(device) = self.backend_data.backends.get_mut(&node) {
            device
        } else {
            return;
        };

        let surface = if let Some(surface) = device.surfaces.get_mut(&crtc) {
            surface
        } else {
            return;
        };

        let scheduled_deadline = surface
            .repaint_target
            .take()
            .is_some_and(|target| target == frame_target);
        if let Some(timer_token) = surface.repaint_timer.take() {
            self.handle.remove(timer_token);
        }
        let missed_before_render = scheduled_deadline
            && Duration::from(frame_target).saturating_sub(self.clock.now().into())
                == Duration::ZERO;

        let start = Instant::now();

        let image_scale = output.current_scale().integer_scale();
        let (frame, next_cursor_frame) = self.backend_data.pointer_image.get_named_frame(
            match self.cursor_status {
                CursorImageStatus::Named(icon) => icon,
                _ => smithay::input::pointer::CursorIcon::Default,
            },
            image_scale as u32,
            self.clock.now().into(),
        );

        let cursor_on_output = self
            .space
            .output_geometry(&output)
            .is_some_and(|geometry| geometry.to_f64().contains(self.pointer.current_location()));
        let named_cursor = matches!(self.cursor_status, CursorImageStatus::Named(_));
        if cursor_on_output || !named_cursor {
            let next = next_cursor_frame.filter(|_| named_cursor && self.desktop.active);
            if let Some(delay) = next {
                let deadline = Instant::now() + delay;
                if self
                    .backend_data
                    .cursor_timer
                    .as_ref()
                    .is_none_or(|(_, old)| deadline < *old)
                {
                    if let Some((timer, _)) = self.backend_data.cursor_timer.take() {
                        self.handle.remove(timer);
                    }
                    if let Ok(timer) =
                        self.handle
                            .insert_source(Timer::from_duration(delay), |_, _, state| {
                                state.backend_data.cursor_timer = None;
                                state.desktop.redraw = true;
                                TimeoutAction::Drop
                            })
                    {
                        self.backend_data.cursor_timer = Some((timer, deadline));
                    }
                }
            } else if let Some((timer, _)) = self.backend_data.cursor_timer.take() {
                self.handle.remove(timer);
            }
        }

        let primary_gpu = self.backend_data.primary_gpu;
        let render_node = surface.render_node.unwrap_or(primary_gpu);
        let mut renderer = if primary_gpu == render_node {
            self.backend_data.gpus.single_renderer(&render_node)
        } else {
            let format = surface.drm_output.format();
            self.backend_data
                .gpus
                .renderer(&primary_gpu, &render_node, format)
        }
        .unwrap();

        let named_hotspot = (
            frame.xhot as f64 / image_scale as f64,
            frame.yhot as f64 / image_scale as f64,
        )
            .into();
        let pointer_images = &mut self.backend_data.pointer_images;
        let pointer_image = pointer_images
            .iter()
            .find_map(|(image, scale, texture)| {
                if image == &frame && *scale == image_scale {
                    Some(texture.clone())
                } else {
                    None
                }
            })
            .unwrap_or_else(|| {
                let buffer = MemoryRenderBuffer::from_slice(
                    &frame.pixels_rgba,
                    Fourcc::Abgr8888,
                    (frame.width as i32, frame.height as i32),
                    image_scale,
                    Transform::Normal,
                    None,
                );
                // Bound imported image retention across shapes, animation
                // frames and output scales. In-flight elements own clones.
                if pointer_images.len() >= 128 {
                    pointer_images.remove(0);
                }
                pointer_images.push((frame, image_scale, buffer.clone()));
                buffer
            });

        let result = render_surface(
            surface,
            &mut renderer,
            &self.space,
            &output,
            self.pointer.current_location(),
            &pointer_image,
            named_hotspot,
            &mut self.backend_data.pointer_element,
            &self.dnd_icon,
            &mut self.cursor_status,
            self.show_window_preview,
        );
        let reschedule = match result {
            Ok((has_rendered, direct_scanout, states)) => {
                let elapsed = start.elapsed();
                if has_rendered {
                    surface.performance.render.record(elapsed);
                    if direct_scanout {
                        surface.performance.direct_scanout_frames =
                            surface.performance.direct_scanout_frames.saturating_add(1);
                    } else {
                        surface.performance.composed_frames =
                            surface.performance.composed_frames.saturating_add(1);
                    }
                } else {
                    surface.performance.empty_frames =
                        surface.performance.empty_frames.saturating_add(1);
                }
                if has_rendered && input_generation != surface.last_input_generation {
                    if let Some(input_elapsed) = input_elapsed {
                        surface.performance.input_to_submit.record(input_elapsed);
                    }
                    surface.last_input_generation = input_generation;
                }
                if has_rendered
                    && scheduled_deadline
                    && (missed_before_render
                        || Duration::from(frame_target).saturating_sub(self.clock.now().into())
                            == Duration::ZERO)
                {
                    surface.performance.missed_deadlines =
                        surface.performance.missed_deadlines.saturating_add(1);
                }
                let dmabuf_feedback = surface.dmabuf_feedback.clone();
                self.post_repaint(&output, frame_target, dmabuf_feedback, &states);
                false // No damage: sleep until a commit, input, or desktop change.
            }
            Err(err) => {
                warn!("Error during rendering: {:#?}", err);
                match err {
                    SwapBuffersError::AlreadySwapped => false,
                    SwapBuffersError::TemporaryFailure(err) => match err.downcast_ref::<DrmError>()
                    {
                        Some(DrmError::DeviceInactive) => true,
                        Some(DrmError::Access(DrmAccessError { source, .. })) => {
                            source.kind() == io::ErrorKind::PermissionDenied
                        }
                        _ => false,
                    },
                    SwapBuffersError::ContextLost(err) => match err.downcast_ref::<DrmError>() {
                        Some(DrmError::TestFailed(_)) => {
                            // reset the complete state, disabling all connectors and planes in case we hit a test failed
                            // most likely we hit this after a tty switch when a foreign master changed CRTC <-> connector bindings
                            // and we run in a mismatch
                            device
                                .drm_output_manager
                                .device_mut()
                                .reset_state()
                                .expect("failed to reset drm device");
                            true
                        }
                        _ => panic!("Rendering loop lost: {err}"),
                    },
                }
            }
        };

        if reschedule {
            let output_refresh = match output.current_mode() {
                Some(mode) => mode.refresh,
                None => return,
            };

            // If reschedule is true we either hit a temporary failure or more likely rendering
            // did not cause any damage on the output. In this case we just re-schedule a repaint
            // after approx. one frame to re-test for damage.
            let next_frame_target =
                frame_target + Duration::from_millis(1_000_000 / output_refresh as u64);
            let reschedule_timeout =
                Duration::from(next_frame_target).saturating_sub(self.clock.now().into());
            trace!(
                "reschedule repaint timer with delay {:?} on {:?}",
                reschedule_timeout, crtc,
            );
            let timer = Timer::from_duration(reschedule_timeout);
            self.handle
                .insert_source(timer, move |_, _, data| {
                    data.render(node, Some(crtc), next_frame_target);
                    TimeoutAction::Drop
                })
                .expect("failed to schedule frame timer");
        } else {
            tracing::trace!(elapsed = ?start.elapsed(), "rendered surface");
        }

        profiling::finish_frame!();
    }
}

#[allow(clippy::too_many_arguments)]
#[profiling::function]
fn render_surface<'a>(
    surface: &'a mut SurfaceData,
    renderer: &mut UdevRenderer<'a>,
    space: &Space<WindowElement>,
    output: &Output,
    pointer_location: Point<f64, Logical>,
    pointer_image: &MemoryRenderBuffer,
    named_hotspot: Point<f64, Logical>,
    pointer_element: &mut PointerElement,
    dnd_icon: &Option<DndIcon>,
    cursor_status: &mut CursorImageStatus,
    show_window_preview: bool,
) -> Result<(bool, bool, RenderElementStates), SwapBuffersError> {
    let output_geometry = space.output_geometry(output).unwrap();
    let scale = Scale::from(output.current_scale().fractional_scale());

    let mut custom_elements: Vec<CustomRenderElements<_>> = Vec::new();

    if output_geometry.to_f64().contains(pointer_location) {
        let cursor_hotspot = if let CursorImageStatus::Surface(surface) = cursor_status {
            compositor::with_states(surface, |states| {
                states
                    .data_map
                    .get::<Mutex<CursorImageAttributes>>()
                    .unwrap()
                    .lock()
                    .unwrap()
                    .hotspot
                    .to_f64()
            })
        } else {
            named_hotspot
        };
        let cursor_pos = pointer_location - output_geometry.loc.to_f64();

        // set cursor
        pointer_element.set_buffer(pointer_image.clone());

        // draw the cursor as relevant
        {
            // reset the cursor if the surface is no longer alive
            let mut reset = false;
            if let CursorImageStatus::Surface(ref surface) = *cursor_status {
                reset = !surface.alive();
            }
            if reset {
                *cursor_status = CursorImageStatus::default_named();
            }

            pointer_element.set_status(cursor_status.clone());
        }

        custom_elements.extend(
            pointer_element.render_elements(
                renderer,
                (cursor_pos - cursor_hotspot)
                    .to_physical(scale)
                    .to_i32_round(),
                scale,
                1.0,
            ),
        );

        // draw the dnd icon if applicable
        {
            if let Some(icon) = dnd_icon.as_ref() {
                let dnd_icon_pos = (cursor_pos + icon.offset.to_f64())
                    .to_physical(scale)
                    .to_i32_round();
                if icon.surface.alive() {
                    custom_elements.extend(AsRenderElements::<UdevRenderer<'a>>::render_elements(
                        &SurfaceTree::from_surface(&icon.surface),
                        renderer,
                        dnd_icon_pos,
                        scale,
                        1.0,
                    ));
                }
            }
        }
    }

    #[cfg(feature = "debug")]
    if let Some(element) = surface.fps_element.as_mut() {
        element.update_fps(surface.fps.avg().round() as u32);
        surface.fps.tick();
        custom_elements.push(CustomRenderElements::Fps(element.clone()));
    }

    let (elements, clear_color) = output_elements(
        output,
        space,
        custom_elements,
        renderer,
        show_window_preview,
    );

    let frame_mode = if surface.disable_direct_scanout {
        FrameFlags::empty()
    } else {
        FrameFlags::DEFAULT
    };
    let (rendered, direct_scanout, states) = surface
        .drm_output
        .render_frame(renderer, &elements, clear_color, frame_mode)
        .map(|render_frame_result| {
            let direct_scanout = matches!(
                &render_frame_result.primary_element,
                PrimaryPlaneElement::Element(_)
            );
            #[cfg(feature = "renderer_sync")]
            if let PrimaryPlaneElement::Swapchain(element) = &render_frame_result.primary_element {
                element.sync.wait();
            }
            (
                !render_frame_result.is_empty,
                direct_scanout,
                render_frame_result.states,
            )
        })
        .map_err(|err| match err {
            smithay::backend::drm::compositor::RenderFrameError::PrepareFrame(err) => {
                SwapBuffersError::from(err)
            }
            smithay::backend::drm::compositor::RenderFrameError::RenderFrame(
                OutputDamageTrackerError::Rendering(err),
            ) => SwapBuffersError::from(err),
            _ => unreachable!(),
        })?;

    update_primary_scanout_output(space, output, dnd_icon, cursor_status, &states);

    if rendered {
        let output_presentation_feedback = take_presentation_feedback(output, space, &states);
        surface
            .drm_output
            .queue_frame(PresentedFrame {
                feedback: output_presentation_feedback,
                lock_generation: output
                    .user_data()
                    .get::<std::sync::Mutex<crate::lock::LockOutput>>()
                    .and_then(|lock| {
                        let lock = lock.lock().unwrap();
                        lock.locked.then_some(lock.generation)
                    }),
            })
            .map_err(Into::<SwapBuffersError>::into)?;
    }

    Ok((rendered, direct_scanout, states))
}
