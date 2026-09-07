//! Shared-memory screenshot path. Streaming DMA-BUF capture is separate work.
use crate::shell::WindowElement;
use smithay::{
    backend::{
        allocator::Fourcc,
        renderer::{
            Bind, ExportMem, Offscreen, TextureMapping,
            damage::OutputDamageTracker,
            element::{AsRenderElements, memory::MemoryRenderBuffer},
            gles::{GlesRenderer, GlesTexture},
        },
    },
    desktop::Space,
    input::pointer::{CursorImageAttributes, CursorImageStatus},
    output::Output,
    reexports::wayland_server::protocol::{wl_buffer::WlBuffer, wl_shm},
    utils::{Logical, Physical, Point, Rectangle, Scale, Transform},
    wayland::shm::{BufferData, with_buffer_contents, with_buffer_contents_mut},
};
use std::{sync::Mutex, time::Duration};

#[derive(Default)]
pub struct CaptureCursor {
    theme: Option<crate::cursor::Cursor>,
    image: Option<xcursor::parser::Image>,
    image_scale: i32,
    element: crate::drawing::PointerElement,
    location: Point<i32, Physical>,
}

impl std::fmt::Debug for CaptureCursor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CaptureCursor")
            .field("location", &self.location)
            .finish_non_exhaustive()
    }
}

impl CaptureCursor {
    pub fn render_elements(
        &self,
        renderer: &mut GlesRenderer,
        output: &Output,
    ) -> Vec<crate::render::CustomRenderElements<GlesRenderer>> {
        self.element.render_elements(
            renderer,
            self.location,
            Scale::from(output.current_scale().fractional_scale()),
            1.0,
        )
    }
    pub fn prepare(
        &mut self,
        mut status: CursorImageStatus,
        position: Point<f64, Logical>,
        output_geometry: Rectangle<i32, Logical>,
        output: &Output,
        time: Duration,
    ) {
        use smithay::utils::IsAlive;
        if matches!(&status, CursorImageStatus::Surface(s) if !s.alive()) {
            status = CursorImageStatus::default_named();
        }
        if !output_geometry.to_f64().contains(position) {
            status = CursorImageStatus::Hidden;
        }
        let scale = Scale::from(output.current_scale().fractional_scale());
        let mut hotspot = Point::<f64, Logical>::from((0.0, 0.0));
        match &status {
            CursorImageStatus::Surface(surface) => {
                hotspot = smithay::wayland::compositor::with_states(surface, |states| {
                    states
                        .data_map
                        .get::<Mutex<CursorImageAttributes>>()
                        .map(|attrs| attrs.lock().unwrap().hotspot.to_f64())
                        .unwrap_or_default()
                });
            }
            CursorImageStatus::Named(icon) => {
                let image_scale = output.current_scale().integer_scale();
                let image = self
                    .theme
                    .get_or_insert_with(crate::cursor::Cursor::load)
                    .get_named_image(*icon, image_scale as u32, time);
                hotspot = Point::from((
                    image.xhot as f64 / image_scale as f64,
                    image.yhot as f64 / image_scale as f64,
                ));
                if self.image.as_ref() != Some(&image) || self.image_scale != image_scale {
                    self.element.set_buffer(MemoryRenderBuffer::from_slice(
                        &image.pixels_rgba,
                        Fourcc::Abgr8888,
                        (image.width as i32, image.height as i32),
                        image_scale,
                        Transform::Normal,
                        None,
                    ));
                    self.image = Some(image);
                    self.image_scale = image_scale;
                }
            }
            CursorImageStatus::Hidden => {}
        }
        self.location = (position - output_geometry.loc.to_f64() - hotspot)
            .to_physical(scale)
            .to_i32_round();
        self.element.set_status(status);
    }
}

pub fn render(
    renderer: &mut GlesRenderer,
    space: &Space<WindowElement>,
    output: &Output,
    cursor: Option<&CaptureCursor>,
) -> Result<Vec<u8>, String> {
    // Export an upright image in output coordinates. The framebuffer dimensions
    // follow the transformed mode; the offscreen render itself needs no scanout
    // rotation or reflection.
    let size = output
        .current_transform()
        .transform_size(output.current_mode().ok_or("output has no mode")?.size);
    let bytes = (size.w as usize)
        .checked_mul(size.h as usize)
        .and_then(|n| n.checked_mul(4))
        .filter(|n| *n <= 256 * 1024 * 1024)
        .ok_or("capture exceeds size limit")?;
    let mut texture: GlesTexture = renderer
        .create_buffer(Fourcc::Argb8888, (size.w, size.h).into())
        .map_err(|e| e.to_string())?;
    let mut target = renderer.bind(&mut texture).map_err(|e| e.to_string())?;
    let mut damage = OutputDamageTracker::new(
        size,
        output.current_scale().fractional_scale(),
        Transform::Normal,
    );
    let elements = cursor
        .map(|cursor| cursor.render_elements(renderer, output))
        .unwrap_or_default();
    crate::render::render_output(
        output,
        space,
        elements,
        renderer,
        &mut target,
        &mut damage,
        0,
        false,
    )
    .map_err(|e| e.to_string())?;
    let mapping = renderer
        .copy_framebuffer(
            &target,
            Rectangle::from_size((size.w, size.h).into()),
            Fourcc::Argb8888,
        )
        .map_err(|e| e.to_string())?;
    let flipped = mapping.flipped();
    let mapped = renderer.map_texture(&mapping).map_err(|e| e.to_string())?;
    if mapped.len() != bytes {
        return Err("unexpected capture mapping size".into());
    }
    let stride = size.w as usize * 4;
    let mut pixels = vec![0; bytes];
    for y in 0..size.h as usize {
        // Mapping orientation is relative to GL's lower-left origin; SHM
        // images instead start with the upper-left row.
        let src_y = if flipped { y } else { size.h as usize - 1 - y };
        pixels[y * stride..(y + 1) * stride]
            .copy_from_slice(&mapped[src_y * stride..(src_y + 1) * stride]);
    }
    Ok(pixels)
}

struct Layout {
    row: usize,
    height: usize,
    stride: usize,
    offset: usize,
    bytes: usize,
}

fn layout(data: BufferData, len: usize, width: i32, height: i32) -> Result<Layout, String> {
    if data.width != width
        || data.height != height
        || !matches!(
            data.format,
            wl_shm::Format::Argb8888 | wl_shm::Format::Xrgb8888
        )
    {
        return Err("capture buffer does not match current output".into());
    }
    let row = usize::try_from(width)
        .ok()
        .and_then(|w| w.checked_mul(4))
        .ok_or("invalid capture width")?;
    let height = usize::try_from(height).map_err(|_| "invalid capture height")?;
    let stride = usize::try_from(data.stride).map_err(|_| "invalid stride")?;
    let offset = usize::try_from(data.offset).map_err(|_| "invalid buffer offset")?;
    let end = stride
        .checked_mul(height.saturating_sub(1))
        .and_then(|n| n.checked_add(row))
        .and_then(|n| n.checked_add(offset))
        .ok_or("capture buffer overflow")?;
    let bytes = row
        .checked_mul(height)
        .filter(|n| *n <= 256 * 1024 * 1024)
        .ok_or("capture exceeds size limit")?;
    if row == 0 || height == 0 || stride < row || end > len {
        return Err("invalid capture buffer bounds".into());
    }
    Ok(Layout {
        row,
        height,
        stride,
        offset,
        bytes,
    })
}

/// Reject stale or incompatible buffers before allocating a render target.
pub fn validate(buffer: &WlBuffer, width: i32, height: i32) -> Result<(), String> {
    with_buffer_contents(buffer, |_, len, data| {
        layout(data, len, width, height).map(|_| ())
    })
    .map_err(|e| e.to_string())?
}

pub fn write(buffer: &WlBuffer, width: i32, height: i32, pixels: &[u8]) -> Result<(), String> {
    with_buffer_contents_mut(buffer, |ptr, len, data| {
        // Validate again inside the write guard: preflight does not grant
        // access to this mapping or guarantee that it is still accessible.
        let Layout {
            row,
            height,
            stride,
            offset,
            bytes,
        } = layout(data, len, width, height)?;
        if pixels.len() != bytes {
            return Err("unexpected capture pixel size".into());
        }
        for y in 0..height {
            // The SHM pool is mapped and SIGBUS-guarded by Smithay for this
            // closure. Checked bounds above include the last row and offset.
            unsafe {
                std::ptr::copy_nonoverlapping(
                    pixels.as_ptr().add(y * row),
                    ptr.add(offset + y * stride),
                    row,
                );
            }
        }
        Ok(())
    })
    .map_err(|e| e.to_string())?
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn padded_capture_bounds_and_stale_dimensions() {
        let data = BufferData {
            offset: 64,
            width: 320,
            height: 200,
            stride: 1296,
            format: wl_shm::Format::Argb8888,
        };
        let end = 64 + 1296 * 199 + 1280;
        let valid = layout(data, end, 320, 200).unwrap();
        assert_eq!(valid.bytes, 320 * 200 * 4);
        assert!(layout(data, end - 1, 320, 200).is_err());
        assert!(layout(data, end, 640, 200).is_err());
        for invalid in [
            BufferData { offset: -1, ..data },
            BufferData {
                stride: 1279,
                ..data
            },
            BufferData { width: 0, ..data },
            BufferData { height: 0, ..data },
            BufferData {
                format: wl_shm::Format::Rgb565,
                ..data
            },
        ] {
            assert!(layout(invalid, end, invalid.width, invalid.height).is_err());
        }
        let excessive = BufferData {
            width: 16384,
            height: 16384,
            stride: 65536,
            ..data
        };
        assert!(layout(excessive, usize::MAX, excessive.width, excessive.height).is_err());
    }
}
