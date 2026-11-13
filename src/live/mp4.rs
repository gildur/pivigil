//! ISO-BMFF initialization and one-sample fragments for browser MSE and VLC.
use anyhow::{Context, Result, ensure};
use mp4::{AvcConfig, FourCC, Mp4Config, Mp4Writer, TrackConfig};
use std::io::Cursor;
pub const TIMESCALE: u64 = 90_000;

fn boxed(kind: &[u8; 4], payload: &[u8]) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(payload.len() + 8);
    bytes.extend_from_slice(&((payload.len() + 8) as u32).to_be_bytes());
    bytes.extend_from_slice(kind);
    bytes.extend_from_slice(payload);
    bytes
}
fn full(kind: &[u8; 4], version: u8, flags: u32, payload: &[u8]) -> Vec<u8> {
    let mut data = Vec::with_capacity(payload.len() + 4);
    data.extend_from_slice(&((u32::from(version) << 24) | flags).to_be_bytes());
    data.extend_from_slice(payload);
    boxed(kind, &data)
}
fn words(values: &[u32]) -> Vec<u8> {
    values
        .iter()
        .flat_map(|value| value.to_be_bytes())
        .collect()
}

pub fn initialization(
    sps: &[u8],
    pps: &[u8],
    width: u32,
    height: u32,
    framerate: u32,
) -> Result<Vec<u8>> {
    ensure!(
        (4..=u16::MAX as usize).contains(&sps.len())
            && (1..=u16::MAX as usize).contains(&pps.len()),
        "invalid live H.264 parameter sets"
    );
    let config = Mp4Config {
        major_brand: FourCC::from(*b"iso6"),
        minor_version: 512,
        compatible_brands: vec![
            FourCC::from(*b"isom"),
            FourCC::from(*b"iso6"),
            FourCC::from(*b"avc1"),
            FourCC::from(*b"mp41"),
        ],
        timescale: TIMESCALE as u32,
    };
    let mut writer = Mp4Writer::write_start(Cursor::new(Vec::new()), &config)?;
    let mut track = TrackConfig::from(AvcConfig {
        width: width.try_into()?,
        height: height.try_into()?,
        seq_param_set: sps.to_vec(),
        pic_param_set: pps.to_vec(),
    });
    track.timescale = TIMESCALE as u32;
    writer.add_track(&track)?;
    writer.write_end()?;
    let bytes = writer.into_writer().into_inner();
    let trex = full(
        b"trex",
        0,
        0,
        &words(&[1, 1, TIMESCALE as u32 / framerate, 0, 0]),
    );
    let mvex = boxed(b"mvex", &trex);
    let mut output = Vec::new();
    let mut offset = 0;
    let mut movie = false;
    while offset + 8 <= bytes.len() {
        let size = u32::from_be_bytes(bytes[offset..offset + 4].try_into().unwrap()) as usize;
        ensure!(
            size >= 8 && offset + size <= bytes.len(),
            "invalid live initialization box"
        );
        match &bytes[offset + 4..offset + 8] {
            b"ftyp" => output.extend_from_slice(&bytes[offset..offset + size]),
            b"moov" => {
                let mut payload = bytes[offset + 8..offset + size].to_vec();
                payload.extend_from_slice(&mvex);
                output.extend_from_slice(&boxed(b"moov", &payload));
                movie = true;
            }
            _ => {}
        }
        offset += size;
    }
    ensure!(movie, "live initialization has no movie");
    Ok(output)
}

pub fn fragment(
    sequence: u32,
    timestamp_us: u64,
    duration_us: u64,
    base_us: u64,
    size: usize,
    idr: bool,
) -> Result<Vec<u8>> {
    let relative = timestamp_us
        .checked_sub(base_us)
        .context("live timestamp moved backwards")?;
    let ticks = |micros: u64| ((u128::from(micros) * u128::from(TIMESCALE)) / 1_000_000) as u64;
    let decode = ticks(relative);
    let duration = ticks(
        relative
            .checked_add(duration_us)
            .context("live duration overflow")?,
    )
    .saturating_sub(decode)
    .clamp(1, u32::MAX as u64) as u32;
    let size: u32 = size.try_into().context("live sample exceeds MP4 limits")?;
    let mfhd = full(b"mfhd", 0, 0, &words(&[sequence]));
    let tfhd = full(b"tfhd", 0, 0x020000, &words(&[1])); // default-base-is-moof.
    let tfdt = full(b"tfdt", 1, 0, &decode.to_be_bytes());
    let flags = if idr { 0x02000000 } else { 0x01010000 };
    // Fixed one-sample layout: moof is 100 bytes and mdat adds 8 bytes.
    let trun = full(b"trun", 0, 0x701, &words(&[1, 108, duration, size, flags]));
    let traf = boxed(b"traf", &[tfhd, tfdt, trun].concat());
    let mut output = boxed(b"moof", &[mfhd, traf].concat());
    ensure!(output.len() == 100, "unexpected live fragment layout");
    output.extend_from_slice(
        &size
            .checked_add(8)
            .context("live mdat exceeds limits")?
            .to_be_bytes(),
    );
    output.extend_from_slice(b"mdat");
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn initialization_and_fragments_are_readable_and_preserve_timing() {
        let mut bytes =
            initialization(&[0x67, 0x64, 0, 0x2a], &[0x68, 0x34], 640, 360, 15).unwrap();
        assert_eq!(&bytes[4..8], b"ftyp");
        assert!(bytes.windows(4).any(|s| s == b"mvex"));
        let init_len = bytes.len();
        let sample = [0, 0, 0, 2, 0x65, 0xaa];
        bytes.extend(fragment(1, 1_000_000, 66_667, 1_000_000, sample.len(), true).unwrap());
        bytes.extend_from_slice(&sample);
        bytes.extend(fragment(2, 1_066_667, 133_333, 1_000_000, sample.len(), false).unwrap());
        bytes.extend_from_slice(&sample);
        let len = bytes.len() as u64;
        let reader = ::mp4::Mp4Reader::read_header(Cursor::new(&bytes), len).unwrap();
        assert_eq!(reader.moofs.len(), 2);
        let first = &reader.moofs[0].trafs[0];
        assert_eq!(first.tfhd.flags, 0x020000);
        assert_eq!(first.tfdt.as_ref().unwrap().base_media_decode_time, 0);
        assert_eq!(first.trun.as_ref().unwrap().data_offset, Some(108));
        assert_eq!(first.trun.as_ref().unwrap().sample_flags, vec![0x02000000]);
        assert_eq!(
            reader.moofs[1].trafs[0]
                .tfdt
                .as_ref()
                .unwrap()
                .base_media_decode_time,
            6000
        );
        assert_eq!(
            &bytes[init_len + 108..init_len + 108 + sample.len()],
            &sample
        );
        assert_eq!(
            reader.moofs[1].trafs[0]
                .trun
                .as_ref()
                .unwrap()
                .sample_durations,
            vec![12000]
        );
    }
    #[test]
    fn invalid_parameter_sets_and_timestamps_are_rejected() {
        assert!(initialization(&[], &[], 640, 360, 15).is_err());
        assert!(fragment(1, 0, 1, 2, 10, true).is_err());
    }
}
