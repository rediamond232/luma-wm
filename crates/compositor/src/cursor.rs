use smithay::input::pointer::CursorIcon;
use std::{collections::HashMap, io::Read, time::Duration};

use tracing::warn;
use xcursor::{
    CursorTheme,
    parser::{Image, parse_xcursor},
};

static FALLBACK_CURSOR_DATA: &[u8] = include_bytes!("../resources/cursor.rgba");

pub struct Cursor {
    icons: Vec<Image>,
    size: u32,
    theme: CursorTheme,
    named: HashMap<CursorIcon, Option<Vec<Image>>>,
}

impl Cursor {
    pub fn load() -> Cursor {
        let name = std::env::var("XCURSOR_THEME")
            .ok()
            .unwrap_or_else(|| "default".into());
        let size = std::env::var("XCURSOR_SIZE")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(24);

        let theme = CursorTheme::load(&name);
        let icons = load_icon(&theme, "default")
            .map_err(|err| warn!("Unable to load xcursor: {}, using fallback cursor", err))
            .unwrap_or_else(|_| {
                vec![Image {
                    size: 32,
                    width: 64,
                    height: 64,
                    xhot: 1,
                    yhot: 1,
                    delay: 1,
                    pixels_rgba: Vec::from(FALLBACK_CURSOR_DATA),
                    pixels_argb: vec![], //unused
                }]
            });

        Cursor {
            icons,
            size,
            theme,
            named: HashMap::new(),
        }
    }

    fn get_image(&self, scale: u32, time: Duration) -> (Image, Option<Duration>) {
        let size = self.size.saturating_mul(scale);
        frame(time, size, &self.icons)
    }

    pub fn get_named_frame(
        &mut self,
        icon: CursorIcon,
        scale: u32,
        time: Duration,
    ) -> (Image, Option<Duration>) {
        if icon == CursorIcon::Default {
            return self.get_image(scale, time);
        }
        let images = self.named.entry(icon).or_insert_with(|| {
            std::iter::once(icon.name())
                .chain(icon.alt_names().iter().copied())
                .find_map(|name| load_icon(&self.theme, name).ok())
        });
        frame(
            time,
            self.size.saturating_mul(scale),
            images.as_deref().unwrap_or(&self.icons),
        )
    }
}

fn nearest_images(size: u32, images: &[Image]) -> impl Iterator<Item = &Image> {
    // Follow the nominal size of the cursor to choose the nearest
    let nearest_image = images
        .iter()
        .min_by_key(|image| size.abs_diff(image.size))
        .unwrap();

    images.iter().filter(move |image| {
        image.width == nearest_image.width && image.height == nearest_image.height
    })
}

// The delay is relative to the current frame boundary, not the latest repaint.
// Repainting midway through a frame must not postpone the following frame.
fn frame(time: Duration, size: u32, images: &[Image]) -> (Image, Option<Duration>) {
    let count = nearest_images(size, images).count();
    let total: u64 = nearest_images(size, images)
        .map(|image| u64::from(image.delay))
        .sum();
    if count <= 1 || total == 0 {
        return (nearest_images(size, images).next().unwrap().clone(), None);
    }
    let mut millis = time.as_millis() % u128::from(total);
    for image in nearest_images(size, images) {
        if millis < u128::from(image.delay) {
            return (
                image.clone(),
                Some(Duration::from_millis(
                    (u128::from(image.delay) - millis) as u64,
                )),
            );
        }
        millis -= u128::from(image.delay);
    }
    unreachable!("cursor cycle contains a nonzero frame delay")
}
#[derive(thiserror::Error, Debug)]
enum Error {
    #[error("Theme has no default cursor")]
    NoDefaultCursor,
    #[error("Error opening xcursor file: {0}")]
    File(#[from] std::io::Error),
    #[error("Failed to parse XCursor file")]
    Parse,
}

fn load_icon(theme: &CursorTheme, name: &str) -> Result<Vec<Image>, Error> {
    let icon_path = theme.load_icon(name).ok_or(Error::NoDefaultCursor)?;
    let mut cursor_file = std::fs::File::open(icon_path)?;
    let mut cursor_data = Vec::new();
    cursor_file.read_to_end(&mut cursor_data)?;
    parse_xcursor(&cursor_data)
        .filter(|images| !images.is_empty())
        .ok_or(Error::Parse)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn image(size: u32, delay: u32, marker: u32) -> Image {
        Image {
            size,
            width: size,
            height: size,
            xhot: marker,
            yhot: 0,
            delay,
            pixels_rgba: vec![],
            pixels_argb: vec![],
        }
    }
    #[test]
    fn animation_boundaries_and_idle_deadlines() {
        let images = [image(24, 100, 1), image(24, 50, 2), image(48, 500, 3)];
        for (time, marker, remaining) in [
            (0, 1, 100),
            (99, 1, 1),
            (100, 2, 50),
            (149, 2, 1),
            (150, 1, 100),
        ] {
            let (frame, next) = frame(Duration::from_millis(time), 24, &images);
            assert_eq!(frame.xhot, marker);
            assert_eq!(next, Some(Duration::from_millis(remaining)));
        }
        assert!(frame(Duration::from_millis(300), 48, &images).1.is_none());
        // Cursor selection must not wrap the clock after u32 milliseconds.
        let time = u64::from(u32::MAX) + 123;
        let actual = frame(Duration::from_millis(time), 24, &images);
        let expected = frame(Duration::from_millis(time % 150), 24, &images);
        assert_eq!((actual.0.xhot, actual.1), (expected.0.xhot, expected.1));
    }
    #[test]
    fn static_and_zero_delay_frames_do_not_spin() {
        assert!(frame(Duration::ZERO, 24, &[image(24, 1, 1)]).1.is_none());
        assert!(
            frame(Duration::ZERO, 24, &[image(24, 0, 1), image(24, 0, 2)])
                .1
                .is_none()
        );
        let (selected, next) = frame(Duration::ZERO, 24, &[image(24, 0, 1), image(24, 20, 2)]);
        assert_eq!(selected.xhot, 2);
        assert_eq!(next, Some(Duration::from_millis(20)));
        let images = [image(24, u32::MAX, 1), image(24, u32::MAX, 2)];
        assert_eq!(
            frame(Duration::from_millis(u64::from(u32::MAX)), 24, &images)
                .0
                .xhot,
            2
        );
    }
}
