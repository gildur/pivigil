use crate::recording::{PacketData, PacketInfo};
use std::{borrow::Cow, collections::VecDeque, time::SystemTime};

pub struct BufferedFrame {
    pub timestamp_us: u64,
    pub captured_at: SystemTime,
    pub data: PacketData<'static>,
    pub info: PacketInfo,
}

/// Keep complete H.264 GOPs, including the IDR at or before the requested window.
pub struct PreRoll {
    window_us: u64,
    max_bytes: usize,
    max_frames: usize,
    bytes: usize,
    frames: VecDeque<BufferedFrame>,
}

impl PreRoll {
    pub fn new(seconds: u64, framerate: u32, max_bytes: usize) -> Self {
        Self {
            window_us: seconds * 1_000_000,
            max_bytes,
            max_frames: (seconds as usize + 2) * framerate as usize * 2,
            bytes: 0,
            frames: VecDeque::new(),
        }
    }

    /// Read the hardware output once when pre-roll will need an owned copy.
    /// Parsing and muxing can then read cached RAM; disabled/oversized packets
    /// stay borrowed, preserving the no-buffering path and memory limit.
    pub fn cache_packet<'a>(&self, data: &'a [u8]) -> Cow<'a, [u8]> {
        if self.window_us > 0 && data.len() <= self.max_bytes {
            Cow::Owned(data.to_vec())
        } else {
            Cow::Borrowed(data)
        }
    }

    fn pop_front(&mut self) {
        if let Some(frame) = self.frames.pop_front() {
            self.bytes -= frame.data.len();
        }
    }

    fn next_idr(&self) -> Option<usize> {
        self.frames
            .iter()
            .enumerate()
            .skip(1)
            .find_map(|(i, frame)| frame.info.idr.then_some(i))
    }

    fn drop_gop(&mut self, count: usize) {
        for _ in 0..count {
            self.pop_front();
        }
    }

    pub fn trim(&mut self, now_us: u64) {
        let cutoff = now_us.saturating_sub(self.window_us);
        while let Some(next) = self.next_idr() {
            if self.frames[next].timestamp_us > cutoff {
                break;
            }
            self.drop_gop(next);
        }
        while self.bytes > self.max_bytes || self.frames.len() > self.max_frames {
            // If a complete GOP exceeds the cap, discard it and wait for another IDR.
            let count = self.next_idr().unwrap_or(self.frames.len());
            self.drop_gop(count);
        }
    }

    #[cfg(test)]
    fn push(
        &mut self,
        timestamp_us: u64,
        captured_at: SystemTime,
        data: Cow<'_, [u8]>,
        info: PacketInfo,
    ) {
        self.push_prepared(
            timestamp_us,
            captured_at,
            PacketData::prepare(data, &info),
            info,
        );
    }

    pub fn push_prepared(
        &mut self,
        timestamp_us: u64,
        captured_at: SystemTime,
        data: PacketData<'_>,
        info: PacketInfo,
    ) {
        if self.window_us == 0 || !info.vcl {
            return;
        }
        if self.frames.is_empty() && !info.idr {
            return;
        }
        if data.len() > self.max_bytes {
            self.frames.clear();
            self.bytes = 0;
            return;
        }
        self.bytes += data.len();
        self.frames.push_back(BufferedFrame {
            timestamp_us,
            captured_at,
            data: data.into_owned(),
            info,
        });
        self.trim(timestamp_us);
    }

    pub fn frames(&self) -> impl Iterator<Item = &BufferedFrame> {
        self.frames.iter()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::UNIX_EPOCH;

    fn frame(buffer: &mut PreRoll, second: u64, idr: bool) {
        buffer.push(
            second * 1_000_000,
            UNIX_EPOCH,
            Cow::Borrowed(&[second as u8; 10]),
            PacketInfo {
                vcl: true,
                idr,
                ..PacketInfo::default()
            },
        );
    }

    #[test]
    fn prepared_storage_preserves_gop_eviction_and_byte_limits() {
        let mut annex_b = PreRoll::new(5, 15, 35);
        let mut prepared = PreRoll::new(5, 15, 35);
        let mut writer = crate::recording::ClipWriter::new(
            std::path::Path::new("/tmp"),
            300,
            &crate::config::Camera::default(),
        );
        for index in 0..40 {
            let mut raw = vec![42; 10];
            raw[..5].copy_from_slice(&[0, 0, 0, 1, if index % 3 == 0 { 0x65 } else { 0x41 }]);
            let info = writer.inspect(&raw).unwrap();
            annex_b.push_prepared(
                index * 1_000_000,
                UNIX_EPOCH,
                PacketData::AnnexB(Cow::Borrowed(&raw)),
                info,
            );
            let data = PacketData::prepare(prepared.cache_packet(&raw), &info);
            prepared.push_prepared(index * 1_000_000, UNIX_EPOCH, data, info);
            assert_eq!(prepared.bytes, annex_b.bytes);
            assert!(prepared.bytes <= 35);
            let left: Vec<_> = annex_b.frames().collect();
            let right: Vec<_> = prepared.frames().collect();
            assert_eq!(left.len(), right.len());
            for (left, right) in left.iter().zip(right) {
                assert_eq!(left.timestamp_us, right.timestamp_us);
                assert_eq!(left.info.idr, right.info.idr);
                assert_eq!(
                    left.data.sample(Default::default()).unwrap(),
                    right.data.sample(Default::default()).unwrap()
                );
            }
        }
    }

    #[test]
    fn cached_packet_ownership_is_reused_and_disabled_or_oversized_packets_stay_borrowed() {
        let mut buffer = PreRoll::new(3, 15, 64 * 1024 * 1024);
        let source = [42; 16];
        let packet = buffer.cache_packet(&source);
        assert!(matches!(packet, Cow::Owned(_)));
        assert_eq!(&*packet, &source);
        let address = packet.as_ptr();
        buffer.push(
            0,
            UNIX_EPOCH,
            packet,
            PacketInfo {
                vcl: true,
                idr: true,
                ..PacketInfo::default()
            },
        );
        let stored = buffer.frames().next().unwrap();
        assert_eq!(stored.data.as_ptr(), address);
        assert_eq!(&*stored.data, &source);

        let disabled = PreRoll::new(0, 15, 64 * 1024 * 1024);
        assert!(matches!(disabled.cache_packet(&source), Cow::Borrowed(_)));
        buffer.max_bytes = 8;
        let packet = buffer.cache_packet(&source);
        assert!(matches!(packet, Cow::Borrowed(_)));
        buffer.push(
            1,
            UNIX_EPOCH,
            packet,
            PacketInfo {
                vcl: true,
                idr: true,
                ..PacketInfo::default()
            },
        );
        assert_eq!(buffer.frames().count(), 0);
        assert_eq!(buffer.bytes, 0);
    }

    #[test]
    fn retains_the_keyframe_before_the_window_and_waits_for_initial_idr() {
        let mut buffer = PreRoll::new(5, 15, 64 * 1024 * 1024);
        frame(&mut buffer, 0, false);
        assert_eq!(buffer.frames().count(), 0);
        for second in 0..12 {
            frame(&mut buffer, second, second % 2 == 0);
        }
        buffer.trim(12_000_000);
        let frames: Vec<_> = buffer.frames().collect();
        assert!(frames[0].info.idr);
        assert_eq!(frames[0].timestamp_us, 6_000_000);
        assert_eq!(frames.last().unwrap().timestamp_us, 11_000_000);
    }

    #[test]
    fn memory_limit_discards_whole_gops_and_recovers_at_next_idr() {
        let mut buffer = PreRoll::new(30, 15, 25);
        for second in 0..5 {
            frame(&mut buffer, second, second % 2 == 0);
        }
        assert_eq!(buffer.frames().next().unwrap().timestamp_us, 4_000_000);
        frame(&mut buffer, 5, false);
        frame(&mut buffer, 6, false);
        assert_eq!(buffer.frames().count(), 0);
        frame(&mut buffer, 7, false);
        assert_eq!(buffer.frames().count(), 0);
        frame(&mut buffer, 8, true);
        assert!(buffer.frames().next().unwrap().info.idr);
        assert!(buffer.bytes <= buffer.max_bytes);
    }

    #[test]
    fn frame_count_is_bounded_and_zero_disables_buffering() {
        let mut buffer = PreRoll::new(5, 15, 64 * 1024 * 1024);
        buffer.max_frames = 2;
        for microsecond in 0..100 {
            buffer.push(
                microsecond,
                UNIX_EPOCH,
                Cow::Borrowed(&[0]),
                PacketInfo {
                    vcl: true,
                    idr: true,
                    ..PacketInfo::default()
                },
            );
        }
        assert_eq!(buffer.frames().count(), 2);
        let mut disabled = PreRoll::new(0, 15, 64 * 1024 * 1024);
        frame(&mut disabled, 0, true);
        assert_eq!(disabled.frames().count(), 0);
    }
}
