//! Raspberry Pi camera capture through libcamera Rust bindings. No subprocess backend.
use crate::{
    config::{Config, EncoderBackend},
    controller::Controller,
    encoder::{DmaAccess, Encoder, MAX_BUFFERS, select_backend},
    live::Hub,
    overlay::VideoOverlay,
};
use anyhow::{Context, Result, bail, ensure};
use libcamera::{
    camera::{ActiveCamera, CameraConfigurationStatus},
    camera_manager::CameraManager,
    color_space::ColorSpace,
    control::{ControlEntry, ControlInfoMap, ControlList},
    controls,
    framebuffer::{AsFrameBuffer, FrameMetadataStatus},
    framebuffer_allocator::{FrameBuffer, FrameBufferAllocator},
    framebuffer_map::MemoryMappedFrameBuffer,
    geometry::Size,
    pixel_format::PixelFormat,
    request::{Request, RequestStatus, ReuseFlag},
    stream::StreamRole,
};
use std::{
    collections::VecDeque,
    sync::Arc,
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc::RecvTimeoutError,
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

const YUV420: PixelFormat = PixelFormat::new(u32::from_le_bytes(*b"YU12"), 0);

#[derive(Debug, PartialEq)]
struct FocusControls {
    mode: Option<controls::AfMode>,
    lens_position: Option<f32>,
}

impl FocusControls {
    fn select(autofocus: bool, position: f32, has_mode: bool, has_lens: bool) -> Self {
        let mode = has_mode.then_some(if autofocus {
            controls::AfMode::Continuous
        } else {
            controls::AfMode::Manual
        });
        Self {
            mode,
            lens_position: (mode == Some(controls::AfMode::Manual) && has_lens).then_some(position),
        }
    }

    fn detect(autofocus: bool, position: f32, available: &ControlInfoMap) -> Self {
        Self::select(
            autofocus,
            position,
            available.count(controls::AfMode::ID) > 0,
            available.count(controls::LensPosition::ID) > 0,
        )
    }

    fn apply(&self, list: &mut ControlList) -> Result<()> {
        if let Some(mode) = self.mode {
            list.set(mode)?;
        }
        if let Some(position) = self.lens_position {
            list.set(controls::LensPosition(position))?;
        }
        Ok(())
    }
}

/// Assigned at capture completion, before encoding or storage can delay processing.
#[derive(Default)]
struct FrameCounter {
    second: Option<u64>,
    next: u32,
}

impl FrameCounter {
    fn index(&mut self, captured_at: SystemTime) -> u32 {
        let second = captured_at
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        if self.second != Some(second) {
            self.second = Some(second);
            self.next = 0;
        }
        let index = self.next;
        self.next = self.next.saturating_add(1);
        index
    }
}

fn usable_status(status: FrameMetadataStatus) -> Result<bool> {
    ensure!(
        matches!(
            status,
            FrameMetadataStatus::Success | FrameMetadataStatus::Startup
        ),
        "camera returned an unsuccessful frame: {status:?}"
    );
    Ok(status == FrameMetadataStatus::Success)
}

fn requeue(camera: &ActiveCamera<'_>, mut request: Request) -> Result<()> {
    request.reuse(ReuseFlag::REUSE_BUFFERS);
    camera
        .queue_request(request)
        .map_err(|(_, error)| error)
        .context("requeue camera request")
}

/// Plane descriptors remain fixed while requests reuse their original buffers.
#[derive(Clone, Copy)]
struct DmaLayout {
    fd: i32,
    len: usize,
    y_len: usize,
    fds: [i32; 3],
    fd_count: usize,
}

fn dma_layout(frame: &impl AsFrameBuffer, expected: usize, hardware: bool) -> Result<DmaLayout> {
    let planes = frame.planes();
    ensure!(planes.len() <= 3, "YUV420 frame has too many planes");
    let mut descriptors = [(-1, None, 0); 3];
    for (descriptor, plane) in descriptors.iter_mut().zip(&*planes) {
        *descriptor = (plane.fd(), plane.offset(), plane.len());
    }
    plane_layout(&descriptors[..planes.len()], expected, hardware)
}

fn plane_layout(
    planes: &[(i32, Option<usize>, usize)],
    expected: usize,
    hardware: bool,
) -> Result<DmaLayout> {
    let first = planes.first().context("camera frame has no planes")?;
    ensure!(planes.len() <= 3, "YUV420 frame has too many planes");
    let fd = first.0;
    let mut fds = [-1; 3];
    let mut fd_count = 0;
    let mut end = 0usize;
    let mut ranges = [(0, 0usize, 0usize); 3];
    for (index, &(plane_fd, offset, len)) in planes.iter().enumerate() {
        ensure!(plane_fd >= 0 && len > 0, "invalid camera plane descriptor");
        let offset = offset.context("camera plane has no offset")?;
        let plane_end = offset
            .checked_add(len)
            .context("camera plane size overflow")?;
        ensure!(
            !ranges[..index]
                .iter()
                .any(|&(fd, start, end)| fd == plane_fd && offset < end && start < plane_end),
            "camera planes overlap"
        );
        ranges[index] = (plane_fd, offset, plane_end);
        if !fds[..fd_count].contains(&plane_fd) {
            fds[fd_count] = plane_fd;
            fd_count += 1;
        }
        ensure!(
            !hardware || (plane_fd == fd && offset == end),
            "hardware encoding requires contiguous camera planes in one DMA-BUF"
        );
        end = end
            .checked_add(len)
            .context("camera buffer size overflow")?;
    }
    ensure!(
        !hardware || end >= expected,
        "camera frame is shorter than its configured layout"
    );
    Ok(DmaLayout {
        fd,
        len: end,
        y_len: first.2,
        fds,
        fd_count,
    })
}

impl DmaLayout {
    fn access(&self, writable: bool) -> Result<[Option<DmaAccess>; 3]> {
        let mut guards = std::array::from_fn(|_| None);
        for (guard, &fd) in guards.iter_mut().zip(&self.fds[..self.fd_count]) {
            *guard = Some(if writable {
                DmaAccess::read_write(fd)?
            } else {
                DmaAccess::read(fd)?
            });
        }
        Ok(guards)
    }
}

fn finish_cpu(guards: [Option<DmaAccess>; 3]) -> Result<()> {
    for guard in guards.into_iter().flatten() {
        guard.finish()?;
    }
    Ok(())
}

fn annotate(
    request: &mut Request,
    stream: &libcamera::stream::Stream,
    overlay: &mut VideoOverlay,
    stride: usize,
    captured_at: SystemTime,
    frame: u32,
) -> Result<()> {
    let video: &mut MemoryMappedFrameBuffer<FrameBuffer> = request
        .buffer_mut(stream)
        .context("request has no writable video buffer")?;
    let mut planes = video.data_mut().context("access writable overlay planes")?;
    overlay.draw(&mut planes, stride, captured_at, frame)
}

pub fn run(config: &Config, quit: &AtomicBool, live: Option<&Arc<Hub>>) -> Result<()> {
    let manager = CameraManager::new()
        .context("start libcamera (IPA modules must support in-process operation)")?;
    let cameras = manager.cameras();
    let camera = cameras
        .get(config.camera.index as usize)
        .context("configured camera index not found")?;
    crate::log(format!("acquiring camera {}", camera.id()));
    let mut camera = camera.acquire().context("acquire camera")?;
    let mut configuration = camera
        .generate_configuration(&[StreamRole::VideoRecording])
        .context("camera cannot provide a video stream")?;
    ensure!(
        configuration.len() == 1,
        "camera did not provide one video stream"
    );
    let c = &config.camera;
    let backend = select_backend(c.encoder)?;
    let use_hardware = backend == EncoderBackend::Hardware;
    let mut overlay = VideoOverlay::new(&config.overlay, c.width, c.height)?;
    {
        let mut stream = configuration.get_mut(0).context("missing video stream")?;
        stream.set_pixel_format(YUV420);
        stream.set_size(Size::new(c.width, c.height));
        stream.set_buffer_count(6);
        stream.set_color_space(Some(ColorSpace::rec709()));
    }
    ensure!(
        !matches!(configuration.validate(), CameraConfigurationStatus::Invalid),
        "invalid camera stream configuration"
    );
    {
        let stream = configuration
            .get(0)
            .context("missing validated video stream")?;
        ensure!(
            stream.get_pixel_format() == YUV420
                && stream.get_size().width == c.width
                && stream.get_size().height == c.height
                && stream.get_color_space() == Some(ColorSpace::rec709()),
            "camera adjusted the video stream incompatibly; choose supported YUV420 dimensions"
        );
    }
    camera
        .configure(&mut configuration)
        .context("configure camera")?;
    let focus = FocusControls::detect(c.autofocus, c.lens_position, camera.controls());
    if focus.mode.is_none() {
        crate::log("camera has no autofocus control; leaving focus unchanged");
    } else if !c.autofocus && focus.lens_position.is_none() {
        crate::log("manual focus selected; camera has no lens-position control");
    }
    let video = configuration.get(0).unwrap();
    let video_stream = video.stream().context("missing configured video stream")?;
    let stride = video.get_stride();
    let frame_size = video.get_frame_size();
    ensure!(
        stride >= c.width && frame_size as u64 >= u64::from(stride) * u64::from(c.height) * 3 / 2,
        "invalid camera YUV420 layout"
    );
    let mut allocator = FrameBufferAllocator::new(&camera);
    let video_buffers = allocator
        .alloc(&video_stream)
        .context("allocate camera video DMA-BUFs")?;
    ensure!(
        video_buffers.len() >= 3,
        "camera provided too few video buffers"
    );
    let mut requests = Vec::new();
    let mut layouts = Vec::with_capacity(video_buffers.len());
    for (index, video) in video_buffers.into_iter().enumerate() {
        layouts.push(dma_layout(&video, frame_size as usize, use_hardware)?);
        let mut request = camera
            .create_request(Some(index as u64))
            .context("create camera request")?;
        let mapped = if overlay.is_some() {
            MemoryMappedFrameBuffer::new_writable(video)
        } else {
            MemoryMappedFrameBuffer::new(video)
        }
        .context("map original video DMA-BUF")?;
        if !use_hardware {
            // Mapping slices include each plane's actual FD offset. Validate sizes
            // before capture starts, without reading pixels owned by the device.
            crate::encoder::yuv::Yuv420::new(
                &mapped.data(),
                c.width as usize,
                c.height as usize,
                stride as usize,
            )?;
        }

        request
            .add_buffer(&video_stream, mapped)
            .context("attach original video DMA-BUF")?;
        request.controls_mut().set(controls::FrameDurationLimits(
            [1_000_000 / i64::from(c.framerate); 2],
        ))?;
        focus.apply(request.controls_mut())?;
        requests.push(request);
    }
    let (sender, receiver) = std::sync::mpsc::channel();
    let mut frames = FrameCounter::default();
    camera.on_request_completed(move |request| {
        let captured_at = SystemTime::now();
        let frame = frames.index(captured_at);
        let _ = sender.send((request, captured_at, frame));
    });
    // Declare every request owner before the encoder: on errors STREAMOFF/close must
    // release DMA-BUF references before Rust drops any camera request or allocator.
    let mut completed: VecDeque<(Request, SystemTime, u32)> = VecDeque::new();
    let mut held: [Option<Request>; MAX_BUFFERS] = std::array::from_fn(|_| None);
    let mut encoder = if use_hardware {
        Some(Encoder::new(c, stride, frame_size)?)
    } else {
        None
    };
    let mut software = if !use_hardware {
        Some(crate::encoder::software::SoftwareEncoder::new(c)?)
    } else {
        None
    };
    let mut controller = Controller::with_async_recording(config, live.cloned())?;
    let _live_capture = live.map(|hub| hub.capture());
    camera.start(None).context("start camera capture")?;
    for request in requests {
        camera
            .queue_request(request)
            .map_err(|(_, error)| error)
            .context("queue initial camera request")?;
    }
    crate::log(format!(
        "camera started; capturing original {}x{} frames; motion detection: {}; encoder backend: {:?}{}",
        c.width,
        c.height,
        config.recording.enabled && config.motion.enabled,
        backend,
        if live.is_some() {
            format!("; streaming: enabled; listen: {}", config.stream.listen)
        } else {
            String::new()
        }
    ));
    let launched = Instant::now();
    let mut last_frame: Option<Instant> = None;
    let mut last_capture: Option<(u32, u64)> = None;
    while !quit.load(Ordering::Relaxed) {
        if let Some(last) = last_frame {
            ensure!(
                last.elapsed() < Duration::from_secs(config.camera.frame_timeout_seconds),
                "camera frame stream stalled"
            );
        } else {
            ensure!(
                launched.elapsed() < Duration::from_secs(config.camera.startup_timeout_seconds),
                "camera did not deliver its first usable frame"
            );
        }
        controller.check_encoder_progress()?;
        if let Some(encoder) = encoder.as_mut() {
            encoder.receive(|packet| controller.encoded(packet))?;
            for index in encoder.reclaim()? {
                let request = held
                    .get_mut(index as usize)
                    .and_then(Option::take)
                    .context("encoder returned an unowned camera request")?;
                requeue(&camera, request)?;
            }
        }
        completed.extend(receiver.try_iter());
        if encoder.as_ref().is_none_or(Encoder::has_slot)
            && let Some((mut request, captured_at, frame)) = completed.pop_front()
        {
            ensure!(
                request.status() == RequestStatus::Complete,
                "camera capture was cancelled"
            );
            let video: &MemoryMappedFrameBuffer<FrameBuffer> = request
                .buffer(&video_stream)
                .context("request has no video buffer")?;
            let metadata = video.metadata().context("video metadata unavailable")?;
            let usable = usable_status(metadata.status())?;
            let timestamp_us = metadata.timestamp() / 1000;
            if !usable {
                // Raspberry Pi pipelines return FrameStartup while AE/AWB settle.
                // Recycle those buffers without analyzing or encoding unready images.
                requeue(&camera, request)?;
                continue;
            }
            if let Some((sequence, previous_us)) = last_capture {
                let steps = metadata.sequence().wrapping_sub(sequence);
                let gap_us = timestamp_us.saturating_sub(previous_us);
                if (steps > 1 && steps < 1 << 31) || gap_us > 1_500_000 / u64::from(c.framerate) {
                    crate::log(format!(
                        "camera frame gap: {:.3}ms, sequence advanced {steps}",
                        gap_us as f64 / 1000.0
                    ));
                }
            }
            last_capture = Some((metadata.sequence(), timestamp_us));
            if last_frame.is_none() {
                crate::log("first usable camera frame received");
            }
            last_frame = Some(Instant::now());
            let layout = layouts
                .get(request.cookie() as usize)
                .context("camera returned an unknown request cookie")?;
            let decision = if controller.needs_motion_pixels() {
                let sync = layout.access(overlay.is_some())?;
                let data = video.data();
                let decision = controller.frame(
                    data.first()
                        .context("video frame has no mapped luminance data")?,
                    stride as usize,
                )?;
                if decision.encode
                    && let Some(renderer) = overlay.as_mut()
                {
                    annotate(
                        &mut request,
                        &video_stream,
                        renderer,
                        stride as usize,
                        captured_at,
                        frame,
                    )?;
                }
                finish_cpu(sync)?;
                decision
            } else {
                let decision = controller.frame_without_pixels(layout.y_len, stride as usize)?;
                if decision.encode
                    && let Some(renderer) = overlay.as_mut()
                {
                    let sync = layout.access(true)?;
                    annotate(
                        &mut request,
                        &video_stream,
                        renderer,
                        stride as usize,
                        captured_at,
                        frame,
                    )?;
                    finish_cpu(sync)?;
                }
                decision
            };
            if decision.encode {
                controller.submitted(timestamp_us, decision.event, captured_at)?;
                if let Some(encoder) = encoder.as_mut() {
                    if decision.force_keyframe {
                        encoder.force_keyframe()?;
                    }
                    let index = encoder.submit(layout.fd, layout.len, timestamp_us)?;
                    match held.get_mut(index as usize) {
                        Some(slot) if slot.is_none() => *slot = Some(request),
                        _ => {
                            encoder.stop();
                            bail!("encoder input slot was reused too early");
                        }
                    }
                } else {
                    {
                        let sync = layout.access(false)?;
                        let video: &MemoryMappedFrameBuffer<FrameBuffer> = request
                            .buffer(&video_stream)
                            .context("request has no software encoder video buffer")?;
                        let planes = video.data();
                        let frame = crate::encoder::yuv::Yuv420::new(
                            &planes,
                            c.width as usize,
                            c.height as usize,
                            stride as usize,
                        )?;
                        software
                            .as_mut()
                            .context("missing software encoder")?
                            .encode(&frame, timestamp_us, decision.force_keyframe, |packet| {
                                controller.encoded(packet)
                            })?;
                        finish_cpu(sync)?;
                    }
                    requeue(&camera, request)?;
                }
            } else {
                requeue(&camera, request)?;
            }
            continue;
        }
        if held.iter().any(Option::is_some) || controller.has_pending() {
            encoder
                .as_ref()
                .context("software encoder left unmatched frames")?
                .wait(10)?;
        } else {
            match receiver.recv_timeout(Duration::from_millis(50)) {
                Ok(request) => completed.push_back(request),
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => {
                    bail!("camera completion channel disconnected")
                }
            }
        }
    }
    camera.stop().context("stop camera capture")?;
    if let Some(software) = software.as_mut() {
        software.finish();
    }
    if held.iter().any(Option::is_some) || controller.has_pending() {
        let encoder = encoder
            .as_mut()
            .context("software encoder left unmatched frames at shutdown")?;
        // bcm2835-codec lacks VIDIOC_ENCODER_CMD. Stop feeding input and collect
        // every timestamped frame before STREAMOFF instead of issuing unsupported EOS controls.
        let deadline = Instant::now() + Duration::from_secs(3);
        while held.iter().any(Option::is_some) || controller.has_pending() {
            ensure!(
                Instant::now() < deadline,
                "hardware encoder drain timed out"
            );
            encoder.receive(|packet| controller.encoded(packet))?;
            for index in encoder.reclaim()? {
                held.get_mut(index as usize).and_then(Option::take);
            }
            encoder.wait(10)?;
        }
        ensure!(
            !controller.has_pending(),
            "encoder drained without returning every frame"
        );
    }
    if let Some(encoder) = encoder.as_mut() {
        encoder.stop();
    }
    controller.finish()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fixed_focus_and_partial_capabilities_do_not_receive_unsupported_controls() {
        for autofocus in [false, true] {
            for has_lens in [false, true] {
                let focus = FocusControls::select(autofocus, 2.5, false, has_lens);
                let mut list = ControlList::new();
                focus.apply(&mut list).unwrap();
                assert!(list.is_empty());
            }
        }
        let focus = FocusControls::select(false, 2.5, true, false);
        let mut list = ControlList::new();
        focus.apply(&mut list).unwrap();
        assert_eq!(
            list.get::<controls::AfMode>().unwrap(),
            controls::AfMode::Manual
        );
        assert!(!list.contains(controls::LensPosition::ID));
    }

    #[test]
    fn module_3_focus_controls_preserve_autofocus_and_manual_settings() {
        for autofocus in [false, true] {
            let focus = FocusControls::select(autofocus, 2.5, true, true);
            let mut list = ControlList::new();
            focus.apply(&mut list).unwrap();
            assert_eq!(
                list.get::<controls::AfMode>().unwrap(),
                if autofocus {
                    controls::AfMode::Continuous
                } else {
                    controls::AfMode::Manual
                }
            );
            assert_eq!(list.contains(controls::LensPosition::ID), !autofocus);
            if !autofocus {
                assert_eq!(list.get::<controls::LensPosition>().unwrap().0, 2.5);
            }
        }
    }

    #[test]
    fn software_layout_allows_offsets_and_separate_fds_but_rejects_overlap() {
        let planes = [
            (10, Some(4096), 4096),
            (11, Some(128), 1024),
            (11, Some(2048), 1024),
        ];
        let layout = plane_layout(&planes, 6144, false).unwrap();
        assert_eq!(&layout.fds[..layout.fd_count], &[10, 11]);
        assert_eq!(layout.y_len, 4096);
        assert!(plane_layout(&planes, 6144, true).is_err());
        assert!(plane_layout(&[(10, Some(0), 4096), (10, Some(4096), 2048)], 6144, true).is_ok());
        assert!(plane_layout(&[(10, Some(0), 4096), (10, Some(128), 1024)], 0, false).is_err());
        assert!(plane_layout(&[(10, None, 4096)], 0, false).is_err());
        assert!(plane_layout(&[(10, Some(usize::MAX), 2)], 0, false).is_err());
    }

    #[test]
    fn capture_frame_counter_resets_each_second_and_after_clock_changes() {
        let mut frames = FrameCounter::default();
        let start = UNIX_EPOCH + Duration::from_secs(12345);
        for index in 0..15 {
            assert_eq!(
                frames.index(start + Duration::from_millis(index * 66)),
                index as u32
            );
        }
        assert_eq!(frames.index(start + Duration::from_secs(1)), 0);
        assert_eq!(frames.index(start + Duration::from_millis(1100)), 1);
        assert_eq!(frames.index(start + Duration::from_secs(60)), 0);
        assert_eq!(frames.index(start), 0);
        assert_eq!(frames.index(start), 1);
    }

    #[test]
    fn skips_startup_frames_and_rejects_actual_capture_errors() {
        use FrameMetadataStatus::*;
        assert!(!usable_status(Startup).unwrap());
        assert!(usable_status(Success).unwrap());
        assert!(usable_status(Error).is_err());
        assert!(usable_status(Cancelled).is_err());
    }
}
