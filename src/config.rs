use anyhow::{Context, Result, ensure};
use serde::Deserialize;
use std::{
    ffi::CString,
    fs,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

#[cfg_attr(test, derive(serde::Serialize))]
#[derive(Debug, Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub camera: Camera,
    pub motion: Motion,
    pub recording: Recording,
    pub overlay: Overlay,
    pub stream: Stream,
}

#[cfg_attr(test, derive(serde::Serialize))]
#[derive(Debug, Deserialize, Clone)]
#[serde(default, deny_unknown_fields)]
pub struct Camera {
    pub encoder: EncoderBackend,
    pub index: u32,
    pub autofocus: bool,
    pub lens_position: f32,
    pub width: u32,
    pub height: u32,
    pub framerate: u32,
    pub quality: f64,
    pub warmup_seconds: u64,
    pub restart_seconds: u64,
    pub startup_timeout_seconds: u64,
    pub frame_timeout_seconds: u64,
}

impl Default for Camera {
    fn default() -> Self {
        Self {
            encoder: EncoderBackend::Auto,
            index: 0,
            autofocus: false,
            lens_position: 0.0,
            width: 1280,
            height: 720,
            framerate: 15,
            quality: 0.6,
            warmup_seconds: 3,
            restart_seconds: 5,
            startup_timeout_seconds: 30,
            frame_timeout_seconds: 10,
        }
    }
}

impl Camera {
    pub fn validate_dimensions(&self) -> Result<()> {
        // AVC sample entries store each dimension in an unsigned 16-bit field.
        // Camera and encoder capabilities are negotiated when capture starts.
        for (name, dimension) in [("width", self.width), ("height", self.height)] {
            ensure!(
                dimension >= 64 && dimension.is_multiple_of(2),
                "camera.{name} must be even and >=64"
            );
            ensure!(
                u16::try_from(dimension).is_ok(),
                "camera.{name} exceeds the MP4 dimension field"
            );
        }
        // libcamera uses a u32 frame size; Rust slices must also fit in isize.
        let bytes = u64::from(self.width) * u64::from(self.height) * 3 / 2;
        ensure!(
            bytes <= u64::from(u32::MAX) && bytes <= isize::MAX as u64,
            "camera YUV420 frame size exceeds the platform buffer range"
        );
        Ok(())
    }
}

#[cfg_attr(test, derive(serde::Serialize))]
#[derive(Debug, Deserialize, Clone, Copy, Default, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum EncoderBackend {
    #[default]
    Auto,
    Hardware,
    Software,
}

#[cfg_attr(test, derive(serde::Serialize))]
#[derive(Debug, Deserialize, Clone)]
#[serde(default, deny_unknown_fields)]
pub struct Motion {
    pub enabled: bool,
    pub pixel_difference_threshold: f64,
    pub region_threshold: f64,
    pub frame_period: u32,
}

impl Default for Motion {
    fn default() -> Self {
        Self {
            enabled: true,
            pixel_difference_threshold: 0.05,
            region_threshold: 0.005,
            frame_period: 3,
        }
    }
}

#[cfg_attr(test, derive(serde::Serialize))]
#[derive(Debug, Deserialize, Clone)]
#[serde(default, deny_unknown_fields)]
pub struct Recording {
    pub enabled: bool,
    pub output_dir: PathBuf,
    pub filename_format: String,
    pub pre_motion_seconds: u64,
    pub pre_motion_buffer_mb: usize,
    pub backlog_buffer_mb: usize,
    pub post_motion_seconds: u64,
    pub max_file_seconds: u64,
    pub min_free_mb: u64,
}

impl Default for Recording {
    fn default() -> Self {
        Self {
            enabled: true,
            output_dir: "/var/lib/pivigil".into(),
            filename_format: DEFAULT_FILENAME_FORMAT.into(),
            pre_motion_seconds: 2,
            pre_motion_buffer_mb: 64,
            backlog_buffer_mb: 64,
            post_motion_seconds: 4,
            max_file_seconds: 300,
            min_free_mb: 256,
        }
    }
}

pub const DEFAULT_FILENAME_FORMAT: &str = "%Y-%m-%d_%H-%M-%S.mp4";

/// A strftime filename, confined to a single directory entry. Validate again
/// on every rotation because expansion depends on capture time and timezone.
pub fn recording_filename(format: &str, captured_at: SystemTime) -> Result<String> {
    ensure!(
        !format.is_empty() && format.len() <= 512,
        "recording.filename_format must be 1..512 bytes"
    );
    let format = CString::new(format).context("recording.filename_format contains NUL")?;
    let seconds: libc::time_t = captured_at
        .duration_since(UNIX_EPOCH)?
        .as_secs()
        .try_into()
        .context("recording datetime exceeds system time range")?;
    let mut local = std::mem::MaybeUninit::<libc::tm>::uninit();
    // SAFETY: the pointers are valid and localtime_r initializes local on success.
    ensure!(
        !unsafe { libc::localtime_r(&seconds, local.as_mut_ptr()) }.is_null(),
        "convert recording capture datetime"
    );
    let mut output = [0u8; 4096];
    // SAFETY: local is initialized; output is writable and format is NUL-terminated.
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
        "recording.filename_format produced empty or oversized text"
    );
    let name = std::str::from_utf8(&output[..len])?;
    ensure!(
        name.len() <= 250
            && !name.trim().is_empty()
            && name != "."
            && name != ".."
            && !name.contains(['/', '\\'])
            && !name.chars().any(char::is_control),
        "recording.filename_format must produce one filename of at most 250 bytes, without path separators or control characters"
    );
    Ok(name.to_owned())
}

#[cfg_attr(test, derive(serde::Serialize))]
#[derive(Debug, Deserialize, Clone, Copy, Default, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum OverlayPosition {
    #[default]
    TopLeft,
    TopRight,
    BottomLeft,
    BottomRight,
}

#[cfg_attr(test, derive(serde::Serialize))]
#[derive(Debug, Deserialize, Clone)]
#[serde(default, deny_unknown_fields)]
pub struct Overlay {
    pub enabled: bool,
    pub text: String,
    pub datetime_format: String,
    pub position: OverlayPosition,
    pub font_size: u32,
    pub margin: u32,
    pub background_opacity: f64,
}

impl Default for Overlay {
    fn default() -> Self {
        Self {
            enabled: true,
            text: String::new(),
            datetime_format: "%Y-%m-%d %H:%M:%S.{frame}".into(),
            position: OverlayPosition::TopLeft,
            font_size: 12,
            margin: 12,
            background_opacity: 0.0,
        }
    }
}

#[cfg_attr(test, derive(serde::Serialize))]
#[derive(Debug, Deserialize, Clone)]
#[serde(default, deny_unknown_fields)]
pub struct Stream {
    pub enabled: bool,
    pub listen: std::net::SocketAddr,
    pub max_clients: usize,
    pub max_buffer_mb: usize,
}

impl Default for Stream {
    fn default() -> Self {
        Self {
            enabled: true,
            listen: "0.0.0.0:8080".parse().unwrap(),
            max_clients: 2,
            max_buffer_mb: 8,
        }
    }
}

impl Config {
    pub fn load(path: &Path) -> Result<Self> {
        let text = fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
        let config: Self = toml::from_str(&text).context("parse configuration")?;
        config.validate()?;
        Ok(config)
    }

    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.recording.output_dir.is_absolute(),
            "recording.output_dir must be absolute"
        );
        recording_filename(&self.recording.filename_format, UNIX_EPOCH)?;
        let c = &self.camera;
        ensure!(
            c.lens_position.is_finite() && c.lens_position >= 0.0,
            "camera.lens_position must be finite and >=0 dioptres"
        );
        c.validate_dimensions()?;
        ensure!(
            (1..=30).contains(&c.framerate),
            "camera.framerate must be 1..30"
        );
        ensure!(
            c.quality.is_finite() && (0.0..=1.0).contains(&c.quality),
            "camera.quality must be a finite fraction from 0.0 to 1.0"
        );
        ensure!(
            c.warmup_seconds <= 300,
            "camera.warmup_seconds must be <=300"
        );
        ensure!(
            (1..=300).contains(&c.restart_seconds),
            "camera.restart_seconds must be 1..300"
        );
        ensure!(
            (5..=300).contains(&c.startup_timeout_seconds),
            "camera.startup_timeout_seconds must be 5..300"
        );
        ensure!(
            (2..=300).contains(&c.frame_timeout_seconds),
            "camera.frame_timeout_seconds must be 2..300"
        );
        let m = &self.motion;
        ensure!(
            m.pixel_difference_threshold.is_finite()
                && (0.0..=1.0).contains(&m.pixel_difference_threshold),
            "motion.pixel_difference_threshold must be a finite fraction from 0.0 to 1.0"
        );
        ensure!(
            m.region_threshold.is_finite() && m.region_threshold > 0.0 && m.region_threshold <= 1.0,
            "motion.region_threshold must be >0 and <=1"
        );
        ensure!(
            (1..=c.framerate * 10).contains(&m.frame_period),
            "motion.frame_period must be 1..framerate*10"
        );
        let sampled_pixels = f64::from(c.width) * f64::from(c.height);
        ensure!(
            sampled_pixels * m.region_threshold >= 1.0,
            "motion.region_threshold must correspond to at least one image pixel"
        );
        let r = &self.recording;
        ensure!(
            r.pre_motion_seconds <= 30,
            "recording.pre_motion_seconds must be 0..30"
        );
        ensure!(
            r.pre_motion_buffer_mb > 0
                && r.pre_motion_buffer_mb
                    .checked_mul(1024 * 1024)
                    .is_some_and(|bytes| bytes <= isize::MAX as usize),
            "recording.pre_motion_buffer_mb must be positive and fit in the platform buffer range"
        );
        ensure!(
            r.backlog_buffer_mb > 0
                && r.backlog_buffer_mb
                    .checked_mul(1024 * 1024)
                    .is_some_and(|bytes| bytes <= isize::MAX as usize),
            "recording.backlog_buffer_mb must be positive and fit in the platform buffer range"
        );
        ensure!(
            !r.enabled
                || !m.enabled
                || r.pre_motion_seconds == 0
                || r.backlog_buffer_mb >= r.pre_motion_buffer_mb,
            "recording.backlog_buffer_mb must be >= recording.pre_motion_buffer_mb when pre-motion recording is enabled"
        );
        ensure!(
            (1..=3600).contains(&r.post_motion_seconds),
            "recording.post_motion_seconds must be 1..3600"
        );
        ensure!(
            (1..=86400).contains(&r.max_file_seconds),
            "recording.max_file_seconds must be 1..86400"
        );
        ensure!(
            (1..=1_000_000).contains(&r.min_free_mb),
            "recording.min_free_mb must be 1..1000000"
        );
        let o = &self.overlay;
        ensure!(
            (8..=72).contains(&o.font_size),
            "overlay.font_size must be 8..72 pixels"
        );
        ensure!(o.margin <= 128, "overlay.margin must be <=128 pixels");
        ensure!(
            o.background_opacity.is_finite() && (0.0..=1.0).contains(&o.background_opacity),
            "overlay.background_opacity must be a finite fraction from 0.0 to 1.0"
        );
        ensure!(
            o.text.chars().count() <= 256,
            "overlay.text must be <=256 characters"
        );
        ensure!(
            o.text.chars().all(|ch| !ch.is_control() || ch == '\n'),
            "overlay.text permits only newlines as control characters"
        );
        ensure!(
            o.datetime_format.len() <= 512 && !o.datetime_format.contains('\0'),
            "overlay.datetime_format must be <=512 bytes and contain no NUL"
        );
        if o.enabled {
            ensure!(
                !o.text.trim().is_empty() || !o.datetime_format.is_empty(),
                "enabled overlay requires text or a datetime format"
            );
            ensure!(
                o.margin * 2 + 8 < c.width && o.margin * 2 + 8 < c.height,
                "overlay margin leaves no room for text in the camera frame"
            );
        }
        ensure!(
            (1..=8).contains(&self.stream.max_clients),
            "stream.max_clients must be 1..8"
        );
        ensure!(
            (8..=64).contains(&self.stream.max_buffer_mb),
            "stream.max_buffer_mb must be 8..64 MiB"
        );
        ensure!(
            self.stream.listen.port() != 0 || !self.stream.enabled,
            "enabled stream requires a nonzero port"
        );
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pre_motion_memory_budget_defaults_and_validates_platform_limits() {
        let omitted: Config = toml::from_str("[recording]\npre_motion_seconds=3").unwrap();
        assert_eq!(omitted.recording.pre_motion_buffer_mb, 64);
        omitted.validate().unwrap();
        for mb in [1, 64, 128, isize::MAX as usize / (1024 * 1024)] {
            let config: Config = toml::from_str(&format!(
                "[recording]\npre_motion_buffer_mb={mb}\nbacklog_buffer_mb={mb}"
            ))
            .unwrap();
            assert_eq!(config.recording.pre_motion_buffer_mb, mb);
            config.validate().unwrap();
        }
        for mb in [0, isize::MAX as usize / (1024 * 1024) + 1, usize::MAX] {
            let mut config = Config::default();
            config.recording.pre_motion_buffer_mb = mb;
            let error = config.validate().unwrap_err();
            assert!(error.to_string().contains("recording.pre_motion_buffer_mb"));
        }
        assert!(toml::from_str::<Config>("[recording]\npre_motion_buffer_mb=-1").is_err());
        assert!(toml::from_str::<Config>("[recording]\npre_motion_buffer_mb=1.5").is_err());
    }

    #[test]
    fn backlog_memory_budget_defaults_and_validates_platform_limits() {
        let omitted: Config = toml::from_str("[recording]").unwrap();
        assert_eq!(omitted.recording.backlog_buffer_mb, 64);
        omitted.validate().unwrap();
        for mb in [1, 64, 128, isize::MAX as usize / (1024 * 1024)] {
            let config: Config = toml::from_str(&format!(
                "[recording]\npre_motion_buffer_mb=1\nbacklog_buffer_mb={mb}"
            ))
            .unwrap();
            assert_eq!(config.recording.backlog_buffer_mb, mb);
            config.validate().unwrap();
        }
        for mb in [0, isize::MAX as usize / (1024 * 1024) + 1, usize::MAX] {
            let mut config = Config::default();
            config.recording.backlog_buffer_mb = mb;
            let error = config.validate().unwrap_err();
            assert!(error.to_string().contains("recording.backlog_buffer_mb"));
        }
        assert!(toml::from_str::<Config>("[recording]\nbacklog_buffer_mb=-1").is_err());
        assert!(toml::from_str::<Config>("[recording]\nbacklog_buffer_mb=1.5").is_err());
    }

    #[test]
    fn backlog_must_accommodate_active_pre_motion_history() {
        let mut config = Config::default();
        config.recording.pre_motion_buffer_mb = 128;
        let error = config.validate().unwrap_err();
        assert!(error.to_string().contains("recording.backlog_buffer_mb"));
        config.recording.backlog_buffer_mb = 128;
        config.validate().unwrap();
        config.recording.backlog_buffer_mb = 129;
        config.validate().unwrap();
        config.recording.backlog_buffer_mb = 1;
        config.recording.pre_motion_seconds = 0;
        config.validate().unwrap();
        config.recording.pre_motion_seconds = 2;
        config.motion.enabled = false;
        config.validate().unwrap();
        config.motion.enabled = true;
        config.recording.enabled = false;
        config.validate().unwrap();
    }

    #[test]
    fn recording_options_and_filename_validation() {
        let mut c: Config =
            toml::from_str("[recording]\nenabled=false\noutput_dir='/dev/null/unavailable'")
                .unwrap();
        assert!(!c.recording.enabled);
        assert_eq!(c.recording.filename_format, DEFAULT_FILENAME_FORMAT);
        c.validate().unwrap();
        assert!(Recording::default().enabled);
        assert!(toml::from_str::<Config>("output_dir='/tmp'").is_err());
        let at = UNIX_EPOCH + std::time::Duration::from_secs(1_794_343_684);
        let default_name = recording_filename(DEFAULT_FILENAME_FORMAT, at).unwrap();
        assert_eq!(default_name.len(), 23);
        assert!(default_name.ends_with(".mp4"));
        for format in [
            "camera-%Y%m%d-%H%M%S.mp4",
            "Zażółć-%%-%s.mp4",
            "constant.mp4",
        ] {
            assert!(recording_filename(format, at).is_ok());
        }
        for format in [
            "",
            ".",
            "..",
            "../camera.mp4",
            "/tmp/camera.mp4",
            "sub/camera.mp4",
            "sub\\camera.mp4",
            "%D.mp4",
            "%n.mp4",
            "bad\0.mp4",
            " ",
            "%99999Y",
        ] {
            c.recording.filename_format = format.into();
            assert!(c.validate().is_err(), "accepted {format:?}");
        }
        assert!(recording_filename(&"a".repeat(251), at).is_err());
        c.recording.filename_format = DEFAULT_FILENAME_FORMAT.into();
        c.recording.output_dir = "relative".into();
        assert!(c.validate().is_err());
    }

    #[test]
    fn motion_detection_defaults_to_enabled_and_can_be_disabled() {
        assert!(Motion::default().enabled);
        let omitted: Config = toml::from_str("[motion]\nframe_period=3").unwrap();
        assert!(omitted.motion.enabled);
        let continuous: Config = toml::from_str("[motion]\nenabled=false").unwrap();
        assert!(!continuous.motion.enabled);
        continuous.validate().unwrap();
        assert!(toml::from_str::<Config>("[motion]\nenabled='false'").is_err());
    }

    #[test]
    fn encoder_selection_defaults_and_explicit_backends() {
        let mut c: Config = toml::from_str("[camera]\nindex=0").unwrap();
        assert_eq!(c.camera.encoder, EncoderBackend::Auto);
        c.validate().unwrap();
        c.camera.encoder = EncoderBackend::Hardware;
        c.validate().unwrap();
        c.camera.encoder = EncoderBackend::Software;
        c.validate().unwrap();
        assert!(toml::from_str::<Config>("[camera]\nencoder='unknown'").is_err());
    }

    #[test]
    fn stream_defaults_are_optional_and_limits_are_validated() {
        let mut config = Config::default();
        assert!(config.stream.enabled);
        assert!(config.stream.listen.ip().is_unspecified());
        config.stream.enabled = true;
        config.validate().unwrap();
        config.stream.max_clients = 0;
        assert!(config.validate().is_err());
        config.stream = Stream::default();
        config.stream.max_buffer_mb = 7;
        assert!(config.validate().is_err());
        assert!(toml::from_str::<Config>("[stream]\nlisten='invalid'").is_err());
        assert!(toml::from_str::<Config>("[stream]\nmax_cliens=2").is_err());
    }

    #[test]
    fn overlay_defaults_and_validation() {
        let mut config = Config::default();
        assert!(config.overlay.enabled);
        assert_eq!(config.overlay.background_opacity, 0.0);
        for opacity in [0.0, 0.5, 1.0] {
            config.overlay.background_opacity = opacity;
            config.validate().unwrap();
        }
        for opacity in [-0.01, 1.01, f64::NAN, f64::INFINITY] {
            config.overlay.background_opacity = opacity;
            assert!(config.validate().is_err());
        }
        config.overlay.background_opacity = 0.0;
        config.overlay.enabled = true;
        config.overlay.text = "Camera – wejście".into();
        config.validate().unwrap();
        config.overlay.datetime_format.clear(); // text-only is supported.
        config.validate().unwrap();
        config.overlay.text.clear();
        assert!(config.validate().is_err());
        config.overlay = Overlay {
            enabled: true,
            ..Overlay::default()
        }; // datetime-only.
        config.validate().unwrap();
        config.overlay.font_size = 0;
        assert!(config.validate().is_err());
        config.overlay = Overlay::default();
        config.overlay.text = "bad\0label".into();
        assert!(config.validate().is_err());
        config.overlay = Overlay::default();
        config.overlay.datetime_format = "bad\0format".into();
        assert!(config.validate().is_err());
        assert!(toml::from_str::<Config>("[overlay]\nposition = 'somewhere'").is_err());
        assert!(toml::from_str::<Config>("[overlay]\nfont_sise = 24").is_err());
    }

    #[test]
    fn quality_is_a_finite_fraction_and_rejects_unknown_fields() {
        let mut config = Config::default();
        for quality in [0.0, 0.6, 1.0] {
            config.camera.quality = quality;
            config.validate().unwrap();
        }
        for quality in [-0.01, 1.01, f64::NAN, f64::INFINITY] {
            config.camera.quality = quality;
            assert!(config.validate().is_err());
        }
        assert!(toml::from_str::<Config>("[camera]\nqualitty = 0.6").is_err());
    }

    #[test]
    fn pixel_difference_threshold_is_a_finite_fraction_and_rejects_unknown_fields() {
        let mut config = Config::default();
        assert_eq!(config.motion.pixel_difference_threshold, 0.05);
        for threshold in [0.0, 0.05, 1.0] {
            config.motion.pixel_difference_threshold = threshold;
            config.validate().unwrap();
        }
        for threshold in [-0.01, 1.01, f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            config.motion.pixel_difference_threshold = threshold;
            assert!(config.validate().is_err());
        }
        let error =
            toml::from_str::<Config>("[motion]\npixel_difference_threshhold = 0.5").unwrap_err();
        assert!(error.to_string().contains("pixel_difference_threshhold"));
        assert!(error.to_string().contains("pixel_difference_threshold"));
    }

    #[test]
    fn example_is_valid_and_matches_defaults() {
        let c: Config = toml::from_str(include_str!("../pivigil.toml")).unwrap();
        c.validate().unwrap();
        let documented: toml::Value = toml::from_str(include_str!("../pivigil.toml")).unwrap();
        let defaults = toml::Value::try_from(Config::default()).unwrap();
        assert_eq!(
            documented, defaults,
            "all options must be explicit and match the tested defaults"
        );
        assert_eq!(c.camera.framerate, Config::default().camera.framerate);
        assert_eq!(
            c.motion.pixel_difference_threshold,
            Motion::default().pixel_difference_threshold
        );
        assert!(toml::from_str::<Config>("unknown_option = true").is_err());
    }

    #[test]
    fn camera_dimensions_follow_evenness_and_format_limits() {
        let mut config = Config::default();
        for (width, height) in [
            (1920, 1080),
            (1640, 1232),
            (2304, 1296),
            (3280, 2464),
            (4608, 2592),
            (65534, 64),
            (64, 65534),
        ] {
            config.camera.width = width;
            config.camera.height = height;
            config.validate().unwrap();
        }
        for (width, height) in [
            (0, 1080),
            (1920, 62),
            (1921, 1080),
            (1920, 1081),
            (65536, 1080),
            (1920, 65536),
            (65534, 65534),
            (u32::MAX, u32::MAX),
        ] {
            config.camera.width = width;
            config.camera.height = height;
            assert!(config.validate().is_err(), "accepted {width}x{height}");
        }
    }

    #[test]
    fn rejects_typos_and_dangerous_values() {
        assert!(toml::from_str::<Config>("outpt_dir = '/tmp'").is_err());
        assert!(toml::from_str::<Config>("[camera]\nfps = 20").is_err());
        let mut c = Config::default();
        assert_eq!(c.recording.pre_motion_seconds, 2);
        c.recording.pre_motion_seconds = 31;
        assert!(c.validate().is_err());
        c.recording.pre_motion_seconds = 30;
        c.validate().unwrap();
        c.recording.pre_motion_seconds = 0;
        c.motion.region_threshold = 0.5 / (f64::from(c.camera.width) * f64::from(c.camera.height));
        assert!(c.validate().is_err());
        c.motion = Motion::default();
        c.camera.width = 1279;
        assert!(c.validate().is_err());
    }
}
