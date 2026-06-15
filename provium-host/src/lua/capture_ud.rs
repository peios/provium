//! `bridge:capture()` Lua userdata.
//!
//! Spawns `tcpdump -i <bridge> -U -w -` and exposes its pcap output
//! as a stream readable from Lua via `:next()` / `:read_until()` /
//! `:drain()` / `:close()` — the same surface as a tail-file stream.
//!
//! Requires `tcpdump` on `PATH` and the host process to hold both
//! `CAP_NET_ADMIN` (for the bridge to exist) and `CAP_NET_RAW` (for
//! the raw socket tcpdump opens). When either capability is missing
//! the spawn surfaces the failure as a clean Lua error.

use std::io::Read;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};

use mlua::{MetaMethod, UserData, UserDataMethods};

/// Lua-facing stream for `bridge:capture()`.
#[derive(Clone)]
pub(crate) struct CaptureUd {
    inner: Arc<Mutex<CaptureInner>>,
    creation_site: Option<(String, i32)>,
    /// Holds a [`crate::bridge::CaptureGuard`] for the lifetime
    /// of the underlying tcpdump child so the snapshot
    /// precondition can see this capture in `active_captures`.
    /// `None` for the legacy `spawn` constructor (test-only).
    _guard: Option<Arc<crate::bridge::CaptureGuard>>,
}

struct CaptureInner {
    child: Option<Child>,
    stdout: Option<std::process::ChildStdout>,
    eof: bool,
    /// Bytes that an earlier read_until/expect read past the
    /// matched suffix — replayed at the head of the next
    /// next/read_until/expect call so they aren't silently
    /// discarded. Mirrors TailUd / ConsoleStreamUd's pending
    /// buffer.
    pending: Vec<u8>,
}

impl std::fmt::Debug for CaptureInner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CaptureInner")
            .field("eof", &self.eof)
            .finish()
    }
}

impl CaptureUd {
    /// Spawn `tcpdump` against the named bridge. Test-only —
    /// production callers use [`Self::spawn_with_guard`] so the
    /// bridge's active-capture counter tracks this stream.
    #[allow(dead_code)]
    pub(crate) fn spawn(bridge_name: &str) -> std::io::Result<Self> {
        Self::spawn_inner(bridge_name, None, None)
    }

    /// Spawn variant that records a creation site for the snapshot
    /// diagnostic / `:creation_site()` accessor. No
    /// active-capture guard — kept for back-compat with the
    /// existing test corpus.
    #[allow(dead_code)]
    pub(crate) fn spawn_with_site(
        bridge_name: &str,
        creation_site: Option<(String, i32)>,
    ) -> std::io::Result<Self> {
        Self::spawn_inner(bridge_name, creation_site, None)
    }

    /// Spawn `tcpdump -i <bridge>` and hold a
    /// [`crate::bridge::CaptureGuard`] so the bridge's snapshot
    /// precondition sees this capture. Used by `bridge:capture()`.
    pub(crate) fn spawn_with_guard(
        bridge: &crate::bridge::Bridge,
        creation_site: Option<(String, i32)>,
    ) -> std::io::Result<Self> {
        let guard = bridge.register_capture();
        Self::spawn_inner(bridge.name(), creation_site, Some(Arc::new(guard)))
    }

    /// Spawn `tcpdump -i <iface>` for a specific interface (the
    /// per-VM TAP) while still pinning the capture-counter to its
    /// owning `bridge`. Used by `nic:capture()` so the test author
    /// gets per-NIC traffic instead of the whole-bridge mirror.
    pub(crate) fn spawn_on_iface_with_guard(
        iface: &str,
        bridge: &crate::bridge::Bridge,
        creation_site: Option<(String, i32)>,
    ) -> std::io::Result<Self> {
        let guard = bridge.register_capture();
        Self::spawn_inner(iface, creation_site, Some(Arc::new(guard)))
    }

    fn spawn_inner(
        bridge_name: &str,
        creation_site: Option<(String, i32)>,
        guard: Option<Arc<crate::bridge::CaptureGuard>>,
    ) -> std::io::Result<Self> {
        let mut cmd = Command::new("tcpdump");
        cmd.args(["-i", bridge_name, "-U", "-w", "-", "-s", "65535"]);
        cmd.stdout(Stdio::piped());
        cmd.stderr(Stdio::null());
        cmd.stdin(Stdio::null());
        let mut child = cmd.spawn()?;
        let stdout = child.stdout.take();
        Ok(Self {
            inner: Arc::new(Mutex::new(CaptureInner {
                child: Some(child),
                stdout,
                eof: false,
                pending: Vec::new(),
            })),
            creation_site,
            _guard: guard,
        })
    }
}

impl UserData for CaptureUd {
    fn add_methods<M: UserDataMethods<Self>>(methods: &mut M) {
        methods.add_method("creation_site", |lua, this, ()| {
            match &this.creation_site {
                Some((file, line)) => {
                    let table = lua.create_table()?;
                    table.set("file", file.clone())?;
                    table.set("line", *line)?;
                    Ok(mlua::Value::Table(table))
                }
                None => Ok(mlua::Value::Nil),
            }
        });

        methods.add_method("next", |lua, this, timeout: Option<mlua::Value>| {
            let timeout_secs = super::result_ud::parse_duration(timeout, 10.0)?;
            let mut g = this.inner.lock().unwrap();
            // Replay leftover bytes from a prior expect/read_until
            // before going back to the wire.
            if !g.pending.is_empty() {
                let bytes = std::mem::take(&mut g.pending);
                return lua.create_string(&bytes).map(mlua::Value::String);
            }
            if g.eof {
                return Ok(mlua::Value::Nil);
            }
            let mut buf = vec![0u8; 64 * 1024];
            let stdout = match g.stdout.as_mut() {
                Some(s) => s,
                None => {
                    g.eof = true;
                    return Ok(mlua::Value::Nil);
                }
            };
            // tcpdump's pipe doesn't honour set_read_timeout; use
            // poll() with the caller's deadline so `next("5s")`
            // returns nil after 5s of no traffic instead of
            // blocking indefinitely.
            use std::os::fd::AsRawFd;
            let fd = stdout.as_raw_fd();
            let timeout_ms = ((timeout_secs * 1000.0).ceil() as i64).clamp(0, i32::MAX as i64) as i32;
            let mut pfd = libc::pollfd {
                fd,
                events: libc::POLLIN,
                revents: 0,
            };
            // SAFETY: pfd is a single valid pollfd; nfds=1 matches.
            let pr = unsafe { libc::poll(&mut pfd, 1, timeout_ms) };
            if pr == 0 {
                return Ok(mlua::Value::Nil); // timed out, no data
            }
            if pr < 0 {
                return Err(mlua::Error::external(std::io::Error::last_os_error()));
            }
            match stdout.read(&mut buf) {
                Ok(0) => {
                    g.eof = true;
                    Ok(mlua::Value::Nil)
                }
                Ok(n) => {
                    let s = lua.create_string(&buf[..n])?;
                    Ok(mlua::Value::String(s))
                }
                Err(e) => Err(mlua::Error::external(e)),
            }
        });

        methods.add_method("eof", |_, this, ()| {
            // Pending bytes mean there's still data to deliver
            // before EOF is honest. TailUd::eof has the same
            // guard; CaptureUd was the outlier reporting eof=true
            // even when next() would still produce data.
            let g = this.inner.lock().unwrap();
            if !g.pending.is_empty() {
                return Ok(false);
            }
            Ok(g.eof)
        });

        // capture:read_until(pattern[, timeout]) — pull bytes until
        // `pattern` appears or timeout. Returns the prefix-including
        // match. Per `DESIGN.md` § Stream.
        methods.add_method(
            "read_until",
            |lua, this, (pattern, timeout): (mlua::String, Option<mlua::Value>)| {
                let needle = pattern.as_bytes().to_vec();
                let timeout_secs = super::result_ud::parse_duration(timeout, 10.0)?;
                let deadline = std::time::Instant::now()
                    + std::time::Duration::from_secs_f64(timeout_secs);
                // Seed acc with any leftover bytes from a prior
                // match so we don't drop them.
                let mut acc = {
                    let mut g = this.inner.lock().unwrap();
                    std::mem::take(&mut g.pending)
                };
                let mut buf = vec![0u8; 64 * 1024];
                if let Some(idx) = acc
                    .windows(needle.len())
                    .position(|w| w == needle.as_slice())
                {
                    let consume = idx + needle.len();
                    let leftover = acc.split_off(consume);
                    if !leftover.is_empty() {
                        let mut g = this.inner.lock().unwrap();
                        g.pending = leftover;
                    }
                    return lua.create_string(&acc).map(mlua::Value::String);
                }
                use std::os::fd::AsRawFd;
                let fd = {
                    let g = this.inner.lock().unwrap();
                    g.stdout
                        .as_ref()
                        .map(|s| s.as_raw_fd())
                        .ok_or_else(|| mlua::Error::external("capture closed"))?
                };
                loop {
                    let now = std::time::Instant::now();
                    let Some(rem) = deadline.checked_duration_since(now) else {
                        return Err(mlua::Error::external(format!(
                            "capture:read_until timed out waiting for {}",
                            String::from_utf8_lossy(&needle)
                        )));
                    };
                    let timeout_ms =
                        (rem.as_millis() as i64).clamp(0, i32::MAX as i64) as i32;
                    let mut pfd = libc::pollfd {
                        fd,
                        events: libc::POLLIN,
                        revents: 0,
                    };
                    // SAFETY: single valid pollfd, nfds=1.
                    let pr = unsafe { libc::poll(&mut pfd, 1, timeout_ms) };
                    if pr <= 0 {
                        return Err(mlua::Error::external(format!(
                            "capture:read_until timed out waiting for {}",
                            String::from_utf8_lossy(&needle)
                        )));
                    }
                    // Now re-acquire the mutex briefly for the
                    // actual read.
                    let read_outcome = {
                        let mut g = this.inner.lock().unwrap();
                        let Some(stdout) = g.stdout.as_mut() else {
                            return Err(mlua::Error::external("capture closed"));
                        };
                        let r = stdout.read(&mut buf);
                        if matches!(r, Ok(0)) {
                            g.eof = true;
                        }
                        r
                    };
                    match read_outcome {
                        Ok(0) => {
                            return Err(mlua::Error::external(format!(
                                "capture:read_until eof waiting for {}",
                                String::from_utf8_lossy(&needle)
                            )));
                        }
                        Ok(n) => acc.extend_from_slice(&buf[..n]),
                        Err(e) => return Err(mlua::Error::external(e)),
                    }
                    if let Some(idx) = acc
                        .windows(needle.len())
                        .position(|w| w == needle.as_slice())
                    {
                        let consume = idx + needle.len();
                        let leftover = acc.split_off(consume);
                        if !leftover.is_empty() {
                            let mut g = this.inner.lock().unwrap();
                            g.pending = leftover;
                        }
                        return lua
                            .create_string(&acc)
                            .map(mlua::Value::String);
                    }
                }
            },
        );

        // capture:expect — assertion variant of read_until.
        methods.add_method(
            "expect",
            |_, this, (pattern, timeout): (mlua::String, Option<mlua::Value>)| {
                let needle = pattern.as_bytes().to_vec();
                let timeout_secs = super::result_ud::parse_duration(timeout, 10.0)?;
                let deadline = std::time::Instant::now()
                    + std::time::Duration::from_secs_f64(timeout_secs);
                let mut acc = {
                    let mut g = this.inner.lock().unwrap();
                    std::mem::take(&mut g.pending)
                };
                let mut buf = vec![0u8; 64 * 1024];
                if let Some(idx) = acc
                    .windows(needle.len())
                    .position(|w| w == needle.as_slice())
                {
                    let consume = idx + needle.len();
                    let leftover = acc.split_off(consume);
                    if !leftover.is_empty() {
                        let mut g = this.inner.lock().unwrap();
                        g.pending = leftover;
                    }
                    return Ok(());
                }
                use std::os::fd::AsRawFd;
                let fd = {
                    let g = this.inner.lock().unwrap();
                    g.stdout
                        .as_ref()
                        .map(|s| s.as_raw_fd())
                        .ok_or_else(|| mlua::Error::external("capture closed"))?
                };
                loop {
                    let now = std::time::Instant::now();
                    let Some(rem) = deadline.checked_duration_since(now) else {
                        return Err(mlua::Error::external(format!(
                            "capture:expect timed out waiting for {}",
                            String::from_utf8_lossy(&needle)
                        )));
                    };
                    let timeout_ms =
                        (rem.as_millis() as i64).clamp(0, i32::MAX as i64) as i32;
                    let mut pfd = libc::pollfd {
                        fd,
                        events: libc::POLLIN,
                        revents: 0,
                    };
                    // SAFETY: single valid pollfd, nfds=1.
                    let pr = unsafe { libc::poll(&mut pfd, 1, timeout_ms) };
                    if pr <= 0 {
                        return Err(mlua::Error::external(format!(
                            "capture:expect timed out waiting for {}",
                            String::from_utf8_lossy(&needle)
                        )));
                    }
                    let read_outcome = {
                        let mut g = this.inner.lock().unwrap();
                        let Some(stdout) = g.stdout.as_mut() else {
                            return Err(mlua::Error::external("capture closed"));
                        };
                        let r = stdout.read(&mut buf);
                        if matches!(r, Ok(0)) {
                            g.eof = true;
                        }
                        r
                    };
                    match read_outcome {
                        Ok(0) => {
                            return Err(mlua::Error::external(format!(
                                "capture:expect eof waiting for {}",
                                String::from_utf8_lossy(&needle)
                            )));
                        }
                        Ok(n) => acc.extend_from_slice(&buf[..n]),
                        Err(e) => return Err(mlua::Error::external(e)),
                    }
                    if let Some(idx) = acc
                        .windows(needle.len())
                        .position(|w| w == needle.as_slice())
                    {
                        let consume = idx + needle.len();
                        let leftover = acc.split_off(consume);
                        if !leftover.is_empty() {
                            let mut g = this.inner.lock().unwrap();
                            g.pending = leftover;
                        }
                        return Ok(());
                    }
                    if std::time::Instant::now() >= deadline {
                        return Err(mlua::Error::external(format!(
                            "capture:expect timed out waiting for {}",
                            String::from_utf8_lossy(&needle)
                        )));
                    }
                }
            },
        );

        methods.add_method("close", |_, this, ()| {
            let mut g = this.inner.lock().unwrap();
            g.stdout = None;
            if let Some(mut child) = g.child.take() {
                let _ = child.kill();
                let _ = child.wait();
            }
            g.eof = true;
            // Drop leftover bytes — without this, a `:next()`
            // after `:close()` would drain stale pending data
            // before the eof check fires. TailUd / ConsoleStreamUd
            // both clear pending in close.
            g.pending.clear();
            Ok(())
        });

        methods.add_method("drain", |lua, this, timeout: Option<mlua::Value>| {
            let timeout_secs = super::result_ud::parse_duration(timeout, 0.5)?;
            let deadline = std::time::Instant::now()
                + std::time::Duration::from_secs_f64(timeout_secs.max(0.001));
            let mut g = this.inner.lock().unwrap();
            // Flush any leftover bytes from a prior expect /
            // read_until match — without this, drain silently
            // discarded whatever was past the matched suffix.
            let mut acc: Vec<Vec<u8>> = Vec::new();
            if !g.pending.is_empty() {
                acc.push(std::mem::take(&mut g.pending));
            }
            if g.eof {
                let table = lua.create_table()?;
                for (i, chunk) in acc.into_iter().enumerate() {
                    table.set(i + 1, lua.create_string(&chunk)?)?;
                }
                return Ok(table);
            }
            let mut buf = vec![0u8; 64 * 1024];
            if let Some(stdout) = g.stdout.as_mut() {
                use std::os::fd::AsRawFd;
                let fd = stdout.as_raw_fd();
                loop {
                    let now = std::time::Instant::now();
                    let Some(remaining) = deadline.checked_duration_since(now) else {
                        break;
                    };
                    let timeout_ms = (remaining.as_millis() as i64)
                        .clamp(0, i32::MAX as i64) as i32;
                    let mut pfd = libc::pollfd {
                        fd,
                        events: libc::POLLIN,
                        revents: 0,
                    };
                    // SAFETY: single valid pollfd, nfds=1.
                    let pr = unsafe { libc::poll(&mut pfd, 1, timeout_ms) };
                    if pr <= 0 {
                        break; // timed out or error → stop draining
                    }
                    match stdout.read(&mut buf) {
                        Ok(0) => {
                            g.eof = true;
                            break;
                        }
                        Ok(n) => acc.push(buf[..n].to_vec()),
                        Err(_) => break,
                    }
                }
            }
            let table = lua.create_table()?;
            for (i, chunk) in acc.into_iter().enumerate() {
                table.set(i + 1, lua.create_string(&chunk)?)?;
            }
            Ok(table)
        });

        methods.add_meta_method(MetaMethod::ToString, |_, _this, ()| {
            Ok("capture(bridge)".to_owned())
        });
    }
}

impl Drop for CaptureInner {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}
