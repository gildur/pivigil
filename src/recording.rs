#[path = "recording/worker.rs"]
pub mod worker;
#[cfg(test)]
use crate::config::DEFAULT_FILENAME_FORMAT;
use crate::config::{Camera, Recording, recording_filename};
use anyhow::{Context, Result, bail, ensure};
use mp4::{AvcConfig, FourCC, Mp4Config, Mp4Sample, Mp4Writer, TrackConfig};
use std::{
    borrow::Cow,
    fs::{File, OpenOptions},
    io::{self, BufWriter, Write},
    os::unix::fs::OpenOptionsExt,
    path::{Path, PathBuf},
    time::SystemTime,
};

const TIMESCALE: u32 = 90_000;
const MAX_REUSE_BYTES: usize = 4 * 1024 * 1024;

#[cfg(test)]
type SpaceQuery = Box<dyn FnMut(&Path) -> Result<u64> + Send>;

#[derive(Default)]
struct Headers {
    sps: Option<Vec<u8>>,
    pps: Option<Vec<u8>>,
}

#[derive(Default, Debug, Clone, Copy)]
pub struct PacketInfo {
    pub vcl: bool,
    pub idr: bool,
    pub single_vcl_prefix4: bool,
}

/// Prepared bytes are never passed back to the Annex-B parser. Only an owned,
/// single-VCL, four-byte-prefix packet can be changed in place; its size is exact.
#[derive(Debug)]
pub enum PacketData<'a> {
    AnnexB(Cow<'a, [u8]>),
    Avc(mp4::Bytes),
}

impl<'a> PacketData<'a> {
    pub fn prepare(data: Cow<'a, [u8]>, info: &PacketInfo) -> Self {
        match data {
            Cow::Owned(mut data)
                if info.single_vcl_prefix4
                    && data.len() >= 5
                    && data.starts_with(&[0, 0, 0, 1]) =>
            {
                if let Ok(size) = u32::try_from(data.len() - 4) {
                    data[..4].copy_from_slice(&size.to_be_bytes());
                    Self::Avc(data.into())
                } else {
                    Self::AnnexB(Cow::Owned(data))
                }
            }
            data => Self::AnnexB(data),
        }
    }

    pub fn into_owned(self) -> PacketData<'static> {
        match self {
            Self::AnnexB(data) => PacketData::AnnexB(Cow::Owned(data.into_owned())),
            Self::Avc(data) => PacketData::Avc(data),
        }
    }

    pub fn sample(&self, recycle: mp4::Bytes) -> Result<mp4::Bytes> {
        match self {
            Self::AnnexB(data) => avc_sample_reusing(data, recycle),
            Self::Avc(data) => Ok(data.clone()),
        }
    }
}

impl std::ops::Deref for PacketData<'_> {
    type Target = [u8];
    fn deref(&self) -> &[u8] {
        match self {
            Self::AnnexB(data) => data,
            Self::Avc(data) => data,
        }
    }
}

fn next_start_code(data: &[u8], mut offset: usize) -> Option<(usize, usize)> {
    while offset + 3 < data.len() {
        offset += memchr::memchr(0, &data[offset..data.len() - 3])?;
        let prefix = if data[offset..].starts_with(&[0, 0, 0, 1]) {
            4
        } else if data[offset..].starts_with(&[0, 0, 1]) {
            3
        } else {
            offset += 1;
            continue;
        };
        return (offset + prefix < data.len()).then_some((offset, offset + prefix));
    }
    None
}

struct NalUnits<'a> {
    data: &'a [u8],
    next: Option<(usize, usize)>,
}

impl<'a> NalUnits<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self {
            data,
            next: next_start_code(data, 0),
        }
    }
}

impl<'a> Iterator for NalUnits<'a> {
    type Item = &'a [u8];
    fn next(&mut self) -> Option<Self::Item> {
        let (_, payload) = self.next?;
        self.next = next_start_code(self.data, payload);
        let end = self.next.map_or(self.data.len(), |next| next.0);
        Some(&self.data[payload..end])
    }
}

/// MP4 uses length-prefixed NAL units and stores SPS/PPS in its AVC sample entry.
#[cfg(test)]
fn avc_sample(data: &[u8]) -> Result<mp4::Bytes> {
    avc_sample_reusing(data, mp4::Bytes::new())
}

fn avc_sample_reusing(data: &[u8], recycle: mp4::Bytes) -> Result<mp4::Bytes> {
    // The muxer copies samples into its chunk buffer. A completed, unshared
    // sample can be reused; shared pre-roll bytes always remain immutable.
    let mut sample = recycle.try_into_mut().unwrap_or_default();
    if sample.capacity() > MAX_REUSE_BYTES {
        sample = Default::default();
    }
    sample.clear();
    sample.reserve(data.len());
    for nal in NalUnits::new(data) {
        if matches!(nal.first().copied().unwrap_or(0) & 0x1f, 7 | 8) {
            continue;
        }
        let size: u32 = nal
            .len()
            .try_into()
            .context("H.264 NAL unit exceeds MP4 limits")?;
        sample.extend_from_slice(&size.to_be_bytes());
        sample.extend_from_slice(nal);
    }
    ensure!(!sample.is_empty(), "encoded frame has no MP4 sample data");
    Ok(sample.freeze())
}

#[cfg(test)]
fn datetime_stamp(time: SystemTime) -> Result<String> {
    Ok(recording_filename(DEFAULT_FILENAME_FORMAT, time)?
        .trim_end_matches(".mp4")
        .to_owned())
}

fn create_clip(directory: &Path, filename: &str) -> Result<(PathBuf, File)> {
    let (stem, extension) = match filename.rsplit_once('.') {
        Some((stem, extension)) if !stem.is_empty() => (stem, format!(".{extension}")),
        _ => (filename, String::new()),
    };
    for suffix in 0..10_000 {
        let name = if suffix == 0 {
            filename.to_owned()
        } else {
            format!("{stem}_{suffix:03}{extension}")
        };
        let path = directory.join(name);
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o640)
            .open(&path)
        {
            Ok(file) => return Ok((path, file)),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error).with_context(|| format!("create {}", path.display())),
        }
    }
    bail!("too many recording filename collisions for {filename}")
}

struct Clip {
    muxer: Mp4Writer<BufWriter<File>>,
    pending: Option<Mp4Sample>,
    last_duration: u32,
    first_timestamp_us: u64,
}

impl Clip {
    fn new(file: File, headers: &Headers, camera: &Camera, timestamp_us: u64) -> Result<Self> {
        let config = Mp4Config {
            major_brand: FourCC::from(*b"isom"),
            minor_version: 512,
            compatible_brands: vec![
                FourCC::from(*b"isom"),
                FourCC::from(*b"iso2"),
                FourCC::from(*b"avc1"),
                FourCC::from(*b"mp41"),
            ],
            timescale: TIMESCALE,
        };
        let mut muxer =
            Mp4Writer::write_start(BufWriter::new(file), &config).context("start MP4 container")?;
        let mut track = TrackConfig::from(AvcConfig {
            width: camera
                .width
                .try_into()
                .context("MP4 width exceeds format limits")?,
            height: camera
                .height
                .try_into()
                .context("MP4 height exceeds format limits")?,
            seq_param_set: headers.sps.clone().context("missing H.264 SPS")?,
            pic_param_set: headers.pps.clone().context("missing H.264 PPS")?,
        });
        track.timescale = TIMESCALE;
        muxer.add_track(&track).context("add MP4 H.264 track")?;
        Ok(Self {
            muxer,
            pending: None,
            last_duration: TIMESCALE / camera.framerate,
            first_timestamp_us: timestamp_us,
        })
    }

    fn write(&mut self, timestamp_us: u64, data: &PacketData<'_>, sync: bool) -> Result<()> {
        let ticks = timestamp_us
            .checked_sub(self.first_timestamp_us)
            .context("MP4 frame timestamp moved backwards")?
            * u64::from(TIMESCALE)
            / 1_000_000;
        let mut recycle = mp4::Bytes::new();
        if let Some(mut previous) = self.pending.take() {
            let duration = ticks
                .checked_sub(previous.start_time)
                .context("MP4 frame time moved backwards")?;
            ensure!(duration > 0, "duplicate MP4 frame timestamp");
            previous.duration = duration
                .try_into()
                .context("MP4 frame duration exceeds range")?;
            self.last_duration = previous.duration;
            self.muxer
                .write_sample(1, &previous)
                .context("write MP4 frame")?;
            recycle = previous.bytes;
        }
        // One frame of lookahead preserves timing when the camera drops frames.
        self.pending = Some(Mp4Sample {
            start_time: ticks,
            duration: self.last_duration,
            rendering_offset: 0,
            is_sync: sync,
            bytes: data.sample(recycle)?,
        });
        Ok(())
    }

    fn finish(mut self) -> Result<()> {
        if let Some(mut sample) = self.pending.take() {
            sample.duration = self.last_duration;
            self.muxer
                .write_sample(1, &sample)
                .context("write final MP4 frame")?;
        }
        self.muxer.write_end().context("finalize MP4 container")?;
        self.muxer
            .into_writer()
            .flush()
            .context("flush MP4 recording")
    }
}

pub struct ClipWriter {
    directory: PathBuf,
    enabled: bool,
    filename_format: String,
    max_file_us: u64,
    min_free_bytes: u64,
    space_available: Option<bool>,
    #[cfg(test)]
    space_query: Option<SpaceQuery>,
    camera: Camera,
    headers: Headers,
    clip: Option<Clip>,
    event: Option<u64>,
    started_us: u64,
}

impl ClipWriter {
    #[cfg(test)]
    pub fn new(directory: &Path, max_file_seconds: u64, camera: &Camera) -> Self {
        Self::configured(
            &Recording {
                output_dir: directory.into(),
                max_file_seconds,
                ..Recording::default()
            },
            camera,
        )
    }

    pub fn configured(settings: &Recording, camera: &Camera) -> Self {
        Self {
            directory: settings.output_dir.clone(),
            enabled: settings.enabled,
            filename_format: settings.filename_format.clone(),
            max_file_us: settings.max_file_seconds * 1_000_000,
            min_free_bytes: settings.min_free_mb * 1024 * 1024,
            space_available: None,
            #[cfg(test)]
            space_query: None,
            camera: camera.clone(),
            headers: Headers::default(),
            clip: None,
            event: None,
            started_us: 0,
        }
    }

    #[cfg(test)]
    pub(crate) fn set_space_query(
        &mut self,
        query: impl FnMut(&Path) -> Result<u64> + Send + 'static,
    ) {
        self.space_query = Some(Box::new(query));
    }

    fn free_bytes(&mut self) -> Result<u64> {
        #[cfg(test)]
        if let Some(query) = &mut self.space_query {
            return query(&self.directory);
        }
        crate::storage::free_bytes(&self.directory)
    }

    fn check_space(&mut self) -> Result<bool> {
        let available = self.free_bytes()? >= self.min_free_bytes;
        if self.space_available != Some(available) {
            crate::log(if available {
                "disk space available"
            } else {
                "disk reserve reached; recording suspended"
            });
        }
        self.space_available = Some(available);
        Ok(available)
    }

    pub fn h264_headers(&self) -> Option<(&[u8], &[u8])> {
        Some((self.headers.sps.as_deref()?, self.headers.pps.as_deref()?))
    }

    pub fn inspect(&mut self, data: &[u8]) -> Result<PacketInfo> {
        let mut units = NalUnits::new(data).peekable();
        ensure!(
            units.peek().is_some(),
            "encoder returned an invalid Annex-B H.264 packet"
        );
        let mut info = PacketInfo::default();
        let mut count = 0usize;
        for nal in units {
            count += 1;
            match nal.first().copied().unwrap_or(0) & 0x1f {
                1..=4 => info.vcl = true,
                5 => {
                    info.vcl = true;
                    info.idr = true;
                }
                7 => self.headers.sps = Some(nal.to_vec()),
                8 => self.headers.pps = Some(nal.to_vec()),
                _ => {}
            }
        }
        info.single_vcl_prefix4 = count == 1 && info.vcl && data.starts_with(&[0, 0, 0, 1]);
        Ok(info)
    }

    #[cfg(test)]
    pub fn write(
        &mut self,
        event: u64,
        timestamp_us: u64,
        captured_at: SystemTime,
        data: &[u8],
        info: &PacketInfo,
    ) -> Result<()> {
        self.write_packet(
            event,
            timestamp_us,
            captured_at,
            &PacketData::AnnexB(Cow::Borrowed(data)),
            info,
        )
    }

    pub fn write_packet(
        &mut self,
        event: u64,
        timestamp_us: u64,
        captured_at: SystemTime,
        data: &PacketData<'_>,
        info: &PacketInfo,
    ) -> Result<()> {
        if !self.enabled || !info.vcl {
            return Ok(());
        }
        if self.event != Some(event) {
            self.finish()?;
            self.event = Some(event);
        }
        if info.idr
            && self.clip.is_some()
            && timestamp_us.saturating_sub(self.started_us) >= self.max_file_us
        {
            self.close_file()?;
        }
        if self.clip.is_none() {
            if !info.idr {
                return Ok(());
            }
            ensure!(
                self.headers
                    .sps
                    .as_ref()
                    .is_some_and(|sps| (4..=u16::MAX as usize).contains(&sps.len()))
                    && self
                        .headers
                        .pps
                        .as_ref()
                        .is_some_and(|pps| (1..=u16::MAX as usize).contains(&pps.len())),
                "encoder IDR has missing or invalid SPS/PPS headers"
            );
            let filename = recording_filename(&self.filename_format, captured_at)?;
            if !self.check_space()? {
                return Ok(());
            }
            let (path, file) = create_clip(&self.directory, &filename)?;
            self.clip = Some(Clip::new(file, &self.headers, &self.camera, timestamp_us)?);
            self.started_us = timestamp_us;
            crate::log(format!("opened {}", path.display()));
        }
        self.clip
            .as_mut()
            .unwrap()
            .write(timestamp_us, data, info.idr)
    }

    fn close_file(&mut self) -> Result<()> {
        if let Some(clip) = self.clip.take() {
            clip.finish()?;
        }
        Ok(())
    }

    pub fn finish(&mut self) -> Result<()> {
        self.close_file()?;
        self.event = None;
        Ok(())
    }
}

impl Drop for ClipWriter {
    fn drop(&mut self) {
        // Finish valid samples even when camera/device errors unwind the capture loop.
        if let Err(error) = self.finish() {
            crate::log(format!("finalizing recording failed: {error:#}"));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::UNIX_EPOCH;
    use std::{fs, time::Duration};
    const HEADERS: &[u8] = &[0, 0, 0, 1, 0x67, 0x64, 0, 0x28, 0, 0, 1, 0x68, 0x34];
    const IDR: &[u8] = &[0, 0, 1, 0x65, 0xaa];
    const INTER: &[u8] = &[0, 0, 1, 0x41, 0xbb];

    fn reference_nals(data: &[u8]) -> Vec<&[u8]> {
        let mut starts = Vec::new();
        let mut index = 0;
        while index + 3 < data.len() {
            let prefix = if data[index..].starts_with(&[0, 0, 0, 1]) {
                4
            } else if data[index..].starts_with(&[0, 0, 1]) {
                3
            } else {
                index += 1;
                continue;
            };
            if index + prefix < data.len() {
                starts.push((index, index + prefix));
            }
            index += prefix;
        }
        starts
            .iter()
            .enumerate()
            .map(|(i, &(_, payload))| {
                &data[payload..starts.get(i + 1).map_or(data.len(), |next| next.0)]
            })
            .collect()
    }

    fn check_parser_matches_reference(data: &[u8]) {
        let expected = reference_nals(data);
        let actual: Vec<_> = NalUnits::new(data).collect();
        assert_eq!(actual, expected, "{data:?}");
        let mut bytes = Vec::new();
        for nal in expected {
            if matches!(nal.first().copied().unwrap_or(0) & 0x1f, 7 | 8) {
                continue;
            }
            bytes.extend_from_slice(&(nal.len() as u32).to_be_bytes());
            bytes.extend_from_slice(nal);
        }
        if bytes.is_empty() {
            assert!(avc_sample(data).is_err());
        } else {
            assert_eq!(&avc_sample(data).unwrap()[..], bytes);
            let mut writer = ClipWriter::new(Path::new("/tmp"), 300, &Camera::default());
            let info = writer.inspect(data).unwrap();
            let prepared = PacketData::prepare(Cow::Owned(data.to_vec()), &info);
            assert_eq!(&prepared.sample(mp4::Bytes::new()).unwrap()[..], bytes);
        }
    }

    #[test]
    fn prepared_and_annex_b_packets_produce_identical_complete_mp4_files() {
        let left = temporary();
        let right = temporary();
        let time = UNIX_EPOCH + Duration::from_secs(1_794_343_684);
        let mut annex_b = ClipWriter::new(&left, 2, &Camera::default());
        let mut prepared = ClipWriter::new(&right, 2, &Camera::default());
        annex_b.inspect(HEADERS).unwrap();
        prepared.inspect(HEADERS).unwrap();
        let packets: &[&[u8]] = &[
            &[0, 0, 0, 1, 0x65, 0xaa],
            &[0, 0, 0, 1, 0x41, 0xbb],
            &[0, 0, 1, 0x65, 0xcc],
            &[0, 0, 0, 1, 0x41, 0xdd, 0, 0, 1, 6, 0xee],
            &[0, 0, 0, 1, 0x65, 0xab],
            &[0, 0, 0, 1, 0x41, 0xbc],
            &[0, 0, 0, 1, 0x65, 0xcd],
            &[0, 0, 0, 1, 0x41, 0xde],
        ];
        for (i, &raw) in packets.iter().enumerate() {
            let event = if i < 5 { 1 } else { 2 };
            let info = annex_b.inspect(raw).unwrap();
            annex_b
                .write(
                    event,
                    i as u64 * 1_000_000,
                    time + Duration::from_secs(i as u64),
                    raw,
                    &info,
                )
                .unwrap();
            let info = prepared.inspect(raw).unwrap();
            let data = PacketData::prepare(Cow::Owned(raw.to_vec()), &info);
            prepared
                .write_packet(
                    event,
                    i as u64 * 1_000_000,
                    time + Duration::from_secs(i as u64),
                    &data,
                    &info,
                )
                .unwrap();
        }
        annex_b.finish().unwrap();
        prepared.finish().unwrap();
        let mut names: Vec<_> = fs::read_dir(&left)
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        names.sort();
        assert!(names.len() >= 3);
        assert_eq!(names.len(), fs::read_dir(&right).unwrap().count());
        for name in names {
            assert_eq!(
                fs::read(left.join(&name)).unwrap(),
                fs::read(right.join(&name)).unwrap()
            );
        }
        fs::remove_dir_all(left).unwrap();
        fs::remove_dir_all(right).unwrap();
    }

    #[test]
    fn prepared_samples_match_annex_b_conversion_and_share_only_owned_single_vcl_data() {
        let packets: &[&[u8]] = &[
            &[0, 0, 0, 1, 0x41, 0xaa],
            &[0, 0, 0, 1, 0x65, 0xbb, 0, 0, 1],
            &[0, 0, 1, 0x65, 0xcc],
            &[99, 0, 0, 0, 1, 0x41, 0xdd],
            &[0, 0, 0, 1, 0x65, 1, 0, 0, 0, 1, 0x41, 2],
            &[
                0, 0, 0, 1, 0x67, 0x64, 0, 0x28, 0, 0, 0, 1, 0x68, 0x34, 0, 0, 0, 1, 0x65, 3,
            ],
        ];
        let mut writer = ClipWriter::new(Path::new("/tmp"), 300, &Camera::default());
        for &raw in packets {
            let expected = avc_sample(raw).unwrap();
            let info = writer.inspect(raw).unwrap();
            let owned = raw.to_vec();
            let address = owned.as_ptr();
            let prepared = PacketData::prepare(Cow::Owned(owned), &info);
            assert_eq!(prepared.len(), raw.len());
            assert_eq!(prepared.sample(mp4::Bytes::new()).unwrap(), expected);
            if info.single_vcl_prefix4 {
                assert!(matches!(prepared, PacketData::Avc(_)));
                assert_eq!(prepared.as_ptr(), address);
                let sample = prepared.sample(mp4::Bytes::new()).unwrap();
                assert_eq!(sample.as_ptr(), address);
                assert!(sample.try_into_mut().is_err());
            } else {
                assert!(matches!(prepared, PacketData::AnnexB(_)));
            }
            let borrowed = PacketData::prepare(Cow::Borrowed(raw), &info);
            assert!(matches!(borrowed, PacketData::AnnexB(_)));
            assert_eq!(borrowed.sample(mp4::Bytes::new()).unwrap(), expected);
        }
    }

    #[test]
    fn conversion_reuses_only_unique_bounded_sample_buffers() {
        let raw = [0, 0, 1, 0x41, 0xaa];
        let mut sample = avc_sample(&raw).unwrap();
        let pointer = sample.as_ptr();
        for _ in 0..10 {
            sample = avc_sample_reusing(&raw, sample).unwrap();
            assert_eq!(sample.as_ptr(), pointer);
        }
        let shared = sample.clone();
        let next = avc_sample_reusing(&[0, 0, 1, 0x41, 0xbb], sample).unwrap();
        assert_eq!(&shared[..], &[0, 0, 0, 2, 0x41, 0xaa]);
        assert_eq!(&next[..], &[0, 0, 0, 2, 0x41, 0xbb]);
        let oversized: mp4::Bytes = vec![42; MAX_REUSE_BYTES + 1].into();
        let result = avc_sample_reusing(&raw, oversized).unwrap();
        assert!(result.try_into_mut().unwrap().capacity() <= MAX_REUSE_BYTES);
    }

    #[test]
    fn streaming_nal_parser_preserves_prefix_padding_and_empty_unit_behavior() {
        for data in [
            b"".as_slice(),
            &[0],
            &[0, 0, 1],
            &[0, 0, 0, 1],
            &[0, 0, 1, 0x65],
            &[0, 0, 0, 1, 0, 0, 0, 1, 0x41, 0xaa],
            &[
                99, 0, 0, 0, 0, 1, 0x67, 0x64, 0, 0x28, 0, 0, 1, 0x68, 0x34, 0, 0, 1, 0x65, 0xaa,
                0, 0, 3, 1,
            ],
            &[0, 0, 1, 0x41, 0xaa, 0, 0, 0, 1],
            &[0, 0, 1, 0x41, 0xaa, 0, 0, 1],
        ] {
            check_parser_matches_reference(data);
        }
    }

    #[test]
    fn streaming_nal_parser_and_avc_conversion_match_reference_on_generated_packets() {
        let mut seed = 0x1234_5678u32;
        for length in (0usize..4096).step_by(17) {
            let mut data: Vec<u8> = (0..length)
                .map(|_| {
                    seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                    (seed >> 24) as u8
                })
                .collect();
            for offset in (0..length.saturating_sub(5)).step_by(31) {
                let prefix: &[u8] = if offset % 2 == 0 {
                    &[0, 0, 0, 1]
                } else {
                    &[0, 0, 1]
                };
                data[offset..offset + prefix.len()].copy_from_slice(prefix);
            }
            check_parser_matches_reference(&data);
        }
    }

    pub(super) fn temporary() -> PathBuf {
        let root = std::env::temp_dir().join(format!(
            "pivigil-mp4-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir(&root).unwrap();
        root
    }

    #[test]
    fn space_is_checked_only_before_new_files_and_retried_at_keyframes() {
        use std::sync::{
            Arc,
            atomic::{AtomicU64, AtomicUsize, Ordering},
        };

        let root = temporary();
        let settings = Recording {
            output_dir: root.clone(),
            max_file_seconds: 2,
            min_free_mb: 1,
            ..Default::default()
        };
        let reserve = settings.min_free_mb * 1024 * 1024;
        let free = Arc::new(AtomicU64::new(reserve - 1));
        let checks = Arc::new(AtomicUsize::new(0));
        let mut writer = ClipWriter::configured(&settings, &Camera::default());
        let available = Arc::clone(&free);
        let checked = Arc::clone(&checks);
        let directory = root.clone();
        writer.set_space_query(move |path| {
            assert_eq!(path, directory);
            checked.fetch_add(1, Ordering::SeqCst);
            Ok(available.load(Ordering::SeqCst))
        });
        writer.inspect(HEADERS).unwrap();
        let idr = writer.inspect(IDR).unwrap();
        let inter = writer.inspect(INTER).unwrap();
        let at = UNIX_EPOCH + Duration::from_secs(1_794_343_684);
        let write = |writer: &mut ClipWriter, event, second: u64, keyframe, expected_checks| {
            let (raw, info) = if keyframe {
                (IDR, &idr)
            } else {
                (INTER, &inter)
            };
            writer
                .write(event, second * 1_000_000, at, raw, info)
                .unwrap();
            assert_eq!(checks.load(Ordering::SeqCst), expected_checks);
        };
        assert_eq!(checks.load(Ordering::SeqCst), 0);
        write(&mut writer, 1, 0, false, 0);
        write(&mut writer, 1, 1, true, 1);
        write(&mut writer, 1, 2, false, 1);
        write(&mut writer, 1, 3, true, 2);
        assert_eq!(fs::read_dir(&root).unwrap().count(), 0);

        free.store(reserve, Ordering::SeqCst);
        write(&mut writer, 1, 4, false, 2);
        assert_eq!(fs::read_dir(&root).unwrap().count(), 0);
        write(&mut writer, 1, 5, true, 3);
        assert!(writer.clip.is_some());

        free.store(0, Ordering::SeqCst);
        write(&mut writer, 1, 6, true, 3);
        write(&mut writer, 1, 7, false, 3);
        write(&mut writer, 1, 8, true, 4);
        assert!(writer.clip.is_none());
        // Rotation must finalize the old file even when the replacement is denied.
        let path = fs::read_dir(&root).unwrap().next().unwrap().unwrap().path();
        let file = File::open(path).unwrap();
        let mut mp4 = mp4::Mp4Reader::read_header(&file, file.metadata().unwrap().len()).unwrap();
        assert_eq!(mp4.sample_count(1).unwrap(), 3);
        assert!(mp4.read_sample(1, 1).unwrap().unwrap().is_sync);
        write(&mut writer, 1, 9, true, 5);

        free.store(reserve, Ordering::SeqCst);
        write(&mut writer, 1, 10, false, 5);
        write(&mut writer, 1, 11, true, 6);
        write(&mut writer, 2, 12, false, 6);
        write(&mut writer, 2, 13, true, 7);
        writer.finish().unwrap();
        assert_eq!(checks.load(Ordering::SeqCst), 7);
        assert_eq!(fs::read_dir(&root).unwrap().count(), 3);
        for entry in fs::read_dir(&root).unwrap() {
            let file = File::open(entry.unwrap().path()).unwrap();
            let mut mp4 =
                mp4::Mp4Reader::read_header(&file, file.metadata().unwrap().len()).unwrap();
            assert!(mp4.read_sample(1, 1).unwrap().unwrap().is_sync);
        }
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn failed_space_query_is_returned_without_creating_a_file() {
        let root = temporary();
        let mut writer = ClipWriter::new(&root, 2, &Camera::default());
        writer.set_space_query(|_| anyhow::bail!("simulated space query failure"));
        writer.inspect(HEADERS).unwrap();
        let idr = writer.inspect(IDR).unwrap();
        assert!(
            writer
                .write(1, 0, UNIX_EPOCH, IDR, &idr)
                .unwrap_err()
                .to_string()
                .contains("simulated space query failure")
        );
        writer.finish().unwrap();
        assert_eq!(fs::read_dir(&root).unwrap().count(), 0);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn writes_flat_dated_mp4_clips_waits_for_idr_and_preserves_collisions() {
        let root = temporary();
        let time = UNIX_EPOCH + Duration::from_secs(1_794_343_684);
        let stamp = datetime_stamp(time).unwrap();
        let mut writer = ClipWriter::new(&root, 2, &Camera::default());
        writer.inspect(HEADERS).unwrap();
        let inter = writer.inspect(INTER).unwrap();
        writer.write(1, 0, time, INTER, &inter).unwrap();
        assert_eq!(fs::read_dir(&root).unwrap().count(), 0);
        let idr = writer.inspect(IDR).unwrap();
        writer.write(1, 100, time, IDR, &idr).unwrap();
        writer.write(1, 3_000_000, time, INTER, &inter).unwrap();
        writer.write(1, 4_000_000, time, IDR, &idr).unwrap();
        writer.write(2, 5_000_000, time, INTER, &inter).unwrap();
        writer.write(2, 6_000_000, time, IDR, &idr).unwrap();
        writer.finish().unwrap();
        for index in 0..3 {
            let name = if index == 0 {
                format!("{stamp}.mp4")
            } else {
                format!("{stamp}_{index:03}.mp4")
            };
            let path = root.join(name);
            let file = File::open(&path).unwrap();
            let mut mp4 =
                mp4::Mp4Reader::read_header(&file, file.metadata().unwrap().len()).unwrap();
            assert_eq!(mp4.tracks()[&1].width(), Camera::default().width as u16);
            assert_eq!(mp4.tracks()[&1].height(), Camera::default().height as u16);
            let first = mp4.read_sample(1, 1).unwrap().unwrap();
            assert!(first.is_sync);
            assert_eq!(&first.bytes[..], &[0, 0, 0, 2, 0x65, 0xaa]);
        }
        let first = root.join(format!("{stamp}.mp4"));
        let saved = fs::read(&first).unwrap();
        let mut again = ClipWriter::new(&root, 2, &Camera::default());
        again.inspect(HEADERS).unwrap();
        again.write(1, 0, time, IDR, &idr).unwrap();
        drop(again); // Error recovery/drop also finalizes the container.
        assert!(root.join(format!("{stamp}_003.mp4")).exists());
        assert_eq!(fs::read(first).unwrap(), saved);
        assert!(writer.inspect(b"garbage").is_err());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn configured_filenames_preserve_collisions_and_recording_disable() {
        let root = temporary();
        let settings = Recording {
            output_dir: root.clone(),
            filename_format: "camera-%%-%s.mp4".into(),
            ..Default::default()
        };
        let at = UNIX_EPOCH + Duration::from_secs(1_794_343_684);
        let name = recording_filename(&settings.filename_format, at).unwrap();
        let mut writer = ClipWriter::configured(&settings, &Camera::default());
        writer.inspect(HEADERS).unwrap();
        let idr = writer.inspect(IDR).unwrap();
        writer.write(1, 0, at, IDR, &idr).unwrap();
        writer.write(2, 1_000_000, at, IDR, &idr).unwrap();
        writer.finish().unwrap();
        assert!(root.join(&name).is_file());
        assert!(root.join(name.replace(".mp4", "_001.mp4")).is_file());
        assert_eq!(fs::read_dir(&root).unwrap().count(), 2);
        let mut disabled = ClipWriter::configured(
            &Recording {
                enabled: false,
                output_dir: root.join("missing"),
                ..settings
            },
            &Camera::default(),
        );
        disabled.inspect(HEADERS).unwrap();
        disabled.write(1, 0, at, IDR, &idr).unwrap();
        disabled.finish().unwrap();
        assert!(!root.join("missing").exists());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn mp4_sample_timing_keeps_camera_frame_gaps() {
        let root = temporary();
        let time = UNIX_EPOCH + Duration::from_secs(1_794_343_684);
        let mut writer = ClipWriter::new(&root, 300, &Camera::default());
        writer.inspect(HEADERS).unwrap();
        let idr = writer.inspect(IDR).unwrap();
        let inter = writer.inspect(INTER).unwrap();
        writer.write(1, 1_000_000, time, IDR, &idr).unwrap();
        writer.write(1, 1_066_667, time, INTER, &inter).unwrap();
        writer.write(1, 1_200_000, time, INTER, &inter).unwrap();
        writer.finish().unwrap();
        let file = File::open(root.join(format!("{}.mp4", datetime_stamp(time).unwrap()))).unwrap();
        let mut mp4 = mp4::Mp4Reader::read_header(&file, file.metadata().unwrap().len()).unwrap();
        assert_eq!(mp4.sample_count(1).unwrap(), 3);
        assert_eq!(mp4.read_sample(1, 1).unwrap().unwrap().duration, 6000);
        assert_eq!(mp4.read_sample(1, 2).unwrap().unwrap().duration, 12000);
        assert_eq!(mp4.read_sample(1, 3).unwrap().unwrap().start_time, 18000);
        fs::remove_dir_all(root).unwrap();
    }
}
