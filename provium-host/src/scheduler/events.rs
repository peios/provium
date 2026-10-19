//! Event sinks + emitter — host-side fan-out of the
//! observability event stream defined in
//! [`provium_protocol::events`].
//!
//! Slice 12 emits a useful subset:
//!
//! * `file_dispatched` — once per file at the start of its runner.
//! * `file_completed` — once per file with the aggregated outcome.
//! * `test_started` / `test_passed` / `test_failed` — once per
//!   `test()` block.
//!
//! Slices 5.5+ will fill in `pool_state`, `claim_acquired`,
//! `vm_spawned`, `vm_shutdown`, `fixture_*` as the corresponding
//! features land. Adding more emitters is mechanical; the wire
//! types are already complete.
//!
//! ## Sink layering
//!
//! [`EventSink`] is a one-method trait. [`MultiSink`] composes
//! multiple sinks so the binary can fan stdout + `--save-events
//! <file>` simultaneously without bespoke plumbing in the
//! scheduler.

use std::fs::OpenOptions;
use std::io::Write;
use std::net::Shutdown;
use std::os::fd::AsRawFd;
use std::os::unix::fs::MetadataExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::SystemTime;

use provium_protocol::events::{Event, EventFrame};
use provium_protocol::frame::{write_frame, DEFAULT_MAX_FRAME_BYTES};

/// One frame writer. Called from the scheduler / runners as events
/// are produced. Failures are logged + swallowed — losing an event
/// must never break a test run.
pub trait EventSink: Send + Sync {
    /// Emit an event. The implementation captures the wall-clock
    /// timestamp; callers don't need to.
    fn emit(&self, event: Event);
}

/// No-op sink. Used by integration tests and any binary that
/// intentionally discards events.
#[derive(Debug, Default)]
pub struct NullSink;

impl EventSink for NullSink {
    fn emit(&self, _event: Event) {}
}

/// Sink that writes msgpack-framed events to an [`std::io::Write`].
///
/// Owns the writer behind a [`Mutex`] so calls from parallel runner
/// threads serialise cleanly. Buffered for throughput; explicitly
/// flushed in [`Drop`].
pub struct WriteSink<W: Write + Send> {
    writer: Mutex<W>,
}

impl<W: Write + Send + std::fmt::Debug> std::fmt::Debug for WriteSink<W> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WriteSink").finish()
    }
}

impl<W: Write + Send> WriteSink<W> {
    /// Wrap an arbitrary writer. Callers usually construct via the
    /// [`Self::file`] / [`Self::stdout_buffered`] helpers below.
    pub fn new(writer: W) -> Self {
        Self {
            writer: Mutex::new(writer),
        }
    }
}

impl<W: Write + Send> EventSink for WriteSink<W> {
    fn emit(&self, event: Event) {
        let frame = EventFrame {
            ts: now_ns(),
            event,
        };
        let mut writer = match self.writer.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        if let Err(e) = write_frame(&mut *writer, &frame, DEFAULT_MAX_FRAME_BYTES) {
            eprintln!("provium: event emit failed: {e}");
            return;
        }
        // Flush per-event so the file is up-to-date even if a
        // detached background thread (e.g. the pool_state emitter)
        // dies on process-exit before BufWriter::Drop fires.
        if let Err(e) = writer.flush() {
            eprintln!("provium: event flush failed: {e}");
        }
    }
}

impl WriteSink<LockedFile> {
    /// Open `path` for append. Each `emit` takes an advisory
    /// exclusive `flock(LOCK_EX)` over the file before writing
    /// the framed event, so multiple `provium --save-events`
    /// processes pointed at the same file (e.g. when running
    /// nested suites under the same dashboard) interleave at
    /// frame boundaries instead of mid-frame. Buffering would
    /// fight the lock semantics, so the writer is unbuffered —
    /// which is fine because [`Self::emit`] already flushes per
    /// event.
    pub fn file(path: impl AsRef<Path>) -> std::io::Result<Self> {
        Self::file_with_mode(path, false)
    }

    /// Like [`Self::file`] but takes a `truncate` flag. Used by
    /// `--watch` so each iteration starts with a fresh event log
    /// (otherwise every iteration's frames pile up in the same
    /// file and replay tools see runs concatenated together).
    pub fn file_with_mode(path: impl AsRef<Path>, truncate: bool) -> std::io::Result<Self> {
        let f = OpenOptions::new()
            .create(true)
            .truncate(truncate)
            .append(!truncate)
            .write(truncate)
            .open(path)?;
        Ok(Self::new(LockedFile::new(f)))
    }
}

/// Wraps a [`std::fs::File`] so that every `Write` call is
/// bracketed by an advisory `flock(LOCK_EX)` /
/// `flock(LOCK_UN)`. Used by [`WriteSink::file`] so concurrent
/// `--save-events` processes don't tear msgpack frames.
#[derive(Debug)]
pub struct LockedFile(std::fs::File);

impl LockedFile {
    /// Wrap an existing file. The fd is *not* locked at
    /// construction — locking happens per `Write::write` call.
    pub fn new(f: std::fs::File) -> Self {
        Self(f)
    }
}

impl Write for LockedFile {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        // Routed through write_all so the LOCK_EX bracket covers
        // any retry loop the caller would otherwise drive (and
        // matches the per-frame atomicity contract advertised on
        // WriteSink::file).
        self.write_all(buf)?;
        Ok(buf.len())
    }

    fn write_all(&mut self, buf: &[u8]) -> std::io::Result<()> {
        use std::os::fd::AsRawFd;
        let fd = self.0.as_raw_fd();
        // SAFETY: fd is valid for the lifetime of self.0; flock is
        // a kernel-level advisory lock with no aliasing concerns.
        let lock = unsafe { libc::flock(fd, libc::LOCK_EX) };
        if lock != 0 {
            return Err(std::io::Error::last_os_error());
        }
        // Drive write_all manually so EINTR / partial writes retry
        // *under* the lock, instead of dropping it between syscall
        // attempts (which would let a concurrent writer wedge a
        // header between our header and body).
        let mut cursor = 0;
        let mut result: std::io::Result<()> = Ok(());
        while cursor < buf.len() {
            match self.0.write(&buf[cursor..]) {
                Ok(0) => {
                    result = Err(std::io::Error::new(
                        std::io::ErrorKind::WriteZero,
                        "LockedFile: write returned 0",
                    ));
                    break;
                }
                Ok(n) => cursor += n,
                Err(ref e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(e) => {
                    result = Err(e);
                    break;
                }
            }
        }
        // Unlock unconditionally — `?` would skip it on write
        // error, leaving the lock held until the fd closes.
        unsafe {
            libc::flock(fd, libc::LOCK_UN);
        }
        result
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.0.flush()
    }
}

/// Compose multiple sinks. Events fan out to every wrapped sink in
/// the order the binary registered them.
pub struct MultiSink {
    sinks: Vec<Box<dyn EventSink>>,
}

impl std::fmt::Debug for MultiSink {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MultiSink")
            .field("sink_count", &self.sinks.len())
            .finish()
    }
}

impl MultiSink {
    /// Build from a vec of boxed sinks.
    pub fn new(sinks: Vec<Box<dyn EventSink>>) -> Self {
        Self { sinks }
    }
}

impl EventSink for MultiSink {
    fn emit(&self, event: Event) {
        for sink in &self.sinks {
            sink.emit(event.clone());
        }
    }
}

/// Unix-socket multiplexing sink. Binds on construction; spawns a
/// listener thread that accepts new clients and adds their write
/// halves to the live-fanout list. Frames go to every connected
/// client; failed writes silently drop the client.
#[derive(Clone)]
pub struct UnixSocketSink {
    inner: std::sync::Arc<UnixSocketSinkInner>,
}

type EventClients = Arc<Mutex<Vec<UnixStream>>>;

struct UnixSocketSinkInner {
    clients: EventClients,
    stop: UnixStream,
    listener: Option<JoinHandle<()>>,
    // Removed after the listener has stopped and released its descriptor.
    _socket_path: SocketPath,
}

impl Drop for UnixSocketSinkInner {
    fn drop(&mut self) {
        // The worker owns only the client list, never this owner. Closing the
        // control stream wakes its poll even when there are no incoming clients.
        let _ = self.stop.shutdown(Shutdown::Both);
        if let Some(listener) = self.listener.take() {
            let _ = listener.join();
        }
    }
}

struct SocketPath {
    path: PathBuf,
    device: u64,
    inode: u64,
}

impl SocketPath {
    fn record(path: PathBuf) -> std::io::Result<Self> {
        let metadata = std::fs::symlink_metadata(&path)?;
        Ok(Self {
            path,
            device: metadata.dev(),
            inode: metadata.ino(),
        })
    }
}

impl Drop for SocketPath {
    fn drop(&mut self) {
        // A later listener may have rebound this name. Leave its socket alone.
        if let Ok(metadata) = std::fs::symlink_metadata(&self.path) {
            if metadata.dev() == self.device && metadata.ino() == self.inode {
                let _ = std::fs::remove_file(&self.path);
            }
        }
    }
}

impl std::fmt::Debug for UnixSocketSink {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("UnixSocketSink").finish()
    }
}

impl UnixSocketSink {
    /// Bind on `path`. Removes any pre-existing socket file first.
    /// The last clone stops and joins the listener, then removes its socket.
    pub fn bind(path: &Path) -> std::io::Result<Self> {
        let path = std::path::absolute(path)?;
        let _ = std::fs::remove_file(&path);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let listener = UnixListener::bind(&path)?;
        let socket_path = SocketPath::record(path)?;
        listener.set_nonblocking(true)?;
        let (stop, stop_receiver) = UnixStream::pair()?;
        let clients = Arc::new(Mutex::new(Vec::new()));
        let worker_clients = Arc::clone(&clients);
        let worker = std::thread::Builder::new()
            .name("provium-events-listener".into())
            .spawn(move || accept_event_clients(listener, stop_receiver, worker_clients))?;
        Ok(Self {
            inner: Arc::new(UnixSocketSinkInner {
                clients,
                stop,
                listener: Some(worker),
                _socket_path: socket_path,
            }),
        })
    }
}

/// Wait without periodic wakeups; the control stream makes shutdown immediate.
fn wait_for_event_client(listener: &UnixListener, stop: &UnixStream) -> std::io::Result<bool> {
    let mut descriptors = [
        libc::pollfd {
            fd: listener.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        },
        libc::pollfd {
            fd: stop.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        },
    ];
    loop {
        // SAFETY: both borrowed streams keep their descriptors alive. The array
        // contains exactly two initialized pollfd values and remains writable
        // for the duration of poll; no other thread accesses the array.
        let ready = unsafe { libc::poll(descriptors.as_mut_ptr(), 2, -1) };
        if ready >= 0 {
            return Ok(descriptors[1].revents == 0 && descriptors[0].revents & libc::POLLIN != 0);
        }
        let error = std::io::Error::last_os_error();
        if error.kind() != std::io::ErrorKind::Interrupted {
            return Err(error);
        }
    }
}

fn accept_event_clients(listener: UnixListener, stop: UnixStream, clients: EventClients) {
    loop {
        match wait_for_event_client(&listener, &stop) {
            Ok(true) => {}
            Ok(false) => return,
            Err(error) => {
                eprintln!("provium: event listener failed: {error}");
                return;
            }
        }
        match listener.accept() {
            Ok((stream, _)) => {
                // Keep the existing per-client write timeout. A client is only
                // published once the timeout has been installed successfully.
                if stream
                    .set_write_timeout(Some(std::time::Duration::from_millis(100)))
                    .is_ok()
                {
                    clients.lock().unwrap().push(stream);
                }
            }
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::Interrupted | std::io::ErrorKind::WouldBlock
                ) => {}
            Err(error) => {
                eprintln!("provium: event accept failed: {error}");
                return;
            }
        }
    }
}

impl EventSink for UnixSocketSink {
    fn emit(&self, event: Event) {
        let frame = EventFrame {
            ts: now_ns(),
            event,
        };
        let mut clients = self.inner.clients.lock().unwrap();
        // Drop dead clients in-place. Stable, safe iteration via
        // retain_mut.
        clients.retain_mut(|stream| write_frame(stream, &frame, DEFAULT_MAX_FRAME_BYTES).is_ok());
    }
}

fn now_ns() -> i64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| i64::try_from(d.as_nanos()).unwrap_or(i64::MAX))
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use provium_protocol::events::{FileCompleted, FileStatus, TestPassed};
    use provium_protocol::frame::read_frame;
    use std::io::Cursor;
    use std::sync::Arc;

    #[test]
    fn null_sink_swallows() {
        let s = NullSink;
        s.emit(Event::TestPassed(TestPassed {
            path: "x".into(),
            name: "y".into(),
            duration_ns: 0,
            meta: Default::default(),
        }));
    }

    #[test]
    fn write_sink_round_trips_a_frame() {
        let mut buf = Vec::new();
        {
            let sink = WriteSink::new(&mut buf);
            sink.emit(Event::FileCompleted(FileCompleted {
                path: "a.test.lua".into(),
                status: FileStatus::Passed,
                duration_ns: 1_234_567,
            }));
        }
        let mut cursor = Cursor::new(&buf);
        let decoded: EventFrame = read_frame(&mut cursor, DEFAULT_MAX_FRAME_BYTES).unwrap();
        match decoded.event {
            Event::FileCompleted(c) => {
                assert_eq!(c.path, "a.test.lua");
                assert_eq!(c.status, FileStatus::Passed);
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn multi_sink_fans_out() {
        let counter = Arc::new(Mutex::new(0u32));

        struct Counting(Arc<Mutex<u32>>);
        impl EventSink for Counting {
            fn emit(&self, _: Event) {
                *self.0.lock().unwrap() += 1;
            }
        }

        let multi = MultiSink::new(vec![
            Box::new(Counting(Arc::clone(&counter))),
            Box::new(Counting(Arc::clone(&counter))),
            Box::new(NullSink),
        ]);

        multi.emit(Event::TestPassed(TestPassed {
            path: "x".into(),
            name: "y".into(),
            duration_ns: 0,
            meta: Default::default(),
        }));

        assert_eq!(*counter.lock().unwrap(), 2);
    }

    fn socket_directory() -> tempfile::TempDir {
        tempfile::Builder::new()
            .prefix("provium-events-")
            .tempdir_in("/tmp")
            .unwrap()
    }

    fn connect_client(sink: &UnixSocketSink, path: &Path) -> UnixStream {
        let client = UnixStream::connect(path).unwrap();
        client
            .set_read_timeout(Some(std::time::Duration::from_secs(2)))
            .unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while sink.inner.clients.lock().unwrap().is_empty() {
            assert!(
                std::time::Instant::now() < deadline,
                "listener accepted the client"
            );
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        client
    }

    #[test]
    fn last_sink_drop_stops_listener_and_closes_clients() {
        use std::io::Read;

        let directory = socket_directory();
        let path = directory.path().join("events.sock");
        let sink = UnixSocketSink::bind(&path).unwrap();
        let remaining = sink.clone();
        let mut client = connect_client(&sink, &path);
        drop(sink);
        assert!(path.exists(), "a remaining owner keeps the listener alive");
        remaining.emit(Event::TestPassed(TestPassed {
            path: "x".into(),
            name: "still-connected".into(),
            duration_ns: 0,
            meta: Default::default(),
        }));
        let frame: EventFrame = read_frame(&mut client, DEFAULT_MAX_FRAME_BYTES).unwrap();
        assert!(matches!(frame.event, Event::TestPassed(_)));
        drop(remaining);
        assert!(!path.exists(), "the final owner removes its socket");
        assert_eq!(
            client.read(&mut [0]).unwrap(),
            0,
            "clients see end of stream"
        );
        assert!(
            UnixStream::connect(&path).is_err(),
            "the listener has closed"
        );
    }

    #[test]
    fn idle_listener_stops_when_its_owner_drops() {
        let directory = socket_directory();
        let path = directory.path().join("events.sock");
        let sink = UnixSocketSink::bind(&path).unwrap();
        drop(sink);
        assert!(
            !path.exists(),
            "shutdown does not require a client connection"
        );
    }

    #[test]
    fn old_listener_cleanup_preserves_a_replacement_socket() {
        let directory = socket_directory();
        let path = directory.path().join("events.sock");
        let old = UnixSocketSink::bind(&path).unwrap();
        let replacement = UnixSocketSink::bind(&path).unwrap();
        let replacement_inode = std::fs::symlink_metadata(&path).unwrap().ino();
        drop(old);
        assert_eq!(
            std::fs::symlink_metadata(&path).unwrap().ino(),
            replacement_inode
        );
        let _client = connect_client(&replacement, &path);
        drop(replacement);
        assert!(!path.exists());
    }
}
