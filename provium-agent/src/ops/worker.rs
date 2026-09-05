//! Worker (sub-agent) op handlers.
//!
//! A worker is a **real, separate OS process**: a re-exec'd copy of the
//! agent launched as `provium-agent --worker-fd N`, where `N` is the
//! child end of a `socketpair(2)`. The parent agent relays ops down the
//! socket; the child executes them in *its own* process context via
//! [`crate::connection::serve_worker_child`] and replies. The point is
//! credential separation — the worker has its own kernel token, PSB, and
//! privilege set, so a test can stand up two distinct security
//! principals in one VM (a caller and the target of a process-SD check,
//! an unprivileged caller against a privileged op, two SCM_RIGHTS peers).
//!
//! ## What is relayed
//!
//! [`WorkerSyscall`] and [`WorkerExec`] run in the child — these are the
//! credential-bearing ops. A worker establishes its own identity simply
//! by issuing ordinary `worker:syscall` calls (KACS token-install /
//! adjust-privs / set-psb), which now stick to the child alone.
//!
//! [`WorkerRunAsync`] also runs in the child: the grandchild is forked
//! and exec'd by the worker, so it starts life with the worker's
//! credentials — which is the whole point when the worker is a
//! principal the test minted (exec under a token, NEW_PROCESS_MIN, a
//! descriptor surviving exec). The process lands in the *worker's*
//! table; the parent files it under a handle of its own in
//! [`AgentState::insert_worker_process`], and the process-family ops
//! (`Wait`, `Kill`, `GetPid`, `ProcStatus`, stdin) look there first and
//! relay down the worker's socket when they find it. The host sees an
//! ordinary Process. `ProcStream` is the one op not relayed — a stream
//! owns the connection for its lifetime and the worker channel is a
//! strict request/reply pipe.
//!
//! ## What is not (v1)
//!
//! `WorkerOpenFile` is rejected: a file handle minted in the child's
//! table can't be reached by the host's parent-scoped `file:read` /
//! `file:write` routing. Tests open files from inside the worker via
//! raw `worker:syscall(openat, …)` instead, keeping the descriptor in
//! the child's process where its credentials apply.

use std::io::Write;
use std::os::unix::io::RawFd;
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use provium_protocol::frame::{read_frame, write_frame, DEFAULT_MAX_FRAME_BYTES};
use provium_protocol::handle::{ProcessHandle, WorkerHandle};
use provium_protocol::wire::{
    AgentError, AgentErrorKind, AgentMessage, HostMessage, OpResult, ProcStatusArgs,
    ProcessLiveStatus, SpawnWorkerArgs, WaitArgs, WorkerExecArgs, WorkerJoinArgs,
    WorkerJoinPayload, WorkerKillArgs, WorkerOpenFileArgs, WorkerRunAsyncArgs, WorkerSyscallArgs,
    WorkerSyscallAwaitArgs, WorkerSyscallBeginArgs,
};

use crate::state::{AgentState, WorkerConn};

/// How often a relayed `Wait` asks the worker whether the grandchild
/// is still running. The worker's serve loop is serial, so the wait
/// is polled rather than relayed as one blocking `Wait`: between
/// polls the channel is free for the other ops a test may still be
/// issuing to that worker.
const WORKER_WAIT_POLL: Duration = Duration::from_millis(20);

/// `SpawnWorker` — fork+exec a sub-agent and register its control
/// channel. Returns the new worker handle.
///
/// Process model: a `socketpair(2)` is created with `SOCK_CLOEXEC` on
/// *both* ends so no concurrent `run_async`/`exec` on another handler
/// thread can leak either fd. We then re-exec this very binary with
/// `--worker-fd <child_fd>`; a `pre_exec` hook clears `FD_CLOEXEC` on
/// the child end **in the forked child only**, so it survives the exec
/// without ever being inheritable from the parent's other spawns. The
/// parent keeps the (still-`CLOEXEC`) peer end as the relay socket.
pub fn spawn(_args: SpawnWorkerArgs, state: &Arc<AgentState>) -> AgentMessage {
    let exe = match std::env::current_exe() {
        Ok(p) => p,
        Err(e) => return spawn_err(format!("worker spawn: current_exe: {e}")),
    };

    // socketpair, both ends CLOEXEC (atomic — no leak window).
    let mut fds = [0 as libc::c_int; 2];
    // SAFETY: fds is a valid 2-element array; we check the return code.
    let rc = unsafe {
        libc::socketpair(
            libc::AF_UNIX,
            libc::SOCK_STREAM | libc::SOCK_CLOEXEC,
            0,
            fds.as_mut_ptr(),
        )
    };
    if rc != 0 {
        return spawn_err(format!(
            "worker spawn: socketpair: {}",
            std::io::Error::last_os_error()
        ));
    }
    let parent_fd: RawFd = fds[0];
    let child_fd: RawFd = fds[1];

    let mut cmd = Command::new(&exe);
    cmd.arg("--worker-fd").arg(child_fd.to_string());
    // The sub-agent has no use for stdin/stdout; keep stderr so a panic
    // or log line is visible in the VM console.
    cmd.stdin(Stdio::null()).stdout(Stdio::null());
    // SAFETY: the pre_exec closure runs in the forked child between
    // fork and exec. It performs only an async-signal-safe `fcntl`,
    // clearing FD_CLOEXEC on the inherited child end so it survives the
    // imminent exec. Because this runs post-fork, the parent's copy of
    // child_fd stays CLOEXEC and cannot leak into any other spawn.
    unsafe {
        cmd.pre_exec(move || {
            let flags = libc::fcntl(child_fd, libc::F_GETFD);
            if flags < 0 {
                return Err(std::io::Error::last_os_error());
            }
            if libc::fcntl(child_fd, libc::F_SETFD, flags & !libc::FD_CLOEXEC) < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }

    let child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            // SAFETY: both fds are ours and still open; close them so a
            // failed spawn doesn't leak the pair.
            unsafe {
                libc::close(parent_fd);
                libc::close(child_fd);
            }
            return spawn_err(format!("worker spawn: exec {}: {e}", exe.display()));
        }
    };

    // Parent no longer needs the child end.
    // SAFETY: child_fd is the parent's own copy of an open fd; the
    // forked child holds an independent copy.
    unsafe {
        libc::close(child_fd);
    }

    // SAFETY: parent_fd is an open socket fd owned solely by us now.
    let control = unsafe { <UnixStream as std::os::unix::io::FromRawFd>::from_raw_fd(parent_fd) };
    let handle = state.insert_worker_conn(WorkerConn { child, control });
    AgentMessage::SpawnWorkerResult(OpResult::Ok(handle))
}

/// `WorkerSyscall` — run a raw Layer-0 syscall in the worker's process.
/// The syscall carries the worker's own token/PSB/privileges.
pub fn worker_syscall(args: WorkerSyscallArgs, state: &Arc<AgentState>) -> AgentMessage {
    let Some(conn) = state.worker_conn(args.handle) else {
        return unknown_worker("worker_syscall", args.handle);
    };
    let mut conn = conn.lock().unwrap();
    match relay(&mut conn, HostMessage::Syscall(args.args)) {
        // Re-wrap as the worker variant — the host's worker client
        // expects `WorkerSyscallResult` and rejects the bare variant.
        Ok(AgentMessage::SyscallResult(payload)) => AgentMessage::WorkerSyscallResult(payload),
        Ok(other) => relay_shape_err("worker_syscall", other),
        Err(msg) => relay_io_err("worker_syscall", msg),
    }
}

/// `WorkerSyscallBegin` — relay an async-syscall start to the worker.
/// The worker runs the syscall on a background thread and returns an
/// async handle immediately, so this relay returns fast (the worker's
/// syscall keeps running) and the host stays free to serve sources.
pub fn worker_syscall_begin(args: WorkerSyscallBeginArgs, state: &Arc<AgentState>) -> AgentMessage {
    let Some(conn) = state.worker_conn(args.handle) else {
        return unknown_worker("worker_syscall_begin", args.handle);
    };
    let mut conn = conn.lock().unwrap();
    match relay(&mut conn, HostMessage::WorkerSyscallBegin(args)) {
        Ok(AgentMessage::WorkerSyscallBeginResult(payload)) => {
            AgentMessage::WorkerSyscallBeginResult(payload)
        }
        Ok(other) => relay_shape_err("worker_syscall_begin", other),
        Err(msg) => relay_io_err("worker_syscall_begin", msg),
    }
}

/// `WorkerSyscallAwait` — relay an async-syscall collect to the worker;
/// blocks until the worker's background syscall thread completes.
pub fn worker_syscall_await(args: WorkerSyscallAwaitArgs, state: &Arc<AgentState>) -> AgentMessage {
    let Some(conn) = state.worker_conn(args.handle) else {
        return unknown_worker("worker_syscall_await", args.handle);
    };
    let mut conn = conn.lock().unwrap();
    match relay(&mut conn, HostMessage::WorkerSyscallAwait(args)) {
        Ok(AgentMessage::WorkerSyscallAwaitResult(payload)) => {
            AgentMessage::WorkerSyscallAwaitResult(payload)
        }
        Ok(other) => relay_shape_err("worker_syscall_await", other),
        Err(msg) => relay_io_err("worker_syscall_await", msg),
    }
}

/// `WorkerExec` — run a binary in the worker's process context (the
/// grandchild inherits the worker's credentials). Returns full output.
pub fn worker_exec(args: WorkerExecArgs, state: &Arc<AgentState>) -> AgentMessage {
    let Some(conn) = state.worker_conn(args.handle) else {
        return unknown_worker("worker_exec", args.handle);
    };
    let mut conn = conn.lock().unwrap();
    match relay(&mut conn, HostMessage::Exec(args.exec)) {
        Ok(AgentMessage::ExecResult(payload)) => AgentMessage::WorkerExecResult(payload),
        Ok(other) => relay_shape_err("worker_exec", other),
        Err(msg) => relay_io_err("worker_exec", msg),
    }
}

/// `WorkerKill` — send `signal` to the worker process itself. Returns
/// the number of processes signalled (0 or 1).
pub fn worker_kill(args: WorkerKillArgs, state: &Arc<AgentState>) -> AgentMessage {
    let Some(conn) = state.worker_conn(args.handle) else {
        return unknown_worker("worker_kill", args.handle);
    };
    let conn = conn.lock().unwrap();
    let pid = conn.child.id() as libc::pid_t;
    // SAFETY: pid is the worker child's, valid until we reap it on
    // worker_join; signal is a POSIX integer.
    let rc = unsafe { libc::kill(pid, args.signal) };
    let count = if rc == 0 { 1 } else { 0 };
    AgentMessage::WorkerKillResult(OpResult::Ok(count))
}

/// `WorkerJoin` — drop the control socket (giving the worker EOF so its
/// serve loop exits), reap the child, and return its exit status.
pub fn worker_join(args: WorkerJoinArgs, state: &Arc<AgentState>) -> AgentMessage {
    let Some(conn) = state.remove_worker_conn(args.handle) else {
        return unknown_worker("worker_join", args.handle);
    };
    // Its process table goes with it; anything still running there is
    // an orphan now, not something a handle of ours can reach.
    state.remove_worker_processes_of(args.handle);
    // We removed the only registry reference; unwrap the Arc/Mutex to
    // own the connection so we can drop the socket and wait.
    let conn = match Arc::try_unwrap(conn) {
        Ok(m) => m.into_inner().unwrap(),
        // Another op holds a clone mid-flight — fall back to operating
        // through the lock (it will release once that op returns).
        Err(arc) => {
            let mut guard = arc.lock().unwrap();
            let status = reap_worker(&mut guard);
            return AgentMessage::WorkerJoinResult(OpResult::Ok(WorkerJoinPayload {
                exit_status: status,
            }));
        }
    };
    let mut conn = conn;
    let status = reap_worker(&mut conn);
    AgentMessage::WorkerJoinResult(OpResult::Ok(WorkerJoinPayload {
        exit_status: status,
    }))
}

/// `WorkerOpenFile` — not supported under a process-isolated worker;
/// the resulting handle would live in the child's file table, out of
/// reach of the host's parent-scoped `file:read`/`:write`/`:close`.
/// Open files from inside the worker with `worker:syscall(openat, …)`.
pub fn worker_open_file(args: WorkerOpenFileArgs, state: &Arc<AgentState>) -> AgentMessage {
    if !state.has_worker(args.handle) {
        return unknown_worker("worker_open_file", args.handle);
    }
    AgentMessage::AgentError(AgentError {
        kind: AgentErrorKind::BadRequest,
        message: "worker_open_file: not supported for a process-isolated worker; \
                  use worker:syscall(openat, …) so the fd lives in the worker process"
            .into(),
    })
}

/// `WorkerRunAsync` — fork+exec a child *in the worker's process*, so
/// it inherits the worker's credentials. The worker registers the
/// child in its own table and hands back its handle; we file that under
/// a handle from our counter and return ours. Every later process op on
/// it finds the mapping and relays — see [`relay_process_op`].
pub fn worker_run_async(args: WorkerRunAsyncArgs, state: &Arc<AgentState>) -> AgentMessage {
    let Some(conn) = state.worker_conn(args.handle) else {
        return unknown_worker("worker_run_async", args.handle);
    };
    let mut conn = conn.lock().unwrap();
    match relay(&mut conn, HostMessage::RunAsync(args.args)) {
        Ok(AgentMessage::RunAsyncResult(OpResult::Ok(inner))) => {
            let outer = state.insert_worker_process(args.handle, inner);
            AgentMessage::WorkerRunAsyncResult(OpResult::Ok(outer))
        }
        Ok(AgentMessage::RunAsyncResult(OpResult::Err(e))) => {
            AgentMessage::WorkerRunAsyncResult(OpResult::Err(e))
        }
        Ok(other) => relay_shape_err("worker_run_async", other),
        Err(msg) => relay_io_err("worker_run_async", msg),
    }
}

/// Relay a process-family op (`Kill`, `GetPid`, `ProcStatus`,
/// `ProcStdinWrite`, `ProcStdinClose`) for a process that
/// [`worker_run_async`] spawned. `op` must already name the process by
/// the *worker's* handle for it; the reply is the worker's own, which
/// is the same variant the host expects, so it passes straight through.
pub fn relay_process_op(
    op_name: &str,
    worker: WorkerHandle,
    op: HostMessage,
    state: &Arc<AgentState>,
) -> AgentMessage {
    let Some(conn) = state.worker_conn(worker) else {
        return worker_gone(op_name, worker);
    };
    let mut conn = conn.lock().unwrap();
    match relay(&mut conn, op) {
        Ok(reply) => reply,
        Err(msg) => relay_io_err(op_name, msg),
    }
}

/// `Wait` for a worker-spawned process. Polls the worker's `ProcStatus`
/// until the child has exited or `timeout_ms` has elapsed, then relays
/// one `Wait` to collect the status and captured output. On our
/// timeout that `Wait` carries a zero cap, so the worker kills the
/// child and reports `TimedOut` exactly as the parent table would
/// have. The mapping is dropped once collected: a `Wait` consumes the
/// process on both sides.
pub fn worker_wait(
    outer: ProcessHandle,
    worker: WorkerHandle,
    inner: ProcessHandle,
    timeout_ms: Option<u64>,
    state: &Arc<AgentState>,
) -> AgentMessage {
    let Some(conn) = state.worker_conn(worker) else {
        state.remove_worker_process(outer);
        return worker_gone("wait", worker);
    };
    let deadline = timeout_ms.map(|ms| Instant::now() + Duration::from_millis(ms));
    let mut timed_out = false;
    loop {
        let status = {
            let mut conn = conn.lock().unwrap();
            relay(&mut conn, HostMessage::ProcStatus(ProcStatusArgs { handle: inner }))
        };
        match status {
            Ok(AgentMessage::ProcStatusResult(OpResult::Ok(ProcessLiveStatus::Running))) => {}
            // Exited, or the worker no longer knows it: collect.
            Ok(AgentMessage::ProcStatusResult(_)) => break,
            Ok(other) => return relay_shape_err("wait", other),
            Err(msg) => return relay_io_err("wait", msg),
        }
        if deadline.is_some_and(|d| Instant::now() >= d) {
            timed_out = true;
            break;
        }
        std::thread::sleep(WORKER_WAIT_POLL);
    }
    let collect = HostMessage::Wait(WaitArgs {
        handle: inner,
        timeout_ms: if timed_out { Some(0) } else { None },
    });
    let reply = {
        let mut conn = conn.lock().unwrap();
        relay(&mut conn, collect)
    };
    state.remove_worker_process(outer);
    match reply {
        Ok(reply) => reply,
        Err(msg) => relay_io_err("wait", msg),
    }
}

// --- helpers ---------------------------------------------------------

/// Relay one op down a worker's control socket and read its reply.
fn relay(conn: &mut WorkerConn, op: HostMessage) -> Result<AgentMessage, String> {
    write_frame(&mut conn.control, &op, DEFAULT_MAX_FRAME_BYTES)
        .map_err(|e| format!("write: {e}"))?;
    conn.control.flush().map_err(|e| format!("flush: {e}"))?;
    let resp: AgentMessage = read_frame(&mut conn.control, DEFAULT_MAX_FRAME_BYTES)
        .map_err(|e| format!("read: {e}"))?;
    Ok(resp)
}

/// Drop the control socket (EOF → worker serve loop exits) and reap the
/// child, returning its exit status (`128 + signo` if signalled).
fn reap_worker(conn: &mut WorkerConn) -> i32 {
    // Shut the socket down so the worker's blocking read returns EOF and
    // its serve loop falls out, rather than waiting on `wait()` while the
    // child blocks on a read that never ends.
    let _ = conn.control.shutdown(std::net::Shutdown::Both);
    match conn.child.wait() {
        Ok(status) => status.code().unwrap_or_else(|| {
            use std::os::unix::process::ExitStatusExt;
            128 + status.signal().unwrap_or(0)
        }),
        Err(_) => 1,
    }
}

/// The process was spawned by a worker that has since been joined.
fn worker_gone(op: &str, worker: WorkerHandle) -> AgentMessage {
    AgentMessage::AgentError(AgentError {
        kind: AgentErrorKind::UnknownHandle,
        message: format!("{op}: the process was spawned by {worker}, which has been joined"),
    })
}

fn unknown_worker(op: &str, handle: provium_protocol::handle::WorkerHandle) -> AgentMessage {
    AgentMessage::AgentError(AgentError {
        kind: AgentErrorKind::UnknownHandle,
        message: format!("{op} on unknown worker {handle}"),
    })
}

fn spawn_err(message: String) -> AgentMessage {
    AgentMessage::SpawnWorkerResult(OpResult::Err(provium_protocol::OsError {
        errno: 0,
        message,
    }))
}

fn relay_io_err(op: &str, detail: String) -> AgentMessage {
    AgentMessage::AgentError(AgentError {
        kind: AgentErrorKind::BadRequest,
        message: format!("{op}: worker control channel error: {detail}"),
    })
}

fn relay_shape_err(op: &str, got: AgentMessage) -> AgentMessage {
    AgentMessage::AgentError(AgentError {
        kind: AgentErrorKind::BadRequest,
        message: format!("{op}: unexpected reply shape from worker: {}", got.kind()),
    })
}
