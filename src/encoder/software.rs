//! Synchronous, statically linked OpenH264 backend with borrowed YUV input.
use super::{Encoded, quality_quantizer, yuv::Yuv420};
use crate::config::Camera;
use anyhow::{Context, Result, ensure};

#[path = "openh264.rs"]
mod native;

pub struct SoftwareEncoder {
    encoder: native::Encoder,
    output: Vec<u8>,
    width: usize,
    height: usize,
    last_timestamp: Option<u64>,
    finished: bool,
}

impl SoftwareEncoder {
    pub fn new(camera: &Camera) -> Result<Self> {
        camera.validate_dimensions()?;
        ensure!(
            (1..=30).contains(&camera.framerate)
                && camera.quality.is_finite()
                && (0.0..=1.0).contains(&camera.quality),
            "invalid software encoder configuration"
        );
        ensure!(
            camera.width.max(camera.height) <= 3840 && camera.width.min(camera.height) <= 2160,
            "OpenH264 supports at most 3840x2160 or 2160x3840"
        );
        let mut encoder = native::Encoder::new()?;
        encoder.initialize(camera)?;
        crate::log(format!(
            "software H.264 encoder: statically linked OpenH264, QP {}",
            quality_quantizer(camera.quality)
        ));
        Ok(Self {
            encoder,
            output: Vec::new(),
            width: camera.width as usize,
            height: camera.height as usize,
            last_timestamp: None,
            finished: false,
        })
    }

    pub fn encode(
        &mut self,
        frame: &Yuv420<'_>,
        timestamp_us: u64,
        force_idr: bool,
        mut callback: impl FnMut(Encoded<'_>) -> Result<()>,
    ) -> Result<()> {
        ensure!(!self.finished, "software encoder is already finished");
        ensure!(
            (frame.width, frame.height) == (self.width, self.height),
            "software encoder frame dimensions changed"
        );
        ensure!(
            self.last_timestamp.is_none_or(|last| timestamp_us > last),
            "non-monotonic software encoder timestamp"
        );
        i64::try_from(timestamp_us).context("software encoder timestamp overflow")?;
        // Validate again at the FFI boundary, including values assembled directly.
        let validated = Yuv420::new(&frame.planes, frame.width, frame.height, frame.strides[0])?;
        ensure!(
            validated.strides == frame.strides,
            "invalid software chroma strides"
        );
        for stride in validated.strides {
            i32::try_from(stride).context("software encoder stride overflow")?;
        }
        self.encoder
            .encode(&validated, timestamp_us, force_idr, &mut self.output)?;
        self.last_timestamp = Some(timestamp_us);
        callback(Encoded {
            // The native API takes milliseconds, but output is synchronous:
            // retain the exact capture timestamp for the recording/live muxers.
            timestamp_us,
            data: &self.output,
        })
    }

    pub fn finish(&mut self) {
        // OpenH264 emits each frame synchronously and has no delayed frames.
        self.finished = true;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::recording::ClipWriter;

    #[test]
    fn encodes_and_records_above_1080p_with_an_appropriate_h264_level() {
        let camera = Camera {
            width: 2304,
            height: 1296,
            ..Camera::default()
        };
        let mut encoder = SoftwareEncoder::new(&camera).unwrap();
        let pixels = vec![128; camera.width as usize * camera.height as usize * 3 / 2];
        let frame = Yuv420::new(
            &[&pixels],
            camera.width as usize,
            camera.height as usize,
            camera.width as usize,
        )
        .unwrap();
        let directory = std::env::temp_dir().join(format!(
            "pivigil-large-frame-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir(&directory).unwrap();
        let mut writer = ClipWriter::new(&directory, 300, &camera);
        let mut encoded = false;
        encoder
            .encode(&frame, 1_000_000, true, |packet| {
                let info = writer.inspect(packet.data)?;
                assert!(info.idr && info.vcl);
                let (sps, _) = writer.h264_headers().unwrap();
                // 2304x1296 exceeds the frame-size limit of H.264 level 4.2.
                assert!(sps[3] > 42, "encoder signalled level {}", sps[3]);
                writer.write(
                    1,
                    packet.timestamp_us,
                    std::time::UNIX_EPOCH,
                    packet.data,
                    &info,
                )?;
                encoded = true;
                Ok(())
            })
            .unwrap();
        assert!(encoded);
        encoder.finish();
        writer.finish().unwrap();
        let path = std::fs::read_dir(&directory)
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        let file = std::fs::File::open(path).unwrap();
        let mut reader =
            mp4::Mp4Reader::read_header(&file, file.metadata().unwrap().len()).unwrap();
        assert_eq!(reader.tracks()[&1].width(), 2304);
        assert_eq!(reader.tracks()[&1].height(), 1296);
        assert!(reader.read_sample(1, 1).unwrap().unwrap().is_sync);
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn real_openh264_preserves_timestamps_keyframes_headers_and_input_lifetimes() {
        let camera = Camera {
            width: 96,
            height: 64,
            framerate: 15,
            ..Camera::default()
        };
        let mut writer = ClipWriter::new(std::path::Path::new("/tmp"), 300, &camera);
        for quality in [0.0, 0.6, 1.0] {
            let mut encoder = SoftwareEncoder::new(&Camera {
                quality,
                ..camera.clone()
            })
            .unwrap();
            let (w, h, s) = (96, 64, 112);
            let mut y = vec![40; s * h];
            let u = vec![90; s * h / 4];
            let v = vec![170; s * h / 4];
            let mut last_idr = 0;
            for index in 0..47 {
                y[(index * 97) % (s * h)] = 180;
                let combined = [y.as_slice(), u.as_slice(), v.as_slice()].concat();
                let frame = match index % 3 {
                    0 => Yuv420::new(&[&combined], w, h, s),
                    1 => Yuv420::new(&[&y, &combined[y.len()..]], w, h, s),
                    _ => Yuv420::new(&[&y, &u, &v], w, h, s),
                }
                .unwrap();
                let snapshot = combined.clone();
                let pts = 1_000_000 + index as u64 * 66_667 + if index >= 23 { 200_000 } else { 0 };
                let mut count = 0;
                encoder
                    .encode(&frame, pts, index == 7, |packet| {
                        assert_eq!(packet.timestamp_us, pts);
                        let info = writer.inspect(packet.data)?;
                        assert!(info.vcl);
                        if index == 0 || index == 7 {
                            assert!(info.idr);
                        }
                        if info.idr {
                            assert!(packet.data.windows(5).any(|p| p == [0, 0, 0, 1, 0x67]));
                            assert!(writer.h264_headers().is_some());
                            last_idr = index;
                        }
                        assert!(index - last_idr < camera.framerate as usize);
                        count += 1;
                        Ok(())
                    })
                    .unwrap();
                assert_eq!(count, 1);
                assert_eq!(combined, snapshot); // OpenH264 reads rather than mutates caller pixels.
            }
            encoder.finish();
            encoder.finish();
            let frame = Yuv420::new(&[&y, &u, &v], w, h, s).unwrap();
            assert!(
                encoder
                    .encode(&frame, 10_000_000, false, |_| Ok(()))
                    .is_err()
            );
        }
    }

    #[test]
    fn bundled_encoder_handles_the_full_quality_range_and_cropped_frames() {
        for (width, height, stride) in [(98, 66, 112), (640, 480, 656), (2304, 1296, 2320)] {
            let pixels: Vec<u8> = (0..stride * height * 3 / 2)
                .map(|i| 16 + (((i * 73) ^ ((i >> 2) * 29)) % 220) as u8)
                .collect();
            let original = pixels.clone();
            let frame = Yuv420::new(&[&pixels], width, height, stride).unwrap();
            let mut sizes = Vec::new();
            for quality in [0.0, 0.6, 0.9, 1.0] {
                let camera = Camera {
                    width: width as u32,
                    height: height as u32,
                    quality,
                    ..Default::default()
                };
                let mut encoder = SoftwareEncoder::new(&camera).unwrap();
                let mut writer = ClipWriter::new(std::path::Path::new("/tmp"), 300, &camera);
                for index in 0..4 {
                    let timestamp = 1_000_001 + index * 100;
                    let mut count = 0;
                    encoder
                        .encode(&frame, timestamp, index == 2, |packet| {
                            assert_eq!(packet.timestamp_us, timestamp);
                            let info = writer.inspect(packet.data)?;
                            assert!(info.vcl);
                            if index == 0 || index == 2 {
                                assert!(info.idr);
                            }
                            if index == 0 {
                                sizes.push(packet.data.len());
                            }
                            count += 1;
                            Ok(())
                        })
                        .unwrap_or_else(|error| {
                            panic!("quality {quality}, frame {index}: {error:#}");
                        });
                    assert_eq!(count, 1); // No frames skipped, even within one millisecond.
                    assert_eq!(pixels, original);
                }
            }
            assert_eq!(sizes.len(), 4);
            assert!(
                sizes.windows(2).all(|pair| pair[0] < pair[1]),
                "quality must affect the whole range: {sizes:?}"
            );
        }
    }

    #[test]
    fn invalid_input_is_rejected_before_crossing_ffi() {
        let camera = Camera {
            width: 64,
            height: 64,
            ..Default::default()
        };
        let mut encoder = SoftwareEncoder::new(&camera).unwrap();
        let pixels = vec![80; 64 * 64 * 3 / 2];
        let mut frame = Yuv420::new(&[&pixels], 64, 64, 64).unwrap();
        frame.planes[2] = &[];
        assert!(encoder.encode(&frame, 1, false, |_| Ok(())).is_err());
        let frame = Yuv420::new(&[&pixels], 64, 64, 64).unwrap();
        assert!(encoder.encode(&frame, u64::MAX, false, |_| Ok(())).is_err());
        encoder.encode(&frame, 100, false, |_| Ok(())).unwrap();
        assert!(encoder.encode(&frame, 100, false, |_| Ok(())).is_err());
        assert!(
            SoftwareEncoder::new(&Camera {
                quality: f64::NAN,
                ..camera.clone()
            })
            .is_err()
        );
        assert!(
            SoftwareEncoder::new(&Camera {
                width: 65,
                ..camera
            })
            .is_err()
        );
    }
}
