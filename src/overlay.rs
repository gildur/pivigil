//! Cached in-process text rendering into the small annotated region of a YUV420 frame.
use crate::config::{Overlay, OverlayPosition};
use anyhow::{Context, Result, ensure};
use fontdue::{
    Font, FontSettings,
    layout::{CoordinateSystem, GlyphRasterConfig, Layout, LayoutSettings, TextStyle},
};
use std::{
    collections::HashMap,
    ffi::CString,
    time::{SystemTime, UNIX_EPOCH},
};

const FONT: &[u8] = include_bytes!("../assets/NK57Monospace-Regular.otf");
const PADDING: usize = 4;

pub struct VideoOverlay {
    settings: Overlay,
    font: Font,
    layout: Layout,
    glyphs: HashMap<GlyphRasterConfig, Vec<u8>>,
    format: Option<CString>,
    datetime_template: String,
    show_frame: bool,
    width: usize,
    height: usize,
    last_second: Option<u64>,
    last_frame: u32,
    text: String,
    tile: Vec<u8>, // Cached glyph coverage, not a flattened background.
    opaque_tile: Vec<u8>,
    chroma_alpha: Vec<u8>,
    background_alpha: u8,
    tile_width: usize,
    tile_height: usize,
    x: usize,
    y: usize,
}

impl VideoOverlay {
    pub fn new(settings: &Overlay, width: u32, height: u32) -> Result<Option<Self>> {
        if !settings.enabled {
            return Ok(None);
        }
        let font = Font::from_bytes(
            FONT,
            FontSettings {
                scale: settings.font_size as f32,
                load_substitutions: false,
                ..FontSettings::default()
            },
        )
        .map_err(|e| anyhow::anyhow!("load embedded overlay font: {e}"))?;
        let format = if settings.datetime_format.is_empty() {
            None
        } else {
            Some(
                CString::new(settings.datetime_format.as_str())
                    .context("overlay datetime format contains NUL")?,
            )
        };
        let mut renderer = Self {
            settings: settings.clone(),
            font,
            layout: Layout::new(CoordinateSystem::PositiveYDown),
            glyphs: HashMap::new(),
            format,
            datetime_template: String::new(),
            show_frame: settings.datetime_format.contains("{frame}"),
            width: width as usize,
            height: height as usize,
            last_second: None,
            last_frame: 0,
            text: String::new(),
            tile: Vec::new(),
            opaque_tile: Vec::new(),
            chroma_alpha: Vec::new(),
            background_alpha: (settings.background_opacity * 255.0).round() as u8,
            tile_width: 0,
            tile_height: 0,
            x: 0,
            y: 0,
        };
        renderer.refresh(UNIX_EPOCH, 0)?; // Validate formatting/layout before starting capture.
        Ok(Some(renderer))
    }

    fn refresh(&mut self, captured_at: SystemTime, frame: u32) -> Result<()> {
        let second = if self.format.is_some() {
            captured_at.duration_since(UNIX_EPOCH)?.as_secs()
        } else {
            0
        };
        if self.last_second == Some(second) && (!self.show_frame || self.last_frame == frame) {
            return Ok(());
        }
        if self.last_second != Some(second) {
            self.datetime_template = match &self.format {
                Some(format) => datetime(second, format)?,
                None => String::new(),
            };
        }
        self.text.clear();
        self.text.push_str(&self.settings.text);
        if !self.datetime_template.is_empty() {
            if !self.text.is_empty() {
                self.text.push(' ');
            }
            if self.show_frame {
                self.text.push_str(
                    &self
                        .datetime_template
                        .replace("{frame}", &format!("{frame:02}")),
                );
            } else {
                self.text.push_str(&self.datetime_template);
            }
        }
        let margin = self.settings.margin as usize;
        let max_width = self
            .width
            .checked_sub(2 * margin)
            .context("overlay margin exceeds image width")?
            & !1;
        let max_height = self
            .height
            .checked_sub(2 * margin)
            .context("overlay margin exceeds image height")?
            & !1;
        ensure!(
            max_width > 2 * PADDING && max_height > 2 * PADDING,
            "overlay has no drawable area"
        );
        // Keep label and datetime on one line; long lines are clipped to the
        // frame rather than automatically wrapping the datetime underneath.
        self.layout.reset(&LayoutSettings::default());
        self.layout.append(
            &[&self.font],
            &TextStyle::new(&self.text, self.settings.font_size as f32, 0),
        );
        let ink_width = self
            .layout
            .glyphs()
            .iter()
            .map(|g| (g.x + g.width as f32).max(0.0).ceil() as usize)
            .max()
            .unwrap_or(0);
        self.tile_width = ((ink_width + 2 * PADDING + 1) & !1).min(max_width);
        self.tile_height = ((self.layout.height().max(1.0).ceil() as usize + 2 * PADDING + 1) & !1)
            .min(max_height);
        self.tile.resize(self.tile_width * self.tile_height, 0);
        self.tile.fill(0); // Transparent outside actual glyph strokes.
        for glyph in self.layout.glyphs() {
            if glyph.width == 0 || glyph.height == 0 {
                continue;
            }
            let bitmap = self
                .glyphs
                .entry(glyph.key)
                .or_insert_with(|| self.font.rasterize_config(glyph.key).1);
            let x = glyph.x as isize + PADDING as isize;
            let y = glyph.y as isize + PADDING as isize;
            for row in 0..glyph.height {
                let ty = y + row as isize;
                if ty < 0 || ty >= self.tile_height as isize {
                    continue;
                }
                for column in 0..glyph.width {
                    let tx = x + column as isize;
                    if tx < 0 || tx >= self.tile_width as isize {
                        continue;
                    }
                    let value = bitmap[row * glyph.width + column];
                    let target = &mut self.tile[ty as usize * self.tile_width + tx as usize];
                    *target = (*target).max(value);
                }
            }
        }
        if self.background_alpha == 255 {
            self.opaque_tile.resize(self.tile.len(), 16);
            for (out, &alpha) in self.opaque_tile.iter_mut().zip(&self.tile) {
                *out = blend(16, 235, alpha);
            }
        }
        self.chroma_alpha
            .resize(self.tile_width / 2 * (self.tile_height / 2), 0);
        for row in 0..self.tile_height / 2 {
            for column in 0..self.tile_width / 2 {
                let top = row * 2 * self.tile_width + column * 2;
                let mut sum = 0u16;
                for offset in [0, 1, self.tile_width, self.tile_width + 1] {
                    let glyph = self.tile[top + offset];
                    // White glyph over a black background, then over the image.
                    sum +=
                        u16::from(glyph) + u16::from(blend(0, self.background_alpha, 255 - glyph));
                }
                self.chroma_alpha[row * (self.tile_width / 2) + column] = ((sum + 2) / 4) as u8;
            }
        }
        let right = matches!(
            self.settings.position,
            OverlayPosition::TopRight | OverlayPosition::BottomRight
        );
        let bottom = matches!(
            self.settings.position,
            OverlayPosition::BottomLeft | OverlayPosition::BottomRight
        );
        self.x = (if right {
            self.width - margin - self.tile_width
        } else {
            margin
        }) & !1;
        self.y = (if bottom {
            self.height - margin - self.tile_height
        } else {
            margin
        }) & !1;
        self.last_second = Some(second);
        self.last_frame = frame;
        Ok(())
    }

    /// Call only after raw-image motion analysis, while the completed request is
    /// CPU-owned and bracketed by a DMA-BUF READ|WRITE synchronization guard.
    pub fn draw(
        &mut self,
        planes: &mut [&mut [u8]],
        stride: usize,
        captured_at: SystemTime,
        frame: u32,
    ) -> Result<()> {
        ensure!(
            stride >= self.width && stride.is_multiple_of(2),
            "invalid overlay YUV420 stride"
        );
        self.refresh(captured_at, frame)?;
        let y_size = stride * self.height;
        let chroma_size = stride / 2 * (self.height / 2);
        match planes.len() {
            0 => anyhow::bail!("overlay frame has no planes"),
            1 => {
                ensure!(
                    planes[0].len() >= y_size + 2 * chroma_size,
                    "overlay YUV420 buffer is too short"
                );
                let (y, chroma) = planes[0].split_at_mut(y_size);
                let (u, v) = chroma.split_at_mut(chroma_size);
                self.draw_planes(y, u, v, stride)
            }
            2 => {
                let (y, chroma) = planes.split_at_mut(1);
                ensure!(
                    chroma[0].len() >= 2 * chroma_size,
                    "overlay chroma buffer is too short"
                );
                let (u, v) = chroma[0].split_at_mut(chroma_size);
                self.draw_planes(y[0], u, v, stride)
            }
            _ => {
                let (y, chroma) = planes.split_at_mut(1);
                let (u, v) = chroma.split_at_mut(1);
                self.draw_planes(y[0], u[0], v[0], stride)
            }
        }
    }

    fn draw_planes(&self, y: &mut [u8], u: &mut [u8], v: &mut [u8], stride: usize) -> Result<()> {
        ensure!(
            y.len() >= stride * self.height
                && u.len() >= stride / 2 * (self.height / 2)
                && v.len() >= stride / 2 * (self.height / 2),
            "overlay frame planes are too short"
        );
        if self.background_alpha == 255 {
            // An opaque background allows copying the tile without per-pixel blending.
            for row in 0..self.tile_height {
                let destination = (self.y + row) * stride + self.x;
                y[destination..destination + self.tile_width].copy_from_slice(
                    &self.opaque_tile[row * self.tile_width..(row + 1) * self.tile_width],
                );
            }
            for row in self.y / 2..(self.y + self.tile_height) / 2 {
                let start = row * (stride / 2) + self.x / 2;
                u[start..start + self.tile_width / 2].fill(128);
                v[start..start + self.tile_width / 2].fill(128);
            }
            return Ok(());
        } else {
            for row in 0..self.tile_height {
                let destination = (self.y + row) * stride + self.x;
                for (column, &alpha) in self.tile
                    [row * self.tile_width..(row + 1) * self.tile_width]
                    .iter()
                    .enumerate()
                {
                    if alpha == 255 {
                        y[destination + column] = 235;
                    } else if alpha != 0 || self.background_alpha != 0 {
                        let original = y[destination + column];
                        let background = if self.background_alpha == 0 {
                            original
                        } else {
                            blend(original, 16, self.background_alpha)
                        };
                        y[destination + column] = blend(background, 235, alpha);
                    }
                }
            }
        }
        for row in 0..self.tile_height / 2 {
            let start = (self.y / 2 + row) * (stride / 2) + self.x / 2;
            for column in 0..self.tile_width / 2 {
                let alpha = self.chroma_alpha[row * (self.tile_width / 2) + column];
                if alpha != 0 {
                    u[start + column] = blend(u[start + column], 128, alpha);
                    v[start + column] = blend(v[start + column], 128, alpha);
                }
            }
        }
        Ok(())
    }
}

fn blend(original: u8, overlay: u8, alpha: u8) -> u8 {
    let alpha = u16::from(alpha);
    ((u16::from(original) * (255 - alpha) + u16::from(overlay) * alpha + 127) / 255) as u8
}

fn datetime(second: u64, format: &CString) -> Result<String> {
    let seconds: libc::time_t = second
        .try_into()
        .context("overlay datetime exceeds system range")?;
    let mut local = std::mem::MaybeUninit::<libc::tm>::uninit();
    // SAFETY: pointers are valid; localtime_r initializes local on success.
    ensure!(
        !unsafe { libc::localtime_r(&seconds, local.as_mut_ptr()) }.is_null(),
        "convert overlay capture datetime"
    );
    let mut output = [0u8; 2048];
    // SAFETY: format is NUL-terminated, local was initialized, and output is writable.
    let len = unsafe {
        libc::strftime(
            output.as_mut_ptr().cast(),
            output.len(),
            format.as_ptr(),
            local.as_ptr(),
        )
    };
    ensure!(
        len > 0,
        "overlay datetime format produced empty or oversized text"
    );
    Ok(std::str::from_utf8(&output[..len])?.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{config::Motion, motion::Detector};
    use std::time::Duration;

    fn settings() -> Overlay {
        Overlay {
            enabled: true,
            text: "Camera – Zażółć".into(),
            datetime_format: "%s".into(),
            font_size: 18,
            margin: 10,
            ..Overlay::default()
        }
    }

    #[test]
    fn embedded_font_covers_ascii_polish_and_supported_sizes() {
        let font = Font::from_bytes(
            FONT,
            FontSettings {
                scale: 12.0,
                load_substitutions: false,
                ..FontSettings::default()
            },
        )
        .unwrap();
        for ch in (' '..='~').chain("ąćęłńóśźżĄĆĘŁŃÓŚŹŻ–".chars()) {
            assert_ne!(font.lookup_glyph_index(ch), 0, "missing glyph {ch}");
            for size in [8.0, 12.0, 18.0, 24.0, 72.0] {
                let (metrics, bitmap) = font.rasterize(ch, size);
                assert_eq!(bitmap.len(), metrics.width * metrics.height);
                if ch != ' ' {
                    assert!(
                        bitmap.iter().any(|&alpha| alpha > 0),
                        "blank {ch} at {size}px"
                    );
                }
            }
        }
    }

    #[test]
    fn disabled_overlay_does_not_initialize_a_renderer() {
        assert!(
            VideoOverlay::new(
                &Overlay {
                    enabled: false,
                    ..Overlay::default()
                },
                320,
                192
            )
            .unwrap()
            .is_none()
        );
    }

    #[test]
    fn excessive_datetime_expansion_is_reported_before_capture() {
        let settings = Overlay {
            enabled: true,
            datetime_format: "%99999Y".into(),
            ..Overlay::default()
        };
        assert!(VideoOverlay::new(&settings, 320, 192).is_err());
    }

    #[test]
    fn capture_datetime_cache_unicode_and_all_corners_are_correct() {
        let mut renderer = VideoOverlay::new(&settings(), 320, 192).unwrap().unwrap();
        assert_ne!(renderer.font.lookup_glyph_index('ł'), 0);
        let first = UNIX_EPOCH + Duration::from_secs(12345);
        renderer.refresh(first, 0).unwrap();
        assert_eq!(renderer.text, "Camera – Zażółć 12345");
        assert_eq!(renderer.layout.lines().unwrap().len(), 1);
        let tile = renderer.tile.clone();
        let glyph_count = renderer.glyphs.len();
        renderer
            .refresh(first + Duration::from_millis(900), 14)
            .unwrap();
        assert_eq!(renderer.tile, tile);
        assert_eq!(renderer.glyphs.len(), glyph_count);
        renderer.refresh(first + Duration::from_secs(1), 0).unwrap();
        assert_eq!(renderer.text, "Camera – Zażółć 12346");
        assert_ne!(renderer.tile, tile);
        for position in [
            OverlayPosition::TopLeft,
            OverlayPosition::TopRight,
            OverlayPosition::BottomLeft,
            OverlayPosition::BottomRight,
        ] {
            renderer.settings.position = position;
            renderer.last_second = None;
            renderer.refresh(first, 0).unwrap();
            assert!(
                renderer.x + renderer.tile_width <= 320 && renderer.y + renderer.tile_height <= 192
            );
            assert!(renderer.x.is_multiple_of(2) && renderer.y.is_multiple_of(2));
            if matches!(
                position,
                OverlayPosition::TopRight | OverlayPosition::BottomRight
            ) {
                assert_eq!(renderer.x + renderer.tile_width, 310);
            } else {
                assert_eq!(renderer.x, 10);
            }
            if matches!(
                position,
                OverlayPosition::BottomLeft | OverlayPosition::BottomRight
            ) {
                assert_eq!(renderer.y + renderer.tile_height, 182);
            } else {
                assert_eq!(renderer.y, 10);
            }
        }
        renderer.settings.datetime_format.clear();
        renderer.format = None;
        renderer.last_second = None;
        renderer.refresh(first, 0).unwrap();
        assert_eq!(renderer.text, renderer.settings.text);
        let text_only = renderer.tile.clone();
        renderer
            .refresh(first + Duration::from_secs(600), 10)
            .unwrap();
        assert_eq!(renderer.tile, text_only);
    }

    #[test]
    fn frame_token_updates_within_second_and_preserves_capture_time() {
        let mut renderer = VideoOverlay::new(
            &Overlay {
                text: "camera".into(),
                datetime_format: "%s.{frame} ({frame})".into(),
                font_size: 12,
                ..settings()
            },
            640,
            360,
        )
        .unwrap()
        .unwrap();
        let first = UNIX_EPOCH + Duration::from_secs(12345);
        renderer.refresh(first, 7).unwrap();
        assert_eq!(renderer.text, "camera 12345.07 (07)");
        assert_eq!(renderer.layout.lines().unwrap().len(), 1);
        let tile = renderer.tile.clone();
        renderer
            .refresh(first + Duration::from_millis(500), 8)
            .unwrap();
        assert_eq!(renderer.text, "camera 12345.08 (08)");
        assert_ne!(renderer.tile, tile);
        assert_eq!(renderer.datetime_template, "12345.{frame} ({frame})");
        let tile = renderer.tile.clone();
        renderer.refresh(first, 8).unwrap();
        assert_eq!(renderer.tile, tile);
        renderer.refresh(first + Duration::from_secs(1), 0).unwrap();
        assert_eq!(renderer.text, "camera 12346.00 (00)");
        renderer.refresh(first, 123).unwrap(); // Counter width is not truncated.
        assert_eq!(renderer.text, "camera 12345.123 (123)");
    }

    #[test]
    fn yuv_layouts_match_and_preserve_padding_and_unannotated_pixels() {
        let (width, height, stride) = (320, 192, 336);
        let y_size = stride * height;
        let c_size = stride / 2 * (height / 2);
        let mut original = vec![0xa5; y_size + 2 * c_size];
        for row in 0..height {
            original[row * stride..row * stride + width].fill(80);
            if row < height / 2 {
                original[y_size + row * stride / 2..y_size + row * stride / 2 + width / 2].fill(90);
                original[y_size + c_size + row * stride / 2
                    ..y_size + c_size + row * stride / 2 + width / 2]
                    .fill(170);
            }
        }
        let mut renderer = VideoOverlay::new(
            &Overlay {
                background_opacity: 1.0,
                ..settings()
            },
            width as u32,
            height as u32,
        )
        .unwrap()
        .unwrap();
        let mut expected = original.clone();
        renderer
            .draw(&mut [&mut expected], stride, UNIX_EPOCH, 0)
            .unwrap();
        assert!(renderer.tile.iter().any(|&v| v > 220));
        for row in 0..height {
            for column in 0..stride {
                let inside = row >= renderer.y
                    && row < renderer.y + renderer.tile_height
                    && column >= renderer.x
                    && column < renderer.x + renderer.tile_width;
                if !inside {
                    assert_eq!(
                        expected[row * stride + column],
                        original[row * stride + column]
                    );
                }
            }
        }
        for plane_start in [y_size, y_size + c_size] {
            for row in 0..height / 2 {
                for column in 0..stride / 2 {
                    let index = plane_start + row * stride / 2 + column;
                    let inside = row >= renderer.y / 2
                        && row < (renderer.y + renderer.tile_height) / 2
                        && column >= renderer.x / 2
                        && column < (renderer.x + renderer.tile_width) / 2;
                    assert_eq!(expected[index], if inside { 128 } else { original[index] });
                }
            }
        }
        for planes in [2, 3] {
            let mut actual = original.clone();
            let (y, chroma) = actual.split_at_mut(y_size);
            if planes == 2 {
                renderer
                    .draw(&mut [y, chroma], stride, UNIX_EPOCH, 0)
                    .unwrap();
            } else {
                let (u, v) = chroma.split_at_mut(c_size);
                renderer
                    .draw(&mut [y, u, v], stride, UNIX_EPOCH, 0)
                    .unwrap();
            }
            assert_eq!(actual, expected);
        }
        assert!(
            renderer
                .draw(&mut [&mut [0; 4]], stride, UNIX_EPOCH, 0)
                .is_err()
        );
        assert!(renderer.draw(&mut [], stride, UNIX_EPOCH, 0).is_err());
    }

    #[test]
    fn transparent_and_partial_backgrounds_blend_without_a_desaturated_rectangle() {
        let (width, height, stride) = (320usize, 192usize, 336usize);
        let y_size = stride * height;
        let c_size = stride / 2 * (height / 2);
        let mut original = vec![80; y_size + 2 * c_size];
        original[y_size..y_size + c_size].fill(90);
        original[y_size + c_size..].fill(170);
        let reference = |from: u8, to: u8, alpha: f64| {
            (f64::from(from) * (1.0 - alpha) + f64::from(to) * alpha).round() as u8
        };
        for opacity in [0.0, 0.5, 1.0] {
            let mut renderer = VideoOverlay::new(
                &Overlay {
                    background_opacity: opacity,
                    ..settings()
                },
                width as u32,
                height as u32,
            )
            .unwrap()
            .unwrap();
            let mut actual = original.clone();
            renderer
                .draw(&mut [&mut actual], stride, UNIX_EPOCH, 0)
                .unwrap();
            let background = (opacity * 255.0).round() / 255.0;
            let mut untouched = 0;
            for row in 0..renderer.tile_height {
                for column in 0..renderer.tile_width {
                    let alpha = renderer.tile[row * renderer.tile_width + column];
                    let index = (renderer.y + row) * stride + renderer.x + column;
                    let expected = reference(
                        reference(original[index], 16, background),
                        235,
                        f64::from(alpha) / 255.0,
                    );
                    assert_eq!(actual[index], expected);
                    if opacity == 0.0 && alpha == 0 {
                        assert_eq!(actual[index], original[index]);
                        untouched += 1;
                    }
                }
            }
            if opacity == 0.0 {
                assert!(untouched > renderer.tile.len() / 2);
            }
            for row in 0..renderer.tile_height / 2 {
                for column in 0..renderer.tile_width / 2 {
                    let top = 2 * row * renderer.tile_width + 2 * column;
                    let alpha = [0, 1, renderer.tile_width, renderer.tile_width + 1]
                        .iter()
                        .map(|&offset| {
                            let glyph = f64::from(renderer.tile[top + offset]) / 255.0;
                            glyph + (1.0 - glyph) * background
                        })
                        .sum::<f64>()
                        / 4.0;
                    for plane_start in [y_size, y_size + c_size] {
                        let index = plane_start
                            + (renderer.y / 2 + row) * (stride / 2)
                            + renderer.x / 2
                            + column;
                        let expected = reference(original[index], 128, alpha);
                        assert!(actual[index].abs_diff(expected) <= 1);
                        if alpha == 0.0 {
                            assert_eq!(actual[index], original[index]);
                        }
                    }
                }
            }
            for planes in [2, 3] {
                let mut other = original.clone();
                let (y, chroma) = other.split_at_mut(y_size);
                if planes == 2 {
                    renderer
                        .draw(&mut [y, chroma], stride, UNIX_EPOCH, 0)
                        .unwrap();
                } else {
                    let (u, v) = chroma.split_at_mut(c_size);
                    renderer
                        .draw(&mut [y, u, v], stride, UNIX_EPOCH, 0)
                        .unwrap();
                }
                assert_eq!(other, actual);
            }
        }
    }

    #[test]
    fn overlay_clock_does_not_contaminate_raw_motion_history() {
        let mut renderer = VideoOverlay::new(&settings(), 320, 192).unwrap().unwrap();
        let mut detector = Detector::new(
            &Motion {
                frame_period: 1,
                region_threshold: 0.001,
                ..Motion::default()
            },
            320,
            192,
        );
        for second in 0..4 {
            // Like the camera pipeline, analyze the freshly captured raw image,
            // then annotate the completed buffer before encoding it.
            let mut frame = vec![40; 320 * 192 * 3 / 2];
            assert_eq!(detector.analyze(&frame, 320).unwrap(), Some(false));
            renderer
                .draw(
                    &mut [&mut frame],
                    320,
                    UNIX_EPOCH + Duration::from_secs(second),
                    0,
                )
                .unwrap();
            assert!(frame[..320 * 192].iter().any(|&v| v != 40));
        }
    }
}
