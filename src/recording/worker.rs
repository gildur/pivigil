//! Bounded in-process disk worker. No capture-thread filesystem calls after startup.
use super::{ClipWriter, PacketData, PacketInfo};
use crate::config::{Camera, Recording};
use anyhow::{Context, Result, ensure};
use std::{
    collections::VecDeque,
    sync::{Arc, Condvar, Mutex},
    thread::{self, JoinHandle},
    time::SystemTime,
};

const MAX_FRAMES: usize = 2048;
const MAX_COMMANDS: usize = 4096;
struct Frame {
    event: u64,
    timestamp_us: u64,
    captured_at: SystemTime,
    data: mp4::Bytes,
    info: PacketInfo,
}
enum Command {
    Headers(Vec<u8>),
    Frame(Frame),
    Finish,
}
struct State {
    commands: VecDeque<Command>,
    bytes: usize,
    frames: usize,
    peak_bytes: usize,
    peak_frames: usize,
    error: Option<String>,
    stopping: bool,
}
struct Shared {
    state: Mutex<State>,
    ready: Condvar,
}

pub struct RecordingWorker {
    shared: Arc<Shared>,
    thread: Option<JoinHandle<()>>,
    headers: Option<(Vec<u8>, Vec<u8>)>,
    event_open: bool,
    max_bytes: usize,
}

trait Output: Send + 'static {
    fn headers(&mut self, headers: &[u8]) -> Result<()>;
    fn frame(&mut self, frame: &Frame) -> Result<()>;
    fn finish(&mut self) -> Result<()>;
}
impl Output for ClipWriter {
    fn headers(&mut self, headers: &[u8]) -> Result<()> {
        self.inspect(headers).map(|_| ())
    }
    fn frame(&mut self, frame: &Frame) -> Result<()> {
        self.write_packet(
            frame.event,
            frame.timestamp_us,
            frame.captured_at,
            &PacketData::Avc(frame.data.clone()),
            &frame.info,
        )
    }
    fn finish(&mut self) -> Result<()> {
        ClipWriter::finish(self)
    }
}

impl RecordingWorker {
    pub fn new(settings: &Recording, camera: &Camera) -> Result<Self> {
        Self::spawn(ClipWriter::configured(settings, camera), settings)
    }
    fn spawn(output: impl Output, settings: &Recording) -> Result<Self> {
        let shared = Arc::new(Shared {
            state: Mutex::new(State {
                commands: VecDeque::new(),
                bytes: 0,
                frames: 0,
                peak_bytes: 0,
                peak_frames: 0,
                error: None,
                stopping: false,
            }),
            ready: Condvar::new(),
        });
        let worker = Arc::clone(&shared);
        let thread = thread::Builder::new()
            .name("recording".into())
            .spawn(move || {
                let result =
                    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| run(output, &worker)));
                let error = match result {
                    Ok(Ok(())) => None,
                    Ok(Err(error)) => Some(format!("{error:#}")),
                    Err(_) => Some("recording worker panicked".into()),
                };
                let mut state = worker.state.lock().unwrap_or_else(|e| e.into_inner());
                if let Some(error) = error {
                    state.error = Some(error);
                }
                state.stopping = true;
                state.commands.clear();
                state.bytes = 0;
                state.frames = 0;
                worker.ready.notify_all();
            })
            .context("start recording worker thread")?;
        Ok(Self {
            shared,
            thread: Some(thread),
            headers: None,
            event_open: false,
            max_bytes: settings.backlog_buffer_mb * 1024 * 1024,
        })
    }
    pub fn check_health(&self) -> Result<()> {
        let state = self.shared.state.lock().unwrap_or_else(|e| e.into_inner());
        ensure!(
            state.error.is_none(),
            "recording worker failed: {}",
            state.error.as_deref().unwrap_or("")
        );
        Ok(())
    }
    fn enqueue(&self, command: Command) -> Result<()> {
        let bytes = match &command {
            Command::Frame(frame) => frame.data.len(),
            Command::Headers(headers) => headers.len(),
            Command::Finish => 0,
        };
        let frames = usize::from(matches!(&command, Command::Frame(_)));
        let mut state = self.shared.state.lock().unwrap_or_else(|e| e.into_inner());
        ensure!(
            state.error.is_none(),
            "recording worker failed: {}",
            state.error.as_deref().unwrap_or("")
        );
        ensure!(!state.stopping, "recording worker stopped");
        ensure!(
            state.bytes + bytes <= self.max_bytes
                && state.frames + frames <= MAX_FRAMES
                && state.commands.len() < MAX_COMMANDS,
            "recording backlog exceeded its bounded queue; storage cannot keep up"
        );
        state.bytes += bytes;
        state.frames += frames;
        state.peak_bytes = state.peak_bytes.max(state.bytes);
        state.peak_frames = state.peak_frames.max(state.frames);
        state.commands.push_back(command);
        drop(state);
        self.shared.ready.notify_one();
        Ok(())
    }
    pub fn headers(&mut self, sps: &[u8], pps: &[u8]) -> Result<()> {
        if self
            .headers
            .as_ref()
            .is_some_and(|(a, b)| a == sps && b == pps)
        {
            return Ok(());
        }
        let mut raw = Vec::with_capacity(sps.len() + pps.len() + 8);
        raw.extend_from_slice(&[0, 0, 0, 1]);
        raw.extend_from_slice(sps);
        raw.extend_from_slice(&[0, 0, 0, 1]);
        raw.extend_from_slice(pps);
        self.enqueue(Command::Headers(raw))?;
        self.headers = Some((sps.to_vec(), pps.to_vec()));
        Ok(())
    }
    pub fn write(
        &mut self,
        event: u64,
        timestamp_us: u64,
        captured_at: SystemTime,
        data: &PacketData<'_>,
        info: &PacketInfo,
    ) -> Result<()> {
        self.enqueue(Command::Frame(Frame {
            event,
            timestamp_us,
            captured_at,
            data: data.sample(Default::default())?,
            info: *info,
        }))?;
        self.event_open = true;
        Ok(())
    }
    pub fn finish_event(&mut self) -> Result<()> {
        if self.event_open {
            self.enqueue(Command::Finish)?;
            self.event_open = false;
        }
        self.check_health()
    }
    pub fn shutdown(&mut self) -> Result<()> {
        {
            let mut state = self.shared.state.lock().unwrap_or_else(|e| e.into_inner());
            state.stopping = true;
        }
        self.shared.ready.notify_one();
        if let Some(thread) = self.thread.take() {
            thread
                .join()
                .map_err(|_| anyhow::anyhow!("recording worker join failed"))?;
            let state = self.shared.state.lock().unwrap_or_else(|e| e.into_inner());
            crate::log(format!(
                "recording worker drained; peak backlog: {} bytes, {} frames",
                state.peak_bytes, state.peak_frames
            ));
        }
        self.check_health()
    }
}
impl Drop for RecordingWorker {
    fn drop(&mut self) {
        if let Err(error) = self.shutdown() {
            crate::log(format!("recording shutdown failed: {error:#}"));
        }
    }
}

fn run(mut output: impl Output, shared: &Shared) -> Result<()> {
    loop {
        let command = {
            let mut state = shared.state.lock().unwrap_or_else(|e| e.into_inner());
            loop {
                if let Some(command) = state.commands.pop_front() {
                    break Some(command);
                }
                if state.stopping {
                    break None;
                }
                state = shared.ready.wait(state).unwrap_or_else(|e| e.into_inner());
            }
        };
        if let Some(command) = command {
            let (bytes, frames) = match command {
                Command::Headers(raw) => {
                    output.headers(&raw)?;
                    (raw.len(), 0)
                }
                Command::Frame(frame) => {
                    output.frame(&frame)?;
                    (frame.data.len(), 1)
                }
                Command::Finish => {
                    output.finish()?;
                    (0, 0)
                }
            };
            let mut state = shared.state.lock().unwrap_or_else(|e| e.into_inner());
            // Include the in-flight frame in both bounds until its I/O finishes.
            state.bytes -= bytes;
            state.frames -= frames;
        } else {
            break;
        }
    }
    output.finish()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        borrow::Cow,
        fs,
        sync::{
            atomic::{AtomicUsize, Ordering},
            mpsc,
        },
        time::{Duration, UNIX_EPOCH},
    };
    fn settings() -> Recording {
        Recording {
            output_dir: std::env::temp_dir(),
            min_free_mb: 1,
            ..Default::default()
        }
    }
    fn sample() -> PacketData<'static> {
        PacketData::Avc(mp4::Bytes::from_static(&[0, 0, 0, 2, 0x65, 0xaa]))
    }
    struct GateOutput {
        gate: Arc<(Mutex<bool>, Condvar)>,
        entered: mpsc::Sender<()>,
        count: Arc<AtomicUsize>,
    }
    impl Output for GateOutput {
        fn headers(&mut self, _: &[u8]) -> Result<()> {
            Ok(())
        }
        fn frame(&mut self, _: &Frame) -> Result<()> {
            if self.count.load(Ordering::SeqCst) == 0 {
                let _ = self.entered.send(());
                let (lock, ready) = &*self.gate;
                let mut open = lock.lock().unwrap();
                while !*open {
                    open = ready.wait(open).unwrap();
                }
            }
            self.count.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
        fn finish(&mut self) -> Result<()> {
            Ok(())
        }
    }

    #[test]
    fn configured_backlog_budget_counts_in_flight_frames_and_drains_after_overflow() {
        for (budget_mb, accepts_second_frame) in [(1, false), (2, true)] {
            let settings = Recording {
                backlog_buffer_mb: budget_mb,
                pre_motion_buffer_mb: 1,
                ..settings()
            };
            let gate = Arc::new((Mutex::new(false), Condvar::new()));
            let count = Arc::new(AtomicUsize::new(0));
            let (entered_tx, entered_rx) = mpsc::channel();
            let mut worker = RecordingWorker::spawn(
                GateOutput {
                    gate: gate.clone(),
                    entered: entered_tx,
                    count: count.clone(),
                },
                &settings,
            )
            .unwrap();
            let data = PacketData::Avc(vec![42; 600 * 1024].into());
            let info = PacketInfo::default();
            worker.write(1, 0, UNIX_EPOCH, &data, &info).unwrap();
            let entered = entered_rx.recv_timeout(Duration::from_secs(2)).is_ok();
            let second = worker.write(1, 66_667, UNIX_EPOCH, &data, &info);
            *gate.0.lock().unwrap() = true;
            gate.1.notify_all();
            worker.shutdown().unwrap();
            assert!(entered, "worker did not start writing the first frame");
            assert_eq!(second.is_ok(), accepts_second_frame);
            if let Err(error) = second {
                assert!(error.to_string().contains("backlog"));
            }
            assert_eq!(
                count.load(Ordering::SeqCst),
                if accepts_second_frame { 2 } else { 1 }
            );
            let state = worker.shared.state.lock().unwrap();
            assert_eq!((state.bytes, state.frames, state.commands.len()), (0, 0, 0));
        }
    }

    #[test]
    fn blocked_storage_does_not_block_capture_submission_and_shutdown_drains() {
        let gate = Arc::new((Mutex::new(false), Condvar::new()));
        let count = Arc::new(AtomicUsize::new(0));
        let (entered_tx, entered_rx) = mpsc::channel();
        let mut worker = RecordingWorker::spawn(
            GateOutput {
                gate: gate.clone(),
                entered: entered_tx,
                count: count.clone(),
            },
            &settings(),
        )
        .unwrap();
        let info = PacketInfo {
            vcl: true,
            idr: true,
            ..Default::default()
        };
        worker.write(1, 0, UNIX_EPOCH, &sample(), &info).unwrap();
        entered_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        let (done_tx, done_rx) = mpsc::channel();
        let producer = thread::spawn(move || {
            for index in 1..60 {
                worker
                    .write(1, index * 66_667, UNIX_EPOCH, &sample(), &info)
                    .unwrap();
            }
            worker.finish_event().unwrap();
            done_tx.send(()).unwrap();
            worker
        });
        let progressed = done_rx.recv_timeout(Duration::from_secs(2)).is_ok();
        *gate.0.lock().unwrap() = true;
        gate.1.notify_all();
        let mut worker = producer.join().unwrap();
        worker.shutdown().unwrap();
        assert!(progressed, "capture submissions waited for storage I/O");
        assert_eq!(count.load(Ordering::SeqCst), 60);
        let state = worker.shared.state.lock().unwrap();
        assert_eq!((state.bytes, state.frames, state.commands.len()), (0, 0, 0));
    }

    #[test]
    fn slow_space_check_runs_only_on_worker_and_capture_submissions_continue() {
        let root = super::super::tests::temporary();
        let settings = Recording {
            output_dir: root.clone(),
            ..settings()
        };
        let mut output = ClipWriter::configured(&settings, &Camera::default());
        let gate = Arc::new((Mutex::new(false), Condvar::new()));
        let blocked = Arc::clone(&gate);
        let checks = Arc::new(AtomicUsize::new(0));
        let checked = Arc::clone(&checks);
        let (entered_tx, entered_rx) = mpsc::channel();
        output.set_space_query(move |_| {
            assert_eq!(thread::current().name(), Some("recording"));
            checked.fetch_add(1, Ordering::SeqCst);
            entered_tx.send(()).unwrap();
            let mut open = blocked.0.lock().unwrap();
            while !*open {
                open = blocked.1.wait(open).unwrap();
            }
            Ok(1024 * 1024)
        });
        let mut worker = RecordingWorker::spawn(output, &settings).unwrap();
        // Idle workers must not query disk space without a recording request.
        let idle = entered_rx
            .recv_timeout(Duration::from_millis(1100))
            .is_err();
        worker
            .headers(&[0x67, 0x64, 0, 0x28], &[0x68, 0x34])
            .unwrap();
        let info = PacketInfo {
            vcl: true,
            idr: true,
            ..Default::default()
        };
        worker.write(1, 0, UNIX_EPOCH, &sample(), &info).unwrap();
        let entered = entered_rx.recv_timeout(Duration::from_secs(2)).is_ok();
        let (done_tx, done_rx) = mpsc::channel();
        let producer = thread::spawn(move || {
            for index in 1..60 {
                worker
                    .write(1, index * 66_667, UNIX_EPOCH, &sample(), &info)
                    .unwrap();
            }
            worker.finish_event().unwrap();
            done_tx.send(()).unwrap();
            worker
        });
        let progressed = done_rx.recv_timeout(Duration::from_secs(2)).is_ok();
        *gate.0.lock().unwrap() = true;
        gate.1.notify_all();
        let mut worker = producer.join().unwrap();
        worker.shutdown().unwrap();
        assert!(idle, "space was checked before a file was requested");
        assert!(
            entered,
            "worker did not check space before opening the file"
        );
        assert!(progressed, "capture submissions waited for the space check");
        assert_eq!(checks.load(Ordering::SeqCst), 1);
        let path = fs::read_dir(&root).unwrap().next().unwrap().unwrap().path();
        let file = fs::File::open(path).unwrap();
        let mp4 = mp4::Mp4Reader::read_header(&file, file.metadata().unwrap().len()).unwrap();
        assert_eq!(mp4.sample_count(1).unwrap(), 60);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn unavailable_storage_is_not_queried_until_a_new_file_is_requested() {
        let mut worker = RecordingWorker::new(
            &Recording {
                output_dir: "/dev/null/unavailable".into(),
                ..settings()
            },
            &Camera::default(),
        )
        .unwrap();
        worker
            .headers(&[0x67, 0x64, 0, 0x28], &[0x68, 0x34])
            .unwrap();
        worker.check_health().unwrap();
        worker
            .write(
                1,
                0,
                UNIX_EPOCH,
                &sample(),
                &PacketInfo {
                    vcl: true,
                    idr: true,
                    ..Default::default()
                },
            )
            .unwrap();
        assert!(
            worker
                .shutdown()
                .unwrap_err()
                .to_string()
                .contains("query recording disk space")
        );
    }

    #[test]
    fn queue_limits_and_worker_errors_are_explicit() {
        let max_bytes = settings().backlog_buffer_mb * 1024 * 1024;
        let shared = Arc::new(Shared {
            state: Mutex::new(State {
                commands: VecDeque::new(),
                bytes: max_bytes,
                frames: 0,
                peak_bytes: 0,
                peak_frames: 0,
                error: None,
                stopping: false,
            }),
            ready: Condvar::new(),
        });
        let mut worker = RecordingWorker {
            shared,
            thread: None,
            headers: None,
            event_open: false,
            max_bytes,
        };
        assert!(
            worker
                .write(1, 0, UNIX_EPOCH, &sample(), &PacketInfo::default())
                .unwrap_err()
                .to_string()
                .contains("backlog")
        );
        {
            let mut state = worker.shared.state.lock().unwrap();
            state.bytes = 0;
            state.frames = MAX_FRAMES;
        }
        assert!(
            worker
                .write(1, 0, UNIX_EPOCH, &sample(), &PacketInfo::default())
                .is_err()
        );
        worker.shared.state.lock().unwrap().error = Some("simulated storage failure".into());
        assert!(
            worker
                .check_health()
                .unwrap_err()
                .to_string()
                .contains("simulated storage failure")
        );
    }
    struct FailingOutput;
    impl Output for FailingOutput {
        fn headers(&mut self, _: &[u8]) -> Result<()> {
            Ok(())
        }
        fn frame(&mut self, _: &Frame) -> Result<()> {
            anyhow::bail!("simulated write failure")
        }
        fn finish(&mut self) -> Result<()> {
            Ok(())
        }
    }
    #[test]
    fn disk_write_failure_is_returned_after_join() {
        let mut worker = RecordingWorker::spawn(FailingOutput, &settings()).unwrap();
        worker
            .write(1, 0, UNIX_EPOCH, &sample(), &PacketInfo::default())
            .unwrap();
        assert!(
            worker
                .shutdown()
                .unwrap_err()
                .to_string()
                .contains("simulated write failure")
        );
    }
    #[test]
    fn threaded_recordings_match_inline_files_byte_for_byte() {
        let root = std::env::temp_dir().join(format!(
            "pivigil-threaded-writer-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let left = root.join("inline");
        let right = root.join("threaded");
        fs::create_dir_all(&left).unwrap();
        fs::create_dir(&right).unwrap();
        let camera = Camera::default();
        let base = UNIX_EPOCH + Duration::from_secs(1_794_420_000);
        let mut inline = ClipWriter::new(&left, 2, &camera);
        inline
            .inspect(&[0, 0, 1, 0x67, 0x64, 0, 0x28, 0, 0, 1, 0x68, 0x34])
            .unwrap();
        let mut worker = RecordingWorker::new(
            &Recording {
                output_dir: right.clone(),
                max_file_seconds: 2,
                ..settings()
            },
            &camera,
        )
        .unwrap();
        worker
            .headers(&[0x67, 0x64, 0, 0x28], &[0x68, 0x34])
            .unwrap();
        for index in 0..60u64 {
            if index == 35 {
                inline.finish().unwrap();
                worker.finish_event().unwrap();
            }
            let event = if index < 35 { 1 } else { 2 };
            let packet = [
                0,
                0,
                0,
                1,
                if index % 15 == 0 || index == 35 {
                    0x65
                } else {
                    0x41
                },
                index as u8,
            ];
            let info = inline.inspect(&packet).unwrap();
            let timestamp = index * 66_667;
            let at = base + Duration::from_micros(timestamp);
            let data = PacketData::AnnexB(Cow::Borrowed(&packet));
            inline
                .write_packet(event, timestamp, at, &data, &info)
                .unwrap();
            worker.write(event, timestamp, at, &data, &info).unwrap();
        }
        inline.finish().unwrap();
        worker.shutdown().unwrap();
        let mut names: Vec<_> = fs::read_dir(&left)
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        names.sort();
        assert_eq!(names.len(), 3);
        assert_eq!(fs::read_dir(&right).unwrap().count(), names.len());
        for name in names {
            assert_eq!(
                fs::read(left.join(&name)).unwrap(),
                fs::read(right.join(&name)).unwrap()
            );
        }
        fs::remove_dir_all(root).unwrap();
    }
}
