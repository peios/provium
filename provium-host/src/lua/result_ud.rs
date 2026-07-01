//! Lua bindings for [`crate::RunResult`] and [`TailFileSession`].

use std::sync::{Arc, Mutex};

use mlua::{Lua, MetaMethod, UserData, UserDataFields, UserDataMethods, Value};

use provium_protocol::wire::ExitStatus;

use crate::vm::{Process, RunResult, VmTailSession, Worker};

/// Wrap a [`RunResult`] in a userdata for return to Lua.
pub(crate) fn wrap_run_result(lua: &Lua, result: RunResult) -> mlua::Result<Value> {
    lua.create_userdata(RunResultUd { inner: result })
        .map(Value::UserData)
}

#[derive(Clone)]
pub(crate) struct RunResultUd {
    inner: RunResult,
}

impl UserData for RunResultUd {
    fn add_fields<F: UserDataFields<Self>>(fields: &mut F) {
        // Mirrors the design's `result.exit_code`, `result.stdout`,
        // `result.stderr` shape — fields, not methods. DESIGN
        // says exit_code is an int; we use sentinels (-1 for
        // signalled, -2 for timed-out) so the type contract
        // holds and `r.exit_code == 0` doesn't silently misfire
        // on signal/timeout. The dedicated `signal` and
        // `timed_out` fields below let callers disambiguate.
        fields.add_field_method_get("exit_code", |_, this| {
            Ok(match this.inner.status {
                ExitStatus::Exited(code) => code,
                ExitStatus::Signalled(_) => -1,
                ExitStatus::TimedOut => -2,
            })
        });

        fields.add_field_method_get("stdout", |lua, this| {
            lua.create_string(&this.inner.stdout)
        });
        fields.add_field_method_get("stderr", |lua, this| {
            lua.create_string(&this.inner.stderr)
        });

        fields.add_field_method_get("status", |_, this| Ok(status_name(this.inner.status)));

        fields.add_field_method_get("timed_out", |_, this| {
            Ok(matches!(this.inner.status, ExitStatus::TimedOut))
        });

        // `signal` — POSIX signal number when the process was
        // killed by a signal, else nil. Lets test code do
        // `if r.signal == 9 then ...` without mistakenly
        // matching the -1 exit_code sentinel.
        fields.add_field_method_get("signal", |_, this| {
            Ok(match this.inner.status {
                ExitStatus::Signalled(n) => Some(n),
                _ => None,
            })
        });
    }

    fn add_methods<M: UserDataMethods<Self>>(methods: &mut M) {
        // `result:ok()` — convenience for the common `exit_code == 0`
        // check. Mirrors the design's Lua API.
        methods.add_method("ok", |_, this, ()| Ok(this.inner.ok()));

        // `result:assert_ok()` — raise on a non-zero exit. Carries
        // stdout/stderr in the message so test failures are
        // self-explanatory.
        methods.add_method("assert_ok", |_, this, ()| {
            if this.inner.ok() {
                Ok(())
            } else {
                // Surface the exit detail in the message so test
                // failures don't require a follow-up status check.
                // For signalled processes, naming the signal turns
                // "command failed: status=signalled" into the much
                // more useful "command failed: status=signalled
                // (SIGTERM 15)".
                let detail = match this.inner.status {
                    ExitStatus::Exited(code) => format!("status=exited code={code}"),
                    ExitStatus::Signalled(sig) => format!("status=signalled signal={sig}"),
                    ExitStatus::TimedOut => "status=timed_out".to_string(),
                };
                Err(mlua::Error::external(format!(
                    "command failed: {detail} stdout={:?} stderr={:?}",
                    this.inner.stdout_str(),
                    this.inner.stderr_str(),
                )))
            }
        });

        methods.add_meta_method(MetaMethod::ToString, |_, this, ()| {
            Ok(format!(
                "RunResult{{ status={}, stdout=<{}B>, stderr=<{}B> }}",
                status_name(this.inner.status),
                this.inner.stdout.len(),
                this.inner.stderr.len(),
            ))
        });
    }
}

fn status_name(s: ExitStatus) -> &'static str {
    match s {
        ExitStatus::Exited(_) => "exited",
        ExitStatus::Signalled(_) => "signalled",
        ExitStatus::TimedOut => "timed_out",
    }
}

// ---------------------------------------------------------------------------
// TailUd
// ---------------------------------------------------------------------------

/// Lua-facing wrapper for [`VmTailSession`].
///
/// `Mutex<Option<…>>` so `:close()` can take ownership and
/// subsequent calls return a clean error. The wrapped
/// `VmTailSession` carries a [`crate::vm::StreamGuard`] so the
/// VM's resource registry sees the stream go away when this
/// userdata is finally GC'd by Lua.
#[derive(Clone)]
pub(crate) struct TailUd {
    inner: Arc<Mutex<TailInner>>,
    /// Source path + line of the test/fixture frame that opened
    /// this stream. `None` if no test-root frame is on the stack
    /// at creation time (e.g. for streams opened from Rust).
    creation_site: Option<(String, i32)>,
}

/// Inner state behind [`TailUd`]'s mutex. The `pending` buffer
/// holds bytes that an earlier `expect`/`read_until` read past
/// the matched suffix — those bytes need to be returned at the
/// front of the next call's accumulator instead of being lost.
/// Without it, `:expect("ready")` followed by `:expect("ack")`
/// would silently discard whatever followed "ready" in the same
/// stream frame.
struct TailInner {
    session: Option<VmTailSession>,
    pending: Vec<u8>,
}

impl TailUd {
    /// Build with no creation-site attribution.
    #[allow(dead_code)]
    pub(crate) fn new(session: VmTailSession) -> Self {
        Self {
            inner: Arc::new(Mutex::new(TailInner {
                session: Some(session),
                pending: Vec::new(),
            })),
            creation_site: None,
        }
    }

    /// Alias for [`Self::new`] — kept so call sites that pre-date
    /// the explicit creation-site overload don't churn.
    #[allow(dead_code)]
    pub(crate) fn wrap(session: VmTailSession) -> Self {
        Self::new(session)
    }

    /// Build with a captured creation site. Used by Lua-side
    /// constructors that have a `&Lua` available.
    pub(crate) fn new_with_site(
        session: VmTailSession,
        site: Option<(String, i32)>,
    ) -> Self {
        Self {
            inner: Arc::new(Mutex::new(TailInner {
                session: Some(session),
                pending: Vec::new(),
            })),
            creation_site: site,
        }
    }
}

/// Parse a Lua-side duration argument into seconds, accepting
/// numbers (seconds) or strings with unit suffix per
/// `DESIGN.md` § Time and timeouts. `None` returns `default`.
///
/// Supported suffixes: `ms`, `s`, `m`, `h`. Bare numbers are
/// seconds. Invalid forms raise.
pub(crate) fn parse_duration(
    v: Option<Value>,
    default: f64,
) -> mlua::Result<f64> {
    let secs = match v {
        None | Some(Value::Nil) => return Ok(default),
        Some(Value::Integer(n)) => n as f64,
        Some(Value::Number(n)) => n,
        Some(Value::String(s)) => {
            let raw = s
                .to_str()
                .map_err(|e| mlua::Error::external(e.to_string()))?
                .to_string();
            parse_duration_str(&raw).ok_or_else(|| {
                mlua::Error::external(format!(
                    "duration: cannot parse `{raw}` (expected number or `Nms`/`Ns`/`Nm`/`Nh`)"
                ))
            })?
        }
        Some(other) => {
            return Err(mlua::Error::external(format!(
                "duration: expected number|string, got {}",
                other.type_name()
            )));
        }
    };
    // Reject NaN, negative, AND infinite durations. NaN/negative
    // would panic Duration::from_secs_f64; +inf would saturate
    // every callers' integer cast (clock:advance(math.huge)
    // silently advanced ~584 million years).
    if !secs.is_finite() || secs < 0.0 {
        return Err(mlua::Error::external(format!(
            "duration: must be a finite, non-negative number (got {secs})"
        )));
    }
    Ok(secs)
}

fn parse_duration_str(s: &str) -> Option<f64> {
    let s = s.trim();
    let (num, unit) = if let Some(stripped) = s.strip_suffix("ms") {
        (stripped, 0.001)
    } else if let Some(stripped) = s.strip_suffix('s') {
        (stripped, 1.0)
    } else if let Some(stripped) = s.strip_suffix('m') {
        (stripped, 60.0)
    } else if let Some(stripped) = s.strip_suffix('h') {
        (stripped, 3600.0)
    } else {
        (s, 1.0)
    };
    num.trim().parse::<f64>().ok().map(|n| n * unit)
}

/// Read the current test's name (if any) from the Lua global the
/// runner sets at test start. Returns `None` outside `test()` blocks
/// (file-scope / fixture build).
pub(crate) fn current_test_name(lua: &mlua::Lua) -> Option<String> {
    match lua.globals().get::<Value>("_provium_current_test").ok()? {
        Value::String(s) => s.to_str().ok().map(|s| s.to_string()),
        _ => None,
    }
}

/// Register a userdata with the test-framework's resource graph so
/// `_provium_close_*_scope` can close it in the design's
/// reverse-dependency order at scope end.
///
/// `kind` is one of `stream`, `proc`, `file`, `worker`, `bridge`,
/// `vm`. Unknown kinds are accepted but treated as the lowest
/// priority (closed last).
pub(crate) fn register_resource(
    lua: &mlua::Lua,
    ud: impl mlua::UserData + Clone + Send + 'static,
    kind: &'static str,
) -> mlua::Result<mlua::AnyUserData> {
    let ud_value = lua.create_userdata(ud)?;
    if let Ok(register) = lua
        .globals()
        .get::<mlua::Function>("_provium_register_resource")
    {
        let _ = register.call::<()>((ud_value.clone(), kind));
    }
    Ok(ud_value)
}

/// Walk the Lua call stack and return the first frame whose source
/// path looks like a `*.test.lua` or `*.fixture.lua` file —
/// the test author's frame, not a helper. Falls back to the
/// immediate caller's frame when no matching frame is found.
///
/// Per `DESIGN.md` § Snapshot precondition / Resource graph tracking
/// (matches pytest's `tb_filter` pattern).
pub(crate) fn capture_creation_site(lua: &mlua::Lua) -> Option<(String, i32)> {
    let mut fallback: Option<(String, i32)> = None;
    for level in 1..32 {
        let dbg = match lua.inspect_stack(level) {
            Some(d) => d,
            None => break,
        };
        let names = dbg.source();
        let Some(src) = names.source.as_deref() else { continue };
        let src_str = String::from_utf8_lossy(src.as_ref()).into_owned();
        let line = dbg.curr_line();
        if fallback.is_none() {
            fallback = Some((src_str.clone(), line));
        }
        // Lua source-name convention: filenames are passed via
        // `set_name(...)` as either the bare filename or a path. We
        // accept either when matched against the suffix.
        let trimmed = src_str.trim_start_matches('@');
        if trimmed.ends_with(".test.lua") || trimmed.ends_with(".fixture.lua") {
            return Some((trimmed.to_owned(), line));
        }
    }
    fallback
}

impl UserData for TailUd {
    fn add_methods<M: UserDataMethods<Self>>(methods: &mut M) {
        // `tail:creation_site()` — debug accessor for the test
        // author's `(file, line)` at stream creation. Returns nil
        // if the stream was opened from a frame that didn't match
        // a `*.test.lua` / `*.fixture.lua` source.
        methods.add_method("creation_site", |lua, this, ()| {
            match &this.creation_site {
                Some((file, line)) => {
                    let table = lua.create_table()?;
                    table.set("file", file.clone())?;
                    table.set("line", *line)?;
                    Ok(Value::Table(table))
                }
                None => Ok(Value::Nil),
            }
        });

        // `tail:next([timeout])` — pull the next frame.
        // Returns the bytes as a Lua string, nil at EOF or on
        // timeout. Per `DESIGN.md` § Stream — timeout accepts
        // numeric seconds or unit-string ("500ms"/"5s").
        methods.add_method("next", |lua, this, timeout: Option<Value>| {
            let mut guard = this
                .inner
                .lock()
                .map_err(|_| mlua::Error::external("tail mutex poisoned"))?;
            // If a previous expect/read_until left bytes after the
            // matched suffix, return them as the next "frame"
            // before going back to the wire.
            if !guard.pending.is_empty() {
                let bytes = std::mem::take(&mut guard.pending);
                return lua.create_string(&bytes).map(Value::String);
            }
            // R9 stream-Mi1: closed stream is semantically EOF —
            // return nil to match ConsoleStreamUd / CaptureUd, so
            // `while s:next() do … end` loops terminate cleanly.
            // Was raising `tail closed` which broke the idiom and
            // diverged from the other two stream types.
            let Some(session) = guard.session.as_mut() else {
                return Ok(Value::Nil);
            };
            // Apply best-effort read timeout to the underlying
            // stream. Default-impl returns NotSupported for
            // connectors that can't honour it.
            let restore = if let Some(v) = timeout {
                let secs = parse_duration(Some(v), 10.0)?;
                let _ = session.set_read_timeout(Some(
                    std::time::Duration::from_secs_f64(secs),
                ));
                true
            } else {
                false
            };
            let outcome = session.next_frame();
            if restore {
                let _ = session.set_read_timeout(None);
            }
            match outcome {
                Ok(Some(frame)) => lua.create_string(&frame.data).map(Value::String),
                Ok(None) => Ok(Value::Nil),
                Err(crate::ClientError::Frame(
                    provium_protocol::FrameError::Io(e),
                )) if matches!(
                    e.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
                {
                    Ok(Value::Nil)
                }
                Err(e) => Err(mlua::Error::external(e)),
            }
        });

        methods.add_method("close", |_, this, ()| {
            let mut guard = this
                .inner
                .lock()
                .map_err(|_| mlua::Error::external("tail mutex poisoned"))?;
            // Drop the session — connection closes — and clear
            // any leftover pending bytes.
            guard.session = None;
            guard.pending.clear();
            Ok(())
        });

        methods.add_method("eof", |_, this, ()| {
            let guard = this
                .inner
                .lock()
                .map_err(|_| mlua::Error::external("tail mutex poisoned"))?;
            // Pending bytes mean there's still data to deliver
            // before EOF is honest.
            if !guard.pending.is_empty() {
                return Ok(false);
            }
            Ok(guard.session.as_ref().map(|s| s.is_eof()).unwrap_or(true))
        });

        // stream:read_until(pattern[, timeout_seconds]) — pull frames
        // until the pattern is seen; returns the matched substring.
        methods.add_method(
            "read_until",
            |lua, this, (pattern, timeout): (String, Option<Value>)| {
                let timeout =
                    std::time::Duration::from_secs_f64(parse_duration(timeout, 10.0)?);
                let deadline = std::time::Instant::now() + timeout;
                let mut acc = {
                    // Drain any leftover bytes from a previous call
                    // first so we don't lose data the prior match
                    // had read past its needle.
                    let mut g = this
                        .inner
                        .lock()
                        .map_err(|_| mlua::Error::external("tail mutex poisoned"))?;
                    std::mem::take(&mut g.pending)
                };
                if let Some(idx) = haystack_find(&acc, pattern.as_bytes()) {
                    let consume = idx + pattern.len();
                    let matched = acc[..consume].to_vec();
                    let leftover = acc.split_off(consume);
                    if !leftover.is_empty() {
                        let mut g = this
                            .inner
                            .lock()
                            .map_err(|_| mlua::Error::external("tail mutex poisoned"))?;
                        g.pending = leftover;
                    }
                    return lua.create_string(&matched).map(Value::String);
                }
                loop {
                    let mut guard = this
                        .inner
                        .lock()
                        .map_err(|_| mlua::Error::external("tail mutex poisoned"))?;
                    let session = match guard.session.as_mut() {
                        Some(s) => s,
                        None => {
                            return Err(mlua::Error::external("stream closed"));
                        }
                    };
                    let now = std::time::Instant::now();
                    let remaining = deadline
                        .checked_duration_since(now)
                        .unwrap_or(std::time::Duration::ZERO);
                    if remaining.is_zero() {
                        return Err(mlua::Error::external(format!(
                            "read_until timed out waiting for `{pattern}`"
                        )));
                    }
                    let _ = session.set_read_timeout(Some(remaining));
                    match session.next_frame() {
                        Ok(Some(f)) => acc.extend_from_slice(&f.data),
                        Ok(None) => return Err(mlua::Error::external("stream EOF")),
                        // Read-timeout fired mid-iteration. Loop
                        // around to the deadline check at top so
                        // the user gets the clean
                        // "read_until timed out waiting for `…`"
                        // message rather than a raw EAGAIN. Mirrors
                        // drain's WouldBlock-as-no-more-data
                        // handling.
                        Err(crate::ClientError::Frame(
                            provium_protocol::FrameError::Io(e),
                        )) if matches!(
                            e.kind(),
                            std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                        ) => {
                            // fall through to deadline check
                        }
                        Err(e) => return Err(mlua::Error::external(e)),
                    }
                    if let Some(idx) = haystack_find(&acc, pattern.as_bytes()) {
                        let consume = idx + pattern.len();
                        let matched = acc[..consume].to_vec();
                        let leftover = acc.split_off(consume);
                        if !leftover.is_empty() {
                            guard.pending = leftover;
                        }
                        // Restore to blocking so a follow-up bare
                        // tail:next() doesn't inherit the tiny
                        // tail-end timeout from this loop and
                        // return nil immediately.
                        if let Some(s) = guard.session.as_mut() {
                            let _ = s.set_read_timeout(None);
                        }
                        drop(guard);
                        return lua.create_string(&matched).map(Value::String);
                    }
                    drop(guard);
                    if std::time::Instant::now() >= deadline {
                        // Same restore on the timeout path.
                        if let Ok(mut g) = this.inner.lock() {
                            if let Some(s) = g.session.as_mut() {
                                let _ = s.set_read_timeout(None);
                            }
                        }
                        return Err(mlua::Error::external(format!(
                            "read_until timed out waiting for `{pattern}`"
                        )));
                    }
                }
            },
        );

        // stream:expect(pattern[, timeout]) — assertion variant of
        // read_until. Returns nil; raises on miss.
        methods.add_method(
            "expect",
            |_, this, (pattern, timeout): (String, Option<Value>)| {
                let timeout =
                    std::time::Duration::from_secs_f64(parse_duration(timeout, 10.0)?);
                let deadline = std::time::Instant::now() + timeout;
                let mut acc = {
                    let mut g = this
                        .inner
                        .lock()
                        .map_err(|_| mlua::Error::external("tail mutex poisoned"))?;
                    std::mem::take(&mut g.pending)
                };
                if let Some(idx) = haystack_find(&acc, pattern.as_bytes()) {
                    let consume = idx + pattern.len();
                    let leftover = acc.split_off(consume);
                    if !leftover.is_empty() {
                        let mut g = this
                            .inner
                            .lock()
                            .map_err(|_| mlua::Error::external("tail mutex poisoned"))?;
                        g.pending = leftover;
                    }
                    return Ok(());
                }
                loop {
                    let mut guard = this
                        .inner
                        .lock()
                        .map_err(|_| mlua::Error::external("tail mutex poisoned"))?;
                    let session = match guard.session.as_mut() {
                        Some(s) => s,
                        None => {
                            return Err(mlua::Error::external("stream closed"));
                        }
                    };
                    let now = std::time::Instant::now();
                    let remaining = deadline
                        .checked_duration_since(now)
                        .unwrap_or(std::time::Duration::ZERO);
                    if remaining.is_zero() {
                        return Err(mlua::Error::external(format!(
                            "expect timed out waiting for `{pattern}`"
                        )));
                    }
                    let _ = session.set_read_timeout(Some(remaining));
                    match session.next_frame() {
                        Ok(Some(f)) => acc.extend_from_slice(&f.data),
                        Ok(None) => return Err(mlua::Error::external("stream EOF")),
                        // Read-timeout — fall through to the
                        // deadline check at top so the user gets
                        // the clean "expect timed out waiting for
                        // `…`" message rather than raw EAGAIN.
                        Err(crate::ClientError::Frame(
                            provium_protocol::FrameError::Io(e),
                        )) if matches!(
                            e.kind(),
                            std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                        ) => {
                            // fall through
                        }
                        Err(e) => return Err(mlua::Error::external(e)),
                    }
                    if let Some(idx) = haystack_find(&acc, pattern.as_bytes()) {
                        let consume = idx + pattern.len();
                        let leftover = acc.split_off(consume);
                        if !leftover.is_empty() {
                            guard.pending = leftover;
                        }
                        if let Some(s) = guard.session.as_mut() {
                            let _ = s.set_read_timeout(None);
                        }
                        drop(guard);
                        return Ok(());
                    }
                    drop(guard);
                    if std::time::Instant::now() >= deadline {
                        if let Ok(mut g) = this.inner.lock() {
                            if let Some(s) = g.session.as_mut() {
                                let _ = s.set_read_timeout(None);
                            }
                        }
                        return Err(mlua::Error::external(format!(
                            "expect timed out waiting for `{pattern}`"
                        )));
                    }
                }
            },
        );

        // stream:drain([timeout]) — collect every available frame
        // and return as a list. Stops when next_frame returns nil
        // OR timeout lapses, whichever first.
        methods.add_method("drain", |lua, this, timeout: Option<Value>| {
            let timeout =
                std::time::Duration::from_secs_f64(parse_duration(timeout, 0.5)?);
            let deadline = std::time::Instant::now() + timeout;
            let table = lua.create_table()?;
            let mut idx = 1;
            // Drain any pending bytes from a previous expect/
            // read_until first so they show up as the first
            // frame.
            {
                let mut g = this
                    .inner
                    .lock()
                    .map_err(|_| mlua::Error::external("tail mutex poisoned"))?;
                if !g.pending.is_empty() {
                    let bytes = std::mem::take(&mut g.pending);
                    table.set(idx, lua.create_string(&bytes)?)?;
                    idx += 1;
                }
            }
            loop {
                let now = std::time::Instant::now();
                let Some(remaining) = deadline.checked_duration_since(now) else {
                    break;
                };
                let mut guard = this
                    .inner
                    .lock()
                    .map_err(|_| mlua::Error::external("tail mutex poisoned"))?;
                let session = match guard.session.as_mut() {
                    Some(s) => s,
                    None => break,
                };
                // Bound this iteration's blocking read to the
                // remaining deadline; without this, next_frame
                // blocks indefinitely on the underlying socket
                // and drain ignores its caller-supplied timeout.
                let _ = session.set_read_timeout(Some(remaining));
                let result = session.next_frame();
                // Restore to blocking so a subsequent bare next()
                // doesn't inherit the tiny tail-end timeout.
                let _ = session.set_read_timeout(None);
                match result {
                    Ok(Some(f)) => {
                        table.set(idx, lua.create_string(&f.data)?)?;
                        idx += 1;
                    }
                    Ok(None) => break,
                    // Treat WouldBlock / TimedOut as "no more data
                    // within our deadline" rather than a hard error.
                    // Mirrors `next`'s behavior — without this,
                    // drain on a quiet stream raises EAGAIN to the
                    // test author who can't act on it usefully.
                    Err(crate::ClientError::Frame(
                        provium_protocol::FrameError::Io(e),
                    )) if matches!(
                        e.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                    ) => break,
                    Err(e) => return Err(mlua::Error::external(e)),
                }
            }
            Ok(Value::Table(table))
        });
    }
}

fn haystack_find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() {
        return Some(0);
    }
    if hay.len() < needle.len() {
        return None;
    }
    for i in 0..=hay.len() - needle.len() {
        if &hay[i..i + needle.len()] == needle {
            return Some(i);
        }
    }
    None
}

// ---------------------------------------------------------------------------
// ProcessUd — async-process handle (slice 10)
// ---------------------------------------------------------------------------

/// Lua-facing wrapper for [`Process`].
#[derive(Clone)]
pub(crate) struct ProcessUd {
    process: Process,
}

impl ProcessUd {
    pub(crate) fn wrap(process: Process) -> Self {
        Self { process }
    }
}

impl UserData for ProcessUd {
    fn add_methods<M: UserDataMethods<Self>>(methods: &mut M) {
        methods.add_method("wait", |lua, this, timeout: Option<Value>| {
            // Accepts numeric seconds or string with unit suffix
            // per `DESIGN.md` § Time and timeouts. `nil` means wait
            // forever (no agent-side timeout).
            //
            // Reject `0` explicitly: timeout=0 looks like a non-
            // blocking poll but the agent-side wait_with_timeout
            // sees the deadline already past and SIGKILLs the
            // child immediately. That's a footgun; if a caller
            // wants to poll, they want proc:status() or a tiny
            // positive timeout that lets the agent's try_wait
            // see exit-without-kill semantics.
            let timeout_ms = match timeout {
                None | Some(Value::Nil) => None,
                Some(other) => {
                    let secs = parse_duration(Some(other), 0.0)?;
                    if secs == 0.0 {
                        return Err(mlua::Error::external(
                            "proc:wait: timeout=0 would SIGKILL the process \
                             immediately. Use proc:status() to poll, or pass \
                             a small positive timeout (e.g. \"100ms\")."
                        ));
                    }
                    Some((secs * 1000.0) as u64)
                }
            };
            let result = this
                .process
                .wait_with_timeout(timeout_ms)
                .map_err(mlua::Error::external)?;
            wrap_run_result(lua, result)
        });

        methods.add_method("kill", |_, this, sig: Option<Value>| {
            // Accept either a numeric signal (`9`) or a friendly
            // string (`"term"`, `"kill"`, `"int"`, `"hup"`,
            // `"stop"`, `"cont"`).
            let signal = match sig {
                None => libc::SIGTERM,
                Some(Value::Integer(n)) => n as i32,
                Some(Value::String(s)) => {
                    let raw = s
                        .to_str()
                        .map_err(|e| e.to_string())
                        .map_err(mlua::Error::external)?
                        .to_string();
                    signal_name_to_num(&raw).map_err(mlua::Error::external)?
                }
                Some(other) => {
                    return Err(mlua::Error::external(format!(
                        "kill: signal must be int or string, got {}",
                        other.type_name()
                    )));
                }
            };
            this.process
                .kill(signal)
                .map_err(mlua::Error::external)?;
            Ok(())
        });

        methods.add_method("pid", |_, this, ()| {
            // Kernel PID, fetched live via GetPid. The opaque
            // provium handle is exposed separately via `:handle()`
            // for diagnostic / scope-tracking purposes.
            this.process.pid().map_err(mlua::Error::external).map(|p| p as u64)
        });
        methods.add_method("handle", |_, this, ()| Ok(this.process.handle().get()));

        methods.add_method("status", |_, this, ()| {
            this.process.status().map_err(mlua::Error::external)
        });
        methods.add_method("stdin_write", |_, this, data: mlua::String| {
            this.process
                .stdin_write(data.as_bytes().to_vec())
                .map_err(mlua::Error::external)
        });
        methods.add_method("close_stdin", |_, this, ()| {
            this.process.close_stdin().map_err(mlua::Error::external)?;
            Ok(())
        });
        // Stream variants of stdout/stderr capture. The agent polls
        // the captured-output buffer + emits frames as they appear.
        methods.add_method("stdout_stream", |lua, this, ()| {
            let session = this
                .process
                .stdout_stream()
                .map_err(mlua::Error::external)?;
            let site = capture_creation_site(lua);
            let test_name = current_test_name(lua);
            this.process.parent_vm().set_stream_meta(
                &session,
                crate::vm::StreamMeta {
                    kind: "proc_stdout_stream".into(),
                    detail: this.process.handle().to_string(),
                    creation_site: site.clone(),
                    test_name,
                },
            );
            register_resource(lua, TailUd::new_with_site(session, site), "stream")
        });
        methods.add_method("stderr_stream", |lua, this, ()| {
            let session = this
                .process
                .stderr_stream()
                .map_err(mlua::Error::external)?;
            let site = capture_creation_site(lua);
            let test_name = current_test_name(lua);
            this.process.parent_vm().set_stream_meta(
                &session,
                crate::vm::StreamMeta {
                    kind: "proc_stderr_stream".into(),
                    detail: this.process.handle().to_string(),
                    creation_site: site.clone(),
                    test_name,
                },
            );
            register_resource(lua, TailUd::new_with_site(session, site), "stream")
        });
        // proc:close() — cleanup hook. Per `DESIGN.md` § Auto-close
        // ordering: when a test scope ends and a process is still
        // running, signal it (SIGTERM, then SIGKILL on timeout) so
        // we don't leak children to file end. Idempotent: errors
        // (e.g. process already exited) are swallowed.
        methods.add_method("close", |_, this, ()| {
            // If the test already called proc:wait, the process
            // has been reaped and the agent-side slot is gone.
            // The Process drop-guard records that via `consumed`;
            // skip the kill+wait round-trip in that case so
            // scope-end cleanup doesn't add unnecessary latency
            // and noise (UnknownHandle errors swallowed on the
            // floor).
            if this.process.consumed() {
                return Ok(());
            }
            // Best-effort: signal first, then reap. The agent's
            // wait_with_timeout escalates to SIGKILL if the child
            // doesn't exit within the timeout.
            let _ = this.process.kill(libc::SIGTERM);
            let _ = this.process.wait_with_timeout(Some(2_000));
            Ok(())
        });
        methods.add_method("signal", |_, this, sig: Value| {
            // Same parse as proc:kill — accept numeric or symbolic
            // names ("term", "kill", "int", "hup", "stop", "cont").
            let signal = match sig {
                Value::Integer(n) => n as i32,
                Value::String(s) => {
                    let raw = s
                        .to_str()
                        .map_err(|e| e.to_string())
                        .map_err(mlua::Error::external)?
                        .to_string();
                    signal_name_to_num(&raw).map_err(mlua::Error::external)?
                }
                other => {
                    return Err(mlua::Error::external(format!(
                        "proc:signal: expected integer or signal name, got {}",
                        other.type_name()
                    )));
                }
            };
            this.process.kill(signal).map_err(mlua::Error::external)?;
            Ok(())
        });

        methods.add_meta_method(MetaMethod::ToString, |_, this, ()| {
            Ok(format!("process({})", this.process.handle()))
        });
    }
}

// ---------------------------------------------------------------------------
// WorkerUd — vm:spawn_worker() (slice 10.5)
// ---------------------------------------------------------------------------

/// Lua-facing wrapper for [`Worker`].
#[derive(Clone)]
pub(crate) struct WorkerUd {
    worker: Worker,
}

impl WorkerUd {
    pub(crate) fn wrap(worker: Worker) -> Self {
        Self { worker }
    }
}

impl UserData for WorkerUd {
    fn add_methods<M: UserDataMethods<Self>>(methods: &mut M) {
        methods.add_method("run", |lua, this, args: mlua::Variadic<Value>| {
            let exec = super::vm_ud::build_exec_args_public(&args)
                .map_err(mlua::Error::external)?;
            let result = this.worker.run(exec).map_err(mlua::Error::external)?;
            wrap_run_result(lua, result)
        });
        // Worker:run_async / open_file / syscall / etc.
        //
        // Routing model: worker ops dispatch through the parent VM's
        // agent client (same vsock CID + port). Process and file
        // handles allocated under a worker live in the worker's
        // [`AgentState`] subnamespace on the agent side; on the host
        // side, the resulting handles are wrapped in [`Process`] /
        // [`crate::lua::file_ud::FileUd`] tied to the parent VM —
        // the difference is invisible to test code, which is the
        // intent of the "same VM API" design line.
        methods.add_method("run_async", |lua, this, args: mlua::Variadic<Value>| {
            let exec = super::vm_ud::build_exec_args_public(&args)
                .map_err(mlua::Error::external)?;
            if exec.timeout_ms.is_some() {
                return Err(mlua::Error::external(
                    "worker:run_async: `timeout` is not honoured here; \
                     pass it to `proc:wait(timeout)` instead",
                ));
            }
            let async_args = provium_protocol::wire::RunAsyncArgs {
                cmd: exec.cmd,
                args: exec.args,
                env: exec.env,
                env_clear: exec.env_clear,
                cwd: exec.cwd,
            };
            let proc = this
                .worker
                .run_async(async_args)
                .map_err(mlua::Error::external)?;
            // Register so the scope walker can SIGTERM+wait this
            // process at scope end — the vm:run_async sibling does
            // the same. Without it, a worker-spawned child leaks
            // past the test boundary.
            register_resource(lua, ProcessUd::wrap(proc), "proc")
        });
        methods.add_method(
            "open_file",
            |lua, this, (path, mode): (String, mlua::Table)| {
                let mode_value =
                    super::vm_ud::open_mode_from_table_public(&mode)?;
                let h = this
                    .worker
                    .open_file(path.clone(), mode_value)
                    .map_err(mlua::Error::external)?;
                // Register so the scope walker auto-closes this
                // file at scope end. Mirrors vm:open_file's
                // resource-registry hook — without it, a
                // worker-opened file leaks past the test.
                register_resource(
                    lua,
                    super::file_ud::FileUd::wrap_with_path(
                        this.worker.parent_vm().clone(),
                        h,
                        path,
                    ),
                    "file",
                )
            },
        );
        // Mirror vm:syscall — DESIGN guarantees workers expose the
        // "same VM API for guest ops". Accepts both the plain
        // (nr, int, int, ...) form and the table form
        // {args=, bufs=, ptrs=}; returns {ret, result, errno, out_bufs}.
        methods.add_method("syscall", |lua, this, args: mlua::Variadic<Value>| {
            let nr_v = args.first().cloned().ok_or_else(|| {
                mlua::Error::external("worker:syscall(nr, ...): missing nr")
            })?;
            let nr = match nr_v {
                Value::Integer(n) => n,
                Value::Number(n) => n as i64,
                _ => return Err(mlua::Error::external(
                    "worker:syscall: nr must be integer",
                )),
            };
            let mut filled = [0i64; 6];
            let mut bufs: Vec<Vec<u8>> = Vec::new();
            let mut ptrs: Vec<u8> = Vec::new();
            let mut nested: Vec<provium_protocol::wire::NestedPtr> = Vec::new();
            let second = args.get(1).cloned();
            if let Some(Value::Table(t)) = second {
                if let Ok(arg_tbl) = t.get::<mlua::Table>("args") {
                    for (i, slot) in filled.iter_mut().enumerate() {
                        let v: Option<i64> = arg_tbl.get((i + 1) as i64).ok();
                        if let Some(v) = v {
                            *slot = v;
                        }
                    }
                }
                if let Ok(buf_tbl) = t.get::<mlua::Table>("bufs") {
                    for pair in buf_tbl.sequence_values::<mlua::String>() {
                        let s = pair?;
                        bufs.push(s.as_bytes().to_vec());
                    }
                }
                if let Ok(ptr_tbl) = t.get::<mlua::Table>("ptrs") {
                    for pair in ptr_tbl.sequence_values::<u8>() {
                        ptrs.push(pair?);
                    }
                }
                // Mirrors vm:syscall — splice bufs[child] into bufs[parent]
                // at a byte offset (1-based buf indices).
                if let Ok(nested_tbl) = t.get::<mlua::Table>("nested") {
                    for entry in nested_tbl.sequence_values::<mlua::Table>() {
                        let e = entry?;
                        let parent: i64 = e.get("parent")?;
                        let child: i64 = e.get("child")?;
                        let offset: u32 = e.get("offset")?;
                        nested.push(provium_protocol::wire::NestedPtr {
                            parent: (parent - 1).max(0) as u8,
                            child: (child - 1).max(0) as u8,
                            offset,
                        });
                    }
                }
            } else {
                let mut all: Vec<i64> = Vec::new();
                for v in args.iter().skip(1) {
                    let n = match v {
                        Value::Integer(n) => *n,
                        Value::Number(n) => *n as i64,
                        _ => 0,
                    };
                    all.push(n);
                }
                for (i, slot) in filled.iter_mut().enumerate() {
                    if let Some(v) = all.get(i) {
                        *slot = *v;
                    }
                }
            }
            let result = this
                .worker
                .syscall_with_bufs(nr, filled, bufs, ptrs, nested)
                .map_err(mlua::Error::external)?;
            let table = lua.create_table()?;
            table.set("ret", result.ret)?;
            table.set("result", result.ret)?;
            table.set("errno", result.errno)?;
            let outs = lua.create_table()?;
            for (i, b) in result.out_bufs.into_iter().enumerate() {
                outs.set(i + 1, lua.create_string(&b)?)?;
            }
            table.set("out_bufs", outs)?;
            Ok(table)
        });
        // worker:syscall_async(nr, ...) — same call forms as worker:syscall,
        // but returns a handle immediately; the worker runs the syscall on a
        // background thread so the host can serve a source (or drive other
        // workers) while it blocks. Collect with handle:await().
        methods.add_method("syscall_async", |_, this, args: mlua::Variadic<Value>| {
            let nr_v = args.first().cloned().ok_or_else(|| {
                mlua::Error::external("worker:syscall_async(nr, ...): missing nr")
            })?;
            let nr = match nr_v {
                Value::Integer(n) => n,
                Value::Number(n) => n as i64,
                _ => return Err(mlua::Error::external("worker:syscall_async: nr must be integer")),
            };
            let mut filled = [0i64; 6];
            let mut bufs: Vec<Vec<u8>> = Vec::new();
            let mut ptrs: Vec<u8> = Vec::new();
            let mut nested: Vec<provium_protocol::wire::NestedPtr> = Vec::new();
            if let Some(Value::Table(t)) = args.get(1).cloned() {
                if let Ok(arg_tbl) = t.get::<mlua::Table>("args") {
                    for (i, slot) in filled.iter_mut().enumerate() {
                        if let Some(v) = arg_tbl.get::<i64>((i + 1) as i64).ok() {
                            *slot = v;
                        }
                    }
                }
                if let Ok(buf_tbl) = t.get::<mlua::Table>("bufs") {
                    for pair in buf_tbl.sequence_values::<mlua::String>() {
                        bufs.push(pair?.as_bytes().to_vec());
                    }
                }
                if let Ok(ptr_tbl) = t.get::<mlua::Table>("ptrs") {
                    for pair in ptr_tbl.sequence_values::<u8>() {
                        ptrs.push(pair?);
                    }
                }
                if let Ok(nested_tbl) = t.get::<mlua::Table>("nested") {
                    for entry in nested_tbl.sequence_values::<mlua::Table>() {
                        let e = entry?;
                        let parent: i64 = e.get("parent")?;
                        let child: i64 = e.get("child")?;
                        let offset: u32 = e.get("offset")?;
                        nested.push(provium_protocol::wire::NestedPtr {
                            parent: (parent - 1).max(0) as u8,
                            child: (child - 1).max(0) as u8,
                            offset,
                        });
                    }
                }
            } else {
                for (i, v) in args.iter().skip(1).enumerate() {
                    if i >= 6 {
                        break;
                    }
                    filled[i] = match v {
                        Value::Integer(n) => *n,
                        Value::Number(n) => *n as i64,
                        _ => 0,
                    };
                }
            }
            let pending = this
                .worker
                .begin_syscall_with_bufs(nr, filled, bufs, ptrs, nested)
                .map_err(mlua::Error::external)?;
            Ok(crate::lua::vm_ud::PendingWorkerSyscallUd::new(pending))
        });
        methods.add_method("kill", |_, this, sig: Option<Value>| {
            // Broadcast a signal to every async process in this
            // worker's namespace. Mirrors proc:kill — the
            // catch-all arm raises an error rather than silently
            // defaulting to SIGTERM (R9 stream-M1: silent SIGTERM
            // hid bugs in test code that passed e.g. a table).
            let signal = match sig {
                None => libc::SIGTERM,
                Some(Value::Integer(n)) => n as i32,
                Some(Value::String(s)) => {
                    let raw = s
                        .to_str()
                        .map_err(|e| e.to_string())
                        .map_err(mlua::Error::external)?
                        .to_string();
                    signal_name_to_num(&raw).map_err(mlua::Error::external)?
                }
                Some(other) => {
                    return Err(mlua::Error::external(format!(
                        "worker:kill: signal must be int or string, got {}",
                        other.type_name(),
                    )));
                }
            };
            this.worker.kill(signal).map_err(mlua::Error::external)?;
            Ok(())
        });
        methods.add_method("join", |_, this, ()| {
            let exit_status = this.worker.join().map_err(mlua::Error::external)?;
            Ok(exit_status)
        });
        methods.add_method("handle", |_, this, ()| Ok(this.worker.handle().get()));
        // Auto-close hook: Workers are registered with kind="worker"
        // and the scope walker dispatches `:close()` at scope end
        // (DESIGN.md § Auto-close ordering step 4). Without this
        // method mlua raises "method not found" mid-cleanup. SIGTERM
        // every in-flight worker child, then reap.
        methods.add_method("close", |_, this, ()| {
            let _ = this.worker.kill(libc::SIGTERM);
            let _ = this.worker.join();
            Ok(())
        });
    }
}

// ---------------------------------------------------------------------------
// ConsoleUd — vm:console() (slice 11.5)
// ---------------------------------------------------------------------------

/// Lua-facing console binding.
///
/// `:read` returns the captured console log to date. `:expect`
/// polls the log waiting for a substring (with timeout). `:write`
/// is documented as future work — the agent's `-serial file:` log
/// is currently outbound-only; slice 11.7 swaps in a bidirectional
/// chardev so writes can land.
#[derive(Clone)]
pub(crate) struct ConsoleUd {
    vm: crate::vm::Vm,
}

impl ConsoleUd {
    pub(crate) fn new(vm: crate::vm::Vm) -> Self {
        Self { vm }
    }
}

impl UserData for ConsoleUd {
    fn add_methods<M: UserDataMethods<Self>>(methods: &mut M) {
        // console:read() returns a Stream userdata (`:next`,
        // `:read_until`, `:expect`, `:drain`, `:close`, `:eof`)
        // backed by the QEMU chardev socket — per `DESIGN.md`
        // § Console.
        methods.add_method("read", |lua, this, ()| {
            let path = this.vm.console_socket_path().ok_or_else(|| {
                mlua::Error::external(
                    "console:read: VMM does not expose a console socket",
                )
            })?;
            let site = capture_creation_site(lua);
            let test_name = current_test_name(lua);
            // Register the console stream in the VM's resource
            // registry so vm:snapshot's open-stream precondition
            // sees it (without this an open console:read would let
            // a snapshot proceed silently with the socket live).
            let guard = this.vm.register_console_stream(crate::vm::StreamMeta {
                kind: "console_read".into(),
                detail: format!("\"{}\"", path.display()),
                creation_site: site.clone(),
                test_name,
            });
            let stream = super::console_stream_ud::ConsoleStreamUd::connect_with_guard(
                &path, site, guard,
            )
            .map_err(mlua::Error::external)?;
            register_resource(lua, stream, "stream")
        });

        // Legacy path: `console:read_log()` returns the entire
        // captured log as a string. Kept for tests that want the
        // simple snapshot shape; design's `read` is the streaming
        // form.
        methods.add_method("read_log", |lua, this, ()| {
            let bytes = this.vm.console_read().map_err(mlua::Error::external)?;
            lua.create_string(&bytes).map(Value::String)
        });

        // console:expect(pattern[, timeout_seconds]) — poll the
        // console log until `pattern` appears or `timeout` lapses.
        // Default timeout: 30s. Returns the matched substring.
        methods.add_method(
            "expect",
            |lua, this, (pattern, timeout): (String, Option<Value>)| {
                let timeout =
                    std::time::Duration::from_secs_f64(parse_duration(timeout, 30.0)?);
                let deadline = std::time::Instant::now() + timeout;
                let mut last_len = 0usize;
                loop {
                    let bytes = this
                        .vm
                        .console_read()
                        .map_err(mlua::Error::external)?;
                    if bytes.len() >= last_len {
                        let view =
                            String::from_utf8_lossy(&bytes[last_len..]);
                        if view.contains(&pattern) {
                            return lua
                                .create_string(pattern.as_bytes())
                                .map(Value::String);
                        }
                        last_len = bytes.len();
                    }
                    if std::time::Instant::now() >= deadline {
                        return Err(mlua::Error::external(format!(
                            "console:expect timed out waiting for `{pattern}`"
                        )));
                    }
                    std::thread::sleep(std::time::Duration::from_millis(50));
                }
            },
        );

        methods.add_method(
            "write",
            |_, this, (data, opts): (mlua::String, Option<mlua::Table>)| {
                let bytes = data.as_bytes().to_vec();
                // Accept the canonical `timeout = duration` form
                // (string `"500ms"` or seconds-float). The
                // earlier `timeout_ms` integer key + `f64`-only
                // `timeout` block silently dropped any
                // string-form duration.
                let timeout = match opts {
                    Some(t) => {
                        // Reject the legacy `timeout_ms` key
                        // explicitly — early DESIGN drafts
                        // documented it, and the test corpus may
                        // still carry calls. Silently ignoring
                        // the key would be the same silent-pass
                        // pattern the matrix audit kept catching.
                        if t.contains_key("timeout_ms").unwrap_or(false) {
                            return Err(mlua::Error::external(
                                "console:write: opts.timeout_ms is not supported \
                                 (use `timeout = \"500ms\"` or `timeout = 0.5`)",
                            ));
                        }
                        let raw = t.get::<Option<Value>>("timeout")?;
                        let secs = parse_duration(raw, 0.0)?;
                        if secs > 0.0 {
                            Some(std::time::Duration::from_secs_f64(secs))
                        } else {
                            None
                        }
                    }
                    None => None,
                };
                this.vm
                    .console_write_timeout(&bytes, timeout)
                    .map_err(mlua::Error::external)
                    .map(|_| ())
            },
        );
        methods.add_method("close", |_, _this, ()| Ok(()));
    }
}

// ---------------------------------------------------------------------------
// ClockUd — vm:clock() (slice 11)
// ---------------------------------------------------------------------------

/// Lua-facing clock binding. All ops dispatch into the parent
/// [`crate::vm::Vm`].
#[derive(Clone)]
pub(crate) struct ClockUd {
    vm: crate::vm::Vm,
}

impl ClockUd {
    pub(crate) fn new(vm: crate::vm::Vm) -> Self {
        Self { vm }
    }
}

impl UserData for ClockUd {
    fn add_methods<M: UserDataMethods<Self>>(methods: &mut M) {
        // Returns seconds-since-epoch as a float. Note: at the
        // current epoch (~2e9 sec) f64's ~15-decimal mantissa
        // bounds the precision around 250 ns. Tests that need
        // exact ns roundtrip should use clock:get_ns instead.
        methods.add_method("get", |_, this, ()| {
            let ns = this.vm.clock_get().map_err(mlua::Error::external)?;
            Ok(ns as f64 / 1e9)
        });

        // Integer-nanosecond accessors. Lua 5.4 integers are
        // 64-bit so no precision loss either way.
        methods.add_method("get_ns", |_, this, ()| {
            let ns = this.vm.clock_get().map_err(mlua::Error::external)?;
            Ok(ns)
        });
        methods.add_method("set_ns", |_, this, ns: i64| {
            this.vm.clock_set(ns).map_err(mlua::Error::external)?;
            Ok(())
        });

        methods.add_method("set", |_, this, t: f64| {
            // Reject NaN / infinity — `(t * 1e9) as i64` would
            // saturate or yield 0 silently, putting the guest
            // clock into nonsense. Negative is fine: DESIGN
            // explicitly allows clock:set(-N) to send the guest
            // backwards in time.
            if !t.is_finite() {
                return Err(mlua::Error::external(
                    "clock:set: value must be a finite number (got NaN/inf)",
                ));
            }
            let ns = (t * 1e9) as i64;
            this.vm.clock_set(ns).map_err(mlua::Error::external)?;
            Ok(())
        });

        methods.add_method("sleep", |_, this, duration: Value| {
            let secs = parse_duration(Some(duration), 0.0)?;
            let ns = (secs * 1e9) as u64;
            this.vm.clock_sleep(ns).map_err(mlua::Error::external)?;
            Ok(())
        });

        methods.add_method("advance", |_, this, duration: Value| {
            // Inline parse — parse_duration rejects negative
            // (its consumers feed it into Duration::from_secs_f64
            // which panics on negative). But clock:advance
            // explicitly allows negative: per DESIGN, the time
            // axis is signed (you can roll the guest backwards).
            // Reject only NaN / infinity here.
            let secs: f64 = match duration {
                Value::Nil => 0.0,
                Value::Integer(n) => n as f64,
                Value::Number(n) => n,
                Value::String(s) => {
                    let raw = s
                        .to_str()
                        .map_err(mlua::Error::external)?
                        .to_string();
                    parse_duration_str(&raw).ok_or_else(|| {
                        mlua::Error::external(format!(
                            "clock:advance: cannot parse `{raw}`"
                        ))
                    })?
                }
                other => {
                    return Err(mlua::Error::external(format!(
                        "clock:advance: expected number|string, got {}",
                        other.type_name()
                    )));
                }
            };
            if !secs.is_finite() {
                return Err(mlua::Error::external(
                    "clock:advance: must be finite (got NaN/inf)",
                ));
            }
            let ns = (secs * 1e9) as i64;
            this.vm.clock_advance(ns).map_err(mlua::Error::external)?;
            Ok(())
        });
    }
}

fn signal_name_to_num(name: &str) -> Result<i32, String> {
    Ok(match name.to_ascii_lowercase().as_str() {
        "term" | "sigterm" | "15" => libc::SIGTERM,
        "kill" | "sigkill" | "9" => libc::SIGKILL,
        "int" | "sigint" | "2" => libc::SIGINT,
        "hup" | "sighup" | "1" => libc::SIGHUP,
        "quit" | "sigquit" | "3" => libc::SIGQUIT,
        "stop" | "sigstop" => libc::SIGSTOP,
        "cont" | "sigcont" => libc::SIGCONT,
        // User-defined signals — common test fodder for graceful
        // reload / dump-stats handlers, after TERM the most-asked
        // names from real test code.
        "usr1" | "sigusr1" | "10" => libc::SIGUSR1,
        "usr2" | "sigusr2" | "12" => libc::SIGUSR2,
        // Other commonly-targeted signals.
        "alrm" | "sigalrm" | "14" => libc::SIGALRM,
        "pipe" | "sigpipe" | "13" => libc::SIGPIPE,
        "chld" | "sigchld" | "17" => libc::SIGCHLD,
        "winch" | "sigwinch" => libc::SIGWINCH,
        other => return Err(format!("unknown signal name `{other}`")),
    })
}
