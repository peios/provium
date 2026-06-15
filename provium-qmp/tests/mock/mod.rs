//! A scripted QMP server used by the integration tests.
//!
//! The mock binds a unix socket, performs the standard QMP greeting +
//! capability dance, then fields commands via a programmable rule
//! table. Each test instance drives its own server. Concurrency is
//! limited to one client per server (matching real QMP).

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use tempfile::TempDir;

/// A scripted QMP server.
///
/// Methods on [`MockServer`] are safe to call from any thread; they
/// communicate with the running server thread via a shared state
/// struct.
pub struct MockServer {
    /// Backing tempdir — the unix socket lives in here, gets unlinked
    /// on drop.
    _dir: TempDir,
    /// Path to the unix socket the [`provium_qmp::Qmp`] should connect to.
    socket_path: PathBuf,
    /// Shared scripting state — accessible to both the test thread
    /// and the server's per-connection thread.
    state: Arc<Mutex<State>>,
    /// Server thread handle. Joined in [`Self::shutdown`].
    server_thread: Option<JoinHandle<()>>,
}

#[derive(Default)]
struct State {
    /// Programmed responses by command name. Persists across multiple
    /// invocations of the same command (matched on every request).
    responses: HashMap<String, ResponseSpec>,
    /// Commands the server has received, oldest first. Used by tests
    /// to assert "the wrapper sent X then Y."
    command_log: Vec<Value>,
    /// Test has asked the server to shut down.
    shutdown: bool,
    /// Write half of the active connection — set when a client is
    /// connected, cleared on disconnect or shutdown. Used by
    /// [`MockServer::push_event`] to inject events asynchronously.
    active_writer: Option<Arc<Mutex<UnixStream>>>,
}

#[derive(Clone)]
enum ResponseSpec {
    Return(Value),
    Error { class: String, desc: String },
    /// Special: don't reply at all; used for testing the wrapper's
    /// timeout path.
    DropSilently,
}

impl MockServer {
    /// Bind a fresh server on a tempfile-located socket and return.
    pub fn start() -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        let socket_path = dir.path().join("qmp.sock");
        let listener = UnixListener::bind(&socket_path).expect("bind");
        listener
            .set_nonblocking(true)
            .expect("set_nonblocking on listener");

        let state = Arc::new(Mutex::new(State::default()));
        let state_for_thread = Arc::clone(&state);

        let server_thread = thread::Builder::new()
            .name("qmp-mock".into())
            .spawn(move || serve(listener, state_for_thread))
            .expect("spawn server thread");

        Self {
            _dir: dir,
            socket_path,
            state,
            server_thread: Some(server_thread),
        }
    }

    pub fn path(&self) -> &Path {
        &self.socket_path
    }

    /// Program a successful response payload for `command`.
    pub fn respond_to(&self, command: &str, return_value: Value) {
        self.state
            .lock()
            .unwrap()
            .responses
            .insert(command.to_owned(), ResponseSpec::Return(return_value));
    }

    /// Program an error response for `command`.
    pub fn respond_to_with_error(&self, command: &str, class: &str, desc: &str) {
        self.state.lock().unwrap().responses.insert(
            command.to_owned(),
            ResponseSpec::Error {
                class: class.to_owned(),
                desc: desc.to_owned(),
            },
        );
    }

    /// Schedule an async event to be pushed `delay` after this call.
    /// The event is written via the active connection's writer; if
    /// the connection has already closed, the event is silently
    /// dropped.
    pub fn push_event(&self, name: &str, data: Value, delay: Duration) {
        let state = Arc::clone(&self.state);
        let name = name.to_owned();
        thread::spawn(move || {
            thread::sleep(delay);
            let writer = state.lock().unwrap().active_writer.clone();
            if let Some(writer) = writer {
                let _ = send_event(&mut *writer.lock().unwrap(), &name, data);
            }
        });
    }

    /// Snapshot of every command the server has received so far —
    /// `qmp_capabilities`, `migrate-set-capabilities`, then anything
    /// the test issued. Each entry is the full request JSON; the
    /// command name is in the `execute` field.
    pub fn commands_received(&self) -> Vec<String> {
        self.state
            .lock()
            .unwrap()
            .command_log
            .iter()
            .filter_map(|c| {
                c.get("execute")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_owned())
            })
            .collect()
    }

    /// Last `arguments` value seen for `command`, or `None` if not yet
    /// observed.
    pub fn last_arguments_for(&self, command: &str) -> Option<Value> {
        self.state
            .lock()
            .unwrap()
            .command_log
            .iter()
            .rev()
            .find(|c| c.get("execute").and_then(|v| v.as_str()) == Some(command))
            .and_then(|c| c.get("arguments"))
            .cloned()
    }

    /// Tear down the server — closes the active connection (if any)
    /// and joins the server thread.
    pub fn shutdown(mut self) {
        self.shutdown_internal();
    }

    fn shutdown_internal(&mut self) {
        {
            let mut s = self.state.lock().unwrap();
            s.shutdown = true;
            // Drop the writer, which causes any in-flight
            // BufReader::read_line on the server side to return EOF
            // when its peer notices.
            s.active_writer = None;
        }
        if let Some(handle) = self.server_thread.take() {
            // Wait at most 1s for the server thread to wind down. If
            // it's wedged we forfeit the join — this is a test
            // helper, not production code.
            let deadline = Instant::now() + Duration::from_secs(1);
            while Instant::now() < deadline && !handle.is_finished() {
                thread::sleep(Duration::from_millis(10));
            }
            if handle.is_finished() {
                let _ = handle.join();
            }
        }
    }
}

impl Drop for MockServer {
    fn drop(&mut self) {
        if self.server_thread.is_some() {
            self.shutdown_internal();
        }
    }
}

fn serve(listener: UnixListener, state: Arc<Mutex<State>>) {
    loop {
        if state.lock().unwrap().shutdown {
            return;
        }
        match listener.accept() {
            Ok((stream, _)) => {
                stream.set_nonblocking(false).ok();
                handle_connection(stream, Arc::clone(&state));
            }
            Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(10));
            }
            Err(_) => return,
        }
    }
}

fn handle_connection(stream: UnixStream, state: Arc<Mutex<State>>) {
    let writer = stream.try_clone().expect("clone stream");
    // Use a short read timeout so the loop wakes periodically and can
    // observe shutdown requests even when the client is idle.
    stream
        .set_read_timeout(Some(Duration::from_millis(50)))
        .expect("set_read_timeout");

    let writer = Arc::new(Mutex::new(writer));
    {
        let mut s = state.lock().unwrap();
        s.active_writer = Some(Arc::clone(&writer));
    }

    // Greeting.
    let greet = json!({
        "QMP": {
            "version": {
                "qemu": {"major": 9, "minor": 0, "micro": 0},
                "package": "mock",
            },
            "capabilities": [],
        },
    });
    if send_value(&mut *writer.lock().unwrap(), &greet).is_err() {
        clear_active(&state);
        return;
    }

    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    loop {
        if state.lock().unwrap().shutdown {
            break;
        }

        line.clear();
        match reader.read_line(&mut line) {
            Ok(0) => break, // peer closed
            Ok(_) => {}
            Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => continue,
            Err(ref e) if e.kind() == std::io::ErrorKind::TimedOut => continue,
            Err(_) => break,
        }

        let request: Value = match serde_json::from_str(line.trim()) {
            Ok(v) => v,
            Err(_) => continue,
        };
        let command = match request.get("execute").and_then(|v| v.as_str()) {
            Some(c) => c.to_owned(),
            None => continue,
        };
        let id = request.get("id").cloned();

        {
            let mut s = state.lock().unwrap();
            s.command_log.push(request.clone());
        }

        let spec = state
            .lock()
            .unwrap()
            .responses
            .get(&command)
            .cloned()
            .unwrap_or_else(|| match command.as_str() {
                // Handshake commands and `quit` always succeed by
                // default so tests don't have to wire them up.
                "qmp_capabilities" | "migrate-set-capabilities" | "quit" => {
                    ResponseSpec::Return(json!({}))
                }
                _ => ResponseSpec::DropSilently,
            });

        let mut response = serde_json::Map::new();
        match spec {
            ResponseSpec::Return(v) => {
                response.insert("return".into(), v);
            }
            ResponseSpec::Error { class, desc } => {
                response.insert("error".into(), json!({"class": class, "desc": desc}));
            }
            ResponseSpec::DropSilently => continue,
        }
        if let Some(id) = id {
            response.insert("id".into(), id);
        }
        if send_value(&mut *writer.lock().unwrap(), &Value::Object(response)).is_err() {
            break;
        }
    }

    clear_active(&state);
}

fn clear_active(state: &Arc<Mutex<State>>) {
    state.lock().unwrap().active_writer = None;
}

fn send_value<W: Write>(w: &mut W, v: &Value) -> std::io::Result<()> {
    let mut buf = serde_json::to_vec(v).expect("serialize");
    buf.push(b'\n');
    w.write_all(&buf)?;
    w.flush()
}

fn send_event<W: Write>(w: &mut W, name: &str, data: Value) -> std::io::Result<()> {
    let frame = json!({
        "event": name,
        "data": data,
        "timestamp": {"seconds": 0, "microseconds": 0},
    });
    send_value(w, &frame)
}
