//! Shared-memory screenshot path. Streaming DMA-BUF capture is separate work.
use crate::shell::WindowElement;
use smithay::backend::renderer::element::Element as _;
use smithay::{
    backend::{
        allocator::{Buffer, Fourcc, dmabuf::Dmabuf},
        renderer::{
            Bind, ExportMem, Frame as _, Offscreen, Renderer as _, TextureMapping,
            damage::OutputDamageTracker,
            element::{
                AsRenderElements,
                memory::MemoryRenderBuffer,
                surface::WaylandSurfaceRenderElement,
                utils::{
                    ConstrainAlign, ConstrainScaleBehavior, CropRenderElement,
                    RelocateRenderElement, RescaleRenderElement,
                },
            },
            gles::{GlesRenderer, GlesTexture},
        },
    },
    desktop::{
        Space,
        space::{ConstrainBehavior, ConstrainReference, constrain_space_element},
    },
    input::pointer::{CursorImageAttributes, CursorImageStatus},
    output::Output,
    reexports::wayland_server::protocol::{wl_buffer::WlBuffer, wl_shm},
    utils::{Logical, Physical, Point, Rectangle, Scale, Transform},
    wayland::shm::{BufferData, with_buffer_contents, with_buffer_contents_mut},
};
use std::{sync::Mutex, time::Duration};

pub fn dmabuf_constraints(
    renderer: &GlesRenderer,
    node: smithay::backend::drm::DrmNode,
) -> Option<smithay::wayland::image_copy_capture::DmabufConstraints> {
    let mut formats: Vec<(Fourcc, Vec<smithay::backend::allocator::Modifier>)> = Vec::new();
    for format in renderer.egl_context().dmabuf_render_formats().iter() {
        if let Some((_, modifiers)) = formats.iter_mut().find(|(code, _)| *code == format.code) {
            if !modifiers.contains(&format.modifier) {
                modifiers.push(format.modifier);
            }
        } else {
            formats.push((format.code, vec![format.modifier]));
        }
    }
    (!formats.is_empty())
        .then_some(smithay::wayland::image_copy_capture::DmabufConstraints { node, formats })
}

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

// Reject invalid dimensions before conversion or GPU allocation. Decorations
// are already included in this logical size.
fn snapshot_size(
    logical: smithay::utils::Size<i32, smithay::utils::Logical>,
    scale: f64,
) -> Result<smithay::utils::Size<i32, smithay::utils::Physical>, String> {
    if logical.w <= 0 || logical.h <= 0 || !scale.is_finite() || scale <= 0.0 {
        return Err("invalid window snapshot dimensions".into());
    }
    let width = (f64::from(logical.w) * scale).round();
    let height = (f64::from(logical.h) * scale).round();
    if width < 1.0 || height < 1.0 || width > f64::from(i32::MAX) || height > f64::from(i32::MAX) {
        return Err("invalid window snapshot dimensions".into());
    }
    let size = (width as i32, height as i32);
    (size.0 as u64)
        .checked_mul(size.1 as u64)
        .and_then(|n| n.checked_mul(4))
        .filter(|bytes| *bytes <= 64 * 1024 * 1024)
        .ok_or("window snapshot exceeds size limit")?;
    Ok(size.into())
}

#[cfg(test)]
mod snapshot_size_tests {
    use super::snapshot_size;

    #[test]
    fn allocation_boundary_includes_fractional_scale_and_decoration_padding() {
        assert_eq!(
            snapshot_size((4096, 4096).into(), 1.0).unwrap(),
            (4096, 4096).into()
        );
        assert!(snapshot_size((4097, 4096).into(), 1.0).is_err());
        assert!(snapshot_size((4096 + 44, 4096 + 44).into(), 1.0).is_err());
        assert_eq!(
            snapshot_size((320 + 44, 200 + 44).into(), 1.5).unwrap(),
            (546, 366).into()
        );
        assert!(snapshot_size((4096, 4096).into(), 1.5).is_err());
    }

    #[test]
    fn invalid_or_unrepresentable_sizes_never_reach_gpu_allocation() {
        for scale in [
            0.0,
            -1.0,
            f64::NAN,
            f64::INFINITY,
            f64::NEG_INFINITY,
            f64::MAX,
            0.00001,
        ] {
            assert!(snapshot_size((320, 200).into(), scale).is_err(), "{scale}");
        }
        for size in [
            (0, 200),
            (320, 0),
            (-1, 200),
            (320, -1),
            (i32::MAX, i32::MAX),
        ] {
            let mut logical = smithay::utils::Size::from((0, 0));
            logical.w = size.0;
            logical.h = size.1;
            assert!(snapshot_size(logical, 1.5).is_err(), "{size:?}");
        }
    }
}

/// Retain a window's current surface tree before destruction clears its buffers.
pub fn render_window_texture(
    renderer: &mut GlesRenderer,
    window: &WindowElement,
    output: &Output,
    fullscreen: bool,
) -> Result<(GlesTexture, Rectangle<i32, smithay::utils::Logical>), String> {
    let scale = output.current_scale().fractional_scale();
    let geometry = smithay::desktop::space::SpaceElement::geometry(window);
    let render_size = window
        .0
        .user_data()
        .get::<crate::effects::WindowRenderSize>()
        .and_then(|size| *size.0.lock().unwrap());
    let surface_bounds = smithay::desktop::space::SpaceElement::bbox(window);
    let visual_bounds = render_size.map(|size| {
        let scale_x = f64::from(size.w) / f64::from(geometry.size.w.max(1));
        let scale_y = f64::from(size.h) / f64::from(geometry.size.h.max(1));
        Rectangle::new(
            (
                geometry.loc.x
                    + (f64::from(surface_bounds.loc.x - geometry.loc.x) * scale_x).round() as i32,
                geometry.loc.y
                    + (f64::from(surface_bounds.loc.y - geometry.loc.y) * scale_y).round() as i32,
            )
                .into(),
            (
                (f64::from(surface_bounds.size.w) * scale_x)
                    .round()
                    .max(1.0) as i32,
                (f64::from(surface_bounds.size.h) * scale_y)
                    .round()
                    .max(1.0) as i32,
            )
                .into(),
        )
    });
    let mut bounds = surface_bounds.merge(window.0.bbox_with_popups());
    if let Some(visual_bounds) = visual_bounds {
        bounds = bounds.merge(visual_bounds);
    }
    let popup_ids = crate::effects::popup_surface_ids(window);
    let toplevel_ids = crate::effects::toplevel_surface_ids(window);
    let border = if fullscreen {
        None
    } else {
        crate::effects::snapshot_border(renderer, window, output, &mut bounds)
            .map_err(|e| e.to_string())?
    };
    let size = snapshot_size(bounds.size, scale)?;
    let mut texture: GlesTexture = renderer
        .create_buffer(Fourcc::Argb8888, (size.w, size.h).into())
        .map_err(|e| e.to_string())?;
    let mut target = renderer.bind(&mut texture).map_err(|e| e.to_string())?;
    let location = smithay::utils::Point::<f64, smithay::utils::Logical>::from((
        -f64::from(bounds.loc.x),
        -f64::from(bounds.loc.y),
    ))
    .to_physical(scale)
    .to_i32_round();
    let elements: Vec<crate::shell::WindowRenderElement<GlesRenderer>> =
        window.render_elements(renderer, location, scale.into(), 1.0);
    // Reuse the live scene's corner mask while rendering into the snapshot.
    // This preserves clipping across the live-to-closing transition without
    // adding any shader work to subsequent closing-animation frames.
    let program = crate::effects::program(renderer).map_err(|e| e.to_string())?;
    let radius = output
        .user_data()
        .get::<std::sync::Mutex<crate::effects::OutputTheme>>()
        .map(|theme| theme.lock().unwrap().0.radius)
        .unwrap_or_default();
    let radius = if !fullscreen && output.current_transform() == Transform::Normal {
        radius * scale as f32
    } else {
        0.0
    };
    let blur = if fullscreen {
        None
    } else {
        crate::effects::snapshot_blur(renderer, window, output)
    };
    let root_id = window.wl_surface().map(|surface| {
        smithay::backend::renderer::element::Id::from_wayland_resource(surface.as_ref())
    });
    let rect: Rectangle<i32, smithay::utils::Physical> =
        Rectangle::new(geometry.loc - bounds.loc, geometry.size).to_physical_precise_round(scale);
    let visual_rect = render_size.map(|size| {
        Rectangle::new(geometry.loc - bounds.loc, size).to_physical_precise_round(scale)
    });
    let elements: Vec<_> = elements
        .into_iter()
        .map(|inner| {
            let geo = inner.geometry(scale.into());
            let radius = if !popup_ids.contains(inner.id()) {
                radius
            } else {
                0.0
            };
            let retained = blur
                .as_ref()
                .filter(|_| root_id.as_ref() == Some(inner.id()));
            let geometry_override = visual_rect.as_ref().and_then(|visual| {
                if !toplevel_ids.contains(inner.id()) || rect.size.w <= 0 || rect.size.h <= 0 {
                    return None;
                }
                let scale_x = f64::from(visual.size.w) / f64::from(rect.size.w);
                let scale_y = f64::from(visual.size.h) / f64::from(rect.size.h);
                Some(Rectangle::new(
                    (
                        visual.loc.x + (f64::from(geo.loc.x - rect.loc.x) * scale_x).round() as i32,
                        visual.loc.y + (f64::from(geo.loc.y - rect.loc.y) * scale_y).round() as i32,
                    )
                        .into(),
                    (
                        (f64::from(geo.size.w) * scale_x).round().max(1.0) as i32,
                        (f64::from(geo.size.h) * scale_y).round().max(1.0) as i32,
                    )
                        .into(),
                ))
            });
            crate::effects::EffectElement {
                inner: smithay::desktop::space::SpaceRenderElements::Element(inner.into()),
                program: program.clone(),
                rect: {
                    let rect = visual_rect.unwrap_or(rect);
                    [
                        rect.loc.x as f32,
                        rect.loc.y as f32,
                        rect.size.w as f32,
                        rect.size.h as f32,
                    ]
                },
                radius,
                blur: retained.map(|(program, _, _, _)| program.clone()),
                blur_strength: retained.map(|(_, _, strength, _)| *strength).unwrap_or(0.0),
                blur_alpha: retained.map(|(_, _, _, alpha)| *alpha).unwrap_or(1.0),
                opacity: 1.0,
                backdrop: retained.map(|(_, backdrop, _, _)| backdrop.clone()),
                frozen_backdrop: true,
                geometry_override,
            }
        })
        .collect();
    let mut elements: Vec<
        crate::render::OutputRenderElements<
            GlesRenderer,
            crate::shell::WindowRenderElement<GlesRenderer>,
        >,
    > = elements
        .into_iter()
        .map(crate::render::OutputRenderElements::Effect)
        .collect();
    if let Some(border) = border {
        elements.push(crate::render::OutputRenderElements::Border(border));
    }
    let mut damage = OutputDamageTracker::new(size, scale, Transform::Normal);
    damage
        .render_output(
            renderer,
            &mut target,
            0,
            &elements,
            if fullscreen {
                crate::drawing::CLEAR_COLOR_FULLSCREEN
            } else {
                smithay::backend::renderer::Color32F::TRANSPARENT
            },
        )
        .map_err(|e| e.to_string())?;
    drop(target);
    Ok((texture, bounds))
}

/// Render an upright output image without a GPU-to-CPU transfer.
/// The texture belongs to this renderer's GLES context; consumers must retain
/// that context and use normal renderer synchronization when crossing contexts.
pub fn render_texture(
    renderer: &mut GlesRenderer,
    space: &Space<WindowElement>,
    output: &Output,
    cursor: Option<&CaptureCursor>,
) -> Result<GlesTexture, String> {
    // Export an upright image in output coordinates. The framebuffer dimensions
    // follow the transformed mode; the offscreen render itself needs no scanout
    // rotation or reflection.
    let size = output
        .current_transform()
        .transform_size(output.current_mode().ok_or("output has no mode")?.size);
    let _bytes = (size.w as usize)
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
    drop(target);
    Ok(texture)
}

/// Render an output directly into a client-provided DMA-BUF. This keeps the
/// capture on the GPU and avoids the synchronous texture mapping used by SHM.
pub fn render_dmabuf(
    renderer: &mut GlesRenderer,
    space: &Space<WindowElement>,
    output: &Output,
    cursor: Option<&CaptureCursor>,
    dmabuf: &mut Dmabuf,
) -> Result<(), String> {
    let size = output
        .current_transform()
        .transform_size(output.current_mode().ok_or("output mode")?.size);
    if dmabuf.size().w != size.w || dmabuf.size().h != size.h {
        return Err("capture DMA-BUF has the wrong size".into());
    }
    (size.w as u64)
        .checked_mul(size.h as u64)
        .and_then(|n| n.checked_mul(4))
        .filter(|n| *n <= 256 * 1024 * 1024)
        .ok_or("capture exceeds size limit")?;
    let mut target = renderer.bind(dmabuf).map_err(|e| e.to_string())?;
    let mut damage_tracker = OutputDamageTracker::new(
        size,
        output.current_scale().fractional_scale(),
        Transform::Normal,
    );
    let cursor_elements = cursor
        .map(|cursor| cursor.render_elements(renderer, output))
        .unwrap_or_default();
    crate::render::render_output(
        output,
        space,
        cursor_elements,
        renderer,
        &mut target,
        &mut damage_tracker,
        0,
        false,
    )
    .map_err(|e| e.to_string())?;
    Ok(())
}

type ConstrainedSurfaceElement = CropRenderElement<
    RelocateRenderElement<RescaleRenderElement<WaylandSurfaceRenderElement<GlesRenderer>>>,
>;

/// Render a single application's client surface and its popups. Using the
/// underlying Smithay window intentionally leaves Luma's server-side frame out.
pub fn render_window_dmabuf(
    renderer: &mut GlesRenderer,
    window: &WindowElement,
    output: &Output,
    dmabuf: &mut Dmabuf,
) -> Result<(), String> {
    let size = output
        .current_transform()
        .transform_size(output.current_mode().ok_or("output mode")?.size);
    if dmabuf.size().w != size.w || dmabuf.size().h != size.h {
        return Err("capture DMA-BUF has the wrong size".into());
    }
    let scale = output.current_scale().fractional_scale();
    let logical_size = size.to_f64().to_logical(scale).to_i32_round();
    let constrain = Rectangle::from_size(logical_size);
    let elements: Vec<ConstrainedSurfaceElement> = constrain_space_element(
        renderer,
        &window.0,
        (0, 0),
        1.0,
        scale,
        constrain,
        ConstrainBehavior {
            reference: ConstrainReference::BoundingBox,
            behavior: ConstrainScaleBehavior::Fit,
            align: ConstrainAlign::CENTER,
        },
    )
    .collect();
    let mut target = renderer.bind(dmabuf).map_err(|e| e.to_string())?;
    let mut damage = OutputDamageTracker::new(size, scale, Transform::Normal);
    damage
        .render_output(
            renderer,
            &mut target,
            0,
            &elements,
            smithay::backend::renderer::Color32F::BLACK,
        )
        .map_err(|e| e.to_string())?;
    Ok(())
}

/// Crop a logical output region and scale it into the recorder's fixed canvas.
/// The intermediate image remains a GPU texture; no CPU readback is involved.
pub fn render_region_dmabuf(
    renderer: &mut GlesRenderer,
    space: &Space<WindowElement>,
    output: &Output,
    cursor: Option<&CaptureCursor>,
    region: wm_core::Rect,
    dmabuf: &mut Dmabuf,
) -> Result<(), String> {
    let output_size = output
        .current_transform()
        .transform_size(output.current_mode().ok_or("output mode")?.size);
    if dmabuf.size().w != output_size.w || dmabuf.size().h != output_size.h {
        return Err("capture DMA-BUF has the wrong size".into());
    }
    let output_geo = space
        .output_geometry(output)
        .ok_or("output has no geometry")?;
    let requested =
        Rectangle::<i32, Logical>::new((region.x, region.y).into(), (region.w, region.h).into());
    let local = requested
        .intersection(output_geo)
        .ok_or("capture region does not intersect the selected output")?;
    let local = Rectangle::new(local.loc - output_geo.loc, local.size);
    let scale = output.current_scale().fractional_scale();
    let output_logical_size = output_size.to_f64().to_logical(scale);
    let source = local
        .to_f64()
        .to_buffer(scale, Transform::Normal, &output_logical_size);
    let texture = render_texture(renderer, space, output, cursor)?;
    let mut target = renderer.bind(dmabuf).map_err(|e| e.to_string())?;
    let mut frame = renderer
        .render(&mut target, output_size, Transform::Normal)
        .map_err(|e| e.to_string())?;
    let full = Rectangle::from_size(output_size);
    frame
        .clear(smithay::backend::renderer::Color32F::BLACK, &[full])
        .map_err(|e| e.to_string())?;
    frame
        .render_texture_from_to(
            &texture,
            source,
            full,
            &[full],
            &[full],
            Transform::Normal,
            1.0,
            None,
            &[],
        )
        .map_err(|e| e.to_string())?;
    let _sync = frame.finish().map_err(|e| e.to_string())?;
    Ok(())
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
    let mut texture = render_texture(renderer, space, output, cursor)?;
    let target = renderer.bind(&mut texture).map_err(|e| e.to_string())?;
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
