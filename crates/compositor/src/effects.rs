//! Damage-aware GLES effects. Fullscreen presentation bypasses this path.
use crate::shell::WindowRenderElement;
use smithay::{
    backend::renderer::{
        Bind, BlitFrame, FrameContext, Offscreen, Texture, TextureFilter,
        element::{Element, Id, RenderElement},
        gles::{
            GlesError, GlesFrame, GlesPixelProgram, GlesRenderer, GlesTexProgram, GlesTexture,
            Uniform, UniformName, UniformType,
        },
        utils::{CommitCounter, DamageSet, OpaqueRegions},
    },
    desktop::space::SpaceRenderElements,
    utils::{Buffer, Logical, Physical, Rectangle, Scale, Transform, user_data::UserDataMap},
};

type Scene = SpaceRenderElements<GlesRenderer, WindowRenderElement<GlesRenderer>>;

#[derive(Debug, Clone, PartialEq)]
struct BorderStyle {
    rect: Rectangle<i32, Logical>,
    color: [f32; 4],
    radius: f32,
    width: f32,
    shadow_size: f32,
    shadow_opacity: f32,
}
#[derive(Debug, Clone)]
pub struct BorderElement {
    id: Id,
    commit: CommitCounter,
    style: BorderStyle,
    program: GlesPixelProgram,
}
#[derive(Default)]
struct BorderCache(std::sync::Mutex<Option<BorderElement>>);
#[derive(Default)]
pub struct WindowFocused(pub std::sync::Mutex<bool>);
pub enum ScenePart {
    Window(EffectElement),
    Border(BorderElement),
}
impl Element for BorderElement {
    fn id(&self) -> &Id {
        &self.id
    }
    fn current_commit(&self) -> CommitCounter {
        self.commit
    }
    fn src(&self) -> Rectangle<f64, Buffer> {
        Rectangle::from_size(
            self.style
                .rect
                .size
                .to_f64()
                .to_buffer(1.0, Transform::Normal),
        )
    }
    fn geometry(&self, scale: Scale<f64>) -> Rectangle<i32, Physical> {
        self.style.rect.to_physical_precise_round(scale)
    }
}
impl BorderElement {
    fn draw_gles(
        &self,
        frame: &mut GlesFrame<'_, '_>,
        src: Rectangle<f64, Buffer>,
        dst: Rectangle<i32, Physical>,
        damage: &[Rectangle<i32, Physical>],
    ) -> Result<(), GlesError> {
        let scale = dst.size.w as f32 / self.style.rect.size.w as f32;
        let inset = self.style.shadow_size * scale;
        frame.render_pixel_shader_to(
            &self.program,
            src,
            dst,
            self.style.rect.size.to_buffer(1, Transform::Normal),
            Some(damage),
            1.0,
            &[
                Uniform::new(
                    "wm_rect",
                    [
                        dst.loc.x as f32 + inset,
                        dst.loc.y as f32 + inset,
                        dst.size.w as f32 - inset * 2.0,
                        dst.size.h as f32 - inset * 2.0,
                    ],
                ),
                Uniform::new("wm_color", self.style.color),
                Uniform::new("wm_radius", self.style.radius * scale),
                Uniform::new("wm_width", self.style.width * scale),
                Uniform::new("wm_shadow_size", inset),
                Uniform::new("wm_shadow_opacity", self.style.shadow_opacity),
            ],
        )
    }
}
impl<R: EffectsRenderer> RenderElement<R> for BorderElement {
    fn draw(
        &self,
        frame: &mut R::Frame<'_, '_>,
        src: Rectangle<f64, Buffer>,
        dst: Rectangle<i32, Physical>,
        damage: &[Rectangle<i32, Physical>],
        _: &[Rectangle<i32, Physical>],
        _: Option<&UserDataMap>,
    ) -> Result<(), R::Error> {
        R::draw_border(frame, self, src, dst, damage)
    }
}

#[derive(Debug)]
pub struct EffectElement {
    pub inner: Scene,
    pub program: GlesTexProgram,
    pub rect: [f32; 4],
    pub radius: f32,
    pub blur: Option<GlesTexProgram>,
    pub blur_strength: f32,
    pub blur_alpha: f32,
}

impl Element for EffectElement {
    fn id(&self) -> &Id {
        self.inner.id()
    }
    fn current_commit(&self) -> CommitCounter {
        self.inner.current_commit()
    }
    fn src(&self) -> Rectangle<f64, Buffer> {
        self.inner.src()
    }
    fn transform(&self) -> Transform {
        self.inner.transform()
    }
    fn geometry(&self, scale: Scale<f64>) -> Rectangle<i32, Physical> {
        self.inner.geometry(scale)
    }
    fn damage_since(
        &self,
        scale: Scale<f64>,
        commit: Option<CommitCounter>,
    ) -> DamageSet<i32, Physical> {
        self.inner.damage_since(scale, commit)
    }
    fn opaque_regions(&self, scale: Scale<f64>) -> OpaqueRegions<i32, Physical> {
        if self.radius > 0.0 {
            let origin = self.geometry(scale).loc;
            let interior = rounded_interior(self.rect, self.radius);
            self.inner
                .opaque_regions(scale)
                .into_iter()
                .flat_map(|region| {
                    interior.iter().filter_map(move |clip| {
                        let local = Rectangle::new(clip.loc - origin, clip.size);
                        region.intersection(local)
                    })
                })
                .collect()
        } else {
            self.inner.opaque_regions(scale)
        }
    }
    fn alpha(&self) -> f32 {
        self.inner.alpha()
    }
    fn is_framebuffer_effect(&self) -> bool {
        self.blur.is_some()
    }
}

/// A conservative, disjoint cross inside the rounded mask. Keep the shader's
/// antialiased edge out of opaque regions so background pixels remain available.
fn rounded_interior(rect: [f32; 4], radius: f32) -> OpaqueRegions<i32, Physical> {
    let [x, y, w, h] = rect;
    let radius = radius.min(w / 2.0).min(h / 2.0).max(0.0);
    let left = (x + 1.0).ceil() as i32;
    let top = (y + 1.0).ceil() as i32;
    let right = (x + w - 1.0).floor() as i32;
    let bottom = (y + h - 1.0).floor() as i32;
    let inner_left = (x + radius + 1.0).ceil() as i32;
    let inner_right = (x + w - radius - 1.0).floor() as i32;
    let inner_top = (y + radius + 1.0).ceil() as i32;
    let inner_bottom = (y + h - radius - 1.0).floor() as i32;
    [
        (inner_left, top, inner_right, bottom),
        (left, inner_top, inner_left.min(right), inner_bottom),
        (
            inner_right.max(inner_left).max(left),
            inner_top,
            right,
            inner_bottom,
        ),
    ]
    .into_iter()
    .filter_map(|(x1, y1, x2, y2)| {
        (x2 > x1 && y2 > y1).then(|| Rectangle::new((x1, y1).into(), (x2 - x1, y2 - y1).into()))
    })
    .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn opaque_pixels_stay_inside_shader_mask() {
        for rect in [
            [0., 0., 80., 50.],
            [-25.5, 3.25, 90.5, 70.25],
            [1., 1., 3., 3.],
        ] {
            for radius in [0.5_f32, 10., 40., 100.] {
                let regions = rounded_interior(rect, radius);
                let r = radius.min(rect[2] / 2.).min(rect[3] / 2.);
                for (i, region) in regions.iter().enumerate() {
                    assert!(
                        regions[i + 1..]
                            .iter()
                            .all(|other| region.intersection(*other).is_none())
                    );
                    for y in region.loc.y..region.loc.y + region.size.h {
                        for x in region.loc.x..region.loc.x + region.size.w {
                            let px =
                                (x as f32 + 0.5 - rect[0] - rect[2] / 2.).abs() - rect[2] / 2. + r;
                            let py =
                                (y as f32 + 0.5 - rect[1] - rect[3] / 2.).abs() - rect[3] / 2. + r;
                            let distance = px.max(0.).hypot(py.max(0.)) + px.max(py).min(0.) - r;
                            assert!(
                                distance <= -0.75,
                                "opaque pixel overlaps antialiasing: {rect:?} {radius} {x},{y}"
                            );
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn typical_window_preserves_almost_all_occlusion() {
        let regions = rounded_interior([0., 0., 1000., 700.], 10.);
        let area: i32 = regions.iter().map(|r| r.size.w * r.size.h).sum();
        assert!(area > 1000 * 700 * 99 / 100);
    }

    #[test]
    fn background_coverage_requires_every_pixel() {
        let area = Rectangle::new((0, 0).into(), (100, 80).into());
        let left = Rectangle::new((-10, 0).into(), (60, 80).into());
        let right = Rectangle::new((50, 0).into(), (60, 80).into());
        assert!(fully_covered(area, [left, right]));
        assert!(!fully_covered(area, [left]));
        assert!(!fully_covered(
            area,
            [left, Rectangle::new((51, 0).into(), (60, 80).into())]
        ));
        assert!(!fully_covered(
            area,
            rounded_interior([0., 0., 100., 80.], 10.)
        ));
        assert!(!fully_covered(Rectangle::default(), [left, right]));
    }
}

impl EffectElement {
    fn draw_gles(
        &self,
        frame: &mut GlesFrame<'_, '_>,
        src: Rectangle<f64, Buffer>,
        dst: Rectangle<i32, Physical>,
        damage: &[Rectangle<i32, Physical>],
        opaque: &[Rectangle<i32, Physical>],
        cache: Option<&UserDataMap>,
    ) -> Result<(), GlesError> {
        if let (Some(program), Some(texture)) = (
            &self.blur,
            cache.and_then(|c| c.get::<std::sync::Mutex<Option<GlesTexture>>>()),
        ) {
            if let Some(texture) = texture.lock().unwrap().as_ref() {
                let size = texture.size();
                frame.render_texture_from_to(
                    texture,
                    Rectangle::from_size(size.to_f64()),
                    dst,
                    damage,
                    &[],
                    Transform::Normal,
                    self.blur_alpha,
                    Some(program),
                    &[
                        Uniform::new(
                            "wm_step",
                            [
                                self.blur_strength / size.w as f32,
                                self.blur_strength / size.h as f32,
                            ],
                        ),
                        Uniform::new("wm_rect", self.rect),
                        Uniform::new("wm_radius", self.radius),
                    ],
                )?;
            }
        }
        if self.radius > 0.0 {
            frame.override_default_tex_program(
                self.program.clone(),
                vec![
                    Uniform::new("wm_rect", self.rect),
                    Uniform::new("wm_radius", self.radius),
                ],
            );
        }
        let result = self.inner.draw(frame, src, dst, damage, opaque, cache);
        frame.clear_tex_program_override();
        result
    }
    fn capture_gles(
        &self,
        frame: &mut GlesFrame<'_, '_>,
        dst: Rectangle<i32, Physical>,
        cache: &UserDataMap,
    ) -> Result<(), GlesError> {
        use smithay::backend::allocator::Fourcc;
        cache.insert_if_missing(|| std::sync::Mutex::new(None::<GlesTexture>));
        let mut texture = cache
            .get::<std::sync::Mutex<Option<GlesTexture>>>()
            .unwrap()
            .lock()
            .unwrap();
        let size = ((dst.size.w / 4).max(1), (dst.size.h / 4).max(1)).into();
        if texture.as_ref().is_none_or(|t| t.size() != size) {
            let mut guard = frame.renderer();
            *texture = Some(guard.as_mut().create_buffer(Fourcc::Abgr8888, size)?);
        }
        let texture = texture.as_mut().unwrap();
        let mut target = {
            let mut guard = frame.renderer();
            guard.as_mut().bind(texture)?
        };
        // Capture and sampling use the same GLES context and command stream.
        let _sync = frame.blit_to(
            &mut target,
            dst,
            Rectangle::from_size((size.w, size.h).into()),
            TextureFilter::Linear,
        )?;
        Ok(())
    }
}

pub trait EffectsRenderer: smithay::backend::renderer::Renderer {
    fn draw_border(
        frame: &mut Self::Frame<'_, '_>,
        border: &BorderElement,
        src: Rectangle<f64, Buffer>,
        dst: Rectangle<i32, Physical>,
        damage: &[Rectangle<i32, Physical>],
    ) -> Result<(), Self::Error>;
    fn gles_mut(&mut self) -> &mut GlesRenderer;
    fn draw_effect(
        frame: &mut Self::Frame<'_, '_>,
        effect: &EffectElement,
        src: Rectangle<f64, Buffer>,
        dst: Rectangle<i32, Physical>,
        damage: &[Rectangle<i32, Physical>],
        opaque: &[Rectangle<i32, Physical>],
        cache: Option<&UserDataMap>,
    ) -> Result<(), Self::Error>;
    fn capture_effect(
        frame: &mut Self::Frame<'_, '_>,
        effect: &EffectElement,
        dst: Rectangle<i32, Physical>,
        cache: &UserDataMap,
    ) -> Result<(), Self::Error>;
}
impl EffectsRenderer for GlesRenderer {
    fn draw_border(
        frame: &mut Self::Frame<'_, '_>,
        border: &BorderElement,
        src: Rectangle<f64, Buffer>,
        dst: Rectangle<i32, Physical>,
        damage: &[Rectangle<i32, Physical>],
    ) -> Result<(), Self::Error> {
        border.draw_gles(frame, src, dst, damage)
    }
    fn gles_mut(&mut self) -> &mut GlesRenderer {
        self
    }
    fn capture_effect(
        frame: &mut Self::Frame<'_, '_>,
        effect: &EffectElement,
        dst: Rectangle<i32, Physical>,
        cache: &UserDataMap,
    ) -> Result<(), Self::Error> {
        effect.capture_gles(frame, dst, cache)
    }
    fn draw_effect(
        frame: &mut Self::Frame<'_, '_>,
        effect: &EffectElement,
        src: Rectangle<f64, Buffer>,
        dst: Rectangle<i32, Physical>,
        damage: &[Rectangle<i32, Physical>],
        opaque: &[Rectangle<i32, Physical>],
        cache: Option<&UserDataMap>,
    ) -> Result<(), Self::Error> {
        effect.draw_gles(frame, src, dst, damage, opaque, cache)
    }
}
#[cfg(feature = "udev")]
impl EffectsRenderer for crate::udev::UdevRenderer<'_> {
    fn draw_border(
        frame: &mut Self::Frame<'_, '_>,
        border: &BorderElement,
        src: Rectangle<f64, Buffer>,
        dst: Rectangle<i32, Physical>,
        damage: &[Rectangle<i32, Physical>],
    ) -> Result<(), Self::Error> {
        border
            .draw_gles(frame.as_mut(), src, dst, damage)
            .map_err(Into::into)
    }
    fn gles_mut(&mut self) -> &mut GlesRenderer {
        self.as_mut()
    }
    fn capture_effect(
        frame: &mut Self::Frame<'_, '_>,
        effect: &EffectElement,
        dst: Rectangle<i32, Physical>,
        cache: &UserDataMap,
    ) -> Result<(), Self::Error> {
        effect
            .capture_gles(frame.as_mut(), dst, cache)
            .map_err(Into::into)
    }
    fn draw_effect(
        frame: &mut Self::Frame<'_, '_>,
        effect: &EffectElement,
        src: Rectangle<f64, Buffer>,
        dst: Rectangle<i32, Physical>,
        damage: &[Rectangle<i32, Physical>],
        opaque: &[Rectangle<i32, Physical>],
        cache: Option<&UserDataMap>,
    ) -> Result<(), Self::Error> {
        effect
            .draw_gles(frame.as_mut(), src, dst, damage, opaque, cache)
            .map_err(Into::into)
    }
}
impl<R: EffectsRenderer> RenderElement<R> for EffectElement {
    fn capture_framebuffer(
        &self,
        frame: &mut R::Frame<'_, '_>,
        _src: Rectangle<f64, Buffer>,
        dst: Rectangle<i32, Physical>,
        cache: &UserDataMap,
    ) -> Result<(), R::Error> {
        R::capture_effect(frame, self, dst, cache)
    }
    fn draw(
        &self,
        frame: &mut R::Frame<'_, '_>,
        src: Rectangle<f64, Buffer>,
        dst: Rectangle<i32, Physical>,
        damage: &[Rectangle<i32, Physical>],
        opaque: &[Rectangle<i32, Physical>],
        cache: Option<&UserDataMap>,
    ) -> Result<(), R::Error> {
        R::draw_effect(frame, self, src, dst, damage, opaque, cache)
    }
}

#[derive(Debug, Clone)]
struct Programs {
    round: GlesTexProgram,
    blur: GlesTexProgram,
    border: GlesPixelProgram,
}

pub fn program(renderer: &mut GlesRenderer) -> Result<GlesTexProgram, GlesError> {
    if let Some(p) = renderer.egl_context().user_data().get::<Programs>() {
        return Ok(p.round.clone());
    }
    let round = renderer.compile_custom_texture_shader(
        include_str!("shaders/round.frag"),
        &[
            UniformName::new("wm_rect", UniformType::_4f),
            UniformName::new("wm_radius", UniformType::_1f),
        ],
    )?;
    let blur = renderer.compile_custom_texture_shader(
        include_str!("shaders/blur.frag"),
        &[
            UniformName::new("wm_step", UniformType::_2f),
            UniformName::new("wm_rect", UniformType::_4f),
            UniformName::new("wm_radius", UniformType::_1f),
        ],
    )?;
    let border = renderer.compile_custom_pixel_shader(
        include_str!("shaders/border.frag"),
        &[
            UniformName::new("wm_rect", UniformType::_4f),
            UniformName::new("wm_color", UniformType::_4f),
            UniformName::new("wm_radius", UniformType::_1f),
            UniformName::new("wm_width", UniformType::_1f),
            UniformName::new("wm_shadow_size", UniformType::_1f),
            UniformName::new("wm_shadow_opacity", UniformType::_1f),
        ],
    )?;
    renderer
        .egl_context()
        .user_data()
        .insert_if_missing(|| Programs {
            round: round.clone(),
            blur,
            border,
        });
    Ok(round)
}

#[derive(Debug, Clone, Default)]
pub struct OutputTheme(pub wm_core::Theme);

/// Conservative renderer evidence: false unless opaque foreground regions cover
/// every output pixel. Blur and rotated outputs keep background playback active.
#[derive(Default)]
pub struct BackgroundCovered(pub std::sync::atomic::AtomicBool);

fn fully_covered(
    area: Rectangle<i32, Physical>,
    regions: impl IntoIterator<Item = Rectangle<i32, Physical>>,
) -> bool {
    if area.is_empty() {
        return false;
    }
    let mut remaining = vec![area];
    for (index, region) in regions.into_iter().enumerate() {
        if index >= 4096 {
            return false;
        }
        remaining = Rectangle::subtract_rects_many_in_place(remaining, [region]);
        if remaining.is_empty() {
            return true;
        }
        // Pathological surface regions must not make visibility work unbounded.
        if remaining.len() > 256 {
            return false;
        }
    }
    false
}

pub fn scene(
    renderer: &mut GlesRenderer,
    space: &smithay::desktop::Space<crate::shell::WindowElement>,
    output: &smithay::output::Output,
) -> Result<Vec<ScenePart>, GlesError> {
    use smithay::desktop::space::SpaceElement;
    let program = program(renderer)?;
    let blur_program = renderer
        .egl_context()
        .user_data()
        .get::<Programs>()
        .unwrap()
        .blur
        .clone();
    let scale = output.current_scale().fractional_scale();
    let output_geo = space.output_geometry(output).unwrap_or_default();
    let theme = output
        .user_data()
        .get::<std::sync::Mutex<OutputTheme>>()
        .map(|t| t.lock().unwrap().0.clone())
        .unwrap_or_default();
    let windows: Vec<_> = space
        .elements_for_output(output)
        .filter_map(|w| {
            let loc = space.element_location(w)?;
            let rect = Rectangle::new(loc + w.geometry().loc - output_geo.loc, w.geometry().size)
                .to_physical_precise_round(scale);
            let mut ids = Vec::new();
            w.with_surfaces(|surface, _| ids.push(Id::from_wayland_resource(surface)));
            let blur =
                w.0.user_data()
                    .get::<crate::shell::WindowBlur>()
                    .is_some_and(|blur| *blur.0.lock().unwrap());
            // Capture once beneath the root, not once for every subsurface.
            let blur_id = if blur {
                w.wl_surface()
                    .map(|surface| Id::from_wayland_resource(surface.as_ref()))
            } else {
                None
            };
            let opening =
                w.0.user_data()
                    .get::<crate::shell::WindowOpening>()
                    .map(|opening| *opening.0.lock().unwrap())
                    .unwrap_or(1.0);
            Some((ids, rect, blur_id, opening))
        })
        .collect();
    let mut borders = std::collections::HashMap::new();
    let shadow_size = if theme.shadow_opacity > 0.0 {
        theme.shadow_size
    } else {
        0
    };
    if (theme.border > 0 || shadow_size > 0) && output.current_transform() == Transform::Normal {
        let border_program = renderer
            .egl_context()
            .user_data()
            .get::<Programs>()
            .unwrap()
            .border
            .clone();
        for window in space.elements_for_output(output) {
            let (Some(loc), Some(root)) = (space.element_location(window), window.wl_surface())
            else {
                continue;
            };
            let geo = window.geometry();
            if geo.is_empty() {
                continue;
            }
            let active = window
                .0
                .user_data()
                .get::<WindowFocused>()
                .is_some_and(|active| *active.0.lock().unwrap());
            let mut color = wm_core::color(if active { &theme.accent } else { &theme.muted })
                .unwrap_or([1.0; 4]);
            let opening = window
                .0
                .user_data()
                .get::<crate::shell::WindowOpening>()
                .map(|opening| *opening.0.lock().unwrap())
                .unwrap_or(1.0);
            color[3] *= opening;
            let width = theme.border;
            let extent = width + shadow_size;
            let style = BorderStyle {
                rect: Rectangle::new(
                    loc + geo.loc - output_geo.loc - smithay::utils::Point::from((extent, extent)),
                    (geo.size.w + extent * 2, geo.size.h + extent * 2).into(),
                ),
                color,
                radius: theme.radius + width as f32,
                width: width as f32,
                shadow_size: shadow_size as f32,
                shadow_opacity: theme.shadow_opacity * opening,
            };
            window.0.user_data().insert_if_missing(BorderCache::default);
            let mut cache = window
                .0
                .user_data()
                .get::<BorderCache>()
                .unwrap()
                .0
                .lock()
                .unwrap();
            let border = cache.get_or_insert_with(|| BorderElement {
                id: Id::new(),
                commit: CommitCounter::default(),
                style: style.clone(),
                program: border_program.clone(),
            });
            if border.style != style {
                border.style = style;
                border.commit.increment();
            }
            border.program = border_program.clone();
            borders.insert(Id::from_wayland_resource(root.as_ref()), border.clone());
        }
    }
    let elements = smithay::desktop::space::space_render_elements::<
        _,
        crate::shell::WindowElement,
        _,
    >(renderer, [space], output, 1.0)
    .unwrap_or_default();
    let map = smithay::desktop::layer_map_for_output(output);
    let blurred: Vec<_> = map
        .layers()
        .filter(|l| {
            matches!(
                l.namespace(),
                "wm-bar" | "wm-launcher" | "wm-notifications" | "wm-notification-center"
            )
        })
        .map(|l| Id::from_wayland_resource(l.wl_surface()))
        .collect();
    let mut background_ids = Vec::new();
    for layer in map.layers().filter(|layer| {
        matches!(
            layer.layer(),
            smithay::wayland::shell::wlr_layer::Layer::Background
                | smithay::wayland::shell::wlr_layer::Layer::Bottom
        )
    }) {
        layer.with_surfaces(|surface, _| background_ids.push(Id::from_wayland_resource(surface)));
    }
    let effects: Vec<_> = elements
        .into_iter()
        .map(|inner| {
            let geo = inner.geometry(scale.into());
            let rect = if matches!(inner, SpaceRenderElements::Element(_)) {
                windows
                    .iter()
                    .find(|(ids, _, _, _)| ids.contains(inner.id()))
                    .map(|(_, rect, _, _)| *rect)
                    .unwrap_or(geo)
            } else {
                geo
            };
            let blur = (theme.blur
                && theme.blur_passes > 0
                && (blurred.contains(inner.id())
                    || windows
                        .iter()
                        .any(|(_, _, id, _)| id.as_ref() == Some(inner.id())))
                && output.current_transform() == Transform::Normal)
                .then(|| blur_program.clone());
            let radius = if (matches!(inner, SpaceRenderElements::Element(_)) || blur.is_some())
                && output.current_transform() == Transform::Normal
            {
                theme.radius * scale as f32
            } else {
                0.0
            };
            let blur_alpha = windows
                .iter()
                .find(|(ids, _, _, _)| ids.contains(inner.id()))
                .map(|(_, _, _, opening)| *opening)
                .unwrap_or(1.0);
            EffectElement {
                inner,
                program: program.clone(),
                rect: [
                    rect.loc.x as f32,
                    rect.loc.y as f32,
                    rect.size.w as f32,
                    rect.size.h as f32,
                ],
                radius,
                blur,
                blur_strength: theme.blur_passes as f32,
                blur_alpha,
            }
        })
        .collect();
    let covered = output.current_transform() == Transform::Normal
        && !effects.iter().any(|effect| effect.blur.is_some())
        && fully_covered(
            Rectangle::from_size(output_geo.size.to_physical_precise_round(scale)),
            effects
                .iter()
                .filter(|effect| !background_ids.contains(effect.id()))
                .flat_map(|effect| {
                    let origin = effect.geometry(scale.into()).loc;
                    effect
                        .opaque_regions(scale.into())
                        .into_iter()
                        .map(move |region| Rectangle::new(region.loc + origin, region.size))
                }),
        );
    output
        .user_data()
        .insert_if_missing(BackgroundCovered::default);
    output
        .user_data()
        .get::<BackgroundCovered>()
        .unwrap()
        .0
        .store(covered, std::sync::atomic::Ordering::Relaxed);
    let mut scene = Vec::with_capacity(effects.len() + borders.len());
    for effect in effects {
        let border = borders.remove(effect.id());
        scene.push(ScenePart::Window(effect));
        if let Some(border) = border {
            scene.push(ScenePart::Border(border));
        }
    }
    Ok(scene)
}
