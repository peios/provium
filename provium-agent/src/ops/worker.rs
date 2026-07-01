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
//! ## What is not (v1)
//!
//! `WorkerOpenFile` / `WorkerRunAsync` are rejected: a file or process
//! handle minted in the child's table can't be reached by the host's
//! parent-scoped `file:read` / `Wait` routing. Tests open files and
//! spawn processes from inside the worker via raw `worker:syscall`
//! (`openat`, `clone`/`execve`) instead, keeping everything in the
//! child's process where its credentials apply.

use std::io::Write;
use std::os::unix::io::RawFd;
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::process::{Command, Stdio};
use std::sync::Arc;

use provium_protocol::frame::{read_frame, write_frame, DEFAULT_MAX_FRAME_BYTES};
use provium_protocol::wire::{
    AgentError, AgentErrorKind, AgentMessage, HostMessage, OpResult, SpawnWorkerArgs,
    WorkerExecArgs, WorkerJoinArgs, WorkerJoinPayload, WorkerKillArgs, WorkerOpenFileArgs,
    WorkerRunAsyncArgs, WorkerSyscallArgs, WorkerSyscallAwaitArgs, WorkerSyscallBeginArgs,
};

use crate::state::{AgentState, WorkerConn};

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

/// `WorkerRunAsync` — not supported under a process-isolated worker; the
/// process handle would live in the child's table, out of reach of the
/// host's parent-scoped `Wait`/`Kill`. Spawn from inside the worker with
/// `worker:syscall(clone/execve, …)` or `worker:exec`.
pub fn worker_run_async(args: WorkerRunAsyncArgs, state: &Arc<AgentState>) -> AgentMessage {
    if !state.has_worker(args.handle) {
        return unknown_worker("worker_run_async", args.handle);
    }
    AgentMessage::AgentError(AgentError {
        kind: AgentErrorKind::BadRequest,
        message: "worker_run_async: not supported for a process-isolated worker; \
                  use worker:exec or worker:syscall to spawn within the worker process"
            .into(),
    })
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
