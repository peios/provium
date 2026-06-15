//! Lua-facing wrapper around the bidirectional console chardev
//! socket as a Stream — same `:next/:read_until/:expect/:drain
//! /:close/:eof/:creation_site` surface as the file/process tail
//! streams.
//!
//! The host opens a `UnixStream` to the QEMU console socket; reads
//! from the socket get whatever bytes the guest has written since
//! the last read. Writes are not exposed here (see `console:write`
//! on [`crate::lua::result_ud::ConsoleUd`]).

use std::io::Read;
use std::os::unix::net::UnixStream;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use mlua::{MetaMethod, UserData, UserDataMethods};

/// Lua-facing console stream. Lifetime = open Unix socket
/// connection to the QEMU chardev. Holds a
/// [`crate::vm::StreamGuard`] so the snapshot precondition can
/// see this stream as live (without it, vm:snapshot would
/// silently succeed even when console:read is mid-flight).
#[derive(Clone)]
pub(crate) struct ConsoleStreamUd {
    inner: Arc<Mutex<ConsoleStreamInner>>,
    creation_site: Option<(String, i32)>,
    /// `None` for the test/back-compat `connect` constructor.
    _guard: Option<Arc<crate::vm::StreamGuard>>,
}

struct ConsoleStreamInner {
    sock: Option<UnixStream>,
    eof: bool,
    /// Buffered bytes read from the socket but not yet returned to
    /// Lua. Allows `read_until`/`expect` to look across reads.
    pending: Vec<u8>,
}

impl std::fmt::Debug for ConsoleStreamInner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ConsoleStreamInner")
            .field("eof", &self.eof)
            .field("pending_len", &self.pending.len())
            .finish()
    }
}

impl ConsoleStreamUd {
    /// Connect to the chardev socket at `path`. Returns the wrapped
    /// userdata. Sets a short read timeout so polling doesn't block
    /// forever — the Lua side drives the deadline with `:next`'s
    /// timeout arg.
    #[allow(dead_code)]
    pub(crate) fn connect(
        path: &std::path::Path,
        creation_site: Option<(String, i32)>,
    ) -> std::io::Result<Self> {
        Self::connect_inner(path, creation_site, None)
    }

    /// Connect AND register the stream against `vm`'s resource
    /// registry so `vm:snapshot()` sees it as live. Used by the
    /// production `console:read()` Lua binding.
    pub(crate) fn connect_with_guard(
        path: &std::path::Path,
        creation_site: Option<(String, i32)>,
        guard: crate::vm::StreamGuard,
    ) -> std::io::Result<Self> {
        Self::connect_inner(path, creation_site, Some(Arc::new(guard)))
    }

    fn connect_inner(
        path: &std::path::Path,
        creation_site: Option<(String, i32)>,
        guard: Option<Arc<crate::vm::StreamGuard>>,
    ) -> std::io::Result<Self> {
        let sock = UnixStream::connect(path)?;
        sock.set_read_timeout(Some(Duration::from_millis(50)))?;
        Ok(Self {
            inner: Arc::new(Mutex::new(ConsoleStreamInner {
                sock: Some(sock),
                eof: false,
                pending: Vec::new(),
            })),
            creation_site,
            _guard: guard,
        })
    }
}

impl UserData for ConsoleStreamUd {
    fn add_methods<M: UserDataMethods<Self>>(methods: &mut M) {
        methods.add_method("creation_site", |lua, this, ()| {
            match &this.creation_site {
                Some((file, line)) => {
                    let t = lua.create_table()?;
                    t.set("file", file.clone())?;
                    t.set("line", *line)?;
                    Ok(mlua::Value::Table(t))
                }
                None => Ok(mlua::Value::Nil),
            }
        });

        methods.add_method("next", |lua, this, timeout: Option<mlua::Value>| {
            let mut g = this.inner.lock().unwrap();
            if !g.pending.is_empty() {
                let bytes = std::mem::take(&mut g.pending);
                return lua.create_string(&bytes).map(mlua::Value::String);
            }
            if g.eof {
                return Ok(mlua::Value::Nil);
            }
            let mut buf = vec![0u8; 64 * 1024];
            let sock = match g.sock.as_mut() {
                Some(s) => s,
                None => {
                    g.eof = true;
                    return Ok(mlua::Value::Nil);
                }
            };
            // Honour the caller-supplied timeout — without this
            // the socket's hard 50ms read_timeout from connect()
            // controls everything and `console:read():next("5s")`
            // returns nil after 50ms instead of waiting.
            if let Some(t) = timeout {
                let secs = super::result_ud::parse_duration(Some(t), 10.0)?;
                let _ = sock.set_read_timeout(Some(
                    Duration::from_secs_f64(secs.max(0.001)),
                ));
            }
            let read_result = sock.read(&mut buf);
            // Restore the connect()-time 50ms default so a bare
            // `next()` after a `next("5s")` doesn't inherit the
            // 5s timeout. The previous fix used None here, which
            // made subsequent calls block forever.
            let _ = sock.set_read_timeout(Some(Duration::from_millis(50)));
            match read_result {
                Ok(0) => {
                    g.eof = true;
                    Ok(mlua::Value::Nil)
                }
                Ok(n) => lua.create_string(&buf[..n]).map(mlua::Value::String),
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    // DESIGN.md § Stream: `stream:next(timeout?)`
                    // returns nil on EOF; symmetric "no data
                    // available" maps to nil too.
                    Ok(mlua::Value::Nil)
                }
                Err(e) => Err(mlua::Error::external(e)),
            }
        });

        methods.add_method("eof", |_, this, ()| {
            // Pending bytes mean there's still data to deliver
            // before EOF is honest. TailUd and CaptureUd both
            // guard the same way; ConsoleStreamUd was the
            // outlier reporting eof=true even when next() would
            // still produce data.
            let g = this.inner.lock().unwrap();
            if !g.pending.is_empty() {
                return Ok(false);
            }
            Ok(g.eof)
        });

        methods.add_method("close", |_, this, ()| {
            let mut g = this.inner.lock().unwrap();
            g.sock = None;
            g.eof = true;
            // Drop any leftover bytes — without this, a `:next()`
            // after `:close()` would drain stale pending data
            // before reaching the closed-sock branch (TailUd
            // already does this; ConsoleStreamUd was the
            // outlier).
            g.pending.clear();
            Ok(())
        });

        methods.add_method(
            "read_until",
            |lua, this, (pattern, timeout): (mlua::String, Option<mlua::Value>)| {
                let needle = pattern.as_bytes().to_vec();
                let timeout_secs = super::result_ud::parse_duration(timeout, 10.0)?;
                let deadline = std::time::Instant::now()
                    + Duration::from_secs_f64(timeout_secs);
                read_until_inner(this, lua, &needle, deadline, false).map(mlua::Value::String)
            },
        );

        methods.add_method(
            "expect",
            |_, this, (pattern, timeout): (mlua::String, Option<mlua::Value>)| {
                let needle = pattern.as_bytes().to_vec();
                let timeout_secs = super::result_ud::parse_duration(timeout, 10.0)?;
                let deadline = std::time::Instant::now()
                    + Duration::from_secs_f64(timeout_secs);
                // expect returns nothing — discards the matched
                // prefix.
                expect_inner(this, &needle, deadline)?;
                Ok(())
            },
        );

        methods.add_method("drain", |lua, this, timeout: Option<mlua::Value>| {
            // Frame-per-element semantics matching TailUd::drain —
            // each read iteration produces one Lua-string entry.
            // The timeout bounds total drain time so an idle
            // console doesn't block forever waiting for the next
            // chunk; default 0.5s mirrors TailUd::drain.
            let timeout_secs = super::result_ud::parse_duration(timeout, 0.5)?;
            let deadline = std::time::Instant::now()
                + Duration::from_secs_f64(timeout_secs.max(0.001));
            let mut g = this.inner.lock().unwrap();
            let table = lua.create_table()?;
            let mut idx = 1;
            if !g.pending.is_empty() {
                let pending = std::mem::take(&mut g.pending);
                table.set(idx, lua.create_string(&pending)?)?;
                idx += 1;
            }
            let mut buf = vec![0u8; 64 * 1024];
            let mut hit_eof = false;
            if let Some(sock) = g.sock.as_mut() {
                loop {
                    let now = std::time::Instant::now();
                    let remaining = deadline.checked_duration_since(now);
                    let Some(rem) = remaining else { break };
                    let _ = sock.set_read_timeout(Some(rem));
                    match sock.read(&mut buf) {
                        Ok(0) => {
                            hit_eof = true;
                            break;
                        }
                        Ok(n) => {
                            table.set(idx, lua.create_string(&buf[..n])?)?;
                            idx += 1;
                        }
                        Err(_) => break,
                    }
                }
                // Restore connect-time 50ms default — without
                // this, a bare `next()` after `drain` inherits
                // whatever tiny remaining-deadline value the last
                // iteration set, returning nil almost
                // immediately.
                let _ = sock.set_read_timeout(Some(Duration::from_millis(50)));
            }
            if hit_eof {
                g.eof = true;
            }
            Ok(table)
        });

        methods.add_meta_method(MetaMethod::ToString, |_, _this, ()| {
            Ok("console_stream".to_owned())
        });
    }
}

fn read_until_inner(
    this: &ConsoleStreamUd,
    lua: &mlua::Lua,
    needle: &[u8],
    deadline: std::time::Instant,
    consume_only: bool,
) -> mlua::Result<mlua::String> {
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        {
            let g = this.inner.lock().unwrap();
            if let Some(idx) = g
                .pending
                .windows(needle.len())
                .position(|w| w == needle)
            {
                drop(g);
                let mut g2 = this.inner.lock().unwrap();
                let split_at = idx + needle.len();
                let prefix = g2.pending.drain(..split_at).collect::<Vec<u8>>();
                let _ = consume_only;
                return lua.create_string(&prefix);
            }
        }
        if std::time::Instant::now() >= deadline {
            return Err(mlua::Error::external(format!(
                "console:read_until timed out waiting for {}",
                String::from_utf8_lossy(needle)
            )));
        }
        let mut g = this.inner.lock().unwrap();
        let sock = match g.sock.as_mut() {
            Some(s) => s,
            None => return Err(mlua::Error::external("console closed")),
        };
        match sock.read(&mut buf) {
            Ok(0) => {
                g.eof = true;
                return Err(mlua::Error::external("console EOF"));
            }
            Ok(n) => g.pending.extend_from_slice(&buf[..n]),
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
            // ConnectionReset / BrokenPipe — QEMU chardev closed
            // mid-read (VM reset/shutdown). Semantically EOF for
            // a console stream, not an error to surface.
            Err(e)
                if e.kind() == std::io::ErrorKind::ConnectionReset
                    || e.kind() == std::io::ErrorKind::BrokenPipe =>
            {
                g.eof = true;
                return Err(mlua::Error::external("console EOF"));
            }
            Err(e) => return Err(mlua::Error::external(e)),
        }
    }
}

fn expect_inner(
    this: &ConsoleStreamUd,
    needle: &[u8],
    deadline: std::time::Instant,
) -> mlua::Result<()> {
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        {
            let mut g = this.inner.lock().unwrap();
            if let Some(idx) = g
                .pending
                .windows(needle.len())
                .position(|w| w == needle)
            {
                // Drain past the match — otherwise a follow-up
                // `expect`/`read_until` re-matches the same prefix.
                let consume = idx + needle.len();
                g.pending.drain(..consume);
                return Ok(());
            }
        }
        if std::time::Instant::now() >= deadline {
            return Err(mlua::Error::external(format!(
                "console:expect timed out waiting for {}",
                String::from_utf8_lossy(needle)
            )));
        }
        let mut g = this.inner.lock().unwrap();
        let sock = match g.sock.as_mut() {
            Some(s) => s,
            None => return Err(mlua::Error::external("console closed")),
        };
        match sock.read(&mut buf) {
            Ok(0) => {
                g.eof = true;
                return Err(mlua::Error::external("console EOF"));
            }
            Ok(n) => g.pending.extend_from_slice(&buf[..n]),
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
            // ConnectionReset / BrokenPipe — QEMU chardev closed
            // mid-read (VM reset/shutdown). Semantically EOF for
            // a console stream, not an error to surface.
            Err(e)
                if e.kind() == std::io::ErrorKind::ConnectionReset
                    || e.kind() == std::io::ErrorKind::BrokenPipe =>
            {
                g.eof = true;
                return Err(mlua::Error::external("console EOF"));
            }
            Err(e) => return Err(mlua::Error::external(e)),
        }
    }
}
