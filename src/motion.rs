use crate::config::Motion;
use anyhow::{Result, ensure};

#[derive(Clone, Copy, Debug)]
pub struct MotionSample {
    pub changed_pixels: usize,
    pub total_pixels: usize,
    pub required_pixels: usize,
    pub pixel_cutoff: u8,
    pub exact: bool,
}

/// Full-resolution luminance motion detection on the same YUV420 frames used for recording.
pub struct Detector {
    config: Motion,
    minimum_change: u8,
    required_changes: usize,
    width: usize,
    height: usize,
    previous: Vec<u8>,
    initialized: bool,
    sequence: u64,
    sample: Option<MotionSample>,
}

impl Detector {
    pub fn new(config: &Motion, width: usize, height: usize) -> Self {
        Self {
            config: config.clone(),
            // Compute the byte cutoff using the public normalized comparison.
            // Enumerating once avoids rounding ambiguities at d/255 boundaries.
            minimum_change: minimum_change(config.pixel_difference_threshold),
            required_changes: (config.region_threshold * (width * height) as f64).ceil() as usize,
            width,
            height,
            previous: if config.enabled {
                vec![0; width * height]
            } else {
                Vec::new()
            },
            initialized: false,
            sequence: 0,
            sample: None,
        }
    }

    pub fn needs_pixels(&self) -> bool {
        self.config.enabled
            && self
                .sequence
                .is_multiple_of(u64::from(self.config.frame_period))
    }

    fn validate_layout(&self, len: usize, stride: usize) -> Result<()> {
        ensure!(
            stride >= self.width && len >= stride * self.height,
            "invalid camera luminance plane layout"
        );
        Ok(())
    }

    pub fn advance_unsampled(&mut self, plane_len: usize, stride: usize) -> Result<()> {
        self.validate_layout(plane_len, stride)?;
        ensure!(
            !self.needs_pixels(),
            "motion sample requires luminance pixels"
        );
        self.sequence = self.sequence.wrapping_add(1);
        Ok(())
    }

    pub fn analyze(&mut self, y_plane: &[u8], stride: usize) -> Result<Option<bool>> {
        self.analyze_inner(y_plane, stride, false)
    }

    /// Count the entire frame when a recording may start, so its trigger log is exact.
    pub fn analyze_counted(&mut self, y_plane: &[u8], stride: usize) -> Result<Option<bool>> {
        self.analyze_inner(y_plane, stride, true)
    }

    pub fn last_sample(&self) -> Option<MotionSample> {
        self.sample
    }

    fn analyze_inner(
        &mut self,
        y_plane: &[u8],
        stride: usize,
        count_all: bool,
    ) -> Result<Option<bool>> {
        self.validate_layout(y_plane.len(), stride)?;
        let sample = self.needs_pixels();
        self.sequence = self.sequence.wrapping_add(1);
        if !sample {
            return Ok(None);
        }
        if !self.initialized {
            copy_history(
                y_plane,
                &mut self.previous,
                self.width,
                self.height,
                stride,
                0,
            );
            self.initialized = true;
            self.sample = Some(MotionSample {
                changed_pixels: 0,
                total_pixels: self.width * self.height,
                required_pixels: self.required_changes,
                pixel_cutoff: self.minimum_change,
                exact: true,
            });
            return Ok(Some(false));
        }
        let changed = count_changed(
            y_plane,
            &mut self.previous,
            self.width,
            self.height,
            stride,
            self.minimum_change,
            if count_all {
                usize::MAX
            } else {
                self.required_changes
            },
        );
        self.sample = Some(MotionSample {
            changed_pixels: changed,
            total_pixels: self.width * self.height,
            required_pixels: self.required_changes,
            pixel_cutoff: self.minimum_change,
            exact: count_all
                || changed < self.required_changes
                || changed == self.width * self.height,
        });
        Ok(Some(changed >= self.required_changes))
    }
}

pub(crate) fn copy_history(
    image: &[u8],
    previous: &mut [u8],
    width: usize,
    height: usize,
    stride: usize,
    first_row: usize,
) {
    #[cfg(target_arch = "aarch64")]
    if std::arch::is_aarch64_feature_detected!("neon") {
        // SAFETY: callers validated the image layout and own the history slice.
        unsafe {
            copy_history_neon(image, previous, width, height, stride, first_row);
        }
        return;
    }
    if stride == width {
        previous[first_row * width..].copy_from_slice(&image[first_row * width..height * width]);
    } else {
        for row in first_row..height {
            previous[row * width..(row + 1) * width]
                .copy_from_slice(&image[row * stride..row * stride + width]);
        }
    }
}

#[cfg(target_arch = "aarch64")]
#[target_feature(enable = "neon")]
unsafe fn copy_history_neon(
    image: &[u8],
    previous: &mut [u8],
    width: usize,
    height: usize,
    stride: usize,
    first_row: usize,
) {
    use std::arch::aarch64::*;
    for row in first_row..height {
        let pixels = &image[row * stride..row * stride + width];
        let old = &mut previous[row * width..(row + 1) * width];
        let mut column = 0;
        while column + 64 <= width {
            // SAFETY: both rows cover this complete 64-byte group and cannot overlap.
            unsafe {
                let values = vld1q_u8_x4(pixels.as_ptr().add(column));
                vst1q_u8_x4(old.as_mut_ptr().add(column), values);
            }
            column += 64;
        }
        old[column..].copy_from_slice(&pixels[column..]);
    }
}

fn minimum_change(threshold: f64) -> u8 {
    (1..=255)
        .find(|&delta| f64::from(delta) / 255.0 >= threshold)
        .expect("pixel difference threshold must be validated before constructing a detector")
}

pub(crate) fn count_changed(
    image: &[u8],
    previous: &mut [u8],
    width: usize,
    height: usize,
    stride: usize,
    minimum_change: u8,
    stop_after: usize,
) -> usize {
    #[cfg(target_arch = "aarch64")]
    if std::arch::is_aarch64_feature_detected!("neon") {
        // SAFETY: NEON was detected, and analyze() checked every image row's bounds.
        // previous has exactly width*height bytes, allocated by Detector::new().
        return unsafe {
            count_changed_neon(
                image,
                previous,
                width,
                height,
                stride,
                minimum_change,
                stop_after,
            )
        };
    }
    count_changed_scalar(
        image,
        previous,
        width,
        height,
        stride,
        minimum_change,
        stop_after,
    )
}

fn count_changed_scalar(
    image: &[u8],
    previous: &mut [u8],
    width: usize,
    height: usize,
    stride: usize,
    minimum_change: u8,
    stop_after: usize,
) -> usize {
    let mut changed = 0;
    for row in 0..height {
        let pixels = &image[row * stride..row * stride + width];
        let old_pixels = &mut previous[row * width..(row + 1) * width];
        for (&value, old) in pixels.iter().zip(old_pixels) {
            changed += usize::from(value.abs_diff(*old) >= minimum_change);
            *old = value;
        }
        if changed >= stop_after {
            copy_history(image, previous, width, height, stride, row + 1);
            return changed;
        }
    }
    changed
}

#[cfg(target_arch = "aarch64")]
#[target_feature(enable = "neon")]
unsafe fn count_changed_neon(
    image: &[u8],
    previous: &mut [u8],
    width: usize,
    height: usize,
    stride: usize,
    minimum_change: u8,
    stop_after: usize,
) -> usize {
    use std::arch::aarch64::*;
    let limit = vdupq_n_u8(minimum_change);
    let mut changed = 0usize;
    for row in 0..height {
        let pixels = &image[row * stride..row * stride + width];
        let old_pixels = &mut previous[row * width..(row + 1) * width];
        let mut column = 0;
        // A true comparison produces 0xff, so subtracting its mask counts one
        // change per byte lane. Bound each block to 252 vectors to prevent byte
        // overflow when all pixels change, including after the four sums merge.
        while column + 16 <= width {
            let block_end = (column + 16 * 252).min(width / 16 * 16);
            let mut sum0 = vdupq_n_u8(0);
            let mut sum1 = vdupq_n_u8(0);
            let mut sum2 = vdupq_n_u8(0);
            let mut sum3 = vdupq_n_u8(0);
            macro_rules! compare {
                ($column:expr, $sum:ident) => {
                    // SAFETY: each call is within the checked block of complete
                    // vectors; stores only update the exclusively borrowed buffer.
                    unsafe {
                        let value = vld1q_u8(pixels.as_ptr().add($column));
                        let old = vld1q_u8(old_pixels.as_ptr().add($column));
                        $sum = vsubq_u8($sum, vcgeq_u8(vabdq_u8(value, old), limit));
                        vst1q_u8(old_pixels.as_mut_ptr().add($column), value);
                    }
                };
            }
            // Independent groups overlap work and reduce branch overhead on the Cortex-A53.
            while column + 64 <= block_end {
                compare!(column, sum0);
                compare!(column + 16, sum1);
                compare!(column + 32, sum2);
                compare!(column + 48, sum3);
                column += 64;
            }
            while column < block_end {
                compare!(column, sum0);
                column += 16;
            }
            let sums = vaddq_u8(vaddq_u8(sum0, sum1), vaddq_u8(sum2, sum3));
            changed += vaddlvq_u8(sums) as usize;
        }
        for (&value, old) in pixels[column..].iter().zip(&mut old_pixels[column..]) {
            changed += usize::from(value.abs_diff(*old) >= minimum_change);
            *old = value;
        }
        if changed >= stop_after {
            copy_history(image, previous, width, height, stride, row + 1);
            return changed;
        }
    }
    changed
}

#[cfg(test)]
mod tests {

    #[test]
    fn disabled_detector_never_samples_or_allocates_image_history() {
        let mut detector = Detector::new(
            &Motion {
                enabled: false,
                ..Motion::default()
            },
            64,
            64,
        );
        assert!(detector.previous.is_empty());
        for index in 0..100 {
            assert!(!detector.needs_pixels());
            if index % 2 == 0 {
                assert_eq!(
                    detector.analyze(&vec![index as u8; 64 * 64], 64).unwrap(),
                    None
                );
            } else {
                detector.advance_unsampled(64 * 64, 64).unwrap();
            }
        }
        assert!(detector.previous.is_empty());
        assert!(!detector.initialized);
        assert!(detector.advance_unsampled(63 * 64, 64).is_err());
    }
    use super::*;

    #[test]
    fn counted_samples_report_full_trigger_values_and_preserve_history_and_cadence() {
        let config = Motion {
            frame_period: 3,
            region_threshold: 0.01,
            pixel_difference_threshold: 0.15,
            ..Motion::default()
        };
        let mut detector = Detector::new(&config, 64, 64);
        let dark = vec![40; 64 * 64];
        let light = vec![180; 64 * 64];
        assert!(detector.last_sample().is_none());
        assert_eq!(detector.analyze_counted(&dark, 64).unwrap(), Some(false));
        assert_eq!(detector.analyze_counted(&light, 64).unwrap(), None);
        assert_eq!(detector.analyze_counted(&light, 64).unwrap(), None);
        assert_eq!(detector.analyze_counted(&light, 64).unwrap(), Some(true));
        let sample = detector.last_sample().unwrap();
        assert_eq!(sample.changed_pixels, 4096);
        assert_eq!(sample.total_pixels, 4096);
        assert_eq!(sample.required_pixels, 41);
        assert_eq!(sample.pixel_cutoff, 39);
        assert!(sample.exact);
        detector.analyze(&dark, 64).unwrap();
        detector.analyze(&dark, 64).unwrap();
        assert_eq!(detector.analyze(&dark, 64).unwrap(), Some(true));
        let sample = detector.last_sample().unwrap();
        assert!(sample.changed_pixels >= 41 && sample.changed_pixels < 4096);
        assert!(!sample.exact);
        detector.analyze_counted(&dark, 64).unwrap();
        detector.analyze_counted(&dark, 64).unwrap();
        assert_eq!(detector.analyze_counted(&dark, 64).unwrap(), Some(false));
        assert_eq!(detector.last_sample().unwrap().changed_pixels, 0);
        assert!(detector.last_sample().unwrap().exact);
    }

    #[test]
    fn early_decisions_preserve_complete_history_across_frame_sequences() {
        for (width, height, stride) in [(17, 9, 32), (64, 8, 64), (1536, 864, 1536)] {
            for region in [0.001, 0.02, 0.5, 1.0] {
                for location in [0, height / 2, height - 1] {
                    let config = Motion {
                        region_threshold: region,
                        frame_period: 1,
                        ..Motion::default()
                    };
                    let mut detector = Detector::new(&config, width, height);
                    let mut expected = vec![0; width * height];
                    let mut frame = vec![40; stride * height];
                    for step in 0..6 {
                        match step {
                            1 => frame[location * stride..location * stride + width].fill(90),
                            3 => frame.fill(210),
                            5 => frame.fill(40),
                            _ => {}
                        }
                        let count = reference_count(
                            &frame,
                            &mut expected,
                            width,
                            height,
                            stride,
                            config.pixel_difference_threshold,
                        );
                        let detected =
                            step != 0 && count as f64 >= region * (width * height) as f64;
                        assert_eq!(detector.analyze(&frame, stride).unwrap(), Some(detected));
                        assert_eq!(
                            detector.previous, expected,
                            "history differs at step {step}"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn metadata_only_advancement_preserves_cadence_and_validation() {
        let config = Motion {
            frame_period: 3,
            ..Motion::default()
        };
        let mut actual = Detector::new(&config, 17, 9);
        let mut expected = Detector::new(&config, 17, 9);
        for sequence in 0..25 {
            let frame = vec![(sequence * 17) as u8; 32 * 9];
            if actual.needs_pixels() {
                assert!(actual.advance_unsampled(frame.len(), 32).is_err());
                assert_eq!(
                    actual.analyze(&frame, 32).unwrap(),
                    expected.analyze(&frame, 32).unwrap()
                );
            } else {
                assert!(actual.advance_unsampled(1, 32).is_err());
                actual.advance_unsampled(frame.len(), 32).unwrap();
                assert_eq!(expected.analyze(&frame, 32).unwrap(), None);
            }
            assert_eq!(actual.sequence, expected.sequence);
            assert_eq!(actual.previous, expected.previous);
        }
    }

    #[test]
    fn ignores_first_frame_noise_padding_and_unsampled_frames() {
        let mut d = Detector::new(
            &Motion {
                frame_period: 2,
                ..Motion::default()
            },
            4,
            2,
        );
        let flat = [40, 40, 40, 40, 200, 200, 40, 40, 40, 40, 200, 200];
        assert_eq!(d.analyze(&flat, 6).unwrap(), Some(false));
        assert_eq!(d.analyze(&[255; 12], 6).unwrap(), None);
        let mut noise = flat;
        noise[0] += 2;
        noise[4] = 0;
        assert_eq!(d.analyze(&noise, 6).unwrap(), Some(false));
        assert!(d.analyze(&[0; 2], 6).is_err());
    }

    #[test]
    fn checks_all_image_corners_and_uses_the_full_frame_pixel_fraction() {
        let config = Motion {
            enabled: true,
            frame_period: 1,
            pixel_difference_threshold: 0.05,
            region_threshold: 0.5,
        };
        let mut d = Detector::new(&config, 4, 2);
        let mut frame = [40; 8];
        assert_eq!(d.analyze(&frame, 4).unwrap(), Some(false));
        frame[0] = 80;
        frame[3] = 80;
        frame[4] = 80; // Three of eight image pixels: below the 50% threshold.
        assert_eq!(d.analyze(&frame, 4).unwrap(), Some(false));
        frame[0] = 120;
        frame[3] = 120;
        frame[4] = 120;
        frame[7] = 80; // All four corners differ: meets the full-frame threshold.
        assert_eq!(d.analyze(&frame, 4).unwrap(), Some(true));
        assert_eq!(d.analyze(&frame, 4).unwrap(), Some(false));
    }

    fn detects_change(pixel_difference_threshold: f64, before: u8, after: u8) -> bool {
        let config = Motion {
            enabled: true,
            pixel_difference_threshold,
            region_threshold: 1.0,
            frame_period: 1,
        };
        let mut detector = Detector::new(&config, 1, 1);
        assert_eq!(detector.analyze(&[before], 1).unwrap(), Some(false));
        detector.analyze(&[after], 1).unwrap().unwrap()
    }

    #[test]
    fn default_threshold_is_independent_of_brightness_and_direction() {
        for before in [0, 40, 100, 200, 242] {
            assert!(!detects_change(0.05, before, before + 12));
            assert!(detects_change(0.05, before, before + 13));
            assert!(!detects_change(0.05, before + 12, before));
            assert!(detects_change(0.05, before + 13, before));
        }
    }

    #[test]
    fn lower_threshold_detects_smaller_changes_and_endpoints_exclude_identical_pixels() {
        assert!(!detects_change(0.1, 40, 65));
        assert!(detects_change(0.1, 40, 66));
        assert!(!detects_change(0.05, 40, 52));
        assert!(detects_change(0.05, 40, 53));
        assert!(detects_change(0.0, 40, 41));
        assert!(!detects_change(0.0, 40, 40));
        assert!(!detects_change(1.0, 0, 254));
        assert!(detects_change(1.0, 0, 255));
        assert!(detects_change(1.0, 255, 0));
        assert!(!detects_change(1.0, 255, 255));
    }

    #[test]
    fn integer_cutoff_matches_normalized_comparison_at_every_quantization_boundary() {
        for delta in 1..=255 {
            let fraction = f64::from(delta) / 255.0;
            for threshold in [fraction.next_down(), fraction, fraction.next_up()] {
                if !(0.0..=1.0).contains(&threshold) {
                    continue;
                }
                let minimum = minimum_change(threshold);
                for change in 0..=255 {
                    assert_eq!(
                        change >= minimum,
                        change != 0 && f64::from(change) / 255.0 >= threshold,
                        "change={change}, threshold={threshold:?}"
                    );
                }
            }
        }
    }

    fn reference_count(
        image: &[u8],
        previous: &mut [u8],
        width: usize,
        height: usize,
        stride: usize,
        threshold: f64,
    ) -> usize {
        let mut count = 0;
        for row in 0..height {
            for column in 0..width {
                let value = image[row * stride + column];
                let old = &mut previous[row * width + column];
                let delta = value.abs_diff(*old);
                count += usize::from(delta != 0 && f64::from(delta) / 255.0 >= threshold);
                *old = value;
            }
        }
        count
    }

    #[test]
    fn optimized_and_scalar_comparisons_match_normalized_reference_for_every_pixel_transition() {
        let previous: Vec<u8> = (0..=255)
            .flat_map(|old| std::iter::repeat_n(old, 256))
            .collect();
        let image: Vec<u8> = (0..256).flat_map(|_| 0..=255).collect();
        for threshold in [0.0, 0.01, 0.05, 0.1, 0.125, 1.0 / 3.0, 0.5, 0.9, 1.0] {
            let config = Motion {
                pixel_difference_threshold: threshold,
                ..Motion::default()
            };
            let detector = Detector::new(&config, 256, 256);
            let mut actual = previous.clone();
            let mut expected = previous.clone();
            let expected_count = reference_count(&image, &mut expected, 256, 256, 256, threshold);
            assert_eq!(
                count_changed(
                    &image,
                    &mut actual,
                    256,
                    256,
                    256,
                    detector.minimum_change,
                    usize::MAX
                ),
                expected_count
            );
            assert_eq!(actual, expected);
            let mut scalar = previous.clone();
            assert_eq!(
                count_changed_scalar(
                    &image,
                    &mut scalar,
                    256,
                    256,
                    256,
                    detector.minimum_change,
                    usize::MAX
                ),
                expected_count
            );
            assert_eq!(scalar, expected);
        }
    }

    #[test]
    fn optimized_comparison_preserves_padding_tails_and_large_change_counts() {
        for (width, height, stride) in [
            (1, 1, 3),
            (7, 4, 11),
            (16, 3, 19),
            (17, 9, 32),
            (33, 7, 45),
            (63, 3, 80),
            (64, 3, 80),
            (65, 3, 80),
            (4031, 2, 4048),
            (4032, 2, 4048),
            (4033, 2, 4048),
            (1536, 864, 1536),
            (262_161, 1, 262_174),
        ] {
            for threshold in [0.0, 0.05, 1.0] {
                let config = Motion {
                    pixel_difference_threshold: threshold,
                    ..Motion::default()
                };
                let detector = Detector::new(&config, width, height);
                let image: Vec<u8> = (0..stride * height)
                    .map(|i| ((i * 73 + i / 19) % 256) as u8)
                    .collect();
                let previous: Vec<u8> = (0..width * height)
                    .map(|i| ((i * 37 + i / 13) % 256) as u8)
                    .collect();
                let mut actual = previous.clone();
                let mut expected = previous;
                let count = reference_count(
                    &image,
                    &mut expected,
                    width,
                    height,
                    stride,
                    config.pixel_difference_threshold,
                );
                assert_eq!(
                    count_changed(
                        &image,
                        &mut actual,
                        width,
                        height,
                        stride,
                        detector.minimum_change,
                        usize::MAX
                    ),
                    count
                );
                assert_eq!(actual, expected);
                actual.fill(0);
                assert_eq!(
                    count_changed(
                        &vec![255; stride * height],
                        &mut actual,
                        width,
                        height,
                        stride,
                        detector.minimum_change,
                        usize::MAX
                    ),
                    width * height
                );
            }
        }
    }
}
