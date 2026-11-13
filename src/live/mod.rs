//! Bounded HTTP live delivery from the capture pipeline's H.264 encoder. No subprocesses.
mod mp4;
use crate::config::{Camera, Stream};
use anyhow::{Context, Result, bail, ensure};
use std::{
    collections::VecDeque,
    io::{self, Read, Write},
    net::{TcpListener, TcpStream},
    os::fd::AsRawFd,
    sync::{
        Arc, Condvar, Mutex, MutexGuard,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

const MAX_FRAMES: usize = 120;
const IO_TIMEOUT: Duration = Duration::from_secs(2);
const SOURCE_TIMEOUT: Duration = Duration::from_secs(30);
const PLAYER: &[u8] = include_bytes!("../../assets/live.html");

struct Codec {
    sps: Vec<u8>,
    pps: Vec<u8>,
    name: String,
    init: Vec<u8>,
}
struct Pending {
    timestamp_us: u64,
    idr: bool,
    data: ::mp4::Bytes,
}
struct Frame {
    sequence: u64,
    timestamp_us: u64,
    duration_us: u64,
    idr: bool,
    data: ::mp4::Bytes,
}
struct State {
    online: bool,
    generation: u64,
    codec: Option<Arc<Codec>>,
    frames: VecDeque<Arc<Frame>>,
    bytes: usize,
    next_sequence: u64,
    pending: Option<Pending>,
}

pub struct Hub {
    state: Mutex<State>,
    changed: Condvar,
    clients: AtomicUsize,
    stopped: AtomicBool,
    max_clients: usize,
    max_bytes: usize,
    width: u32,
    height: u32,
    framerate: u32,
}
impl Hub {
    pub(crate) fn new(settings: &Stream, camera: &Camera) -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(State {
                online: false,
                generation: 0,
                codec: None,
                frames: VecDeque::with_capacity(MAX_FRAMES),
                bytes: 0,
                next_sequence: 0,
                pending: None,
            }),
            changed: Condvar::new(),
            clients: AtomicUsize::new(0),
            stopped: AtomicBool::new(false),
            max_clients: settings.max_clients,
            max_bytes: settings.max_buffer_mb * 1024 * 1024,
            width: camera.width,
            height: camera.height,
            framerate: camera.framerate,
        })
    }
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }
    pub fn has_clients(&self) -> bool {
        self.clients.load(Ordering::Relaxed) > 0
    }
    pub fn capture(self: &Arc<Self>) -> Capture {
        let mut state = self.lock();
        state.online = true;
        state.codec = None;
        state.frames.clear();
        state.bytes = 0;
        state.pending = None;
        drop(state);
        self.changed.notify_all();
        Capture(Arc::clone(self))
    }
    pub fn update_codec(&self, sps: &[u8], pps: &[u8]) {
        {
            let state = self.lock();
            if state
                .codec
                .as_ref()
                .is_some_and(|codec| codec.sps == sps && codec.pps == pps)
            {
                return;
            }
        }
        let Ok(init) = mp4::initialization(sps, pps, self.width, self.height, self.framerate)
        else {
            return;
        };
        let codec = Arc::new(Codec {
            sps: sps.to_vec(),
            pps: pps.to_vec(),
            name: format!("avc1.{:02X}{:02X}{:02X}", sps[1], sps[2], sps[3]),
            init,
        });
        let mut state = self.lock();
        if state.codec.is_some() {
            state.generation = state.generation.wrapping_add(1);
            state.frames.clear();
            state.bytes = 0;
            state.pending = None;
        }
        state.codec = Some(codec);
        drop(state);
        self.changed.notify_all();
    }
    pub fn publish(&self, timestamp_us: u64, idr: bool, data: ::mp4::Bytes) {
        if !self.has_clients() {
            return;
        }
        let mut state = self.lock();
        if !self.has_clients() || !state.online {
            return;
        }
        let current = Pending {
            timestamp_us,
            idr,
            data,
        };
        if let Some(previous) = state.pending.replace(current) {
            if timestamp_us <= previous.timestamp_us {
                return;
            }
            let sequence = state.next_sequence;
            state.next_sequence = state.next_sequence.wrapping_add(1);
            if previous.data.len() > self.max_bytes {
                state.frames.clear();
                state.bytes = 0;
            } else {
                state.bytes += previous.data.len();
                state.frames.push_back(Arc::new(Frame {
                    sequence,
                    timestamp_us: previous.timestamp_us,
                    duration_us: timestamp_us - previous.timestamp_us,
                    idr: previous.idr,
                    data: previous.data,
                }));
                while state.bytes > self.max_bytes || state.frames.len() > MAX_FRAMES {
                    if let Some(frame) = state.frames.pop_front() {
                        state.bytes -= frame.data.len();
                    }
                }
            }
        }
        drop(state);
        self.changed.notify_all();
    }
    fn subscribe(self: &Arc<Self>) -> Result<Viewer> {
        let state = self.lock();
        ensure!(!self.stopped.load(Ordering::Relaxed), "stream stopped");
        ensure!(
            self.clients.load(Ordering::Relaxed) < self.max_clients,
            "live viewer limit reached"
        );
        self.clients.fetch_add(1, Ordering::Relaxed);
        Ok(Viewer {
            hub: Arc::clone(self),
            generation: state.generation,
            cursor: state.next_sequence,
            started: false,
        })
    }
    fn status(&self) -> String {
        let state = self.lock();
        format!(
            "{{\"camera_online\":{},\"codec_ready\":{},\"viewers\":{},\"max_viewers\":{},\"width\":{},\"height\":{}}}\n",
            state.online,
            state.codec.is_some(),
            self.clients.load(Ordering::Relaxed),
            self.max_clients,
            self.width,
            self.height
        )
    }
}
pub struct Capture(Arc<Hub>);
impl Drop for Capture {
    fn drop(&mut self) {
        let mut state = self.0.lock();
        state.online = false;
        state.generation = state.generation.wrapping_add(1);
        state.codec = None;
        state.frames.clear();
        state.bytes = 0;
        state.pending = None;
        drop(state);
        self.0.changed.notify_all();
    }
}
struct Viewer {
    hub: Arc<Hub>,
    generation: u64,
    cursor: u64,
    started: bool,
}
impl Viewer {
    fn next(&mut self, timeout: Duration) -> Result<(Arc<Codec>, Arc<Frame>)> {
        let until = Instant::now() + timeout;
        let mut state = self.hub.lock();
        loop {
            ensure!(
                !self.hub.stopped.load(Ordering::Relaxed) && state.generation == self.generation,
                "live capture ended"
            );
            if state
                .frames
                .front()
                .is_some_and(|frame| self.cursor < frame.sequence)
            {
                self.started = false;
                self.cursor = state.frames.front().unwrap().sequence;
            }
            if let Some(codec) = &state.codec {
                let frame = if self.started {
                    state.frames.iter().find(|f| f.sequence >= self.cursor)
                } else {
                    state
                        .frames
                        .iter()
                        .rev()
                        .find(|f| f.sequence >= self.cursor && f.idr)
                };
                if let Some(frame) = frame {
                    self.cursor = frame.sequence.wrapping_add(1);
                    self.started = true;
                    return Ok((Arc::clone(codec), Arc::clone(frame)));
                }
            }
            let remaining = until.saturating_duration_since(Instant::now());
            ensure!(!remaining.is_zero(), "live source timed out");
            let (guard, _) = self
                .hub
                .changed
                .wait_timeout(state, remaining.min(Duration::from_secs(1)))
                .unwrap_or_else(|e| e.into_inner());
            state = guard;
        }
    }
}
impl Drop for Viewer {
    fn drop(&mut self) {
        if self.hub.clients.fetch_sub(1, Ordering::Relaxed) == 1 {
            let mut state = self.hub.lock();
            state.frames.clear();
            state.bytes = 0;
            state.pending = None;
        }
    }
}

pub struct LiveServer {
    hub: Arc<Hub>,
    worker: Option<JoinHandle<()>>,
    #[cfg(test)]
    address: std::net::SocketAddr,
}
impl LiveServer {
    pub fn start(settings: &Stream, camera: &Camera) -> Result<Option<Self>> {
        if !settings.enabled {
            return Ok(None);
        }
        let listener = TcpListener::bind(settings.listen)
            .with_context(|| format!("bind live endpoint {}", settings.listen))?;
        listener.set_nonblocking(true)?;
        let address = listener.local_addr()?;
        let hub = Hub::new(settings, camera);
        let state = Arc::clone(&hub);
        let worker = thread::Builder::new()
            .name("live-http".into())
            .stack_size(128 * 1024)
            .spawn(move || accept(listener, state))?;
        crate::log(format!(
            "live HTTP viewer: http://{}/; video: /live.mp4",
            address
        ));
        Ok(Some(Self {
            hub,
            worker: Some(worker),
            #[cfg(test)]
            address,
        }))
    }
    pub fn hub(&self) -> &Arc<Hub> {
        &self.hub
    }
}
impl Drop for LiveServer {
    fn drop(&mut self) {
        self.hub.stopped.store(true, Ordering::Relaxed);
        self.hub.changed.notify_all();
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}
struct Connection(Arc<AtomicUsize>);
impl Drop for Connection {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::Relaxed);
    }
}
fn accept(listener: TcpListener, hub: Arc<Hub>) {
    let connections = Arc::new(AtomicUsize::new(0));
    let mut workers: Vec<JoinHandle<()>> = Vec::new();
    while !hub.stopped.load(Ordering::Relaxed) {
        let mut index = 0;
        while index < workers.len() {
            if workers[index].is_finished() {
                let _ = workers.swap_remove(index).join();
            } else {
                index += 1;
            }
        }
        match listener.accept() {
            Ok((mut socket, _)) => {
                if connections.load(Ordering::Relaxed) >= hub.max_clients + 8 {
                    let _ = response(
                        &mut socket,
                        "503 Service Unavailable",
                        "text/plain",
                        b"Busy\n",
                        false,
                    );
                    continue;
                }
                connections.fetch_add(1, Ordering::Relaxed);
                let guard = Connection(Arc::clone(&connections));
                let state = Arc::clone(&hub);
                if let Ok(worker) = thread::Builder::new()
                    .name("live-client".into())
                    .stack_size(128 * 1024)
                    .spawn(move || {
                        let _guard = guard;
                        let _ = serve(socket, &state);
                    })
                {
                    workers.push(worker);
                }
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                let mut poll = libc::pollfd {
                    fd: listener.as_raw_fd(),
                    events: libc::POLLIN,
                    revents: 0,
                };
                // SAFETY: poll receives one initialized descriptor entry.
                unsafe {
                    libc::poll(&mut poll, 1, 100);
                }
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(_) => break,
        }
    }
    hub.stopped.store(true, Ordering::Relaxed);
    hub.changed.notify_all();
    for worker in workers {
        let _ = worker.join();
    }
}
fn headers(socket: &mut TcpStream) -> Result<String> {
    let until = Instant::now() + IO_TIMEOUT;
    let mut bytes = Vec::with_capacity(1024);
    loop {
        let left = until.saturating_duration_since(Instant::now());
        ensure!(!left.is_zero(), "HTTP request timed out");
        socket.set_read_timeout(Some(left))?;
        let mut part = [0u8; 1024];
        let count = socket.read(&mut part)?;
        ensure!(count > 0, "HTTP request ended");
        bytes.extend_from_slice(&part[..count]);
        ensure!(bytes.len() <= 8192, "HTTP request too large");
        if bytes.windows(4).any(|s| s == b"\r\n\r\n") {
            return Ok(String::from_utf8(bytes)?);
        }
    }
}
fn response(
    socket: &mut TcpStream,
    status: &str,
    kind: &str,
    body: &[u8],
    head: bool,
) -> Result<()> {
    socket.set_write_timeout(Some(IO_TIMEOUT))?;
    write!(
        socket,
        "HTTP/1.1 {status}\r\nContent-Type: {kind}\r\nContent-Length: {}\r\nCache-Control: no-store\r\nX-Content-Type-Options: nosniff\r\nConnection: close\r\n\r\n",
        body.len()
    )?;
    if !head {
        socket.write_all(body)?;
    }
    Ok(())
}
fn chunk(socket: &mut TcpStream, parts: &[&[u8]]) -> Result<()> {
    let until = Instant::now() + IO_TIMEOUT;
    let size: usize = parts.iter().map(|p| p.len()).sum();
    let prefix = format!("{size:X}\r\n");
    for part in std::iter::once(prefix.as_bytes())
        .chain(parts.iter().copied())
        .chain(std::iter::once(b"\r\n".as_slice()))
    {
        let mut pending = part;
        while !pending.is_empty() {
            let left = until.saturating_duration_since(Instant::now());
            ensure!(!left.is_zero(), "live client is too slow");
            socket.set_write_timeout(Some(left))?;
            match socket.write(pending) {
                Ok(0) => bail!("live client closed"),
                Ok(n) => pending = &pending[n..],
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => return Err(e.into()),
            }
        }
    }
    Ok(())
}
fn serve(mut socket: TcpStream, hub: &Arc<Hub>) -> Result<()> {
    socket.set_nodelay(true)?;
    let request = match headers(&mut socket) {
        Ok(request) => request,
        Err(_) => {
            return response(
                &mut socket,
                "400 Bad Request",
                "text/plain",
                b"Invalid request\n",
                false,
            );
        }
    };
    let mut line = request.lines().next().unwrap_or("").split_whitespace();
    let method = line.next().unwrap_or("");
    let path = line.next().unwrap_or("").split('?').next().unwrap_or("");
    let head = method == "HEAD";
    if method != "GET" && !head {
        return response(
            &mut socket,
            "405 Method Not Allowed",
            "text/plain",
            b"GET or HEAD required\n",
            false,
        );
    }
    match path {
        "/" => response(
            &mut socket,
            "200 OK",
            "text/html; charset=utf-8",
            PLAYER,
            head,
        ),
        "/status.json" => response(
            &mut socket,
            "200 OK",
            "application/json",
            hub.status().as_bytes(),
            head,
        ),
        "/live.mp4" if head => {
            socket.set_write_timeout(Some(IO_TIMEOUT))?;
            socket.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: video/mp4\r\nCache-Control: no-store\r\nAccept-Ranges: none\r\nConnection: close\r\n\r\n")?;
            Ok(())
        }
        "/live.mp4" => {
            let mut viewer = match hub.subscribe() {
                Ok(viewer) => viewer,
                Err(_) => {
                    return response(
                        &mut socket,
                        "503 Service Unavailable",
                        "text/plain",
                        b"Live viewer limit reached\n",
                        false,
                    );
                }
            };
            let (codec, first) = match viewer.next(SOURCE_TIMEOUT) {
                Ok(frame) => frame,
                Err(_) => {
                    return response(
                        &mut socket,
                        "503 Service Unavailable",
                        "text/plain",
                        b"Camera unavailable\n",
                        false,
                    );
                }
            };
            socket.set_write_timeout(Some(IO_TIMEOUT))?;
            write!(
                socket,
                "HTTP/1.1 200 OK\r\nContent-Type: video/mp4\r\nX-Video-Codec: {}\r\nTransfer-Encoding: chunked\r\nCache-Control: no-store\r\nAccept-Ranges: none\r\nConnection: close\r\n\r\n",
                codec.name
            )?;
            chunk(&mut socket, &[&codec.init])?;
            let base = first.timestamp_us;
            let mut next = Some(first);
            let mut sequence = 1u32;
            loop {
                let frame = match next.take() {
                    Some(frame) => frame,
                    None => match viewer.next(SOURCE_TIMEOUT) {
                        Ok((_, frame)) => frame,
                        Err(_) => break,
                    },
                };
                let prefix = mp4::fragment(
                    sequence,
                    frame.timestamp_us,
                    frame.duration_us,
                    base,
                    frame.data.len(),
                    frame.idr,
                )?;
                chunk(&mut socket, &[&prefix, &frame.data])?;
                sequence = sequence.wrapping_add(1);
            }
            let _ = socket.write_all(b"0\r\n\r\n");
            Ok(())
        }
        _ => response(
            &mut socket,
            "404 Not Found",
            "text/plain",
            b"Not found\n",
            head,
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader};
    fn codec(hub: &Hub) {
        hub.update_codec(&[0x67, 0x64, 0, 0x2a], &[0x68, 0x34]);
    }
    fn frame(hub: &Hub, timestamp: u64, idr: bool, size: usize) {
        hub.publish(timestamp, idr, vec![42; size].into());
    }
    fn hub() -> Arc<Hub> {
        Hub::new(&Stream::default(), &Camera::default())
    }
    #[test]
    fn viewers_wait_for_idr_and_receive_live_frames_in_order() {
        let hub = hub();
        let _capture = hub.capture();
        codec(&hub);
        let mut first = hub.subscribe().unwrap();
        frame(&hub, 0, false, 10);
        frame(&hub, 100, true, 10);
        assert!(first.next(Duration::from_millis(1)).is_err());
        frame(&hub, 200, false, 10);
        assert_eq!(
            first.next(Duration::from_millis(1)).unwrap().1.timestamp_us,
            100
        );
        frame(&hub, 300, false, 10);
        assert_eq!(
            first.next(Duration::from_millis(1)).unwrap().1.timestamp_us,
            200
        );
        let mut second = hub.subscribe().unwrap();
        assert!(hub.subscribe().is_err());
        frame(&hub, 400, true, 10);
        frame(&hub, 500, false, 10);
        assert_eq!(
            second
                .next(Duration::from_millis(1))
                .unwrap()
                .1
                .timestamp_us,
            400
        );
        drop(first);
        drop(second);
        let state = hub.lock();
        assert!(state.frames.is_empty() && state.pending.is_none());
    }
    #[test]
    fn slow_viewer_resynchronizes_without_blocking_or_unbounded_memory() {
        let mut hub = hub();
        Arc::get_mut(&mut hub).unwrap().max_bytes = 1024;
        let _capture = hub.capture();
        codec(&hub);
        let mut viewer = hub.subscribe().unwrap();
        frame(&hub, 0, true, 128);
        frame(&hub, 100, false, 128);
        assert_eq!(
            viewer
                .next(Duration::from_millis(1))
                .unwrap()
                .1
                .timestamp_us,
            0
        );
        for i in 2..30 {
            frame(&hub, i * 100, i % 5 == 0, 128);
        }
        assert!(hub.lock().bytes <= 1024);
        let (_, next) = viewer.next(Duration::from_millis(1)).unwrap();
        assert!(next.idr);
        assert_eq!(next.timestamp_us, 2500);
    }
    #[test]
    fn source_restart_and_codec_changes_end_old_connections() {
        let hub = hub();
        let capture = hub.capture();
        codec(&hub);
        let mut viewer = hub.subscribe().unwrap();
        drop(capture);
        assert!(viewer.next(Duration::from_millis(1)).is_err());
        drop(viewer);
        let _capture = hub.capture();
        codec(&hub);
        let mut viewer = hub.subscribe().unwrap();
        hub.update_codec(&[0x67, 0x64, 0, 0x28], &[0x68, 0x34]);
        assert!(viewer.next(Duration::from_millis(1)).is_err());
    }
    #[test]
    fn no_viewers_keep_no_encoded_frame_backlog() {
        let hub = hub();
        let _capture = hub.capture();
        codec(&hub);
        for i in 0..200 {
            frame(&hub, i * 100, true, 8192);
        }
        assert!(hub.lock().frames.is_empty() && hub.lock().pending.is_none());
    }
    fn server() -> LiveServer {
        LiveServer::start(
            &Stream {
                enabled: true,
                listen: "127.0.0.1:0".parse().unwrap(),
                ..Stream::default()
            },
            &Camera::default(),
        )
        .unwrap()
        .unwrap()
    }
    fn request(server: &LiveServer, request: &[u8]) -> Vec<u8> {
        let mut socket = TcpStream::connect(server.address).unwrap();
        socket
            .set_read_timeout(Some(Duration::from_secs(3)))
            .unwrap();
        socket.write_all(request).unwrap();
        let mut bytes = Vec::new();
        socket.read_to_end(&mut bytes).unwrap();
        bytes
    }
    #[test]
    fn http_viewer_status_errors_and_capacity_are_served() {
        let server = server();
        let page = request(&server, b"GET / HTTP/1.1\r\nHost: localhost\r\n\r\n");
        assert!(page.starts_with(b"HTTP/1.1 200"));
        assert!(page.windows(11).any(|s| s == b"MediaSource"));
        assert!(
            request(&server, b"GET /status.json HTTP/1.1\r\n\r\n")
                .windows(21)
                .any(|s| s == b"\"camera_online\":false")
        );
        assert!(request(&server, b"GET /missing HTTP/1.1\r\n\r\n").starts_with(b"HTTP/1.1 404"));
        assert!(request(&server, b"POST / HTTP/1.1\r\n\r\n").starts_with(b"HTTP/1.1 405"));
        let head = request(&server, b"HEAD /live.mp4 HTTP/1.1\r\n\r\n");
        assert!(head.starts_with(b"HTTP/1.1 200"));
        assert!(!head.windows(14).any(|s| s == b"Content-Length"));
        let _one = server.hub.subscribe().unwrap();
        let _two = server.hub.subscribe().unwrap();
        assert!(request(&server, b"GET /live.mp4 HTTP/1.1\r\n\r\n").starts_with(b"HTTP/1.1 503"));
    }
    fn read_chunk(reader: &mut BufReader<TcpStream>) -> Vec<u8> {
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        let size = usize::from_str_radix(line.trim(), 16).unwrap();
        let mut bytes = vec![0; size];
        reader.read_exact(&mut bytes).unwrap();
        let mut end = [0; 2];
        reader.read_exact(&mut end).unwrap();
        assert_eq!(&end, b"\r\n");
        bytes
    }
    #[test]
    fn http_stream_contains_initialization_and_decodable_boundaries() {
        let server = server();
        let capture = server.hub.capture();
        codec(&server.hub);
        let mut socket = TcpStream::connect(server.address).unwrap();
        socket
            .set_read_timeout(Some(Duration::from_secs(3)))
            .unwrap();
        socket.write_all(b"GET /live.mp4 HTTP/1.1\r\n\r\n").unwrap();
        for _ in 0..100 {
            if server.hub.has_clients() {
                break;
            }
            thread::sleep(Duration::from_millis(2));
        }
        frame(&server.hub, 100, true, 6);
        frame(&server.hub, 66_767, false, 6);
        let mut reader = BufReader::new(socket);
        let mut header = String::new();
        loop {
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            if line == "\r\n" {
                break;
            }
            header.push_str(&line);
        }
        assert!(header.contains("X-Video-Codec: avc1.64002A"));
        let init = read_chunk(&mut reader);
        assert_eq!(&init[4..8], b"ftyp");
        let fragment = read_chunk(&mut reader);
        assert_eq!(&fragment[4..8], b"moof");
        assert_eq!(&fragment[104..108], b"mdat");
        assert_eq!(fragment.len(), 114);
        drop(capture);
        assert!(read_chunk(&mut reader).is_empty());
    }
}
