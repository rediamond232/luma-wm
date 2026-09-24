#[cfg(feature = "xwayland")]
use std::os::unix::io::OwnedFd;
use std::{
    collections::HashMap,
    sync::{Arc, atomic::AtomicBool},
    time::{Duration, Instant},
};

use tracing::{info, warn};

use smithay::{
    backend::{
        input::TabletToolDescriptor,
        renderer::element::{
            RenderElementStates, default_primary_scanout_output_compare,
            utils::select_dmabuf_feedback,
        },
    },
    delegate_dispatch2,
    desktop::{
        PopupKind, PopupManager, Space,
        space::SpaceElement,
        utils::{
            OutputPresentationFeedback, surface_presentation_feedback_flags_from_states,
            surface_primary_scanout_output, update_surface_primary_scanout_output,
            with_surfaces_surface_tree,
        },
    },
    input::{
        Seat, SeatHandler, SeatState,
        dnd::{DnDGrab, DndGrabHandler, DndTarget, GrabType, Source},
        keyboard::{Keysym, LedState, XkbConfig},
        pointer::{CursorImageStatus, Focus, PointerHandle},
        tablet::TabletSeatHandler,
    },
    output::Output,
    reexports::{
        calloop::{
            Interest, LoopHandle, Mode, PostAction,
            generic::Generic,
            timer::{TimeoutAction, Timer},
        },
        wayland_protocols::xdg::decoration::{
            self as xdg_decoration,
            zv1::server::zxdg_toplevel_decoration_v1::Mode as DecorationMode,
        },
        wayland_server::{
            Client, Display, DisplayHandle, Resource,
            backend::{ClientData, ClientId, DisconnectReason},
            protocol::wl_surface::WlSurface,
        },
    },
    utils::{Clock, Logical, Monotonic, Point, Rectangle, Serial, Time},
    wayland::{
        commit_timing::{CommitTimerBarrierStateUserData, CommitTimingManagerState},
        compositor::{
            CompositorClientState, CompositorHandler, CompositorState, get_parent, with_states,
        },
        dmabuf::{DmabufFeedback, get_dmabuf},
        fifo::{FifoBarrierCachedState, FifoManagerState},
        fixes::FixesState,
        fractional_scale::{
            FractionalScaleHandler, FractionalScaleManagerState, with_fractional_scale,
        },
        image_capture_source::{
            ImageCaptureSource, ImageCaptureSourceHandler, ImageCaptureSourceState,
            OutputCaptureSourceHandler, OutputCaptureSourceState,
        },
        image_copy_capture::{
            BufferConstraints, Frame, FrameRef, ImageCopyCaptureHandler, ImageCopyCaptureState,
            Session, SessionRef,
        },
        input_method::{InputMethodHandler, PopupSurface},
        keyboard_shortcuts_inhibit::{
            KeyboardShortcutsInhibitHandler, KeyboardShortcutsInhibitState,
            KeyboardShortcutsInhibitor,
        },
        output::{OutputHandler, OutputManagerState},
        pointer_constraints::{
            ConstraintRemove, PointerConstraint, PointerConstraintsHandler,
            PointerConstraintsState, with_pointer_constraint,
        },
        pointer_gestures::PointerGesturesState,
        presentation::PresentationState,
        relative_pointer::RelativePointerManagerState,
        seat::WaylandFocus,
        security_context::{
            SecurityContext, SecurityContextHandler, SecurityContextListenerSource,
            SecurityContextState,
        },
        selection::{
            SelectionHandler,
            data_device::{
                DataDeviceHandler, DataDeviceState, WaylandDndGrabHandler, set_data_device_focus,
            },
            primary_selection::{
                PrimarySelectionHandler, PrimarySelectionState, set_primary_focus,
            },
            wlr_data_control::{DataControlHandler, DataControlState},
        },
        shell::{
            wlr_layer::WlrLayerShellState,
            xdg::{
                ToplevelSurface, XdgShellState,
                decoration::{XdgDecorationHandler, XdgDecorationState},
            },
        },
        shm::{ShmHandler, ShmState},
        single_pixel_buffer::SinglePixelBufferState,
        socket::ListeningSocketSource,
        tablet_manager::TabletManagerState,
        text_input::TextInputManagerState,
        viewporter::ViewporterState,
        xdg_activation::{
            XdgActivationHandler, XdgActivationState, XdgActivationToken, XdgActivationTokenData,
        },
        xdg_foreign::{XdgForeignHandler, XdgForeignState},
    },
};

#[cfg(feature = "xwayland")]
use crate::cursor::Cursor;
use crate::{
    focus::{KeyboardFocusTarget, PointerFocusTarget},
    shell::WindowElement,
};
#[cfg(feature = "xwayland")]
use smithay::{
    utils::Size,
    wayland::selection::{SelectionSource, SelectionTarget},
    wayland::xwayland_keyboard_grab::{XWaylandKeyboardGrabHandler, XWaylandKeyboardGrabState},
    wayland::xwayland_shell,
    xwayland::{X11Wm, XWayland, XWaylandEvent},
};

#[derive(Debug, Default)]
pub struct ClientState {
    pub compositor_state: CompositorClientState,
    pub security_context: Option<SecurityContext>,
}
impl ClientData for ClientState {
    /// Notification that a client was initialized
    fn initialized(&self, _client_id: ClientId) {}
    /// Notification that a client is disconnected
    fn disconnected(&self, _client_id: ClientId, _reason: DisconnectReason) {}
}

#[derive(Debug)]
pub struct AnvilState<BackendData: Backend + 'static> {
    pub backend_data: BackendData,
    pub socket_name: Option<String>,
    pub display_handle: DisplayHandle,
    pub running: Arc<AtomicBool>,
    pub handle: LoopHandle<'static, AnvilState<BackendData>>,

    // desktop
    pub space: Space<WindowElement>,
    pub desktop: crate::policy::Desktop,
    pub lock: crate::lock::LockState,
    pub session_lock_state: smithay::wayland::session_lock::SessionLockManagerState,
    pub popups: PopupManager,

    // smithay state
    pub compositor_state: CompositorState,
    pub data_device_state: DataDeviceState,
    pub layer_shell_state: WlrLayerShellState,
    pub output_manager_state: OutputManagerState,
    pub primary_selection_state: PrimarySelectionState,
    pub data_control_state: DataControlState,
    pub seat_state: SeatState<AnvilState<BackendData>>,
    pub keyboard_shortcuts_inhibit_state: KeyboardShortcutsInhibitState,
    pub shm_state: ShmState,
    pub viewporter_state: ViewporterState,
    pub xdg_activation_state: XdgActivationState,
    pub xdg_decoration_state: XdgDecorationState,
    pub xdg_shell_state: XdgShellState,
    pub presentation_state: PresentationState,
    pub fractional_scale_manager_state: FractionalScaleManagerState,
    pub xdg_foreign_state: XdgForeignState,
    #[cfg(feature = "xwayland")]
    pub xwayland_shell_state: xwayland_shell::XWaylandShellState,
    pub single_pixel_buffer_state: SinglePixelBufferState,
    pub fifo_manager_state: FifoManagerState,
    pub commit_timing_manager_state: CommitTimingManagerState,
    pub image_capture_source_state: ImageCaptureSourceState,
    pub output_capture_source_state: OutputCaptureSourceState,
    pub image_copy_capture_state: ImageCopyCaptureState,
    pub(crate) pending_screencopies: Vec<crate::screencopy::PendingFrame>,
    pub(crate) pending_capture_frames: Vec<PendingCaptureFrame>,
    pub capture_sessions: Vec<Session>,
    pub cursor_shape_state: smithay::wayland::cursor_shape::CursorShapeManagerState,
    pub(crate) capture_cursor: crate::capture::CaptureCursor,
    pub(crate) capture_generation: u64,
    pub(crate) capture_boost_fps: Option<u32>,
    pub(crate) capture_boost_timer: Option<smithay::reexports::calloop::RegistrationToken>,
    capture_recorder_session_pending: bool,
    pub(crate) capture_override: RecorderCaptureSource,
    /// Complete recorder frames only after the selected client commits a new
    /// buffer. The boost timer may still send frame callbacks at the requested
    /// rate, but it must not manufacture captures from an unchanged surface.
    pub(crate) capture_commit_driven: bool,

    pub dnd_icon: Option<DndIcon>,

    // input-related fields
    pub suppressed_keys: Vec<Keysym>,
    pub super_tap_pending: bool,
    pub super_tap_used: bool,
    pub cursor_status: CursorImageStatus,
    pub seat_name: String,
    pub seat: Seat<AnvilState<BackendData>>,
    pub clock: Clock<Monotonic>,
    pub pointer: PointerHandle<AnvilState<BackendData>>,
    pub cursor_position_hint: Option<(WlSurface, Point<f64, Logical>)>,

    #[cfg(feature = "xwayland")]
    pub xwm: Option<X11Wm>,
    #[cfg(feature = "xwayland")]
    pub xdisplay: Option<u32>,

    #[cfg(feature = "debug")]
    pub renderdoc: Option<renderdoc::RenderDoc<renderdoc::V141>>,

    pub show_window_preview: bool,
}

#[derive(Debug, Default)]
struct CaptureSessionState {
    has_captured: AtomicBool,
    last_generation: std::sync::atomic::AtomicU64,
    recorder: bool,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) enum RecorderCaptureSource {
    #[default]
    Output,
    Window(u64),
    Region(wm_core::Rect),
}

#[derive(Debug)]
pub(crate) struct PendingCaptureFrame {
    session: SessionRef,
    frame: Frame,
}

#[derive(Debug)]
pub struct DndIcon {
    pub surface: WlSurface,
    pub offset: Point<i32, Logical>,
}

impl<BackendData: Backend> DataDeviceHandler for AnvilState<BackendData> {
    fn data_device_state(&mut self) -> &mut DataDeviceState {
        &mut self.data_device_state
    }
}

impl<BackendData: Backend> WaylandDndGrabHandler for AnvilState<BackendData> {
    fn dnd_requested<S: Source>(
        &mut self,
        source: S,
        icon: Option<WlSurface>,
        seat: Seat<Self>,
        serial: Serial,
        type_: GrabType,
    ) {
        self.dnd_icon = icon.map(|surface| DndIcon {
            surface,
            offset: (0, 0).into(),
        });

        match type_ {
            GrabType::Pointer => {
                let pointer = seat.get_pointer().unwrap();
                let start_data = pointer.grab_start_data().unwrap();
                pointer.set_grab(
                    self,
                    DnDGrab::new_pointer(&self.display_handle, start_data, source, seat),
                    serial,
                    Focus::Keep,
                );
            }
            GrabType::Touch => {
                let touch = seat.get_touch().unwrap();
                let start_data = touch.grab_start_data().unwrap();
                touch.set_grab(
                    self,
                    DnDGrab::new_touch(&self.display_handle, start_data, source, seat),
                    serial,
                );
            }
        }
    }
}

impl<BackendData: Backend> DndGrabHandler for AnvilState<BackendData> {
    fn dropped(
        &mut self,
        _target: Option<DndTarget<'_, Self>>,
        _validated: bool,
        _seat: Seat<Self>,
        _location: Point<f64, Logical>,
    ) {
        self.dnd_icon = None;
    }
}

impl<BackendData: Backend> OutputHandler for AnvilState<BackendData> {}

impl<BackendData: Backend> SelectionHandler for AnvilState<BackendData> {
    type SelectionUserData = ();

    #[cfg(feature = "xwayland")]
    fn new_selection(
        &mut self,
        ty: SelectionTarget,
        source: Option<SelectionSource>,
        _seat: Seat<Self>,
    ) {
        if let Some(xwm) = self.xwm.as_mut() {
            if let Err(err) = xwm.new_selection(ty, source.map(|source| source.mime_types())) {
                warn!(?err, ?ty, "Failed to set Xwayland selection");
            }
        }
    }

    #[cfg(feature = "xwayland")]
    fn send_selection(
        &mut self,
        ty: SelectionTarget,
        mime_type: String,
        fd: OwnedFd,
        _seat: Seat<Self>,
        _user_data: &(),
    ) {
        if let Some(xwm) = self.xwm.as_mut() {
            if let Err(err) = xwm.send_selection(ty, mime_type, fd) {
                warn!(?err, "Failed to send primary (X11 -> Wayland)");
            }
        }
    }
}

impl<BackendData: Backend> PrimarySelectionHandler for AnvilState<BackendData> {
    fn primary_selection_state(&mut self) -> &mut PrimarySelectionState {
        &mut self.primary_selection_state
    }
}

impl<BackendData: Backend> DataControlHandler for AnvilState<BackendData> {
    fn data_control_state(&mut self) -> &mut DataControlState {
        &mut self.data_control_state
    }
}

impl<BackendData: Backend> ShmHandler for AnvilState<BackendData> {
    fn shm_state(&self) -> &ShmState {
        &self.shm_state
    }
}

impl<BackendData: Backend> SeatHandler for AnvilState<BackendData> {
    type KeyboardFocus = KeyboardFocusTarget;
    type PointerFocus = PointerFocusTarget;
    type TouchFocus = PointerFocusTarget;

    fn seat_state(&mut self) -> &mut SeatState<AnvilState<BackendData>> {
        &mut self.seat_state
    }

    fn focus_changed(&mut self, seat: &Seat<Self>, target: Option<&KeyboardFocusTarget>) {
        let dh = &self.display_handle;

        let wl_surface = target
            .filter(|_| !self.lock.locked)
            .and_then(WaylandFocus::wl_surface);

        let focus = wl_surface.and_then(|s| dh.get_client(s.id()).ok());
        set_data_device_focus(dh, seat, focus.clone());
        set_primary_focus(dh, seat, focus);
    }
    fn cursor_image(&mut self, _seat: &Seat<Self>, image: CursorImageStatus) {
        self.cursor_status = image;
        self.desktop.redraw = true;
    }

    fn led_state_changed(&mut self, _seat: &Seat<Self>, led_state: LedState) {
        self.backend_data.update_led_state(led_state)
    }
}

impl<BackendData: Backend> TabletSeatHandler for AnvilState<BackendData> {
    type ToolFocus = PointerFocusTarget;

    fn tablet_tool_image(&mut self, _tool: &TabletToolDescriptor, image: CursorImageStatus) {
        // TODO: tablet tools should have their own cursors
        self.cursor_status = image;
    }
}

impl<BackendData: Backend> InputMethodHandler for AnvilState<BackendData> {
    fn new_popup(&mut self, surface: PopupSurface) {
        if let Err(err) = self.popups.track_popup(PopupKind::from(surface)) {
            warn!("Failed to track popup: {}", err);
        }
    }

    fn popup_repositioned(&mut self, _: PopupSurface) {}

    fn dismiss_popup(&mut self, surface: PopupSurface) {
        if let Some(parent) = surface.get_parent().map(|parent| parent.surface.clone()) {
            let _ = PopupManager::dismiss_popup(&parent, &PopupKind::from(surface));
        }
    }

    fn parent_geometry(&self, parent: &WlSurface) -> Rectangle<i32, smithay::utils::Logical> {
        self.space
            .elements()
            .find_map(|window| {
                (window.wl_surface().as_deref() == Some(parent)).then(|| window.geometry())
            })
            .unwrap_or_default()
    }
}

impl<BackendData: Backend> KeyboardShortcutsInhibitHandler for AnvilState<BackendData> {
    fn keyboard_shortcuts_inhibit_state(&mut self) -> &mut KeyboardShortcutsInhibitState {
        &mut self.keyboard_shortcuts_inhibit_state
    }

    fn new_inhibitor(&mut self, inhibitor: KeyboardShortcutsInhibitor) {
        // Just grant the wish for everyone
        inhibitor.activate();
    }
}

impl<BackendData: Backend> PointerConstraintsHandler for AnvilState<BackendData> {
    fn new_constraint(&mut self, surface: &WlSurface, pointer: &PointerHandle<Self>) {
        // XXX region
        let Some(current_focus) = pointer.current_focus() else {
            return;
        };
        if current_focus.wl_surface().as_deref() == Some(surface) {
            with_pointer_constraint(surface, pointer, |constraint| {
                constraint.unwrap().activate();
            });
        }
    }

    fn remove_constraint(
        &mut self,
        _surface: &WlSurface,
        pointer: &PointerHandle<Self>,
        constraint_remove: ConstraintRemove,
    ) {
        // Clear cursor_position_hint to prevent a oneshot PointerLocked constraint
        // from causing this function to be called again during PointerLeave and
        // unexpectedly changing the cursor position.
        let Some((hint_surface, hint_location)) = self.cursor_position_hint.take() else {
            return;
        };

        match constraint_remove {
            ConstraintRemove::Destroyed(pointer_constraint) => match pointer_constraint {
                PointerConstraint::Confined(_confined_pointer) => return,
                PointerConstraint::Locked(locked_pointer) => {
                    let origin = self
                        .space
                        .elements()
                        .find_map(|window| {
                            (window.wl_surface().as_deref() == Some(&hint_surface))
                                .then(|| window.geometry())
                        })
                        .unwrap_or_default()
                        .loc
                        .to_f64();

                    let surface_location = origin + hint_location;
                    if let Some(region) = locked_pointer.region()
                        && region.contains(hint_location.to_i32_floor())
                    {
                        pointer.set_location(surface_location);
                    } else {
                        pointer.set_location(surface_location);
                    }
                }
            },
            ConstraintRemove::PointerLeave(_region) => return,
        }
    }

    fn cursor_position_hint(
        &mut self,
        surface: &WlSurface,
        pointer: &PointerHandle<Self>,
        location: Point<f64, Logical>,
    ) {
        if with_pointer_constraint(surface, pointer, |constraint| {
            constraint.is_some_and(|c| c.is_active())
        }) {
            self.cursor_position_hint = Some((surface.clone(), location));
        }
    }
}

impl<BackendData: Backend> XdgActivationHandler for AnvilState<BackendData> {
    fn activation_state(&mut self) -> &mut XdgActivationState {
        &mut self.xdg_activation_state
    }

    fn token_created(&mut self, _token: XdgActivationToken, data: XdgActivationTokenData) -> bool {
        if let Some((serial, seat)) = data.serial {
            let keyboard = self.seat.get_keyboard().unwrap();
            Seat::from_resource(&seat) == Some(self.seat.clone())
                && keyboard
                    .last_enter()
                    .map(|last_enter| serial.is_no_older_than(&last_enter))
                    .unwrap_or(false)
        } else {
            false
        }
    }

    fn request_activation(
        &mut self,
        _token: XdgActivationToken,
        token_data: XdgActivationTokenData,
        surface: WlSurface,
    ) {
        if token_data.timestamp.elapsed().as_secs() < 10 {
            // Just grant the wish
            let w = self
                .space
                .elements()
                .find(|window| window.wl_surface().map(|s| *s == surface).unwrap_or(false))
                .cloned();
            if let Some(window) = w {
                self.space.raise_element(&window, true);
            }
        }
    }
}

impl<BackendData: Backend> XdgDecorationHandler for AnvilState<BackendData> {
    fn new_decoration(&mut self, toplevel: ToplevelSurface) {
        use xdg_decoration::zv1::server::zxdg_toplevel_decoration_v1::Mode;
        // Set the default to client side
        toplevel.with_pending_state(|state| {
            state.decoration_mode = Some(Mode::ClientSide);
        });
    }
    fn request_mode(&mut self, toplevel: ToplevelSurface, mode: DecorationMode) {
        use xdg_decoration::zv1::server::zxdg_toplevel_decoration_v1::Mode;

        toplevel.with_pending_state(|state| {
            state.decoration_mode = Some(match mode {
                DecorationMode::ServerSide => Mode::ServerSide,
                _ => Mode::ClientSide,
            });
        });

        if toplevel.is_initial_configure_sent() {
            toplevel.send_pending_configure();
        }
    }
    fn unset_mode(&mut self, toplevel: ToplevelSurface) {
        use xdg_decoration::zv1::server::zxdg_toplevel_decoration_v1::Mode;
        toplevel.with_pending_state(|state| {
            state.decoration_mode = Some(Mode::ClientSide);
        });

        if toplevel.is_initial_configure_sent() {
            toplevel.send_pending_configure();
        }
    }
}

impl<BackendData: Backend> FractionalScaleHandler for AnvilState<BackendData> {
    fn new_fractional_scale(
        &mut self,
        surface: smithay::reexports::wayland_server::protocol::wl_surface::WlSurface,
    ) {
        // Here we can set the initial fractional scale
        //
        // First we look if the surface already has a primary scan-out output, if not
        // we test if the surface is a subsurface and try to use the primary scan-out output
        // of the root surface. If the root also has no primary scan-out output we just try
        // to use the first output of the toplevel.
        // If the surface is the root we also try to use the first output of the toplevel.
        //
        // If all the above tests do not lead to a output we just use the first output
        // of the space (which in case of anvil will also be the output a toplevel will
        // initially be placed on)
        #[allow(clippy::redundant_clone)]
        let mut root = surface.clone();
        while let Some(parent) = get_parent(&root) {
            root = parent;
        }

        with_states(&surface, |states| {
            let primary_scanout_output = surface_primary_scanout_output(&surface, states)
                .or_else(|| {
                    if root != surface {
                        with_states(&root, |states| {
                            surface_primary_scanout_output(&root, states).or_else(|| {
                                self.window_for_surface(&root).and_then(|window| {
                                    self.space.outputs_for_element(&window).first().cloned()
                                })
                            })
                        })
                    } else {
                        self.window_for_surface(&root).and_then(|window| {
                            self.space.outputs_for_element(&window).first().cloned()
                        })
                    }
                })
                .or_else(|| self.space.outputs().next().cloned());
            if let Some(output) = primary_scanout_output {
                with_fractional_scale(states, |fractional_scale| {
                    fractional_scale.set_preferred_scale(output.current_scale().fractional_scale());
                });
            }
        });
    }
}

impl<BackendData: Backend + 'static> SecurityContextHandler for AnvilState<BackendData> {
    fn context_created(
        &mut self,
        source: SecurityContextListenerSource,
        security_context: SecurityContext,
    ) {
        self.handle
            .insert_source(source, move |client_stream, _, data| {
                let client_state = ClientState {
                    security_context: Some(security_context.clone()),
                    ..ClientState::default()
                };
                if let Err(err) = data
                    .display_handle
                    .insert_client(client_stream, Arc::new(client_state))
                {
                    warn!("Error adding wayland client: {}", err);
                };
            })
            .expect("Failed to init wayland socket source");
    }
}

#[cfg(feature = "xwayland")]
impl<BackendData: Backend + 'static> XWaylandKeyboardGrabHandler for AnvilState<BackendData> {
    fn keyboard_focus_for_xsurface(&self, surface: &WlSurface) -> Option<KeyboardFocusTarget> {
        if self.lock.locked {
            return None;
        }
        let elem = self
            .space
            .elements()
            .find(|elem| elem.wl_surface().as_deref() == Some(surface))?;
        Some(KeyboardFocusTarget::Window(elem.0.clone()))
    }
}

impl<BackendData: Backend> XdgForeignHandler for AnvilState<BackendData> {
    fn xdg_foreign_state(&mut self) -> &mut XdgForeignState {
        &mut self.xdg_foreign_state
    }
}

impl<BackendData: Backend> ImageCaptureSourceHandler for AnvilState<BackendData> {
    fn source_destroyed(&mut self, _source: ImageCaptureSource) {
        // Anvil doesn't track sources
    }
}

impl<BackendData: Backend> OutputCaptureSourceHandler for AnvilState<BackendData> {
    fn output_capture_source_state(&mut self) -> &mut OutputCaptureSourceState {
        &mut self.output_capture_source_state
    }

    fn output_source_created(&mut self, source: ImageCaptureSource, output: &Output) {
        source.user_data().insert_if_missing(|| output.downgrade());
    }
}

impl<BackendData: Backend> ImageCopyCaptureHandler for AnvilState<BackendData> {
    fn image_copy_capture_state(&mut self) -> &mut ImageCopyCaptureState {
        &mut self.image_copy_capture_state
    }

    fn capture_constraints(&mut self, source: &ImageCaptureSource) -> Option<BufferConstraints> {
        if self.lock.locked || !self.desktop.active {
            return None;
        }
        use smithay::output::WeakOutput;
        let weak_output = source.user_data().get::<WeakOutput>()?;
        let output = weak_output.upgrade()?;
        if !self.space.outputs().any(|mapped| mapped == &output) {
            return None;
        }
        let mode = output.current_mode()?;

        Some(BufferConstraints {
            size: output
                .current_transform()
                .transform_size(mode.size)
                .to_logical(1)
                .to_buffer(1, smithay::utils::Transform::Normal),
            shm: vec![
                smithay::reexports::wayland_server::protocol::wl_shm::Format::Argb8888,
                smithay::reexports::wayland_server::protocol::wl_shm::Format::Xrgb8888,
            ],
            #[cfg(any(feature = "udev", feature = "winit", feature = "x11"))]
            dma: self.backend_data.capture_dmabuf_constraints(&output),
        })
    }

    fn new_session(&mut self, session: Session) {
        // Session owns protocol lifetime: dropping it sends stopped and fails
        // pending frames. Bound client-owned sessions before accepting more.
        if self.capture_sessions.len() < 32 && self.capture_constraints(&session.source()).is_some()
        {
            session
                .user_data()
                .insert_if_missing(|| CaptureSessionState {
                    recorder: self.capture_recorder_session_pending,
                    ..CaptureSessionState::default()
                });
            self.capture_recorder_session_pending = false;
            self.capture_sessions.push(session);
        }
    }

    fn session_destroyed(&mut self, session: SessionRef) {
        self.pending_capture_frames
            .retain(|pending| pending.session != session);
        self.capture_sessions.retain(|owned| owned != &session);
    }

    fn frame(&mut self, session: &SessionRef, frame: Frame) {
        let has_captured = session
            .user_data()
            .get::<CaptureSessionState>()
            .is_some_and(|state| {
                state
                    .has_captured
                    .load(std::sync::atomic::Ordering::Acquire)
            });
        if has_captured {
            if self.pending_capture_frames.len() >= 32 {
                frame.fail(smithay::wayland::image_copy_capture::CaptureFailureReason::Unknown);
            } else {
                self.pending_capture_frames.push(PendingCaptureFrame {
                    session: session.clone(),
                    frame,
                });
            }
            return;
        }
        self.complete_capture_frame(session, frame);
    }

    fn frame_aborted(&mut self, frame: FrameRef) {
        self.pending_capture_frames
            .retain(|pending| pending.frame != frame);
    }
}

delegate_dispatch2!(@<BackendData: Backend + 'static> AnvilState<BackendData>);

impl<BackendData: Backend + 'static> AnvilState<BackendData> {
    fn complete_capture_frame(&mut self, session: &SessionRef, frame: Frame) {
        if self.capture_constraints(&session.source()).is_none() {
            frame.fail(smithay::wayland::image_copy_capture::CaptureFailureReason::Stopped);
            return;
        }
        let source = session.source();
        let output = source
            .user_data()
            .get::<smithay::output::WeakOutput>()
            .and_then(|o| o.upgrade())
            .unwrap();
        let mode = output.current_mode().unwrap();
        let size = output.current_transform().transform_size(mode.size);
        let buffer = frame.buffer();
        let dmabuf = get_dmabuf(&buffer).ok().cloned();
        if dmabuf.is_none() {
            if let Err(error) = crate::capture::validate(&buffer, size.w, size.h) {
                tracing::debug!(%error, "rejected capture buffer");
                frame.fail(
                    smithay::wayland::image_copy_capture::CaptureFailureReason::BufferConstraints,
                );
                return;
            }
        }
        if session.draw_cursor() {
            self.capture_cursor.prepare(
                self.cursor_status.clone(),
                self.pointer.current_location(),
                self.space.output_geometry(&output).unwrap(),
                &output,
                self.clock.now().into(),
            );
        }
        let cursor = session.draw_cursor().then_some(&self.capture_cursor);
        let recorder = session
            .user_data()
            .get::<CaptureSessionState>()
            .is_some_and(|state| state.recorder);
        let result =
            if recorder && self.capture_override != RecorderCaptureSource::Output {
                let Some(dmabuf) = dmabuf else {
                    frame.fail(
                    smithay::wayland::image_copy_capture::CaptureFailureReason::BufferConstraints,
                );
                    return;
                };
                match self.capture_override {
                    RecorderCaptureSource::Window(id) => {
                        let Some(window) = self
                            .desktop
                            .windows
                            .iter()
                            .find(|managed| managed.id == id)
                            .map(|managed| managed.window.clone())
                        else {
                            frame.fail(
                                smithay::wayland::image_copy_capture::CaptureFailureReason::Stopped,
                            );
                            return;
                        };
                        self.backend_data
                            .capture_window_dmabuf(&window, &output, dmabuf)
                    }
                    RecorderCaptureSource::Region(region) => self
                        .backend_data
                        .capture_region_dmabuf(&self.space, &output, cursor, region, dmabuf),
                    RecorderCaptureSource::Output => unreachable!(),
                }
            } else if let Some(dmabuf) = dmabuf {
                tracing::debug!(output = %output.name(), "capturing output into DMA-BUF");
                self.backend_data
                    .capture_output_dmabuf(&self.space, &output, cursor, dmabuf)
            } else {
                tracing::debug!(output = %output.name(), "capturing output into shared memory");
                self.backend_data
                    .capture_output(&self.space, &output, cursor)
                    .and_then(|pixels| crate::capture::write(&buffer, size.w, size.h, &pixels))
            };
        match result {
            Ok(()) => {
                if recorder {
                    self.desktop.recorder.note_source_frame();
                }
                if let Some(state) = session.user_data().get::<CaptureSessionState>() {
                    state
                        .has_captured
                        .store(true, std::sync::atomic::Ordering::Release);
                    state.last_generation.store(
                        self.capture_generation,
                        std::sync::atomic::Ordering::Release,
                    );
                }
                let damage = smithay::utils::Rectangle::<i32, smithay::utils::Buffer>::from_size(
                    (size.w, size.h).into(),
                );
                frame.success(
                    smithay::utils::Transform::Normal,
                    Some(vec![damage]),
                    self.clock.now(),
                );
            }
            Err(error) => {
                tracing::warn!(%error, "capture failed");
                frame.fail(smithay::wayland::image_copy_capture::CaptureFailureReason::Unknown);
            }
        }
    }

    pub(crate) fn process_pending_capture_frames(&mut self, output: &smithay::output::Output) {
        let pending = std::mem::take(&mut self.pending_capture_frames);
        for pending_frame in pending {
            let matches_output = pending_frame
                .session
                .source()
                .user_data()
                .get::<smithay::output::WeakOutput>()
                .and_then(|weak| weak.upgrade())
                .is_some_and(|captured| captured == *output);
            if matches_output {
                let changed = pending_frame
                    .session
                    .user_data()
                    .get::<CaptureSessionState>()
                    .is_none_or(|state| {
                        if state.recorder {
                            // Recorder requests may be completed by either a real
                            // output repaint or the capture boost. Real repaints
                            // carry useful game commits and materially increase
                            // source throughput above the boost-only path.
                            return true;
                        }
                        let current = self.capture_generation;
                        state
                            .last_generation
                            .load(std::sync::atomic::Ordering::Acquire)
                            != current
                    });
                if changed {
                    self.complete_capture_frame(&pending_frame.session, pending_frame.frame);
                } else {
                    self.pending_capture_frames.push(pending_frame);
                }
            } else {
                self.pending_capture_frames.push(pending_frame);
            }
        }
    }

    pub(crate) fn start_capture_boost(&mut self, fps: u32) -> Result<(), String> {
        let fps = fps.clamp(30, 480);
        if let Some(token) = self.capture_boost_timer.take() {
            self.handle.remove(token);
        }
        let interval = Duration::from_secs_f64(1.0 / f64::from(fps));
        let token = self
            .handle
            .insert_source(Timer::from_duration(interval), move |deadline, _, state| {
                state.capture_boost_tick(interval);
                let scheduled = deadline + interval;
                let now = Instant::now();
                TimeoutAction::ToInstant(if now.saturating_duration_since(scheduled) > interval {
                    now + interval
                } else {
                    scheduled
                })
            })
            .map_err(|error| error.to_string())?;
        self.capture_boost_fps = Some(fps);
        self.capture_recorder_session_pending = true;
        self.capture_boost_timer = Some(token);
        Ok(())
    }

    pub(crate) fn stop_capture_boost(&mut self) {
        if let Some(token) = self.capture_boost_timer.take() {
            self.handle.remove(token);
        }
        self.capture_boost_fps = None;
        self.capture_recorder_session_pending = false;
        self.capture_override = RecorderCaptureSource::Output;
        self.capture_commit_driven = false;
    }

    fn capture_boost_tick(&mut self, interval: Duration) {
        if self.lock.locked || !self.desktop.active || self.pending_capture_frames.is_empty() {
            return;
        }
        let now: Duration = self.clock.now().into();
        let outputs: Vec<_> = self
            .space
            .outputs()
            .filter(|output| {
                self.pending_capture_frames.iter().any(|pending| {
                    pending
                        .session
                        .source()
                        .user_data()
                        .get::<smithay::output::WeakOutput>()
                        .and_then(|weak| weak.upgrade())
                        .is_some_and(|captured| captured == **output)
                })
            })
            .cloned()
            .collect();
        let target_window = match self.capture_override {
            RecorderCaptureSource::Window(id) => self
                .desktop
                .windows
                .iter()
                .find(|managed| managed.id == id)
                .map(|managed| managed.window.clone()),
            _ => None,
        };
        for output in outputs {
            self.pre_capture(&output, target_window.as_ref(), now + interval);
            self.space.elements().for_each(|window| {
                if self.space.outputs_for_element(window).contains(&output)
                    && target_window.as_ref().is_none_or(|target| target == window)
                {
                    window.send_frame(&output, now, None, surface_primary_scanout_output);
                }
            });
            if !self.capture_commit_driven {
                self.process_pending_capture_frames(&output);
            }
        }
    }

    pub(crate) fn process_commit_driven_capture(&mut self, window: &WindowElement) {
        if !self.capture_commit_driven || self.pending_capture_frames.is_empty() {
            return;
        }
        let RecorderCaptureSource::Window(target_id) = self.capture_override else {
            return;
        };
        let Some(managed) = self
            .desktop
            .windows
            .iter()
            .find(|managed| managed.id == target_id && managed.window == *window)
        else {
            return;
        };
        let output_name = managed.output.clone();
        let Some(output) = self
            .space
            .outputs()
            .find(|output| output.name() == output_name)
            .cloned()
        else {
            return;
        };
        let Some(index) = self.pending_capture_frames.iter().position(|pending| {
            let matches_output = pending
                .session
                .source()
                .user_data()
                .get::<smithay::output::WeakOutput>()
                .and_then(|weak| weak.upgrade())
                .is_some_and(|captured| captured == output);
            let recorder = pending
                .session
                .user_data()
                .get::<CaptureSessionState>()
                .is_some_and(|state| state.recorder);
            matches_output && recorder
        }) else {
            return;
        };
        let pending = self.pending_capture_frames.remove(index);
        self.complete_capture_frame(&pending.session, pending.frame);
    }

    fn pre_capture(
        &mut self,
        output: &Output,
        target_window: Option<&WindowElement>,
        frame_target: impl Into<Time<Monotonic>>,
    ) {
        let frame_target = frame_target.into();
        #[allow(clippy::mutable_key_type)]
        let mut clients: HashMap<ClientId, Client> = HashMap::new();
        self.space.elements().for_each(|window| {
            if !self.space.outputs_for_element(window).contains(output)
                || target_window.is_some_and(|target| target != window)
            {
                return;
            }
            window.with_surfaces(|surface, states| {
                if let Some(mut commit_timer_state) = states
                    .data_map
                    .get::<CommitTimerBarrierStateUserData>()
                    .map(|commit_timer| commit_timer.lock().unwrap())
                {
                    commit_timer_state.signal_until(frame_target);
                    if let Some(client) = surface.client() {
                        clients.insert(client.id(), client);
                    }
                }
                if let Some(fifo_barrier) = states
                    .cached_state
                    .get::<FifoBarrierCachedState>()
                    .current()
                    .barrier
                    .take()
                {
                    fifo_barrier.signal();
                    if let Some(client) = surface.client() {
                        clients.insert(client.id(), client);
                    }
                }
            });
        });
        let dh = self.display_handle.clone();
        for client in clients.into_values() {
            self.client_compositor_state(&client)
                .blocker_cleared(self, &dh);
        }
    }

    pub fn refresh_capture_sessions(&mut self) {
        let mut sessions = std::mem::take(&mut self.capture_sessions);
        sessions.retain(|session| {
            if let Some(constraints) = self.capture_constraints(&session.source()) {
                if session
                    .current_constraints()
                    .is_none_or(|old| old.size != constraints.size)
                {
                    session.update_constraints(constraints);
                }
                true
            } else {
                // Dropping invalid sessions stops them and fails pending frames.
                false
            }
        });
        self.capture_sessions = sessions;
        let pending = std::mem::take(&mut self.pending_capture_frames);
        for pending_frame in pending {
            if self
                .capture_constraints(&pending_frame.session.source())
                .is_some()
            {
                self.pending_capture_frames.push(pending_frame);
            } else {
                pending_frame
                    .frame
                    .fail(smithay::wayland::image_copy_capture::CaptureFailureReason::Stopped);
            }
        }
        self.image_copy_capture_state.cleanup();
    }

    pub fn init(
        display: Display<AnvilState<BackendData>>,
        handle: LoopHandle<'static, AnvilState<BackendData>>,
        backend_data: BackendData,
        listen_on_socket: bool,
    ) -> AnvilState<BackendData> {
        let dh = display.handle();

        let clock = Clock::new();
        let session_lock_state = smithay::wayland::session_lock::SessionLockManagerState::new::<
            Self,
            _,
        >(&dh, |_| BackendData::SUPPORTS_SESSION_LOCK);

        // init wayland clients
        let socket_name = if listen_on_socket {
            let source = ListeningSocketSource::new_auto().unwrap();
            let socket_name = source.socket_name().to_string_lossy().into_owned();
            handle
                .insert_source(source, |client_stream, _, data| {
                    if let Err(err) = data
                        .display_handle
                        .insert_client(client_stream, Arc::new(ClientState::default()))
                    {
                        warn!("Error adding wayland client: {}", err);
                    };
                })
                .expect("Failed to init wayland socket source");
            info!(name = socket_name, "Listening on wayland socket");
            Some(socket_name)
        } else {
            None
        };
        handle
            .insert_source(
                Generic::new(display, Interest::READ, Mode::Level),
                |_, display, data| {
                    profiling::scope!("dispatch_clients");
                    // Safety: we don't drop the display
                    unsafe {
                        display.get_mut().dispatch_clients(data).unwrap();
                    }
                    Ok(PostAction::Continue)
                },
            )
            .expect("Failed to init wayland server source");

        // init globals
        let compositor_state = CompositorState::new::<Self>(&dh);
        let data_device_state = DataDeviceState::new::<Self>(&dh);
        let layer_shell_state = WlrLayerShellState::new::<Self>(&dh);
        let output_manager_state = OutputManagerState::new_with_xdg_output::<Self>(&dh);
        let primary_selection_state = PrimarySelectionState::new::<Self>(&dh);
        let data_control_state =
            DataControlState::new::<Self, _>(&dh, Some(&primary_selection_state), |_| true);
        let mut seat_state = SeatState::new();
        let shm_state = ShmState::new::<Self>(&dh, vec![]);
        let viewporter_state = ViewporterState::new::<Self>(&dh);
        let xdg_activation_state = XdgActivationState::new::<Self>(&dh);
        let xdg_decoration_state = XdgDecorationState::new::<Self>(&dh);
        let xdg_shell_state = XdgShellState::new::<Self>(&dh);
        let presentation_state = PresentationState::new::<Self>(&dh, clock.id() as u32);
        let fractional_scale_manager_state = FractionalScaleManagerState::new::<Self>(&dh);
        let xdg_foreign_state = XdgForeignState::new::<Self>(&dh);
        let single_pixel_buffer_state = SinglePixelBufferState::new::<Self>(&dh);
        let fifo_manager_state = FifoManagerState::new::<Self>(&dh);
        let commit_timing_manager_state = CommitTimingManagerState::new::<Self>(&dh);
        TextInputManagerState::new::<Self>(&dh);
        // Privileged input injection must only be exposed to authenticated helpers.
        // Until a trusted-helper launch path exists, do not publish these globals.
        // Expose global only if backend supports relative motion events
        if BackendData::HAS_RELATIVE_MOTION {
            RelativePointerManagerState::new::<Self>(&dh);
        }
        PointerConstraintsState::new::<Self>(&dh);
        if BackendData::HAS_GESTURES {
            PointerGesturesState::new::<Self>(&dh);
        }
        TabletManagerState::new::<Self>(&dh);
        SecurityContextState::new::<Self, _>(&dh, |client| {
            client
                .get_data::<ClientState>()
                .is_none_or(|client_state| client_state.security_context.is_none())
        });
        FixesState::new::<Self>(&dh);

        // Image capture protocols (screencopy)
        let cursor_shape_state =
            smithay::wayland::cursor_shape::CursorShapeManagerState::new::<Self>(&dh);
        let image_capture_source_state = ImageCaptureSourceState::new();
        let output_capture_source_state = OutputCaptureSourceState::new::<Self>(&dh);
        let image_copy_capture_state = ImageCopyCaptureState::new::<Self>(&dh);
        crate::screencopy::init::<Self>(&dh);

        // init input
        let seat_name = backend_data.seat_name();
        let mut seat = seat_state.new_wl_seat(&dh, seat_name.clone());

        let pointer = seat.add_pointer();
        seat.add_keyboard(XkbConfig::default(), 200, 25)
            .expect("Failed to initialize the keyboard");

        let keyboard_shortcuts_inhibit_state = KeyboardShortcutsInhibitState::new::<Self>(&dh);

        #[cfg(feature = "xwayland")]
        let xwayland_shell_state = xwayland_shell::XWaylandShellState::new::<Self>(&dh.clone());

        #[cfg(feature = "xwayland")]
        XWaylandKeyboardGrabState::new::<Self>(&dh.clone());

        AnvilState {
            backend_data,
            display_handle: dh,
            socket_name,
            running: Arc::new(AtomicBool::new(true)),
            handle,
            space: Space::default(),
            desktop: crate::policy::Desktop::default(),
            lock: crate::lock::LockState::default(),
            session_lock_state,
            popups: PopupManager::default(),
            compositor_state,
            data_device_state,
            layer_shell_state,
            output_manager_state,
            primary_selection_state,
            data_control_state,
            seat_state,
            keyboard_shortcuts_inhibit_state,
            shm_state,
            viewporter_state,
            xdg_activation_state,
            xdg_decoration_state,
            xdg_shell_state,
            presentation_state,
            fractional_scale_manager_state,
            xdg_foreign_state,
            single_pixel_buffer_state,
            fifo_manager_state,
            commit_timing_manager_state,
            image_capture_source_state,
            output_capture_source_state,
            image_copy_capture_state,
            pending_screencopies: Vec::new(),
            pending_capture_frames: Vec::new(),
            capture_sessions: Vec::new(),
            cursor_shape_state,
            capture_cursor: crate::capture::CaptureCursor::default(),
            capture_generation: 1,
            capture_boost_fps: None,
            capture_boost_timer: None,
            capture_recorder_session_pending: false,
            capture_override: RecorderCaptureSource::Output,
            capture_commit_driven: false,
            dnd_icon: None,
            suppressed_keys: Vec::new(),
            super_tap_pending: false,
            super_tap_used: false,
            cursor_status: CursorImageStatus::default_named(),
            seat_name,
            seat,
            pointer,
            cursor_position_hint: None,
            clock,

            #[cfg(feature = "xwayland")]
            xwayland_shell_state,
            #[cfg(feature = "xwayland")]
            xwm: None,
            #[cfg(feature = "xwayland")]
            xdisplay: None,
            #[cfg(feature = "debug")]
            renderdoc: renderdoc::RenderDoc::new().ok(),
            show_window_preview: false,
        }
    }

    #[cfg(feature = "xwayland")]
    pub fn start_xwayland(&mut self) {
        use std::process::Stdio;

        use smithay::wayland::compositor::CompositorHandler;

        // Smithay's automatic allocator probes displays starting at :0. If an
        // existing XWayland owns the abstract socket but its lock file is
        // missing, that probe removes its filesystem socket before discovering
        // the collision. Keep this compositor in a separate display range so
        // launching a TTY development session cannot disconnect the desktop
        // compositor's X11 clients.
        let displays = std::env::var("WM_XWAYLAND_DISPLAY")
            .ok()
            .and_then(|value| value.parse::<u32>().ok())
            .map(|display| vec![display])
            .unwrap_or_else(|| (100..=132).collect());
        let mut spawned = None;
        for display_number in displays {
            let lock = format!("/tmp/.X{display_number}-lock");
            let socket = format!("/tmp/.X11-unix/X{display_number}");
            if std::path::Path::new(&lock).exists() || std::path::Path::new(&socket).exists() {
                continue;
            }
            match XWayland::spawn(
                &self.display_handle,
                Some(display_number),
                std::iter::empty::<(String, String)>(),
                std::iter::empty::<String>(),
                true,
                Stdio::null(),
                Stdio::null(),
                |_| (),
            ) {
                Ok(server) => {
                    info!(
                        xdisplay = display_number,
                        "Reserved isolated XWayland display"
                    );
                    spawned = Some(server);
                    break;
                }
                Err(error) => {
                    warn!(xdisplay = display_number, %error, "XWayland display unavailable");
                }
            }
        }
        let Some((xwayland, client)) = spawned else {
            tracing::error!("No isolated XWayland display is available");
            return;
        };

        let display_handle = self.display_handle.clone();
        let ret = self
            .handle
            .insert_source(xwayland, move |event, _, data| match event {
                XWaylandEvent::Ready {
                    x11_socket,
                    display_number,
                } => {
                    let xwayland_scale = std::env::var("ANVIL_XWAYLAND_SCALE")
                        .ok()
                        .and_then(|s| s.parse::<f64>().ok())
                        .unwrap_or(1.);
                    data.client_compositor_state(&client)
                        .set_client_scale(xwayland_scale);
                    let mut wm = X11Wm::start_wm(
                        data.handle.clone(),
                        &display_handle,
                        x11_socket,
                        client.clone(),
                    )
                    .expect("Failed to attach X11 Window Manager");

                    let cursor = Cursor::load();
                    let image = cursor.get_image(1, Duration::ZERO);
                    wm.set_cursor(
                        &image.pixels_rgba,
                        Size::from((image.width as u16, image.height as u16)),
                        Point::from((image.xhot as u16, image.yhot as u16)),
                    )
                    .expect("Failed to set xwayland default cursor");
                    data.xwm = Some(wm);
                    data.xdisplay = Some(display_number);
                }
                XWaylandEvent::Error => {
                    warn!("XWayland crashed on startup");
                }
            });
        if let Err(e) = ret {
            tracing::error!(
                "Failed to insert the XWaylandSource into the event loop: {}",
                e
            );
        }
    }
}

impl<BackendData: Backend + 'static> AnvilState<BackendData> {
    pub fn pre_repaint(&mut self, output: &Output, frame_target: impl Into<Time<Monotonic>>) {
        let frame_target = frame_target.into();

        #[allow(clippy::mutable_key_type)]
        let mut clients: HashMap<ClientId, Client> = HashMap::new();
        self.space.elements().for_each(|window| {
            window.with_surfaces(|surface, states| {
                if let Some(mut commit_timer_state) = states
                    .data_map
                    .get::<CommitTimerBarrierStateUserData>()
                    .map(|commit_timer| commit_timer.lock().unwrap())
                {
                    commit_timer_state.signal_until(frame_target);
                    let client = surface.client().unwrap();
                    clients.insert(client.id(), client);
                }
            });
        });

        let map = smithay::desktop::layer_map_for_output(output);
        for layer_surface in map.layers() {
            layer_surface.with_surfaces(|surface, states| {
                if let Some(mut commit_timer_state) = states
                    .data_map
                    .get::<CommitTimerBarrierStateUserData>()
                    .map(|commit_timer| commit_timer.lock().unwrap())
                {
                    commit_timer_state.signal_until(frame_target);
                    let client = surface.client().unwrap();
                    clients.insert(client.id(), client);
                }
            });
        }
        // Drop the lock to the layer map before calling blocker_cleared, which might end up
        // calling the commit handler which in turn again could access the layer map.
        std::mem::drop(map);

        if let CursorImageStatus::Surface(ref surface) = self.cursor_status {
            with_surfaces_surface_tree(surface, |surface, states| {
                if let Some(mut commit_timer_state) = states
                    .data_map
                    .get::<CommitTimerBarrierStateUserData>()
                    .map(|commit_timer| commit_timer.lock().unwrap())
                {
                    commit_timer_state.signal_until(frame_target);
                    let client = surface.client().unwrap();
                    clients.insert(client.id(), client);
                }
            });
        }

        if let Some(surface) = self.dnd_icon.as_ref().map(|icon| &icon.surface) {
            with_surfaces_surface_tree(surface, |surface, states| {
                if let Some(mut commit_timer_state) = states
                    .data_map
                    .get::<CommitTimerBarrierStateUserData>()
                    .map(|commit_timer| commit_timer.lock().unwrap())
                {
                    commit_timer_state.signal_until(frame_target);
                    let client = surface.client().unwrap();
                    clients.insert(client.id(), client);
                }
            });
        }

        let dh = self.display_handle.clone();
        for client in clients.into_values() {
            self.client_compositor_state(&client)
                .blocker_cleared(self, &dh);
        }
    }

    pub fn post_repaint(
        &mut self,
        output: &Output,
        time: impl Into<Duration>,
        dmabuf_feedback: Option<SurfaceDmabufFeedback>,
        render_element_states: &RenderElementStates,
    ) {
        let time = time.into();
        let throttle = Some(Duration::from_secs(1));
        if self.lock.locked {
            let surface = output
                .user_data()
                .get::<std::sync::Mutex<crate::lock::LockOutput>>()
                .and_then(|state| state.lock().unwrap().surface.clone());
            if let Some(surface) = surface {
                smithay::desktop::utils::send_frames_surface_tree(
                    &surface,
                    output,
                    time,
                    None,
                    |_, _| Some(output.clone()),
                );
            }
        }

        #[allow(clippy::mutable_key_type)]
        let mut clients: HashMap<ClientId, Client> = HashMap::new();

        self.space.elements().for_each(|window| {
            window.with_surfaces(|surface, states| {
                let primary_scanout_output = surface_primary_scanout_output(surface, states);

                if let Some(output) = primary_scanout_output.as_ref() {
                    with_fractional_scale(states, |fraction_scale| {
                        fraction_scale
                            .set_preferred_scale(output.current_scale().fractional_scale());
                    });
                }

                if primary_scanout_output
                    .as_ref()
                    .map(|o| o == output)
                    .unwrap_or(true)
                {
                    let fifo_barrier = states
                        .cached_state
                        .get::<FifoBarrierCachedState>()
                        .current()
                        .barrier
                        .take();

                    if let Some(fifo_barrier) = fifo_barrier {
                        fifo_barrier.signal();
                        let client = surface.client().unwrap();
                        clients.insert(client.id(), client);
                    }
                }
            });

            if self.space.outputs_for_element(window).contains(output) {
                window.send_frame(output, time, throttle, surface_primary_scanout_output);
                if let Some(dmabuf_feedback) = dmabuf_feedback.as_ref() {
                    window.send_dmabuf_feedback(
                        output,
                        surface_primary_scanout_output,
                        |surface, _| {
                            select_dmabuf_feedback(
                                surface,
                                render_element_states,
                                &dmabuf_feedback.render_feedback,
                                &dmabuf_feedback.scanout_feedback,
                            )
                        },
                    );
                }
            }
        });
        let map = smithay::desktop::layer_map_for_output(output);
        for layer_surface in map.layers() {
            layer_surface.with_surfaces(|surface, states| {
                let primary_scanout_output = surface_primary_scanout_output(surface, states);

                if let Some(output) = primary_scanout_output.as_ref() {
                    with_fractional_scale(states, |fraction_scale| {
                        fraction_scale
                            .set_preferred_scale(output.current_scale().fractional_scale());
                    });
                }

                if primary_scanout_output
                    .as_ref()
                    .map(|o| o == output)
                    .unwrap_or(true)
                {
                    let fifo_barrier = states
                        .cached_state
                        .get::<FifoBarrierCachedState>()
                        .current()
                        .barrier
                        .take();

                    if let Some(fifo_barrier) = fifo_barrier {
                        fifo_barrier.signal();
                        let client = surface.client().unwrap();
                        clients.insert(client.id(), client);
                    }
                }
            });

            layer_surface.send_frame(output, time, throttle, surface_primary_scanout_output);
            if let Some(dmabuf_feedback) = dmabuf_feedback.as_ref() {
                layer_surface.send_dmabuf_feedback(
                    output,
                    surface_primary_scanout_output,
                    |surface, _| {
                        select_dmabuf_feedback(
                            surface,
                            render_element_states,
                            &dmabuf_feedback.render_feedback,
                            &dmabuf_feedback.scanout_feedback,
                        )
                    },
                );
            }
        }
        // Drop the lock to the layer map before calling blocker_cleared, which might end up
        // calling the commit handler which in turn again could access the layer map.
        std::mem::drop(map);

        if let CursorImageStatus::Surface(ref surface) = self.cursor_status {
            with_surfaces_surface_tree(surface, |surface, states| {
                let primary_scanout_output = surface_primary_scanout_output(surface, states);

                if let Some(output) = primary_scanout_output.as_ref() {
                    with_fractional_scale(states, |fraction_scale| {
                        fraction_scale
                            .set_preferred_scale(output.current_scale().fractional_scale());
                    });
                }

                if primary_scanout_output
                    .as_ref()
                    .map(|o| o == output)
                    .unwrap_or(true)
                {
                    let fifo_barrier = states
                        .cached_state
                        .get::<FifoBarrierCachedState>()
                        .current()
                        .barrier
                        .take();

                    if let Some(fifo_barrier) = fifo_barrier {
                        fifo_barrier.signal();
                        let client = surface.client().unwrap();
                        clients.insert(client.id(), client);
                    }
                }
            });
        }

        if let Some(surface) = self.dnd_icon.as_ref().map(|icon| &icon.surface) {
            with_surfaces_surface_tree(surface, |surface, states| {
                let primary_scanout_output = surface_primary_scanout_output(surface, states);

                if let Some(output) = primary_scanout_output.as_ref() {
                    with_fractional_scale(states, |fraction_scale| {
                        fraction_scale
                            .set_preferred_scale(output.current_scale().fractional_scale());
                    });
                }

                if primary_scanout_output
                    .as_ref()
                    .map(|o| o == output)
                    .unwrap_or(true)
                {
                    let fifo_barrier = states
                        .cached_state
                        .get::<FifoBarrierCachedState>()
                        .current()
                        .barrier
                        .take();

                    if let Some(fifo_barrier) = fifo_barrier {
                        fifo_barrier.signal();
                        let client = surface.client().unwrap();
                        clients.insert(client.id(), client);
                    }
                }
            });
        }

        let dh = self.display_handle.clone();
        for client in clients.into_values() {
            self.client_compositor_state(&client)
                .blocker_cleared(self, &dh);
        }
    }
}

pub fn update_primary_scanout_output(
    space: &Space<WindowElement>,
    output: &Output,
    dnd_icon: &Option<DndIcon>,
    cursor_status: &CursorImageStatus,
    render_element_states: &RenderElementStates,
) {
    space.elements().for_each(|window| {
        window.with_surfaces(|surface, states| {
            update_surface_primary_scanout_output(
                surface,
                output,
                states,
                None,
                render_element_states,
                default_primary_scanout_output_compare,
            );
        });
    });
    let map = smithay::desktop::layer_map_for_output(output);
    for layer_surface in map.layers() {
        layer_surface.with_surfaces(|surface, states| {
            update_surface_primary_scanout_output(
                surface,
                output,
                states,
                None,
                render_element_states,
                default_primary_scanout_output_compare,
            );
        });
    }

    if let CursorImageStatus::Surface(surface) = cursor_status {
        with_surfaces_surface_tree(surface, |surface, states| {
            update_surface_primary_scanout_output(
                surface,
                output,
                states,
                None,
                render_element_states,
                default_primary_scanout_output_compare,
            );
        });
    }

    if let Some(surface) = dnd_icon.as_ref().map(|icon| &icon.surface) {
        with_surfaces_surface_tree(surface, |surface, states| {
            update_surface_primary_scanout_output(
                surface,
                output,
                states,
                None,
                render_element_states,
                default_primary_scanout_output_compare,
            );
        });
    }
}

#[derive(Debug, Clone)]
pub struct SurfaceDmabufFeedback {
    pub render_feedback: DmabufFeedback,
    pub scanout_feedback: DmabufFeedback,
}

#[profiling::function]
pub fn take_presentation_feedback(
    output: &Output,
    space: &Space<WindowElement>,
    render_element_states: &RenderElementStates,
) -> OutputPresentationFeedback {
    let mut output_presentation_feedback = OutputPresentationFeedback::new(output);

    space.elements().for_each(|window| {
        if space.outputs_for_element(window).contains(output) {
            window.take_presentation_feedback(
                &mut output_presentation_feedback,
                surface_primary_scanout_output,
                |surface, _| {
                    surface_presentation_feedback_flags_from_states(
                        surface,
                        None,
                        render_element_states,
                    )
                },
            );
        }
    });
    let map = smithay::desktop::layer_map_for_output(output);
    for layer_surface in map.layers() {
        layer_surface.take_presentation_feedback(
            &mut output_presentation_feedback,
            surface_primary_scanout_output,
            |surface, _| {
                surface_presentation_feedback_flags_from_states(
                    surface,
                    None,
                    render_element_states,
                )
            },
        );
    }

    output_presentation_feedback
}

pub trait Backend {
    fn apply_performance_policy(
        &mut self,
        _performance: &wm_core::Performance,
        _gaming_outputs: &[String],
        _outputs: &std::collections::BTreeMap<String, wm_core::OutputConfig>,
    ) -> Vec<String> {
        Vec::new()
    }
    fn performance_status(&self) -> Vec<wm_core::OutputPerformanceStatus> {
        Vec::new()
    }
    fn reset_performance_metrics(&mut self) {}
    fn snapshot_window(
        &mut self,
        _window: &WindowElement,
        _output: &Output,
        _fullscreen: bool,
    ) -> Result<
        (
            smithay::backend::renderer::gles::GlesTexture,
            smithay::utils::Rectangle<i32, smithay::utils::Logical>,
        ),
        String,
    > {
        Err("window snapshots unavailable on this backend".into())
    }

    fn capture_output(
        &mut self,
        _space: &Space<WindowElement>,
        _output: &Output,
        _cursor: Option<&crate::capture::CaptureCursor>,
    ) -> Result<Vec<u8>, String> {
        Err("capture is unavailable on this backend".into())
    }
    fn capture_dmabuf_constraints(
        &mut self,
        _output: &smithay::output::Output,
    ) -> Option<smithay::wayland::image_copy_capture::DmabufConstraints> {
        None
    }
    fn capture_output_dmabuf(
        &mut self,
        _space: &Space<WindowElement>,
        _output: &smithay::output::Output,
        _cursor: Option<&crate::capture::CaptureCursor>,
        _dmabuf: smithay::backend::allocator::dmabuf::Dmabuf,
    ) -> Result<(), String> {
        Err("DMA-BUF capture unavailable on backend".into())
    }
    fn capture_window_dmabuf(
        &mut self,
        _window: &crate::shell::WindowElement,
        _output: &smithay::output::Output,
        _dmabuf: smithay::backend::allocator::dmabuf::Dmabuf,
    ) -> Result<(), String> {
        Err("window DMA-BUF capture unavailable on backend".into())
    }
    fn capture_region_dmabuf(
        &mut self,
        _space: &Space<WindowElement>,
        _output: &smithay::output::Output,
        _cursor: Option<&crate::capture::CaptureCursor>,
        _region: wm_core::Rect,
        _dmabuf: smithay::backend::allocator::dmabuf::Dmabuf,
    ) -> Result<(), String> {
        Err("region DMA-BUF capture unavailable on backend".into())
    }

    const SUPPORTS_SESSION_LOCK: bool = false;
    const HAS_RELATIVE_MOTION: bool = false;
    const HAS_GESTURES: bool = false;
    fn seat_name(&self) -> String;
    fn reset_buffers(&mut self, output: &Output);
    fn early_import(&mut self, surface: &WlSurface);
    fn update_led_state(&mut self, led_state: LedState);
    /// Nested backends leave physical device configuration to their host.
    fn apply_device_config(&mut self, _config: &wm_core::Input) {}
    fn apply_output_config(
        &mut self,
        _config: &std::collections::BTreeMap<String, wm_core::OutputConfig>,
    ) -> Vec<String> {
        Vec::new()
    }
}
