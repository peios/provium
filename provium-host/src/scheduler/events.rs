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
use std::path::Path;
use std::sync::Mutex;
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
    pub fn file_with_mode(
        path: impl AsRef<Path>,
        truncate: bool,
    ) -> std::io::Result<Self> {
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

struct UnixSocketSinkInner {
    clients: Mutex<Vec<std::os::unix::net::UnixStream>>,
    /// Path the listener was bound to. Recorded so the Drop
    /// impl can unlink the socket file — without this, every
    /// provium exit leaves a stale socket on disk that confuses
    /// liveness checks (consumer scripts often `[ -S path ]` to
    /// see whether the daemon is up).
    path: std::path::PathBuf,
}

impl Drop for UnixSocketSinkInner {
    fn drop(&mut self) {
        // Best-effort unlink; if another process has rebound
        // already, removal will fail and that's fine.
        let _ = std::fs::remove_file(&self.path);
    }
}

impl std::fmt::Debug for UnixSocketSink {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("UnixSocketSink").finish()
    }
}

impl UnixSocketSink {
    /// Bind on `path`. Removes any pre-existing socket file first.
    pub fn bind(path: &Path) -> std::io::Result<Self> {
        let _ = std::fs::remove_file(path);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let listener = std::os::unix::net::UnixListener::bind(path)?;
        let inner = std::sync::Arc::new(UnixSocketSinkInner {
            clients: Mutex::new(Vec::new()),
            path: path.to_path_buf(),
        });
        let inner_for_thread = std::sync::Arc::clone(&inner);
        std::thread::Builder::new()
            .name("provium-events-listener".into())
            .spawn(move || {
                for stream in listener.incoming().flatten() {
                    // Bound write blocking so a stalled subscriber
                    // can't wedge the emit path (which holds the
                    // clients Mutex). 100ms is generous for a
                    // single msgpack frame on a unix socket; on
                    // expiry `write_frame` returns Err and the
                    // emit-side `retain_mut` drops the client.
                    let _ = stream.set_write_timeout(Some(
                        std::time::Duration::from_millis(100),
                    ));
                    inner_for_thread.clients.lock().unwrap().push(stream);
                }
            })?;
        Ok(Self { inner })
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
        clients.retain_mut(|stream| {
            write_frame(stream, &frame, DEFAULT_MAX_FRAME_BYTES).is_ok()
        });
    }
}

fn now_ns() -> i64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| {
            i64::try_from(d.as_nanos()).unwrap_or(i64::MAX)
        })
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
}
