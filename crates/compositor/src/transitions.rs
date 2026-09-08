//! GPU-retained closing images, scoped to the output/context that created them.
use smithay::reexports::wayland_server::{Resource, backend::ObjectId};
use smithay::wayland::seat::WaylandFocus;
use smithay::{
    backend::renderer::{
        Texture,
        element::{Element, Id, RenderElement},
        gles::{GlesError, GlesFrame, GlesTexture},
        utils::{CommitCounter, DamageSet},
    },
    output::Output,
    utils::{Buffer, Logical, Physical, Rectangle, Scale, Transform},
};
use std::{
    sync::Mutex,
    time::{Duration, Instant},
};
#[derive(Clone, Debug)]
pub struct SnapshotElement {
    pub(crate) below: Vec<Id>,
    pub(crate) source_id: Id,
    texture: GlesTexture,
    rect: Rectangle<i32, Logical>,
    alpha: f32,
    id: Id,
    commit: CommitCounter,
}
#[derive(Debug)]
struct Closing {
    element: SnapshotElement,
    started: Instant,
    duration: Duration,
    workspace: u8,
    source: Option<ObjectId>,
}
#[derive(Default, Debug)]
pub(crate) struct ClosingImages(Mutex<Vec<Closing>>);

const MAX_CLOSING_IMAGES: usize = 8;
const MAX_CLOSING_BYTES: u64 = 128 * 1024 * 1024;

fn retention_evictions(sizes: &[u64], required: u64) -> Option<usize> {
    if required > MAX_CLOSING_BYTES {
        return None;
    }
    let mut retained = sizes.iter().copied().fold(0_u64, u64::saturating_add);
    let mut evictions = 0;
    while evictions < sizes.len()
        && (sizes.len() - evictions >= MAX_CLOSING_IMAGES
            || retained.saturating_add(required) > MAX_CLOSING_BYTES)
    {
        retained = retained.saturating_sub(sizes[evictions]);
        evictions += 1;
    }
    Some(evictions)
}

pub(crate) fn retain(
    output: &Output,
    texture: GlesTexture,
    rect: Rectangle<i32, Logical>,
    duration_ms: u32,
    workspace: u8,
    source: Option<ObjectId>,
    below: Vec<Id>,
    source_id: Id,
) {
    output.user_data().insert_if_missing(ClosingImages::default);
    let mut images = output
        .user_data()
        .get::<ClosingImages>()
        .unwrap()
        .0
        .lock()
        .unwrap();
    // Bound GPU retention during application crashes or mass close operations.
    let bytes = |texture: &GlesTexture| {
        let size = texture.size();
        size.w as u64 * size.h as u64 * 4
    };
    let required = bytes(&texture);
    // Keep the retention bound independent of the capture backend's limit.
    let sizes: Vec<_> = images
        .iter()
        .map(|image| bytes(&image.element.texture))
        .collect();
    let Some(evictions) = retention_evictions(&sizes, required) else {
        return;
    };
    images.drain(..evictions);
    images.push(Closing {
        element: SnapshotElement {
            below,
            source_id,
            texture,
            rect,
            alpha: 1.0,
            id: Id::new(),
            commit: CommitCounter::default(),
        },
        started: Instant::now(),
        duration: Duration::from_millis(u64::from(duration_ms)),
        workspace,
        source,
    });
}

#[cfg(test)]
mod retention_tests {
    use super::{MAX_CLOSING_BYTES, retention_evictions};

    #[test]
    fn image_count_evicts_the_oldest_at_the_eighth_slot() {
        let one_megabyte = 1024 * 1024;
        assert_eq!(
            retention_evictions(&[one_megabyte; 7], one_megabyte),
            Some(0)
        );
        assert_eq!(
            retention_evictions(&[one_megabyte; 8], one_megabyte),
            Some(1)
        );
    }

    #[test]
    fn byte_budget_evicts_only_as_many_oldest_images_as_needed() {
        let quarter = MAX_CLOSING_BYTES / 4;
        assert_eq!(retention_evictions(&[quarter; 3], quarter), Some(0));
        assert_eq!(retention_evictions(&[quarter; 4], 1), Some(1));
        assert_eq!(retention_evictions(&[quarter; 4], quarter * 2), Some(2));
    }

    #[test]
    fn oversized_or_overflowing_accounting_stays_bounded() {
        assert_eq!(retention_evictions(&[], MAX_CLOSING_BYTES + 1), None);
        assert_eq!(retention_evictions(&[u64::MAX], 1), Some(1));
        assert_eq!(retention_evictions(&[], MAX_CLOSING_BYTES), Some(0));
    }
}
pub(crate) fn tick(output: &Output, enabled: bool, workspace: u8) -> bool {
    let Some(images) = output.user_data().get::<ClosingImages>() else {
        return false;
    };
    let mut images = images.0.lock().unwrap();
    images.retain(|image| {
        enabled && image.workspace == workspace && image.started.elapsed() < image.duration
    });
    !images.is_empty()
}
pub(crate) fn elements(output: &Output) -> Vec<SnapshotElement> {
    let Some(images) = output.user_data().get::<ClosingImages>() else {
        return vec![];
    };
    let mut images = images.0.lock().unwrap();
    images
        .iter_mut()
        .map(|image| {
            let progress =
                image.started.elapsed().as_secs_f32() / image.duration.as_secs_f32().max(0.001);
            image.element.alpha = (1.0 - progress.clamp(0.0, 1.0)).powi(3);
            image.element.commit.increment();
            image.element.clone()
        })
        .collect()
}
impl Element for SnapshotElement {
    fn id(&self) -> &Id {
        &self.id
    }
    fn current_commit(&self) -> CommitCounter {
        self.commit
    }
    fn src(&self) -> Rectangle<f64, Buffer> {
        Rectangle::from_size(self.texture.size().to_f64())
    }
    fn transform(&self) -> Transform {
        Transform::Normal
    }
    fn geometry(&self, scale: Scale<f64>) -> Rectangle<i32, Physical> {
        self.rect.to_physical_precise_round(scale)
    }
    fn damage_since(
        &self,
        scale: Scale<f64>,
        _commit: Option<CommitCounter>,
    ) -> DamageSet<i32, Physical> {
        DamageSet::from_slice(&[Rectangle::from_size(self.geometry(scale).size)])
    }
    fn alpha(&self) -> f32 {
        self.alpha
    }
}
impl SnapshotElement {
    pub(crate) fn draw_gles(
        &self,
        frame: &mut GlesFrame<'_, '_>,
        src: Rectangle<f64, Buffer>,
        dst: Rectangle<i32, Physical>,
        damage: &[Rectangle<i32, Physical>],
    ) -> Result<(), GlesError> {
        frame.render_texture_from_to(
            &self.texture,
            src,
            dst,
            damage,
            &[],
            Transform::Normal,
            self.alpha,
            None,
            &[],
        )
    }
}
impl<R: crate::effects::EffectsRenderer> RenderElement<R> for SnapshotElement {
    fn draw(
        &self,
        frame: &mut R::Frame<'_, '_>,
        src: Rectangle<f64, Buffer>,
        dst: Rectangle<i32, Physical>,
        damage: &[Rectangle<i32, Physical>],
        _opaque: &[Rectangle<i32, Physical>],
        _cache: Option<&smithay::utils::user_data::UserDataMap>,
    ) -> Result<(), R::Error> {
        R::draw_snapshot(frame, self, src, dst, damage)
    }
}

#[derive(Default)]
pub(crate) struct Captured(pub std::sync::atomic::AtomicBool);
pub(crate) fn is_captured(
    surface: &smithay::reexports::wayland_server::protocol::wl_surface::WlSurface,
) -> bool {
    smithay::wayland::compositor::with_states(surface, |states| {
        states
            .data_map
            .get::<Captured>()
            .is_some_and(|captured| captured.0.load(std::sync::atomic::Ordering::Acquire))
    })
}

impl<B: crate::state::Backend + 'static> crate::AnvilState<B> {
    pub(crate) fn capture_closing_element(&mut self, window: &crate::shell::WindowElement) {
        if let Some(surface) = window.wl_surface() {
            self.capture_closing_window(surface.as_ref());
        }
    }

    pub(crate) fn cancel_closing_window(
        &mut self,
        surface: &smithay::reexports::wayland_server::protocol::wl_surface::WlSurface,
    ) {
        for output in self.space.outputs() {
            if let Some(images) = output.user_data().get::<ClosingImages>() {
                images
                    .0
                    .lock()
                    .unwrap()
                    .retain(|image| image.source.as_ref() != Some(&surface.id()));
            }
        }
        smithay::wayland::compositor::with_states(surface, |states| {
            if let Some(captured) = states.data_map.get::<Captured>() {
                captured
                    .0
                    .store(false, std::sync::atomic::Ordering::Release);
            }
        });
    }

    pub(crate) fn raise_above_closing(&mut self, window: &crate::shell::WindowElement) {
        let mut ids = None;
        for output in self.space.outputs() {
            let Some(images) = output.user_data().get::<ClosingImages>() else {
                continue;
            };
            let mut images = images.0.lock().unwrap();
            if images.is_empty() {
                continue;
            }
            let ids = ids.get_or_insert_with(|| {
                let mut ids = Vec::new();
                window.with_surfaces(|surface, _| ids.push(Id::from_wayland_resource(surface)));
                let decoration = window.decoration_state();
                if decoration.is_ssd {
                    ids.extend(decoration.header_bar.element_ids());
                }
                ids
            });
            for image in images.iter_mut() {
                image.element.below.retain(|id| !ids.contains(id));
            }
        }
        self.desktop.redraw = true;
    }

    pub(crate) fn capture_closing_window(
        &mut self,
        surface: &smithay::reexports::wayland_server::protocol::wl_surface::WlSurface,
    ) {
        use std::sync::atomic::Ordering;
        if !self.desktop.active
            || self.lock.locked
            || self.desktop.config.theme.reduced_motion
            || self.desktop.config.theme.animation_ms == 0
        {
            return;
        }
        let closing = self
            .desktop
            .windows
            .iter()
            .find(|managed| managed.window.0.wl_surface().as_deref() == Some(surface))
            .map(|managed| {
                (
                    managed.window.clone(),
                    managed.output.clone(),
                    managed.workspace,
                    managed.fullscreen,
                )
            });
        let Some((window, name, workspace, fullscreen)) = closing else {
            return;
        };
        if is_captured(surface) {
            return;
        }
        let Some(location) = self.space.element_location(&window) else {
            return;
        };
        let Some(output) = self.space.outputs().find(|o| o.name() == name).cloned() else {
            return;
        };
        let Some(geometry) = self.space.output_geometry(&output) else {
            return;
        };
        // Space is ordered back to front. Retain the identities beneath this
        // window so its closing image does not jump above surviving windows.
        // Fullscreen rendering overrides normal z-order, so every other window
        // belongs beneath an outgoing fullscreen image.
        let mut below = Vec::new();
        for lower in self.space.elements() {
            if *lower == window {
                if fullscreen {
                    continue;
                }
                break;
            }
            lower.with_surfaces(|surface, _| below.push(Id::from_wayland_resource(surface)));
            let decoration = lower.decoration_state();
            if decoration.is_ssd {
                below.extend(decoration.header_bar.element_ids());
            }
        }
        match self
            .backend_data
            .snapshot_window(&window, &output, fullscreen)
        {
            Ok((texture, mut bounds)) => {
                bounds.loc += location - geometry.loc;
                retain(
                    &output,
                    texture,
                    bounds,
                    self.desktop.config.theme.animation_ms,
                    workspace,
                    Some(surface.id()),
                    below,
                    Id::from_wayland_resource(surface),
                );
                smithay::wayland::compositor::with_states(surface, |states| {
                    states.data_map.insert_if_missing(Captured::default);
                    states
                        .data_map
                        .get::<Captured>()
                        .unwrap()
                        .0
                        .store(true, Ordering::Release);
                });
            }
            Err(error) => tracing::debug!(%error, "could not retain closing window"),
        }
    }

    pub(crate) fn capture_workspace_transition(
        &mut self,
        output_name: &str,
        source_workspace: u8,
        target_workspace: u8,
    ) {
        if source_workspace == target_workspace
            || !self.desktop.active
            || self.lock.locked
            || self.desktop.config.theme.reduced_motion
            || self.desktop.config.theme.animation_ms == 0
        {
            return;
        }
        let Some(output) = self
            .space
            .outputs()
            .find(|output| output.name() == output_name)
            .cloned()
        else {
            return;
        };
        let Some(output_geometry) = self.space.output_geometry(&output) else {
            return;
        };
        let fullscreen = self
            .desktop
            .windows
            .iter()
            .find(|managed| {
                managed.output == output_name
                    && managed.workspace == source_workspace
                    && managed.fullscreen
                    && !managed.scratchpad
                    && self.space.element_location(&managed.window).is_some()
            })
            .map(|managed| managed.window.clone());
        let windows: Vec<_> = self
            .desktop
            .windows
            .iter()
            .filter(|managed| {
                managed.output == output_name
                    && managed.workspace == source_workspace
                    && !managed.scratchpad
                    && self.space.element_location(&managed.window).is_some()
                    && fullscreen
                        .as_ref()
                        .is_none_or(|fullscreen| managed.window == *fullscreen)
            })
            .map(|managed| (managed.window.clone(), managed.fullscreen))
            .collect();

        for (window, fullscreen) in windows {
            let Some(location) = self.space.element_location(&window) else {
                continue;
            };
            let mut below = Vec::new();
            for lower in self.space.elements() {
                if *lower == window {
                    if fullscreen {
                        continue;
                    }
                    break;
                }
                lower.with_surfaces(|surface, _| {
                    below.push(Id::from_wayland_resource(surface));
                });
                let decoration = lower.decoration_state();
                if decoration.is_ssd {
                    below.extend(decoration.header_bar.element_ids());
                }
            }
            let surface = window.0.wl_surface();
            let source = surface.as_deref().map(Resource::id);
            let source_id = surface
                .as_deref()
                .map(Id::from_wayland_resource)
                .unwrap_or_else(Id::new);
            match self
                .backend_data
                .snapshot_window(&window, &output, fullscreen)
            {
                Ok((texture, mut bounds)) => {
                    bounds.loc += location - output_geometry.loc;
                    retain(
                        &output,
                        texture,
                        bounds,
                        self.desktop.config.theme.animation_ms,
                        target_workspace,
                        source,
                        below,
                        source_id,
                    );
                }
                Err(error) => {
                    tracing::debug!(%error, "could not retain outgoing workspace window")
                }
            }
        }
    }
}
