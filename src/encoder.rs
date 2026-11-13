//! Direct V4L2 memory-to-memory H.264 encoding with DMA-BUF input.
use crate::config::{Camera, EncoderBackend};

#[path = "encoder/software.rs"]
pub mod software;
#[path = "encoder/yuv.rs"]
pub mod yuv;
use anyhow::{Context, Result, bail, ensure};
use std::{
    fs::{self, File, OpenOptions},
    io, mem,
    os::{
        fd::{AsRawFd, RawFd},
        unix::fs::OpenOptionsExt,
    },
    path::{Path, PathBuf},
    ptr,
};
use v4l2_sys_mit as v;

pub const MAX_BUFFERS: usize = 32;

const OUTPUT: u32 = v::v4l2_buf_type_V4L2_BUF_TYPE_VIDEO_OUTPUT_MPLANE;
const CAPTURE: u32 = v::v4l2_buf_type_V4L2_BUF_TYPE_VIDEO_CAPTURE_MPLANE;
const DMABUF: u32 = v::v4l2_memory_V4L2_MEMORY_DMABUF;
const MMAP: u32 = v::v4l2_memory_V4L2_MEMORY_MMAP;
const H264: u32 = u32::from_le_bytes(*b"H264");
const YUV420: u32 = u32::from_le_bytes(*b"YU12");

/// Application quality scale: 0.0 -> QP 51, 1.0 -> QP 1. Lower QP preserves more detail.
fn quality_quantizer(quality: f64) -> i32 {
    (51.0 - quality * 50.0).round() as i32
}

// Linux generic ioctl encoding is shared by the supported x86-64, ARMv7 and AArch64 ABIs.
fn request<T>(direction: u32, kind: u8, number: u8) -> libc::c_ulong {
    ((direction << 30)
        | ((mem::size_of::<T>() as u32) << 16)
        | ((kind as u32) << 8)
        | number as u32) as _
}

fn ioctl<T>(fd: RawFd, direction: u32, number: u8, argument: &mut T) -> io::Result<()> {
    loop {
        // SAFETY: every call below pairs the kernel command with its specified initialized UAPI structure.
        let result =
            unsafe { libc::ioctl(fd, request::<T>(direction, b'V', number), &mut *argument) };
        if result >= 0 {
            return Ok(());
        }
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::Interrupted {
            return Err(error);
        }
    }
}

/// Only called for Linux UAPI C structures whose zero bit pattern is valid.
fn zeroed<T>() -> T {
    // SAFETY: all call sites in this module use POD structures or integer arrays from Linux UAPI.
    unsafe { mem::zeroed() }
}

fn encoder_candidates(sysfs: &Path) -> Result<Vec<PathBuf>> {
    let mut paths = Vec::new();
    let entries = match fs::read_dir(sysfs) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(paths),
        Err(error) => return Err(error).context("enumerate Linux video devices"),
    };
    for entry in entries {
        let entry = entry.context("read Linux video device entry")?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        let Some(number) = name.strip_prefix("video") else {
            continue;
        };
        if number.is_empty() || !number.bytes().all(|b| b.is_ascii_digit()) {
            continue;
        }
        let identity =
            fs::read_to_string(entry.path().join("name")).context("read video device identity")?;
        if identity.trim() == "bcm2835-codec-encode" {
            paths.push(Path::new("/dev").join(name));
        }
    }
    paths.sort();
    Ok(paths)
}

/// Discover before allocating backend-specific buffers. A present but broken
/// hardware device must fail in Encoder::new, never trigger software fallback.
pub fn select_backend(requested: EncoderBackend) -> Result<EncoderBackend> {
    if requested != EncoderBackend::Auto {
        return Ok(requested);
    }
    let present = !encoder_candidates(Path::new("/sys/class/video4linux"))?.is_empty();
    Ok(choose_backend(requested, present))
}

fn choose_backend(requested: EncoderBackend, hardware_present: bool) -> EncoderBackend {
    match requested {
        EncoderBackend::Hardware => EncoderBackend::Hardware,
        EncoderBackend::Auto if hardware_present => EncoderBackend::Hardware,
        EncoderBackend::Auto | EncoderBackend::Software => EncoderBackend::Software,
    }
}

fn hardware_encoder_capabilities(cap: &v::v4l2_capability) -> bool {
    let card = cap.card.split(|b| *b == 0).next().unwrap_or_default();
    let caps = if cap.capabilities & v::V4L2_CAP_DEVICE_CAPS != 0 {
        cap.device_caps
    } else {
        cap.capabilities
    };
    card == b"bcm2835-codec-encode"
        && caps & v::V4L2_CAP_VIDEO_M2M_MPLANE != 0
        && caps & v::V4L2_CAP_STREAMING != 0
}

fn supports_format(fd: RawFd, queue: u32, fourcc: u32) -> Result<bool> {
    for index in 0..256 {
        let mut format: v::v4l2_fmtdesc = zeroed();
        format.index = index;
        format.type_ = queue;
        match ioctl(fd, 3, 2, &mut format) {
            Ok(()) if format.pixelformat == fourcc => return Ok(true),
            Ok(()) => {}
            Err(error) if error.raw_os_error() == Some(libc::EINVAL) => return Ok(false),
            Err(error) => return Err(error).context("enumerate hardware encoder formats"),
        }
    }
    bail!("hardware encoder returned too many video formats")
}

fn validate_encoder(device: &File) -> Result<()> {
    let mut cap: v::v4l2_capability = zeroed();
    ioctl(device.as_raw_fd(), 2, 0, &mut cap).context("query hardware encoder")?;
    ensure!(
        hardware_encoder_capabilities(&cap),
        "device is not a streaming Raspberry Pi hardware encoder"
    );
    ensure!(
        supports_format(device.as_raw_fd(), CAPTURE, H264)?
            && supports_format(device.as_raw_fd(), OUTPUT, YUV420)?,
        "hardware encoder lacks H.264 output or YUV420 input support"
    );
    Ok(())
}

fn detect_encoder() -> Result<(File, PathBuf)> {
    let paths = encoder_candidates(Path::new("/sys/class/video4linux"))?;
    ensure!(
        !paths.is_empty(),
        "Raspberry Pi H.264 encoder not found; check the bcm2835_codec driver"
    );
    let mut errors = Vec::new();
    for path in paths {
        let device = OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(libc::O_NONBLOCK | libc::O_CLOEXEC)
            .open(&path)
            .with_context(|| format!("open {}", path.display()));
        match device.and_then(|device| {
            validate_encoder(&device)?;
            Ok(device)
        }) {
            Ok(device) => return Ok((device, path)),
            Err(error) => errors.push(format!("{}: {error:#}", path.display())),
        }
    }
    bail!(
        "no usable Raspberry Pi H.264 encoder: {}",
        errors.join("; ")
    )
}

struct Mapping {
    address: *mut libc::c_void,
    len: usize,
}

impl Mapping {
    fn new(fd: RawFd, offset: u32, len: usize) -> Result<Self> {
        ensure!(len > 0, "encoder returned an empty capture buffer");
        // SAFETY: the driver supplied this buffer's offset and length; mapping lifetime is owned by Self.
        let address = unsafe {
            libc::mmap(
                ptr::null_mut(),
                len,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                fd,
                offset as libc::off_t,
            )
        };
        ensure!(
            address != libc::MAP_FAILED,
            "map encoded buffer: {}",
            io::Error::last_os_error()
        );
        Ok(Self { address, len })
    }

    fn bytes(&self, offset: usize, len: usize) -> Result<&[u8]> {
        ensure!(
            offset <= len && len <= self.len,
            "invalid encoded buffer bounds"
        );
        // SAFETY: the dequeued buffer belongs to this mapping and is not requeued until the callback returns.
        Ok(unsafe {
            std::slice::from_raw_parts(self.address.cast::<u8>().add(offset), len - offset)
        })
    }
}

impl Drop for Mapping {
    fn drop(&mut self) {
        // SAFETY: this mapping is live and was created with exactly this address and length.
        unsafe {
            libc::munmap(self.address, self.len);
        }
    }
}

pub struct Encoder {
    device: File,
    capture: Vec<Mapping>,
    free: Vec<u32>,
    input_busy: Vec<bool>,
    input_size: u32,
    output_streaming: bool,
    capture_streaming: bool,
}

pub struct Encoded<'a> {
    pub timestamp_us: u64,
    pub data: &'a [u8],
}

impl Encoder {
    pub fn new(config: &Camera, stride: u32, frame_size: u32) -> Result<Self> {
        let (device, path) = detect_encoder()?;
        crate::log(format!(
            "auto-detected hardware H.264 encoder: {}",
            path.display()
        ));
        let mut encoder = Self {
            device,
            capture: Vec::new(),
            free: Vec::new(),
            input_busy: Vec::new(),
            input_size: 0,
            output_streaming: false,
            capture_streaming: false,
        };
        encoder.configure(config, stride, frame_size)?;
        Ok(encoder)
    }

    fn fd(&self) -> RawFd {
        self.device.as_raw_fd()
    }

    fn control(&self, id: u32, value: i32) -> Result<()> {
        let mut control = v::v4l2_control { id, value };
        ioctl(self.fd(), 3, 28, &mut control)
            .with_context(|| format!("set encoder control {id:#x}"))
    }

    fn configure(&mut self, c: &Camera, stride: u32, frame_size: u32) -> Result<()> {
        let qp = quality_quantizer(c.quality);
        self.control(
            v::V4L2_CID_MPEG_VIDEO_BITRATE_MODE,
            v::v4l2_mpeg_video_bitrate_mode_V4L2_MPEG_VIDEO_BITRATE_MODE_VBR as i32,
        )?;
        self.control(v::V4L2_CID_MPEG_VIDEO_H264_MIN_QP, qp)?;
        self.control(v::V4L2_CID_MPEG_VIDEO_H264_MAX_QP, qp)?;
        crate::log(format!(
            "H.264 quality {:.3}: fixed quantizer QP {qp}",
            c.quality
        ));
        self.control(
            v::V4L2_CID_MPEG_VIDEO_H264_PROFILE,
            v::v4l2_mpeg_video_h264_profile_V4L2_MPEG_VIDEO_H264_PROFILE_HIGH as i32,
        )?;
        self.control(
            v::V4L2_CID_MPEG_VIDEO_H264_LEVEL,
            v::v4l2_mpeg_video_h264_level_V4L2_MPEG_VIDEO_H264_LEVEL_4_2 as i32,
        )?;
        self.control(v::V4L2_CID_MPEG_VIDEO_H264_I_PERIOD, c.framerate as i32)?;
        self.control(v::V4L2_CID_MPEG_VIDEO_REPEAT_SEQ_HEADER, 1)?;
        // bcm2835-codec does not expose B-frame or HEADER_MODE controls.
        // Its normal H.264 mode has no B-frames; REPEAT_SEQ_HEADER supplies IDR headers.

        let mut format: v::v4l2_format = zeroed();
        format.type_ = CAPTURE;
        // SAFETY: pix_mp is the union member specified by the MPLANE buffer type.
        unsafe {
            let p = &mut format.fmt.pix_mp;
            p.width = c.width;
            p.height = c.height;
            p.pixelformat = H264;
            p.num_planes = 1;
            // Fixed-quality IDRs can be much larger than bitrate-controlled frames.
            // Reserve bounded space for two raw frames, with a 1 MiB minimum.
            p.plane_fmt[0].sizeimage = frame_size.saturating_mul(2).max(1024 * 1024);
        }
        ioctl(self.fd(), 3, 5, &mut format).context("set hardware H.264 format")?;
        format = zeroed();
        format.type_ = OUTPUT;
        // SAFETY: same union discriminator as above.
        unsafe {
            let p = &mut format.fmt.pix_mp;
            p.width = c.width;
            p.height = c.height;
            p.pixelformat = YUV420;
            p.field = v::v4l2_field_V4L2_FIELD_NONE;
            p.colorspace = v::v4l2_colorspace_V4L2_COLORSPACE_REC709;
            p.num_planes = 1;
            p.plane_fmt[0].bytesperline = stride;
            p.plane_fmt[0].sizeimage = frame_size;
        }
        ioctl(self.fd(), 3, 5, &mut format).context("set encoder DMA-BUF input format")?;
        // SAFETY: successful S_FMT initialized pix_mp for the requested buffer type.
        let input = unsafe { format.fmt.pix_mp };
        ensure!(
            input.width == c.width
                && input.height == c.height
                && input.pixelformat == YUV420
                && input.num_planes == 1
                && input.plane_fmt[0].bytesperline == stride
                && input.plane_fmt[0].sizeimage <= frame_size,
            "encoder adjusted the camera DMA-BUF layout incompatibly"
        );
        self.input_size = frame_size;
        // Changing OUTPUT can affect CAPTURE; verify the negotiated codec afterwards.
        format = zeroed();
        format.type_ = CAPTURE;
        ioctl(self.fd(), 3, 4, &mut format).context("read negotiated encoder format")?;
        // SAFETY: the CAPTURE MPLANE format is initialized by G_FMT.
        let capture = unsafe { format.fmt.pix_mp };
        ensure!(
            capture.pixelformat == H264 && capture.num_planes == 1,
            "hardware encoder did not negotiate single-plane H.264 output"
        );
        let mut timing: v::v4l2_streamparm = zeroed();
        timing.type_ = OUTPUT;
        timing.parm.output.timeperframe = v::v4l2_fract {
            numerator: 1,
            denominator: c.framerate,
        };
        ioctl(self.fd(), 3, 22, &mut timing).context("set encoder frame rate")?;

        let mut buffers: v::v4l2_requestbuffers = zeroed();
        buffers.type_ = OUTPUT;
        buffers.memory = DMABUF;
        buffers.count = 6;
        ioctl(self.fd(), 3, 8, &mut buffers).context("allocate encoder input slots")?;
        ensure!(
            buffers.count > 0 && buffers.count <= MAX_BUFFERS as u32,
            "invalid encoder input slot count"
        );
        self.free = (0..buffers.count).rev().collect();
        self.input_busy = vec![false; buffers.count as usize];
        buffers = zeroed();
        buffers.type_ = CAPTURE;
        buffers.memory = MMAP;
        buffers.count = 6;
        ioctl(self.fd(), 3, 8, &mut buffers).context("allocate encoded output buffers")?;
        ensure!(
            buffers.count > 0 && buffers.count <= MAX_BUFFERS as u32,
            "invalid encoder capture buffer count"
        );
        for index in 0..buffers.count {
            let (mut buffer, mut plane) = buffer(CAPTURE, MMAP, index);
            buffer.m.planes = &mut plane;
            ioctl(self.fd(), 3, 9, &mut buffer).context("query encoded output buffer")?;
            // SAFETY: QUERYBUF initializes mem_offset for MMAP memory.
            let mapping = Mapping::new(
                self.fd(),
                unsafe { plane.m.mem_offset },
                plane.length as usize,
            )?;
            self.capture.push(mapping);
            self.queue_capture(index)?;
        }
        let mut queue = CAPTURE;
        ioctl(self.fd(), 1, 18, &mut queue).context("start encoded output queue")?;
        self.capture_streaming = true;
        queue = OUTPUT;
        ioctl(self.fd(), 1, 18, &mut queue).context("start encoder input queue")?;
        self.output_streaming = true;
        Ok(())
    }

    fn queue_capture(&self, index: u32) -> Result<()> {
        let (mut buffer, mut plane) = buffer(CAPTURE, MMAP, index);
        plane.length = self.capture[index as usize].len as u32;
        buffer.m.planes = &mut plane;
        ioctl(self.fd(), 3, 15, &mut buffer).context("queue encoded output buffer")
    }

    pub fn force_keyframe(&self) -> Result<()> {
        self.control(v::V4L2_CID_MPEG_VIDEO_FORCE_KEY_FRAME, 1)
    }
    pub fn has_slot(&self) -> bool {
        !self.free.is_empty()
    }

    /// The caller must retain the camera request until this index is returned by reclaim().
    pub fn submit(&mut self, fd: RawFd, len: usize, timestamp_us: u64) -> Result<u32> {
        ensure!(
            len >= self.input_size as usize,
            "camera DMA-BUF is shorter than encoder input"
        );
        let index = self.free.pop().context("encoder input queue is full")?;
        let (mut buffer, mut plane) = buffer(OUTPUT, DMABUF, index);
        plane.m.fd = fd;
        plane.bytesused = self.input_size;
        plane.length = len.try_into().context("camera DMA-BUF too large")?;
        buffer.timestamp.tv_sec = (timestamp_us / 1_000_000)
            .try_into()
            .context("frame timestamp overflow")?;
        buffer.timestamp.tv_usec = (timestamp_us % 1_000_000) as _;
        buffer.m.planes = &mut plane;
        if let Err(error) = ioctl(self.fd(), 3, 15, &mut buffer) {
            self.free.push(index);
            return Err(error).context("queue camera DMA-BUF for encoding");
        }
        self.input_busy[index as usize] = true;
        Ok(index)
    }

    pub fn reclaim(&mut self) -> Result<impl Iterator<Item = u32> + use<>> {
        // No inputs are submitted during this drain, so each of at most 32 busy
        // slots can appear once. Validation rejects duplicate/invalid indices.
        let mut completed = [0; MAX_BUFFERS];
        let mut count = 0;
        loop {
            let (mut buffer, mut plane) = buffer(OUTPUT, DMABUF, 0);
            buffer.m.planes = &mut plane;
            match ioctl(self.fd(), 3, 17, &mut buffer) {
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
                Err(error) => return Err(error).context("dequeue encoder input"),
                Ok(()) => {}
            }
            ensure!(
                buffer.index < self.input_busy.len() as u32
                    && self.input_busy[buffer.index as usize],
                "encoder returned an invalid input slot"
            );
            ensure!(
                buffer.flags & v::V4L2_BUF_FLAG_ERROR == 0,
                "encoder reported a bad input frame"
            );
            self.input_busy[buffer.index as usize] = false;
            self.free.push(buffer.index);
            completed[count] = buffer.index;
            count += 1;
        }
        Ok(completed.into_iter().take(count))
    }

    pub fn receive(&mut self, mut callback: impl FnMut(Encoded<'_>) -> Result<()>) -> Result<()> {
        loop {
            let (mut buffer, mut plane) = buffer(CAPTURE, MMAP, 0);
            buffer.m.planes = &mut plane;
            match ioctl(self.fd(), 3, 17, &mut buffer) {
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
                Err(error) => return Err(error).context("dequeue encoded frame"),
                Ok(()) => {}
            }
            ensure!(
                buffer.flags & v::V4L2_BUF_FLAG_ERROR == 0,
                "hardware encoder reported an output error"
            );
            let mapping = self
                .capture
                .get(buffer.index as usize)
                .context("invalid encoded buffer index")?;
            ensure!(
                buffer.timestamp.tv_sec >= 0 && buffer.timestamp.tv_usec >= 0,
                "invalid encoder timestamp"
            );
            let timestamp_us =
                buffer.timestamp.tv_sec as u64 * 1_000_000 + buffer.timestamp.tv_usec as u64;
            if plane.bytesused > 0 {
                callback(Encoded {
                    timestamp_us,
                    data: mapping.bytes(plane.data_offset as usize, plane.bytesused as usize)?,
                })?;
            }
            ensure!(
                buffer.flags & v::V4L2_BUF_FLAG_LAST == 0,
                "hardware encoder stopped unexpectedly"
            );
            self.queue_capture(buffer.index)?;
        }
        Ok(())
    }

    pub fn wait(&self, timeout_ms: i32) -> Result<()> {
        let mut pfd = libc::pollfd {
            fd: self.fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: poll receives one initialized, writable descriptor entry.
        let status = unsafe { libc::poll(&mut pfd, 1, timeout_ms) };
        if status < 0 && io::Error::last_os_error().kind() != io::ErrorKind::Interrupted {
            bail!("poll encoder: {}", io::Error::last_os_error());
        }
        ensure!(
            pfd.revents & (libc::POLLNVAL | libc::POLLHUP) == 0,
            "hardware encoder disconnected"
        );
        Ok(())
    }

    pub fn stop(&mut self) {
        if self.output_streaming {
            let mut queue = OUTPUT;
            let _ = ioctl(self.fd(), 1, 19, &mut queue);
            self.output_streaming = false;
        }
        if self.capture_streaming {
            let mut queue = CAPTURE;
            let _ = ioctl(self.fd(), 1, 19, &mut queue);
            self.capture_streaming = false;
        }
    }
}

impl Drop for Encoder {
    fn drop(&mut self) {
        self.stop();
    } // Release driver references before the caller drops camera requests.
}

fn buffer(kind: u32, memory: u32, index: u32) -> (v::v4l2_buffer, v::v4l2_plane) {
    let mut b: v::v4l2_buffer = zeroed();
    b.type_ = kind;
    b.memory = memory;
    b.index = index;
    b.field = v::v4l2_field_V4L2_FIELD_NONE;
    b.length = 1;
    // The caller attaches the plane pointer after returning so it points at its final stack location.
    (b, zeroed())
}

/// DMA-BUF cache synchronization for CPU reads or read/write overlay access.
pub struct DmaAccess {
    fd: RawFd,
    flags: u64,
    active: bool,
}

impl DmaAccess {
    fn sync(fd: RawFd, flags: u64) -> io::Result<()> {
        let mut flags = flags;
        loop {
            // SAFETY: DMA_BUF_IOCTL_SYNC expects a pointer to one u64 flags field.
            if unsafe { libc::ioctl(fd, request::<u64>(1, b'b', 0), &mut flags) } == 0 {
                return Ok(());
            }
            let error = io::Error::last_os_error();
            if !matches!(error.raw_os_error(), Some(libc::EINTR | libc::EAGAIN)) {
                return Err(error);
            }
        }
    }

    pub fn read(fd: RawFd) -> Result<Self> {
        Self::begin(fd, 1)
    }
    pub fn read_write(fd: RawFd) -> Result<Self> {
        Self::begin(fd, 3)
    }
    fn begin(fd: RawFd, flags: u64) -> Result<Self> {
        Self::sync(fd, flags).context("begin CPU access to camera DMA-BUF")?;
        Ok(Self {
            fd,
            flags,
            active: true,
        })
    }

    pub fn finish(mut self) -> Result<()> {
        Self::sync(self.fd, self.flags | 4).context("end CPU access to camera DMA-BUF")?;
        self.active = false;
        Ok(())
    }
}

impl Drop for DmaAccess {
    fn drop(&mut self) {
        if self.active {
            let _ = Self::sync(self.fd, self.flags | 4);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        fs,
        time::{SystemTime, UNIX_EPOCH},
    };

    #[test]
    fn backend_selection_prefers_hardware_and_always_supports_software() {
        use EncoderBackend::*;
        for (mode, hardware_present, expected) in [
            (Auto, true, Hardware),
            (Auto, false, Software),
            (Hardware, true, Hardware),
            (Hardware, false, Hardware),
            (Software, true, Software),
            (Software, false, Software),
        ] {
            assert_eq!(choose_backend(mode, hardware_present), expected);
        }
        assert!(
            encoder_candidates(Path::new("/a/nonexistent/video/sysfs/path"))
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn quality_maps_monotonically_to_the_quantizer() {
        assert_eq!(quality_quantizer(0.0), 51);
        assert_eq!(quality_quantizer(0.6), 21);
        assert_eq!(quality_quantizer(0.8), 11);
        assert_eq!(quality_quantizer(1.0), 1);
        let mut previous = 51;
        for step in 0..=1000 {
            let qp = quality_quantizer(f64::from(step) / 1000.0);
            assert!((1..=51).contains(&qp) && qp <= previous);
            previous = qp;
        }
    }

    #[test]
    fn refuses_a_device_without_the_hardware_encoder_interface() {
        let error = validate_encoder(&File::open("/dev/null").unwrap()).unwrap_err();
        assert!(format!("{error:#}").contains("query hardware encoder"));
    }

    #[test]
    fn discovers_by_identity_instead_of_device_number() {
        let root = std::env::temp_dir().join(format!(
            "pivigil-device-scan-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        for (node, identity) in [
            ("video2", "unicam-image"),
            ("video11", "bcm2835-codec-decode"),
            ("video31", "bcm2835-codec-encode_image"),
            ("video42", "bcm2835-codec-encode"),
            ("v4l-subdev0", "bcm2835-codec-encode"),
        ] {
            fs::create_dir_all(root.join(node)).unwrap();
            fs::write(root.join(node).join("name"), format!("{identity}\n")).unwrap();
        }
        assert_eq!(
            encoder_candidates(&root).unwrap(),
            vec![PathBuf::from("/dev/video42")]
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn excludes_wrong_codec_devices_and_incompatible_capabilities() {
        let mut cap: v::v4l2_capability = zeroed();
        let encoder_name = b"bcm2835-codec-encode";
        cap.card[..encoder_name.len()].copy_from_slice(encoder_name);
        cap.capabilities = v::V4L2_CAP_DEVICE_CAPS;
        cap.device_caps = v::V4L2_CAP_VIDEO_M2M_MPLANE | v::V4L2_CAP_STREAMING;
        assert!(hardware_encoder_capabilities(&cap));
        cap.device_caps = v::V4L2_CAP_VIDEO_M2M_MPLANE;
        assert!(!hardware_encoder_capabilities(&cap));
        cap.device_caps |= v::V4L2_CAP_STREAMING;
        cap.card = [0; 32];
        let decoder_name = b"bcm2835-codec-decode";
        cap.card[..decoder_name.len()].copy_from_slice(decoder_name);
        assert!(!hardware_encoder_capabilities(&cap));
    }

    #[test]
    fn mapped_capture_checks_data_offset_and_bytes_used() {
        let path = std::env::temp_dir().join(format!(
            "pivigil-mmap-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(&path)
            .unwrap();
        file.set_len(4096).unwrap();
        fs::write(&path, vec![0x65; 4096]).unwrap();
        let mapping = Mapping::new(file.as_raw_fd(), 0, 4096).unwrap();
        assert_eq!(mapping.bytes(2, 7).unwrap(), &[0x65; 5]);
        assert!(mapping.bytes(7, 2).is_err());
        assert!(mapping.bytes(0, 4097).is_err());
        drop(mapping);
        fs::remove_file(path).unwrap();
    }
}
