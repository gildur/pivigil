use crate::{
    config::Config,
    encoder::Encoded,
    live::Hub,
    motion::Detector,
    pre_roll::PreRoll,
    recording::{ClipWriter, PacketData, worker::RecordingWorker},
    state::{Action, RecorderState},
};
use anyhow::{Context, Result, ensure};
use std::{
    borrow::Cow,
    collections::VecDeque,
    sync::Arc,
    time::{Duration, Instant, SystemTime},
};

pub struct Decision {
    pub event: Option<u64>,
    pub encode: bool,
    pub force_keyframe: bool,
}

struct Route {
    timestamp_us: u64,
    event: Option<u64>,
    queued: Instant,
    captured_at: SystemTime,
}

pub struct Controller<'a> {
    config: &'a Config,
    detector: Detector,
    state: RecorderState,
    writer: ClipWriter,
    disk: Option<RecordingWorker>,
    pre_roll: PreRoll,
    writing_event: Option<u64>,
    started: Option<Instant>,
    recording: bool,
    event: u64,
    routes: VecDeque<Route>,
    last_timestamp: Option<u64>,
    live: Option<Arc<Hub>>,
}

impl<'a> Controller<'a> {
    #[cfg(test)]
    pub fn new(config: &'a Config) -> Self {
        Self::with_live(config, None)
    }

    pub fn with_live(config: &'a Config, live: Option<Arc<Hub>>) -> Self {
        let pre_motion_seconds = if config.recording.enabled && config.motion.enabled {
            config.recording.pre_motion_seconds
        } else {
            0
        };
        let mut motion = config.motion.clone();
        motion.enabled &= config.recording.enabled;
        Self {
            config,
            detector: Detector::new(
                &motion,
                config.camera.width as usize,
                config.camera.height as usize,
            ),
            state: RecorderState::new(
                Duration::from_secs(config.camera.warmup_seconds),
                Duration::from_secs(config.recording.post_motion_seconds),
            ),
            writer: ClipWriter::configured(&config.recording, &config.camera),
            disk: None,
            pre_roll: PreRoll::new(
                pre_motion_seconds,
                config.camera.framerate,
                config.recording.pre_motion_buffer_mb * 1024 * 1024,
            ),
            writing_event: None,
            started: None,
            recording: false,
            event: 0,
            routes: VecDeque::with_capacity(128),
            last_timestamp: None,
            live,
        }
    }

    pub fn with_async_recording(config: &'a Config, live: Option<Arc<Hub>>) -> Result<Self> {
        let mut controller = Self::with_live(config, live);
        if config.recording.enabled {
            controller.disk = Some(RecordingWorker::new(&config.recording, &config.camera)?);
        }
        Ok(controller)
    }

    fn finish_event(&mut self) -> Result<()> {
        if let Some(disk) = self.disk.as_mut() {
            return disk.finish_event();
        }
        #[cfg(test)]
        return self.writer.finish();
        #[cfg(not(test))]
        Ok(())
    }

    fn write_frame(
        &mut self,
        event: u64,
        timestamp_us: u64,
        captured_at: SystemTime,
        data: &PacketData<'_>,
        info: &crate::recording::PacketInfo,
    ) -> Result<()> {
        if let Some(disk) = self.disk.as_mut() {
            return disk.write(event, timestamp_us, captured_at, data, info);
        }
        #[cfg(test)]
        return self
            .writer
            .write_packet(event, timestamp_us, captured_at, data, info);
        #[cfg(not(test))]
        anyhow::bail!("recording worker unavailable")
    }

    pub fn needs_motion_pixels(&self) -> bool {
        self.detector.needs_pixels()
    }

    pub fn frame(&mut self, y: &[u8], stride: usize) -> Result<Decision> {
        let now = self.started.get_or_insert_with(Instant::now).elapsed();
        let motion = if self.recording {
            self.detector.analyze(y, stride)?
        } else {
            self.detector.analyze_counted(y, stride)?
        };
        self.tick(now, motion)
    }

    fn recording_start_message(&self) -> String {
        if !self.config.motion.enabled {
            return "continuous: recording started".into();
        }
        let Some(sample) = self.detector.last_sample() else {
            return "motion: recording started".into();
        };
        // Fast-path samples are lower bounds rather than exact totals.
        let comparison = if sample.exact { "=" } else { ">=" };
        format!(
            "motion: recording started; changed_pixels{comparison}{}/{}; \
             changed_fraction{comparison}{:.6}; required_pixels={}; region_threshold={}; \
             pixel_difference_threshold={}; pixel_cutoff={}/255; frame_period={}",
            sample.changed_pixels,
            sample.total_pixels,
            sample.changed_pixels as f64 / sample.total_pixels as f64,
            sample.required_pixels,
            self.config.motion.region_threshold,
            self.config.motion.pixel_difference_threshold,
            sample.pixel_cutoff,
            self.config.motion.frame_period,
        )
    }

    pub fn frame_without_pixels(&mut self, plane_len: usize, stride: usize) -> Result<Decision> {
        let now = self.started.get_or_insert_with(Instant::now).elapsed();
        self.detector.advance_unsampled(plane_len, stride)?;
        self.tick(now, None)
    }

    fn tick(&mut self, now: Duration, motion: Option<bool>) -> Result<Decision> {
        if !self.config.recording.enabled {
            return Ok(Decision {
                event: None,
                encode: self.live.is_some(),
                force_keyframe: false,
            });
        }
        if !self.config.motion.enabled {
            // A persistent recording request retains warmup without depending
            // on sampled pixels or a quiet tail.
            self.state.motion(true, now);
        } else if let Some(motion) = motion {
            self.state.motion(motion, now);
        }
        if let Some(disk) = self.disk.as_ref() {
            disk.check_health()?;
        }
        let action = self.state.tick(now);
        match action {
            Some(Action::Start) => {
                self.event += 1;
                self.recording = true;
                crate::log(self.recording_start_message());
            }
            Some(Action::Stop) => {
                self.recording = false;
                crate::log(if self.config.motion.enabled {
                    "motion: recording stopped"
                } else {
                    "continuous: recording stopped"
                });
            }
            None => {}
        }
        if !self.recording && self.routes.is_empty() {
            self.finish_event()?;
            self.writing_event = None;
        }
        Ok(Decision {
            event: self.recording.then_some(self.event),
            encode: self.recording
                || (self.config.motion.enabled && self.config.recording.pre_motion_seconds > 0)
                || self.live.is_some(),
            force_keyframe: action == Some(Action::Start),
        })
    }

    pub fn submitted(
        &mut self,
        timestamp_us: u64,
        event: Option<u64>,
        captured_at: SystemTime,
    ) -> Result<()> {
        ensure!(
            self.last_timestamp.is_none_or(|last| timestamp_us > last),
            "non-monotonic camera timestamps"
        );
        ensure!(
            self.routes.len() < 128,
            "encoder is not returning queued frames"
        );
        self.last_timestamp = Some(timestamp_us);
        self.routes.push_back(Route {
            timestamp_us,
            event,
            queued: Instant::now(),
            captured_at,
        });
        Ok(())
    }

    pub fn encoded(&mut self, packet: Encoded<'_>) -> Result<()> {
        let streaming = self.live.as_ref().is_some_and(|hub| hub.has_clients());
        let data = if streaming
            && (!self.config.recording.enabled
                || !self.config.motion.enabled
                || self.config.recording.pre_motion_seconds == 0)
        {
            Cow::Owned(packet.data.to_vec())
        } else {
            self.pre_roll.cache_packet(packet.data)
        };
        let info = self.writer.inspect(&data)?;
        if let (Some(disk), Some((sps, pps))) = (&mut self.disk, self.writer.h264_headers()) {
            disk.headers(sps, pps)?;
        }
        if let (Some(hub), Some((sps, pps))) = (&self.live, self.writer.h264_headers()) {
            hub.update_codec(sps, pps);
        }
        if info.vcl {
            let data = PacketData::prepare(data, &info);
            let data = if self.disk.is_some() {
                PacketData::Avc(data.sample(Default::default())?)
            } else {
                data
            };
            let index = if self
                .routes
                .front()
                .is_some_and(|route| route.timestamp_us == packet.timestamp_us)
            {
                0
            } else {
                self.routes
                    .iter()
                    .position(|route| route.timestamp_us == packet.timestamp_us)
                    .context("encoded frame has no matching camera timestamp")?
            };
            let Route {
                event, captured_at, ..
            } = self
                .routes
                .remove(index)
                .context("encoded frame has no matching camera timestamp")?;
            if streaming && let Some(hub) = &self.live {
                // Publish before any potentially blocking NAS writes. This
                // immutable sample may share the existing pre-roll allocation.
                hub.publish(
                    packet.timestamp_us,
                    info.idr,
                    data.sample(Default::default())?,
                );
            }
            if let Some(event) = event {
                if self.writing_event != Some(event) {
                    self.pre_roll.trim(packet.timestamp_us);
                    let mut frames = 0;
                    let mut first_timestamp = packet.timestamp_us;
                    for frame in self.pre_roll.frames() {
                        if frames == 0 {
                            first_timestamp = frame.timestamp_us;
                        }
                        if let Some(disk) = self.disk.as_mut() {
                            disk.write(
                                event,
                                frame.timestamp_us,
                                frame.captured_at,
                                &frame.data,
                                &frame.info,
                            )?;
                        } else {
                            #[cfg(test)]
                            self.writer.write_packet(
                                event,
                                frame.timestamp_us,
                                frame.captured_at,
                                &frame.data,
                                &frame.info,
                            )?;
                            #[cfg(not(test))]
                            anyhow::bail!("recording worker unavailable");
                        }
                        frames += 1;
                    }
                    if frames > 0 {
                        crate::log(format!(
                            "pre-motion: writing {frames} buffered frames ({:.3}s before trigger)",
                            packet.timestamp_us.saturating_sub(first_timestamp) as f64
                                / 1_000_000.0
                        ));
                    }
                    self.writing_event = Some(event);
                }
                self.write_frame(event, packet.timestamp_us, captured_at, &data, &info)?;
            } else if self.writing_event.take().is_some() {
                self.finish_event()?;
            }
            self.pre_roll
                .push_prepared(packet.timestamp_us, captured_at, data, info);
        }
        if !self.recording && self.routes.is_empty() {
            self.finish_event()?;
            self.writing_event = None;
        }
        Ok(())
    }

    pub fn check_encoder_progress(&self) -> Result<()> {
        if let Some(disk) = &self.disk {
            disk.check_health()?;
        }
        let limit = Duration::from_secs(self.config.camera.frame_timeout_seconds);
        ensure!(
            !self
                .routes
                .front()
                .is_some_and(|route| route.queued.elapsed() > limit),
            "encoder stalled"
        );
        Ok(())
    }

    pub fn has_pending(&self) -> bool {
        !self.routes.is_empty()
    }
    pub fn finish(&mut self) -> Result<()> {
        self.finish_event()?;
        if let Some(disk) = self.disk.as_mut() {
            disk.shutdown()?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::recording::PacketInfo;
    use std::{
        fs,
        time::{SystemTime, UNIX_EPOCH},
    };

    #[test]
    fn configured_pre_motion_budget_controls_retained_history() {
        for (budget_mb, expected_frames) in [(1, 1), (2, 2)] {
            let config: Config =
                toml::from_str(&format!("[recording]\npre_motion_buffer_mb={budget_mb}")).unwrap();
            config.validate().unwrap();
            let mut pipeline = Controller::new(&config);
            let data: mp4::Bytes = vec![42; 600 * 1024].into();
            for second in 0..2 {
                pipeline.pre_roll.push_prepared(
                    second * 1_000_000,
                    UNIX_EPOCH,
                    PacketData::Avc(data.clone()),
                    PacketInfo {
                        vcl: true,
                        idr: true,
                        ..Default::default()
                    },
                );
            }
            assert_eq!(pipeline.pre_roll.frames().count(), expected_frames);
        }
    }

    #[test]
    fn recording_start_logs_exact_pixel_counts_and_both_configured_thresholds() {
        let mut config = Config::default();
        config.camera.width = 64;
        config.camera.height = 64;
        config.camera.warmup_seconds = 0;
        config.recording.output_dir = std::env::temp_dir();
        config.motion.frame_period = 1;
        config.motion.region_threshold = 0.01;
        config.motion.pixel_difference_threshold = 0.15;
        let mut pipeline = Controller::new(&config);
        assert!(pipeline.frame(&vec![40; 4096], 64).unwrap().event.is_none());
        assert!(pipeline.frame(&vec![180; 4096], 64).unwrap().force_keyframe);
        let message = pipeline.recording_start_message();
        for expected in [
            "motion: recording started; changed_pixels=4096/4096",
            "changed_fraction=1.000000",
            "required_pixels=41",
            "region_threshold=0.01",
            "pixel_difference_threshold=0.15",
            "pixel_cutoff=39/255",
            "frame_period=1",
        ] {
            assert!(message.contains(expected), "{message}");
        }
        pipeline.detector.analyze(&vec![40; 4096], 64).unwrap();
        assert!(
            pipeline
                .recording_start_message()
                .contains("changed_pixels>=")
        );
    }

    #[test]
    fn continuous_recording_skips_detection_rotates_and_recovers_from_disk_pressure() {
        let root = std::env::temp_dir().join(format!(
            "pivigil-continuous-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir(&root).unwrap();
        let mut config = Config::default();
        config.camera.width = 64;
        config.camera.height = 64;
        config.camera.framerate = 1;
        config.camera.warmup_seconds = 3;
        config.motion.enabled = false;
        config.recording.output_dir = root.clone();
        config.recording.filename_format = "quiet-%s.mp4".into();
        config.recording.pre_motion_seconds = 30;
        config.recording.post_motion_seconds = 1;
        config.recording.max_file_seconds = 2;
        config.recording.min_free_mb = 1;
        let mut pipeline = Controller::new(&config);
        use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
        let free = Arc::new(AtomicU64::new(1024 * 1024));
        let checks = Arc::new(AtomicUsize::new(0));
        let available = Arc::clone(&free);
        let checked = Arc::clone(&checks);
        pipeline.writer.set_space_query(move |_| {
            checked.fetch_add(1, Ordering::SeqCst);
            Ok(available.load(Ordering::SeqCst))
        });
        assert!(!pipeline.needs_motion_pixels());
        assert!(!pipeline.frame_without_pixels(64 * 64, 64).unwrap().encode);
        pipeline
            .encoded(Encoded {
                timestamp_us: 0,
                data: &[0, 0, 1, 0x67, 0x64, 0, 0x28, 0, 0, 1, 0x68, 0x34],
            })
            .unwrap();
        let base = UNIX_EPOCH + Duration::from_secs(1_794_420_000);
        for second in 0..16 {
            if second == 10 {
                free.store(0, Ordering::SeqCst);
            } else if second == 14 {
                free.store(1024 * 1024, Ordering::SeqCst);
            }
            let decision = pipeline
                .tick(Duration::from_secs(second), Some(false))
                .unwrap();
            assert_eq!(decision.encode, second >= 3);
            assert_eq!(decision.force_keyframe, second == 3);
            assert_eq!(decision.event, if second >= 3 { Some(1) } else { None });
            if decision.encode {
                let pts = second * 1_000_000;
                pipeline
                    .submitted(pts, decision.event, base + Duration::from_secs(second))
                    .unwrap();
                let packet = [
                    0,
                    0,
                    1,
                    if second % 2 == 1 { 0x65 } else { 0x41 },
                    second as u8,
                ];
                pipeline
                    .encoded(Encoded {
                        timestamp_us: pts,
                        data: &packet,
                    })
                    .unwrap();
            }
            assert_eq!(pipeline.pre_roll.frames().count(), 0);
        }
        assert_eq!(
            pipeline
                .tick(Duration::from_secs(100), Some(false))
                .unwrap()
                .event,
            Some(1)
        );
        pipeline.finish().unwrap();
        assert_eq!(checks.load(Ordering::SeqCst), 7);
        let mut recorded = Vec::new();
        let clips: Vec<_> = fs::read_dir(&root)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .collect();
        assert_eq!(clips.len(), 5);
        for clip in clips {
            assert!(
                clip.file_name()
                    .unwrap()
                    .to_str()
                    .unwrap()
                    .starts_with("quiet-")
            );
            let file = fs::File::open(clip).unwrap();
            let mut mp4 =
                mp4::Mp4Reader::read_header(&file, file.metadata().unwrap().len()).unwrap();
            assert!(mp4.read_sample(1, 1).unwrap().unwrap().is_sync);
            for index in 1..=mp4.sample_count(1).unwrap() {
                recorded.push(mp4.read_sample(1, index).unwrap().unwrap().bytes[5]);
            }
        }
        recorded.sort();
        // Finish the open clip through frame 10; skip 11..14 and resume at IDR 15.
        assert_eq!(recorded, [3, 4, 5, 6, 7, 8, 9, 10, 15]);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn disabled_recording_overrides_motion_modes_and_keeps_live_encoding() {
        for motion in [false, true] {
            let mut config = Config::default();
            config.camera.width = 64;
            config.camera.height = 64;
            config.motion.enabled = motion;
            config.recording.enabled = false;
            config.recording.output_dir = "/dev/null/unavailable".into();
            config.recording.pre_motion_seconds = 30;
            let mut pipeline = Controller::new(&config);
            assert!(!pipeline.needs_motion_pixels());
            for now in [0, 3, 100] {
                let decision = pipeline.tick(Duration::from_secs(now), Some(true)).unwrap();
                assert!(!decision.encode && !decision.force_keyframe && decision.event.is_none());
            }
            let hub = Hub::new(&config.stream, &config.camera);
            let _capture = hub.capture();
            let mut live = Controller::with_live(&config, Some(hub));
            let decision = live.frame_without_pixels(64 * 64, 64).unwrap();
            assert!(decision.encode && decision.event.is_none() && !decision.force_keyframe);
            live.submitted(1_000_000, decision.event, UNIX_EPOCH)
                .unwrap();
            live.encoded(Encoded {
                timestamp_us: 1_000_000,
                data: &[
                    0, 0, 1, 0x67, 0x64, 0, 0x28, 0, 0, 1, 0x68, 0x34, 0, 0, 1, 0x65, 0xaa,
                ],
            })
            .unwrap();
            live.finish().unwrap();
            assert!(!live.has_pending());
            assert_eq!(live.pre_roll.frames().count(), 0);
        }
    }

    #[test]
    fn real_software_pipeline_replays_pre_motion_rotates_and_closes_after_tail() {
        use crate::encoder::{software::SoftwareEncoder, yuv::Yuv420};
        let root = std::env::temp_dir().join(format!(
            "pivigil-software-pipeline-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir(&root).unwrap();
        let mut config = Config {
            recording: crate::config::Recording {
                output_dir: root.clone(),
                ..Default::default()
            },
            ..Default::default()
        };
        config.camera.width = 64;
        config.camera.height = 64;
        config.camera.warmup_seconds = 0;
        config.motion.frame_period = 1;
        config.recording.pre_motion_seconds = 2;
        config.recording.post_motion_seconds = 4;
        config.recording.max_file_seconds = 1;
        config.recording.min_free_mb = 1;
        let mut pipeline = Controller::with_async_recording(&config, None).unwrap();
        let mut encoder = SoftwareEncoder::new(&config.camera).unwrap();
        for index in 0..150u64 {
            let relative = index * 1_000_000 / 15;
            let value = if (45..60).contains(&index) {
                if index % 2 == 0 { 100 } else { 140 }
            } else {
                40
            };
            let mut pixels = vec![128; 64 * 64 * 3 / 2];
            pixels[..64 * 64].fill(value);
            // Exercise the detector/tick path with a deterministic monotonic clock.
            let motion = pipeline.detector.analyze(&pixels, 64).unwrap();
            let decision = pipeline
                .tick(Duration::from_micros(relative), motion)
                .unwrap();
            if index < 45 {
                assert!(decision.event.is_none());
            }
            if index == 45 {
                assert!(decision.force_keyframe && decision.event.is_some());
            }
            if index >= 135 {
                assert!(!pipeline.recording && pipeline.writing_event.is_none());
            }
            assert!(decision.encode); // Pre-motion capture continues through idle periods.
            let timestamp = 1_000_000 + relative;
            pipeline
                .submitted(
                    timestamp,
                    decision.event,
                    UNIX_EPOCH
                        + Duration::from_secs(1_794_420_000)
                        + Duration::from_micros(timestamp),
                )
                .unwrap();
            let frame = Yuv420::new(&[&pixels], 64, 64, 64).unwrap();
            encoder
                .encode(&frame, timestamp, decision.force_keyframe, |packet| {
                    pipeline.encoded(packet)
                })
                .unwrap();
            assert!(!pipeline.has_pending());
        }
        encoder.finish();
        pipeline.finish().unwrap();
        let clips: Vec<_> = fs::read_dir(&root)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .collect();
        assert!(clips.len() >= 6);
        let mut samples = 0;
        for clip in clips {
            let file = fs::File::open(clip).unwrap();
            let mut reader =
                mp4::Mp4Reader::read_header(&file, file.metadata().unwrap().len()).unwrap();
            assert!(reader.read_sample(1, 1).unwrap().unwrap().is_sync);
            samples += reader.sample_count(1).unwrap();
        }
        // Two seconds of pre-motion (30 frames), then motion and its four-second
        // tail starts at the first quiet sample (61) and expires at 121.
        assert_eq!(samples, 106);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn live_stream_keeps_encoding_idle_without_starting_a_recording() {
        let config = Config {
            recording: crate::config::Recording {
                output_dir: std::env::temp_dir(),
                ..Default::default()
            },
            ..Config::default()
        };
        let hub = crate::live::Hub::new(&config.stream, &config.camera);
        let _capture = hub.capture();
        let mut pipeline = Controller::with_live(&config, Some(hub));
        let frame = vec![40; config.camera.width as usize * config.camera.height as usize];
        let decision = pipeline
            .frame(&frame, config.camera.width as usize)
            .unwrap();
        assert!(decision.encode);
        assert!(decision.event.is_none());
        assert!(!decision.force_keyframe);
    }

    #[test]
    fn metadata_only_frames_advance_the_five_second_recording_tail() {
        let mut config = Config {
            recording: crate::config::Recording {
                output_dir: std::env::temp_dir(),
                ..Default::default()
            },
            ..Config::default()
        };
        config.camera.width = 64;
        config.camera.height = 64;
        config.camera.warmup_seconds = 0;
        config.recording.post_motion_seconds = 5;
        config.recording.min_free_mb = 1;
        let mut pipeline = Controller::new(&config);
        assert!(pipeline.frame(&[40; 4096], 64).unwrap().event.is_none());
        pipeline.frame_without_pixels(4096, 64).unwrap();
        pipeline.frame_without_pixels(4096, 64).unwrap();
        pipeline.started = Some(Instant::now() - Duration::from_secs(2));
        assert!(pipeline.frame(&[100; 4096], 64).unwrap().event.is_some());
        pipeline.frame_without_pixels(4096, 64).unwrap();
        pipeline.frame_without_pixels(4096, 64).unwrap();
        pipeline.started = Some(Instant::now() - Duration::from_secs(3));
        assert!(pipeline.frame(&[100; 4096], 64).unwrap().event.is_some());
        pipeline.started = Some(Instant::now() - Duration::from_millis(7900));
        assert!(
            pipeline
                .frame_without_pixels(4096, 64)
                .unwrap()
                .event
                .is_some()
        );
        pipeline.started = Some(Instant::now() - Duration::from_millis(8100));
        assert!(
            pipeline
                .frame_without_pixels(4096, 64)
                .unwrap()
                .event
                .is_none()
        );
    }

    #[test]
    fn routes_preserve_out_of_order_matching_capacity_and_oldest_timeout() {
        let config = Config::default();
        let mut pipeline = Controller::new(&config);
        let capacity = pipeline.routes.capacity();
        for timestamp in 1..=128 {
            pipeline.submitted(timestamp, None, UNIX_EPOCH).unwrap();
        }
        assert!(pipeline.submitted(129, None, UNIX_EPOCH).is_err());
        assert_eq!(pipeline.routes.capacity(), capacity);
        for timestamp in [128, 64, 1] {
            pipeline
                .encoded(Encoded {
                    timestamp_us: timestamp,
                    data: &[0, 0, 0, 1, 0x41, 0xaa],
                })
                .unwrap();
        }
        assert_eq!(pipeline.routes.len(), 125);
        assert_eq!(pipeline.routes.front().unwrap().timestamp_us, 2);
        pipeline.routes.front_mut().unwrap().queued =
            Instant::now() - Duration::from_secs(config.camera.frame_timeout_seconds + 1);
        assert!(pipeline.check_encoder_progress().is_err());
        pipeline
            .encoded(Encoded {
                timestamp_us: 2,
                data: &[0, 0, 0, 1, 0x41, 0xaa],
            })
            .unwrap();
        pipeline.check_encoder_progress().unwrap();
        assert!(
            pipeline
                .encoded(Encoded {
                    timestamp_us: 999,
                    data: &[0, 0, 0, 1, 0x41, 0xaa]
                })
                .is_err()
        );
        assert_eq!(pipeline.routes.len(), 124);
    }

    #[test]
    fn native_frame_pipeline_records_motion_without_a_backend_process() {
        let root = std::env::temp_dir().join(format!(
            "pivigil-controller-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir(&root).unwrap();
        let mut config = Config {
            recording: crate::config::Recording {
                output_dir: root.clone(),
                ..Default::default()
            },
            ..Config::default()
        };
        config.camera.warmup_seconds = 0;
        config.recording.pre_motion_seconds = 0;
        config.recording.min_free_mb = 1;
        config.motion.frame_period = 1;
        let pixels = config.camera.width as usize * config.camera.height as usize;
        let stride = config.camera.width as usize;
        let mut pipeline = Controller::new(&config);
        let frame = vec![40; pixels];
        let idle = pipeline.frame(&frame, stride).unwrap();
        assert!(idle.event.is_none());
        assert!(!idle.encode); // pre_motion_seconds=0 keeps idle hardware encoding disabled.
        let decision = pipeline.frame(&vec![100; pixels], stride).unwrap();
        assert!(decision.force_keyframe);
        let event = decision.event.unwrap();
        pipeline
            .submitted(1_000_000, Some(event), SystemTime::now())
            .unwrap();
        pipeline
            .encoded(Encoded {
                timestamp_us: 0,
                data: &[0, 0, 1, 0x67, 0x64, 0x00, 0x28, 0, 0, 1, 0x68, 0x34],
            })
            .unwrap();
        pipeline
            .encoded(Encoded {
                timestamp_us: 1_000_000,
                data: &[0, 0, 1, 0x65, 0xaa],
            })
            .unwrap();
        assert!(!pipeline.has_pending());
        pipeline.finish().unwrap();
        let clips: Vec<_> = fs::read_dir(&root)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .collect();
        assert_eq!(clips.len(), 1);
        assert_eq!(clips[0].extension().unwrap(), "mp4");
        let file = fs::File::open(&clips[0]).unwrap();
        let mut mp4 = mp4::Mp4Reader::read_header(&file, file.metadata().unwrap().len()).unwrap();
        assert_eq!(mp4.sample_count(1).unwrap(), 1);
        let sample = mp4.read_sample(1, 1).unwrap().unwrap();
        assert!(sample.is_sync);
        assert_eq!(&sample.bytes[..], &[0, 0, 0, 2, 0x65, 0xaa]);
        assert!(
            pipeline
                .encoded(Encoded {
                    timestamp_us: 123,
                    data: &[0, 0, 1, 0x41, 0xbb]
                })
                .is_err()
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn pre_motion_replays_history_once_and_closes_clips_while_idle_encoding_continues() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let root = std::env::temp_dir().join(format!(
            "pivigil-preroll-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir(&root).unwrap();
        let mut config = Config {
            recording: crate::config::Recording {
                output_dir: root.clone(),
                ..Default::default()
            },
            ..Config::default()
        };
        config.recording.pre_motion_seconds = 3;
        config.camera.warmup_seconds = 0;
        config.recording.min_free_mb = 1;
        config.motion.frame_period = 1;
        let pixels = config.camera.width as usize * config.camera.height as usize;
        let stride = config.camera.width as usize;
        let mut pipeline = Controller::new(&config);
        let checks = Arc::new(AtomicUsize::new(0));
        let checked = Arc::clone(&checks);
        pipeline.writer.set_space_query(move |_| {
            checked.fetch_add(1, Ordering::SeqCst);
            Ok(1024 * 1024)
        });
        let idle = pipeline.frame(&vec![40; pixels], stride).unwrap();
        assert!(idle.encode && idle.event.is_none());
        pipeline
            .encoded(Encoded {
                timestamp_us: 0,
                data: &[0, 0, 1, 0x67, 0x64, 0, 0x28, 0, 0, 1, 0x68, 0x34],
            })
            .unwrap();

        fn submit(pipeline: &mut Controller<'_>, second: u64, event: Option<u64>) {
            pipeline
                .submitted(
                    second * 1_000_000,
                    event,
                    UNIX_EPOCH + Duration::from_secs(1_794_343_680 + second),
                )
                .unwrap();
        }
        fn encoded(pipeline: &mut Controller<'_>, second: u64) {
            let kind = if second.is_multiple_of(2) { 0x65 } else { 0x41 };
            pipeline
                .encoded(Encoded {
                    timestamp_us: second * 1_000_000,
                    data: &[0, 0, 1, kind, second as u8],
                })
                .unwrap();
        }
        for second in 0..8 {
            submit(&mut pipeline, second, None);
            encoded(&mut pipeline, second);
        }
        assert_eq!(fs::read_dir(&root).unwrap().count(), 0); // RAM only until motion.
        assert_eq!(checks.load(Ordering::SeqCst), 0);
        let event = pipeline
            .frame(&vec![100; pixels], stride)
            .unwrap()
            .event
            .unwrap();
        submit(&mut pipeline, 8, Some(event));
        encoded(&mut pipeline, 8);
        assert_eq!(checks.load(Ordering::SeqCst), 1);

        // Simulate a completed tail with two idle inputs already queued. The first
        // idle encoded output must close the MP4 even though the route map isn't empty.
        pipeline.recording = false;
        submit(&mut pipeline, 9, None);
        submit(&mut pipeline, 10, None);
        encoded(&mut pipeline, 9);
        assert!(pipeline.has_pending());
        let first = fs::read_dir(&root).unwrap().next().unwrap().unwrap().path();
        let file = fs::File::open(first).unwrap();
        let mut mp4 = mp4::Mp4Reader::read_header(&file, file.metadata().unwrap().len()).unwrap();
        assert_eq!(mp4.sample_count(1).unwrap(), 5); // Seconds 4,5,6,7,8; trigger frame appears once.
        assert_eq!(mp4.read_sample(1, 1).unwrap().unwrap().bytes[5], 4);
        assert_eq!(mp4.read_sample(1, 5).unwrap().unwrap().bytes[5], 8);
        encoded(&mut pipeline, 10);

        // A later event can reuse context from the end of the preceding recording.
        pipeline.recording = true;
        submit(&mut pipeline, 11, Some(event + 1));
        encoded(&mut pipeline, 11);
        pipeline.finish().unwrap();
        assert_eq!(checks.load(Ordering::SeqCst), 2);
        let mut clips: Vec<_> = fs::read_dir(&root)
            .unwrap()
            .map(|e| e.unwrap().path())
            .collect();
        clips.sort();
        assert_eq!(clips.len(), 2);
        let file = fs::File::open(&clips[1]).unwrap();
        let mut mp4 = mp4::Mp4Reader::read_header(&file, file.metadata().unwrap().len()).unwrap();
        assert_eq!(mp4.sample_count(1).unwrap(), 4); // Seconds 8,9,10,11.
        assert!(mp4.read_sample(1, 1).unwrap().unwrap().is_sync);
        assert_eq!(mp4.read_sample(1, 1).unwrap().unwrap().bytes[5], 8);
        assert_eq!(mp4.read_sample(1, 4).unwrap().unwrap().bytes[5], 11);
        fs::remove_dir_all(root).unwrap();
    }
}
