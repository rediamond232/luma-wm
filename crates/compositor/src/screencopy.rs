//! Compatibility implementation of the deprecated wlroots screencopy protocol.
//!
//! xdg-desktop-portal-wlr still consumes this protocol. New clients should use
//! ext-image-copy-capture-v1, which is also exposed by the compositor.

use std::{
    sync::{
        Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use smithay::{
    output::{Output, WeakOutput},
    reexports::{
        wayland_protocols_wlr::screencopy::v1::server::{
            zwlr_screencopy_frame_v1::{self, ZwlrScreencopyFrameV1},
            zwlr_screencopy_manager_v1::{self, ZwlrScreencopyManagerV1},
        },
        wayland_server::{
            Client, DataInit, Dispatch, DisplayHandle, GlobalDispatch, New, Resource,
            protocol::{wl_buffer::WlBuffer, wl_output::WlOutput, wl_shm},
        },
    },
};

use crate::state::{AnvilState, Backend};

#[derive(Debug)]
pub struct FrameData {
    output: Option<WeakOutput>,
    overlay_cursor: bool,
    capture_width: i32,
    capture_height: i32,
    x: i32,
    y: i32,
    width: i32,
    height: i32,
    initial: bool,
    used: AtomicBool,
    pending: Mutex<Option<WlBuffer>>,
}

#[derive(Debug)]
pub struct ManagerData {
    captured: AtomicBool,
}

pub(crate) type PendingFrame = ZwlrScreencopyFrameV1;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct Crop {
    x: i32,
    y: i32,
    width: i32,
    height: i32,
}

fn clip_region(
    logical_width: i32,
    logical_height: i32,
    physical_width: i32,
    physical_height: i32,
    requested: Option<(i32, i32, i32, i32)>,
) -> Option<Crop> {
    if logical_width <= 0 || logical_height <= 0 || physical_width <= 0 || physical_height <= 0 {
        return None;
    }
    let Some((x, y, width, height)) = requested else {
        return Some(Crop {
            x: 0,
            y: 0,
            width: physical_width,
            height: physical_height,
        });
    };
    if width <= 0 || height <= 0 {
        return None;
    }
    let x0 = i64::from(x).clamp(0, i64::from(logical_width));
    let y0 = i64::from(y).clamp(0, i64::from(logical_height));
    let x1 = (i64::from(x) + i64::from(width)).clamp(0, i64::from(logical_width));
    let y1 = (i64::from(y) + i64::from(height)).clamp(0, i64::from(logical_height));
    if x1 <= x0 || y1 <= y0 {
        return None;
    }
    let scale_floor =
        |value: i64, physical: i32, logical: i32| value * i64::from(physical) / i64::from(logical);
    let scale_ceil = |value: i64, physical: i32, logical: i32| {
        let numerator = value * i64::from(physical);
        (numerator + i64::from(logical) - 1) / i64::from(logical)
    };
    let px0 = scale_floor(x0, physical_width, logical_width);
    let py0 = scale_floor(y0, physical_height, logical_height);
    let px1 = scale_ceil(x1, physical_width, logical_width);
    let py1 = scale_ceil(y1, physical_height, logical_height);
    Some(Crop {
        x: px0 as i32,
        y: py0 as i32,
        width: (px1 - px0) as i32,
        height: (py1 - py0) as i32,
    })
}

fn crop_pixels(
    pixels: Vec<u8>,
    capture_width: i32,
    capture_height: i32,
    crop: Crop,
) -> Option<Vec<u8>> {
    let full_stride = usize::try_from(capture_width).ok()?.checked_mul(4)?;
    let full_height = usize::try_from(capture_height).ok()?;
    if pixels.len() != full_stride.checked_mul(full_height)? {
        return None;
    }
    if crop.x == 0 && crop.y == 0 && crop.width == capture_width && crop.height == capture_height {
        return Some(pixels);
    }
    let x = usize::try_from(crop.x).ok()?;
    let y = usize::try_from(crop.y).ok()?;
    let width = usize::try_from(crop.width).ok()?;
    let height = usize::try_from(crop.height).ok()?;
    let row_bytes = width.checked_mul(4)?;
    let mut cropped = Vec::with_capacity(row_bytes.checked_mul(height)?);
    for row in y..y.checked_add(height)? {
        let start = row
            .checked_mul(full_stride)?
            .checked_add(x.checked_mul(4)?)?;
        let end = start.checked_add(row_bytes)?;
        cropped.extend_from_slice(pixels.get(start..end)?);
    }
    Some(cropped)
}

pub fn init<State>(display: &DisplayHandle)
where
    State: GlobalDispatch<ZwlrScreencopyManagerV1, ()> + 'static,
{
    display.create_global::<State, ZwlrScreencopyManagerV1, _>(3, ());
}

fn create_frame<BackendData: Backend>(
    state: &mut AnvilState<BackendData>,
    output_resource: WlOutput,
    overlay_cursor: i32,
    requested: Option<(i32, i32, i32, i32)>,
    initial: bool,
    frame: New<ZwlrScreencopyFrameV1>,
    data_init: &mut DataInit<'_, AnvilState<BackendData>>,
) {
    let output = Output::from_resource(&output_resource).filter(|output| {
        !state.lock.locked
            && state.desktop.active
            && state.space.outputs().any(|mapped| mapped == output)
    });
    let dimensions = output.as_ref().and_then(|output| {
        output
            .current_mode()
            .map(|mode| output.current_transform().transform_size(mode.size))
    });
    let (capture_width, capture_height) = dimensions.map_or((0, 0), |size| (size.w, size.h));
    let logical = output
        .as_ref()
        .and_then(|output| state.space.output_geometry(output))
        .map(|geometry| geometry.size)
        .unwrap_or_default();
    let crop = clip_region(
        logical.w,
        logical.h,
        capture_width,
        capture_height,
        requested,
    );
    let (x, y, width, height) = crop.map_or((0, 0, 0, 0), |crop| {
        (crop.x, crop.y, crop.width, crop.height)
    });
    let frame = data_init.init(
        frame,
        FrameData {
            output: output.as_ref().map(Output::downgrade),
            overlay_cursor: overlay_cursor != 0,
            capture_width,
            capture_height,
            x,
            y,
            width,
            height,
            initial,
            used: AtomicBool::new(false),
            pending: Mutex::new(None),
        },
    );
    if output.is_none() || crop.is_none() {
        frame.failed();
        return;
    }
    frame.buffer(
        wl_shm::Format::Argb8888,
        width as u32,
        height as u32,
        width as u32 * 4,
    );
    if frame.version() >= 3 {
        frame.buffer_done();
    }
}

fn perform_copy<BackendData: Backend>(
    state: &mut AnvilState<BackendData>,
    frame: &ZwlrScreencopyFrameV1,
    data: &FrameData,
    buffer: WlBuffer,
    with_damage: bool,
) {
    let Some(output) = data.output.as_ref().and_then(WeakOutput::upgrade) else {
        buffer.release();
        frame.failed();
        return;
    };
    if state.lock.locked
        || !state.desktop.active
        || !state.space.outputs().any(|mapped| mapped == &output)
    {
        buffer.release();
        frame.failed();
        return;
    }
    if crate::capture::validate(&buffer, data.width, data.height).is_err() {
        buffer.release();
        frame.post_error(
            zwlr_screencopy_frame_v1::Error::InvalidBuffer,
            "buffer does not match the advertised screencopy format and dimensions",
        );
        return;
    }
    if data.overlay_cursor {
        state.capture_cursor.prepare(
            state.cursor_status.clone(),
            state.pointer.current_location(),
            state.space.output_geometry(&output).unwrap_or_default(),
            &output,
            state.clock.now().into(),
        );
    }
    let cursor = data.overlay_cursor.then_some(&state.capture_cursor);
    let pixels = state
        .backend_data
        .capture_output(&state.space, &output, cursor);
    let crop = Crop {
        x: data.x,
        y: data.y,
        width: data.width,
        height: data.height,
    };
    let result = pixels
        .and_then(|pixels| {
            crop_pixels(pixels, data.capture_width, data.capture_height, crop)
                .ok_or_else(|| "captured output size changed".to_string())
        })
        .and_then(|pixels| crate::capture::write(&buffer, data.width, data.height, &pixels));
    buffer.release();
    if result.is_err() {
        frame.failed();
        return;
    }
    if with_damage && frame.version() >= 2 {
        frame.damage(0, 0, data.width as u32, data.height as u32);
    }
    frame.flags(zwlr_screencopy_frame_v1::Flags::empty());
    let now: Duration = state.clock.now().into();
    let seconds = now.as_secs();
    frame.ready((seconds >> 32) as u32, seconds as u32, now.subsec_nanos());
}

fn request_copy<BackendData: Backend>(
    state: &mut AnvilState<BackendData>,
    frame: &ZwlrScreencopyFrameV1,
    data: &FrameData,
    buffer: WlBuffer,
    with_damage: bool,
) {
    if data.used.swap(true, Ordering::AcqRel) {
        frame.post_error(
            zwlr_screencopy_frame_v1::Error::AlreadyUsed,
            "a screencopy frame can only be copied once",
        );
        return;
    }
    if with_damage && !data.initial {
        state.pending_screencopies.retain(Resource::is_alive);
        if state.pending_screencopies.len() >= 32 {
            buffer.release();
            frame.failed();
            return;
        }
        *data.pending.lock().unwrap() = Some(buffer);
        state.pending_screencopies.push(frame.clone());
        return;
    }
    perform_copy(state, frame, data, buffer, with_damage);
}

pub(crate) fn process_pending<BackendData: Backend>(state: &mut AnvilState<BackendData>) {
    let frames = std::mem::take(&mut state.pending_screencopies);
    for frame in frames {
        if !frame.is_alive() {
            continue;
        }
        let Some(data) = frame.data::<FrameData>() else {
            continue;
        };
        let Some(buffer) = data.pending.lock().unwrap().take() else {
            continue;
        };
        perform_copy(state, &frame, data, buffer, true);
    }
}

impl<BackendData: Backend> GlobalDispatch<ZwlrScreencopyManagerV1, ()> for AnvilState<BackendData> {
    fn bind(
        _state: &mut Self,
        _handle: &DisplayHandle,
        _client: &Client,
        resource: New<ZwlrScreencopyManagerV1>,
        _global_data: &(),
        data_init: &mut DataInit<'_, Self>,
    ) {
        data_init.init(
            resource,
            ManagerData {
                captured: AtomicBool::new(false),
            },
        );
    }
}

impl<BackendData: Backend> Dispatch<ZwlrScreencopyManagerV1, ManagerData>
    for AnvilState<BackendData>
{
    fn request(
        state: &mut Self,
        _client: &Client,
        _resource: &ZwlrScreencopyManagerV1,
        request: zwlr_screencopy_manager_v1::Request,
        data: &ManagerData,
        _handle: &DisplayHandle,
        data_init: &mut DataInit<'_, Self>,
    ) {
        match request {
            zwlr_screencopy_manager_v1::Request::CaptureOutput {
                frame,
                overlay_cursor,
                output,
            } => create_frame(
                state,
                output,
                overlay_cursor,
                None,
                !data.captured.swap(true, Ordering::AcqRel),
                frame,
                data_init,
            ),
            zwlr_screencopy_manager_v1::Request::CaptureOutputRegion {
                frame,
                overlay_cursor,
                output,
                x,
                y,
                width,
                height,
            } => create_frame(
                state,
                output,
                overlay_cursor,
                Some((x, y, width, height)),
                !data.captured.swap(true, Ordering::AcqRel),
                frame,
                data_init,
            ),
            zwlr_screencopy_manager_v1::Request::Destroy => {}
            _ => unreachable!(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn regions_clip_and_scale_outward() {
        assert_eq!(
            clip_region(800, 600, 1200, 900, Some((10, 20, 101, 51))),
            Some(Crop {
                x: 15,
                y: 30,
                width: 152,
                height: 77,
            })
        );
        assert_eq!(
            clip_region(800, 600, 800, 600, Some((-10, -20, 30, 40))),
            Some(Crop {
                x: 0,
                y: 0,
                width: 20,
                height: 20,
            })
        );
        assert!(clip_region(800, 600, 800, 600, Some((900, 0, 10, 10))).is_none());
        assert!(clip_region(800, 600, 800, 600, Some((0, 0, 0, 10))).is_none());
    }

    #[test]
    fn pixel_crop_preserves_rows() {
        let pixels: Vec<u8> = (0..4 * 3 * 4).collect();
        let cropped = crop_pixels(
            pixels,
            4,
            3,
            Crop {
                x: 1,
                y: 1,
                width: 2,
                height: 2,
            },
        )
        .unwrap();
        assert_eq!(
            cropped,
            [
                20, 21, 22, 23, 24, 25, 26, 27, 36, 37, 38, 39, 40, 41, 42, 43
            ]
        );
    }
}

impl<BackendData: Backend> Dispatch<ZwlrScreencopyFrameV1, FrameData> for AnvilState<BackendData> {
    fn request(
        state: &mut Self,
        _client: &Client,
        resource: &ZwlrScreencopyFrameV1,
        request: zwlr_screencopy_frame_v1::Request,
        data: &FrameData,
        _handle: &DisplayHandle,
        _data_init: &mut DataInit<'_, Self>,
    ) {
        match request {
            zwlr_screencopy_frame_v1::Request::Copy { buffer } => {
                request_copy(state, resource, data, buffer, false)
            }
            zwlr_screencopy_frame_v1::Request::CopyWithDamage { buffer } => {
                request_copy(state, resource, data, buffer, true)
            }
            zwlr_screencopy_frame_v1::Request::Destroy => {
                if let Some(buffer) = data.pending.lock().unwrap().take() {
                    buffer.release();
                }
            }
            _ => unreachable!(),
        }
    }

    fn destroyed(
        _state: &mut Self,
        _client: smithay::reexports::wayland_server::backend::ClientId,
        _resource: &ZwlrScreencopyFrameV1,
        data: &FrameData,
    ) {
        if let Some(buffer) = data.pending.lock().unwrap().take() {
            buffer.release();
        }
    }
}
