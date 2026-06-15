//! [`Qmp`] — the connection type and the reader-thread machinery.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{sync_channel, RecvTimeoutError, SyncSender};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use crate::error::QmpError;
use crate::event::{Event, EventMark, WireEvent};

// ---------------------------------------------------------------------------
// Default timeouts
// ---------------------------------------------------------------------------

/// Default timeout for [`Qmp::execute`] and the typed-command helpers.
/// Selected to be long enough that a healthy QMP rarely trips it but
/// short enough that a wedged QEMU surfaces as a clean diagnostic
/// rather than blocking a test forever.
pub const DEFAULT_COMMAND_TIMEOUT: Duration = Duration::from_secs(30);

// ---------------------------------------------------------------------------
// Internal shared state
// ---------------------------------------------------------------------------

/// Outcome of one command, as classified by the reader thread.
enum RawResponse {
    /// `{"return": ...}` — successful reply.
    Return(Value),
    /// `{"error": {"class": ..., "desc": ...}}` — QEMU rejected the command.
    Error { class: String, desc: String },
    /// Connection closed before a response arrived.
    Closed(&'static str),
}

/// Per-pending-command state held while a caller is waiting on its reply.
struct Pending {
    /// Original command name — preserved so [`QmpError::Command`]
    /// surfaces a useful message even though the wire reply has no
    /// command name.
    command: String,
    /// One-shot channel back to the caller.
    reply_to: SyncSender<RawResponse>,
}

/// Combined event log + closed flag, kept under a single mutex so
/// `wait_event` never has to lock twice.
struct EventLog {
    /// Every event the reader has appended, in receipt order.
    events: Vec<Event>,
    /// `true` once the reader thread has exited (clean close, I/O
    /// error, or peer crash). Pending and event waiters use this to
    /// surface [`QmpError::Closed`] rather than blocking forever.
    closed: bool,
    /// Reason recorded at close time, surfaced in [`QmpError::Closed`].
    close_reason: &'static str,
}

struct Inner {
    writer: Mutex<Option<UnixStream>>,
    pending: Mutex<HashMap<String, Pending>>,
    log: Mutex<EventLog>,
    log_cond: Condvar,
}

impl Inner {
    fn new(writer: UnixStream) -> Self {
        Self {
            writer: Mutex::new(Some(writer)),
            pending: Mutex::new(HashMap::new()),
            log: Mutex::new(EventLog {
                events: Vec::new(),
                closed: false,
                close_reason: "",
            }),
            log_cond: Condvar::new(),
        }
    }

    /// Mark the connection closed, sever the underlying socket so the
    /// reader's blocking `read_line` returns immediately, drain pending
    /// callers with a [`QmpError::Closed`], and wake any
    /// [`Qmp::wait_event`] waiters.
    fn shutdown(&self, reason: &'static str) {
        {
            let mut w = self.writer.lock().unwrap();
            if let Some(stream) = w.as_ref() {
                // Closing all fds for the same socket is not enough —
                // the reader has a separately try_cloned fd. Shutting
                // down the underlying socket forces both directions
                // closed regardless of fd count, which is what wakes
                // the reader thread.
                let _ = stream.shutdown(std::net::Shutdown::Both);
            }
            *w = None;
        }

        {
            let mut log = self.log.lock().unwrap();
            if !log.closed {
                log.closed = true;
                log.close_reason = reason;
            }
        }
        self.log_cond.notify_all();

        let drained: Vec<_> = {
            let mut pending = self.pending.lock().unwrap();
            pending.drain().collect()
        };
        for (_, p) in drained {
            // Receiver may have given up already; ignore send failures.
            let _ = p.reply_to.send(RawResponse::Closed(reason));
        }
    }
}

// ---------------------------------------------------------------------------
// Qmp
// ---------------------------------------------------------------------------

/// A connected QEMU monitor.
///
/// Spawn one [`Qmp`] per QEMU child process. The handshake (greeting +
/// capability negotiation + events-enable) is performed by
/// [`Qmp::connect`] before the constructor returns; all subsequent
/// operations are non-blocking-to-construct.
pub struct Qmp {
    inner: Arc<Inner>,
    reader: Option<JoinHandle<()>>,
    next_id: AtomicU64,
}

impl std::fmt::Debug for Qmp {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let log = self.inner.log.lock().unwrap();
        f.debug_struct("Qmp")
            .field("closed", &log.closed)
            .field("event_count", &log.events.len())
            .finish()
    }
}

impl Qmp {
    /// Connect to a QMP unix socket and complete the handshake.
    ///
    /// On return the connection is ready for [`Self::execute`]:
    ///
    /// 1. Greeting line (`{"QMP": {...}}`) consumed.
    /// 2. `qmp_capabilities` issued — exits "negotiation" mode.
    /// 3. `migrate-set-capabilities` issued with `events: true` — without
    ///    this, MIGRATION events are not emitted (see crate-level docs).
    pub fn connect(path: impl AsRef<Path>) -> Result<Self, QmpError> {
        let stream = UnixStream::connect(path.as_ref())?;
        Self::from_stream(stream)
    }

    /// Build a [`Qmp`] from an already-connected [`UnixStream`]. Used by
    /// [`Self::connect`] and by tests against a mock server.
    pub fn from_stream(stream: UnixStream) -> Result<Self, QmpError> {
        let writer = stream.try_clone()?;
        let mut reader = BufReader::new(stream);

        // Step 1: read the greeting.
        let mut greeting = String::new();
        reader.read_line(&mut greeting)?;
        let greeting_value: Value = serde_json::from_str(&greeting)?;
        if greeting_value.get("QMP").is_none() {
            return Err(QmpError::Handshake(format!(
                "expected QMP greeting, got: {}",
                greeting.trim()
            )));
        }

        // Steps 2+3: capability handshake. We do them inline against
        // the synchronous reader before spawning the async reader
        // thread, because there's no point dispatching responses by id
        // until we know the connection is healthy.
        let mut writer_for_handshake = writer.try_clone()?;
        send_line(
            &mut writer_for_handshake,
            json!({ "execute": "qmp_capabilities" }),
        )?;
        await_simple_return(&mut reader, "qmp_capabilities")?;

        send_line(
            &mut writer_for_handshake,
            json!({
                "execute": "migrate-set-capabilities",
                "arguments": {
                    "capabilities": [{
                        "capability": "events",
                        "state": true,
                    }],
                },
            }),
        )?;
        await_simple_return(&mut reader, "migrate-set-capabilities")?;

        // Now spawn the reader thread for async dispatch.
        let inner = Arc::new(Inner::new(writer));
        let reader_inner = Arc::clone(&inner);
        let reader_handle = thread::Builder::new()
            .name("qmp-reader".into())
            .spawn(move || reader_loop(reader, reader_inner))
            .map_err(QmpError::Io)?;

        Ok(Self {
            inner,
            reader: Some(reader_handle),
            next_id: AtomicU64::new(1),
        })
    }

    /// Send a command and block (up to [`DEFAULT_COMMAND_TIMEOUT`]) for
    /// its reply. Convenience wrapper around [`Self::execute_timeout`].
    pub fn execute(&self, cmd: &str, args: Value) -> Result<Value, QmpError> {
        self.execute_timeout(cmd, args, DEFAULT_COMMAND_TIMEOUT)
    }

    /// Send a command and block up to `timeout` for its reply.
    ///
    /// On timeout the pending entry is removed; a late reply from QEMU
    /// will be silently dropped by the reader.
    pub fn execute_timeout(
        &self,
        cmd: &str,
        args: Value,
        timeout: Duration,
    ) -> Result<Value, QmpError> {
        let id = self.allocate_id();
        let (tx, rx) = sync_channel::<RawResponse>(1);

        {
            let mut pending = self.inner.pending.lock().unwrap();
            pending.insert(
                id.clone(),
                Pending {
                    command: cmd.to_owned(),
                    reply_to: tx,
                },
            );
        }

        let mut request = serde_json::Map::new();
        request.insert("execute".into(), Value::String(cmd.into()));
        if !args.is_null() {
            request.insert("arguments".into(), args);
        }
        request.insert("id".into(), Value::String(id.clone()));

        // Hold the writer lock just long enough to send the line.
        let write_result = {
            let mut writer_guard = self.inner.writer.lock().unwrap();
            match writer_guard.as_mut() {
                Some(stream) => send_line(stream, Value::Object(request)),
                None => Err(QmpError::Closed("write on closed connection")),
            }
        };

        if let Err(e) = write_result {
            self.inner.pending.lock().unwrap().remove(&id);
            return Err(e);
        }

        match rx.recv_timeout(timeout) {
            Ok(RawResponse::Return(v)) => Ok(v),
            Ok(RawResponse::Error { class, desc }) => Err(QmpError::Command {
                command: cmd.to_owned(),
                class,
                desc,
            }),
            Ok(RawResponse::Closed(reason)) => Err(QmpError::Closed(reason)),
            Err(RecvTimeoutError::Timeout) => {
                self.inner.pending.lock().unwrap().remove(&id);
                Err(QmpError::Timeout(timeout))
            }
            Err(RecvTimeoutError::Disconnected) => {
                Err(QmpError::Closed("response channel disconnected"))
            }
        }
    }

    /// Snapshot the current event-log length.
    ///
    /// Capture **before** issuing the command whose async-event
    /// completion you intend to wait on; pass to [`Self::wait_event`]
    /// as the `since` parameter to ignore stale events from prior
    /// operations on this connection.
    pub fn event_mark(&self) -> EventMark {
        EventMark(self.inner.log.lock().unwrap().events.len())
    }

    /// Block until an event matching `name` and `predicate` arrives,
    /// considering only events appended after `since`.
    ///
    /// Returns [`QmpError::Timeout`] if no match arrives in `timeout`.
    /// Returns [`QmpError::Closed`] if the connection ends first.
    pub fn wait_event<F>(
        &self,
        name: &str,
        since: EventMark,
        predicate: F,
        timeout: Duration,
    ) -> Result<Event, QmpError>
    where
        F: Fn(&Event) -> bool,
    {
        let deadline = Instant::now() + timeout;
        let mut log = self.inner.log.lock().unwrap();

        loop {
            if let Some(found) = scan_for_match(&log.events, since.0, name, &predicate) {
                return Ok(found);
            }

            if log.closed {
                return Err(QmpError::Closed(log.close_reason));
            }

            let now = Instant::now();
            if now >= deadline {
                return Err(QmpError::Timeout(timeout));
            }
            let remaining = deadline - now;
            let (new_log, wait_result) = self
                .inner
                .log_cond
                .wait_timeout(log, remaining)
                .unwrap();
            log = new_log;

            if wait_result.timed_out() {
                // Take one more pass in case a wakeup raced the timeout.
                if let Some(found) = scan_for_match(&log.events, since.0, name, &predicate) {
                    return Ok(found);
                }
                return Err(QmpError::Timeout(timeout));
            }
        }
    }

    /// `true` if the reader thread has exited (clean shutdown or I/O
    /// failure). All subsequent ops will return [`QmpError::Closed`].
    pub fn is_closed(&self) -> bool {
        self.inner.log.lock().unwrap().closed
    }

    /// Issue `quit`, then wait for the reader thread to exit.
    ///
    /// Idempotent: if the connection is already closed, returns `Ok`.
    pub fn close(mut self) -> Result<(), QmpError> {
        if !self.is_closed() {
            // Best-effort `quit` with a short timeout. Errors here
            // are non-fatal — the connection is going away anyway,
            // and we sever the socket below regardless.
            let _ = self.execute_timeout("quit", Value::Null, Duration::from_millis(500));
        }
        self.inner.shutdown("explicit close");
        self.join_reader();
        Ok(())
    }

    fn join_reader(&mut self) {
        if let Some(handle) = self.reader.take() {
            let _ = handle.join();
        }
    }

    fn allocate_id(&self) -> String {
        let n = self.next_id.fetch_add(1, Ordering::Relaxed);
        format!("c{n}")
    }

    // -------------------------------------------------------------------
    // Typed convenience commands
    // -------------------------------------------------------------------

    /// Pause the VM (`stop`).
    pub fn stop(&self) -> Result<(), QmpError> {
        self.execute("stop", Value::Null).map(|_| ())
    }

    /// Resume the VM (`cont`).
    pub fn cont(&self) -> Result<(), QmpError> {
        self.execute("cont", Value::Null).map(|_| ())
    }

    /// Send a clean QMP `quit`. Does not wait for the reader to exit;
    /// the caller likely also reaps the QEMU child process.
    pub fn quit(&self) -> Result<(), QmpError> {
        self.execute("quit", Value::Null).map(|_| ())
    }

    /// Save VM state to `path` using QEMU's `migrate file:<path>` form.
    ///
    /// Returns as soon as QEMU accepts the command; **does not** wait
    /// for migration to complete. Pair with
    /// [`Self::wait_migration_completed`] (and an [`Self::event_mark`]
    /// captured *before* this call) for the full snapshot flow.
    pub fn migrate(&self, path: impl AsRef<Path>) -> Result<(), QmpError> {
        let path = path.as_ref();
        // QEMU's migrate command takes a URI string. The `file:` form
        // is what the spike validated; the `exec:` form has a flush
        // race and must not be used.
        let uri = format!("file:{}", path.display());
        self.execute("migrate", json!({ "uri": uri })).map(|_| ())
    }

    /// Wait for a `MIGRATION` event with `status == "completed"`
    /// (success) or `status == "failed"` (failure) to arrive after
    /// `since`. Returns `Ok(())` on success, [`QmpError::Command`] on
    /// failure (with the failed status as the desc), or
    /// [`QmpError::Timeout`] / [`QmpError::Closed`] on infrastructure
    /// problems.
    pub fn wait_migration_completed(
        &self,
        since: EventMark,
        timeout: Duration,
    ) -> Result<(), QmpError> {
        let event = self.wait_event(
            "MIGRATION",
            since,
            |e| {
                let status = e.data.get("status").and_then(|v| v.as_str());
                matches!(status, Some("completed") | Some("failed"))
            },
            timeout,
        )?;

        let status = event
            .data
            .get("status")
            .and_then(|v| v.as_str())
            .unwrap_or("(unknown)");
        match status {
            "completed" => Ok(()),
            other => Err(QmpError::Command {
                command: "migrate".into(),
                class: "MigrationFailed".into(),
                desc: format!("MIGRATION ended with status `{other}`"),
            }),
        }
    }

    /// `query-status` returns `{"status": <RunState>, "running": bool, ...}`.
    /// Returns the `status` string (e.g. `"running"`, `"paused"`,
    /// `"postmigrate"`).
    pub fn query_status(&self) -> Result<String, QmpError> {
        let v = self.execute("query-status", Value::Null)?;
        Ok(v.get("status")
            .and_then(|s| s.as_str())
            .unwrap_or("unknown")
            .to_owned())
    }
}

impl Drop for Qmp {
    fn drop(&mut self) {
        if self.reader.is_some() {
            self.inner.shutdown("Qmp dropped without close()");
            self.join_reader();
        }
    }
}

// ---------------------------------------------------------------------------
// Reader loop
// ---------------------------------------------------------------------------

fn reader_loop(reader: BufReader<UnixStream>, inner: Arc<Inner>) {
    let mut reader = reader;
    let mut line = String::new();
    loop {
        line.clear();
        match reader.read_line(&mut line) {
            Ok(0) => {
                inner.shutdown("peer closed connection");
                return;
            }
            Ok(_) => {
                if let Some(reason) = handle_line(&line, &inner) {
                    inner.shutdown(reason);
                    return;
                }
            }
            Err(_) => {
                inner.shutdown("reader I/O error");
                return;
            }
        }
    }
}

/// Returns `Some(reason)` when the connection should be torn down
/// (catastrophic decode failure that would silently lose responses).
fn handle_line(line: &str, inner: &Inner) -> Option<&'static str> {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return None;
    }

    let value: Value = match serde_json::from_str(trimmed) {
        Ok(v) => v,
        // A garbled line is suspicious but not necessarily fatal —
        // some QEMU builds emit warnings on stderr that bleed into
        // stdout in odd setups. Tolerate by skipping.
        Err(_) => return None,
    };

    if value.get("event").is_some() {
        // It's an async event.
        if let Ok(wire) = serde_json::from_value::<WireEvent>(value) {
            let event: Event = wire.into();
            let mut log = inner.log.lock().unwrap();
            log.events.push(event);
            inner.log_cond.notify_all();
        }
        return None;
    }

    let id_owned = match value.get("id").and_then(|v| v.as_str()) {
        Some(id) => id.to_owned(),
        None => {
            // Some QMP messages (greeting, certain server-pushed
            // notices) carry no id and aren't events. Ignore.
            return None;
        }
    };

    // Late response, or wrong id — caller already gave up.
    let pending = inner.pending.lock().unwrap().remove(&id_owned)?;

    let response = if let Some(ret) = value.get("return") {
        RawResponse::Return(ret.clone())
    } else if let Some(err) = value.get("error") {
        let class = err
            .get("class")
            .and_then(|v| v.as_str())
            .unwrap_or("Unknown")
            .to_owned();
        let desc = err
            .get("desc")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_owned();
        RawResponse::Error { class, desc }
    } else {
        // Neither return nor error — protocol violation. Surface as
        // an error so the caller doesn't hang.
        RawResponse::Error {
            class: "MalformedResponse".into(),
            desc: format!(
                "QMP reply had neither `return` nor `error` for command `{}`",
                pending.command
            ),
        }
    };

    let _ = pending.reply_to.send(response);
    None
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn send_line<W: Write>(writer: &mut W, value: Value) -> Result<(), QmpError> {
    let mut bytes = serde_json::to_vec(&value)?;
    bytes.push(b'\n');
    writer.write_all(&bytes)?;
    writer.flush()?;
    Ok(())
}

/// Synchronous "send command, block on next reply line, expect a
/// `return` shape with no useful payload" helper used during the
/// connection handshake.
fn await_simple_return<R: BufRead>(reader: &mut R, command: &str) -> Result<(), QmpError> {
    let mut line = String::new();
    reader.read_line(&mut line)?;
    let v: Value = serde_json::from_str(&line)?;
    if v.get("return").is_some() {
        Ok(())
    } else if let Some(err) = v.get("error") {
        Err(QmpError::Command {
            command: command.to_owned(),
            class: err
                .get("class")
                .and_then(|v| v.as_str())
                .unwrap_or("Unknown")
                .into(),
            desc: err
                .get("desc")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .into(),
        })
    } else {
        Err(QmpError::Handshake(format!(
            "expected `return` after `{command}`, got: {}",
            line.trim()
        )))
    }
}

fn scan_for_match<F>(
    events: &[Event],
    since: usize,
    name: &str,
    predicate: &F,
) -> Option<Event>
where
    F: Fn(&Event) -> bool,
{
    events
        .get(since..)
        .into_iter()
        .flatten()
        .find(|e| e.name == name && predicate(e))
        .cloned()
}
