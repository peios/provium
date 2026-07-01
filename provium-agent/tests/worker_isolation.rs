//! Integration test for process-isolated workers.
//!
//! A worker is a re-exec'd copy of the agent (`--worker-fd N`) reachable
//! over a `socketpair(2)` control channel. The whole point is that it is
//! a *separate OS process*: a syscall relayed to it executes with the
//! worker's own PID (and, in a real PKM kernel, its own token/PSB). This
//! test re-execs the actual built agent binary — exactly as
//! `ops::worker::spawn` does in production, but using the
//! `CARGO_BIN_EXE_provium-agent` path Cargo hands to integration tests
//! instead of `current_exe()` — then relays `getpid`/`getppid` and
//! proves the relay landed in a distinct child process parented by us.

use std::os::unix::io::{FromRawFd, RawFd};
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::process::{Child, Command, Stdio};

use provium_protocol::frame::{read_frame, write_frame, DEFAULT_MAX_FRAME_BYTES};
use provium_protocol::handle::WorkerHandle;
use provium_protocol::wire::{
    AgentMessage, HostMessage, OpResult, SyscallArgs, WorkerSyscallAwaitArgs, WorkerSyscallBeginArgs,
};

// x86_64 syscall numbers — this test is gated to that arch below.
const NR_GETPID: i64 = 39;
const NR_GETPPID: i64 = 110;

/// Spawn the real agent binary as a worker over a fresh socketpair,
/// mirroring `ops::worker::spawn`. Returns the child handle and the
/// parent end of the control socket.
fn spawn_worker() -> (Child, UnixStream) {
    let exe = env!("CARGO_BIN_EXE_provium-agent");

    let mut fds = [0 as libc::c_int; 2];
    let rc = unsafe {
        libc::socketpair(
            libc::AF_UNIX,
            libc::SOCK_STREAM | libc::SOCK_CLOEXEC,
            0,
            fds.as_mut_ptr(),
        )
    };
    assert_eq!(rc, 0, "socketpair: {}", std::io::Error::last_os_error());
    let parent_fd: RawFd = fds[0];
    let child_fd: RawFd = fds[1];

    let mut cmd = Command::new(exe);
    cmd.arg("--worker-fd").arg(child_fd.to_string());
    cmd.stdin(Stdio::null()).stdout(Stdio::null());
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
    let child = cmd.spawn().expect("spawn worker agent");
    unsafe {
        libc::close(child_fd);
    }
    let control = unsafe { UnixStream::from_raw_fd(parent_fd) };
    (child, control)
}

/// Relay one raw syscall to the worker and return its `(ret, errno)`.
fn worker_syscall(control: &mut UnixStream, nr: i64) -> (i64, i32) {
    let op = HostMessage::Syscall(SyscallArgs {
        nr,
        ..Default::default()
    });
    write_frame(control, &op, DEFAULT_MAX_FRAME_BYTES).expect("write syscall frame");
    let resp: AgentMessage =
        read_frame(control, DEFAULT_MAX_FRAME_BYTES).expect("read syscall reply");
    match resp {
        AgentMessage::SyscallResult(r) => (r.ret, r.errno),
        other => panic!("expected SyscallResult, got {}", other.kind()),
    }
}

#[test]
#[cfg(target_arch = "x86_64")]
fn worker_syscall_runs_in_a_distinct_child_process() {
    let (mut child, mut control) = spawn_worker();

    // getpid in the worker returns the worker's PID — a real, different
    // process from this test.
    let (worker_pid, _) = worker_syscall(&mut control, NR_GETPID);
    let my_pid = std::process::id() as i64;
    assert!(worker_pid > 0, "worker getpid returned {worker_pid}");
    assert_ne!(
        worker_pid, my_pid,
        "worker must be a separate process, not run inline in the parent"
    );
    assert_eq!(
        worker_pid,
        child.id() as i64,
        "the relayed getpid must match the spawned worker child's PID"
    );

    // getppid in the worker is this test process — it really is our
    // child, confirming the parent/child relationship (the foundation
    // for cross-process checks like signals and ptrace).
    let (worker_ppid, _) = worker_syscall(&mut control, NR_GETPPID);
    assert_eq!(
        worker_ppid, my_pid,
        "the worker's parent must be this test process"
    );

    // A second syscall on the same channel still works — the serve loop
    // services many ops over the worker's lifetime, not just one.
    let (worker_pid_again, _) = worker_syscall(&mut control, NR_GETPID);
    assert_eq!(worker_pid_again, worker_pid, "worker PID is stable");

    // Drop the control socket → the worker sees EOF and its serve loop
    // exits cleanly; reap it.
    drop(control);
    let status = child.wait().expect("reap worker");
    assert!(
        status.success(),
        "worker should exit 0 on EOF, got {status:?}"
    );
}

/// Begin an async syscall on the worker; returns the async handle.
fn worker_begin(control: &mut UnixStream, nr: i64) -> u64 {
    let op = HostMessage::WorkerSyscallBegin(WorkerSyscallBeginArgs {
        handle: WorkerHandle::new(1),
        args: SyscallArgs {
            nr,
            ..Default::default()
        },
    });
    write_frame(control, &op, DEFAULT_MAX_FRAME_BYTES).expect("write begin");
    match read_frame(control, DEFAULT_MAX_FRAME_BYTES).expect("read begin reply") {
        AgentMessage::WorkerSyscallBeginResult(OpResult::Ok(id)) => id,
        other => panic!("expected WorkerSyscallBeginResult Ok, got {}", other.kind()),
    }
}

/// Collect a previously-begun async syscall's `(ret, errno)`.
fn worker_await(control: &mut UnixStream, id: u64) -> (i64, i32) {
    let op = HostMessage::WorkerSyscallAwait(WorkerSyscallAwaitArgs {
        handle: WorkerHandle::new(1),
        async_id: id,
    });
    write_frame(control, &op, DEFAULT_MAX_FRAME_BYTES).expect("write await");
    match read_frame(control, DEFAULT_MAX_FRAME_BYTES).expect("read await reply") {
        AgentMessage::WorkerSyscallAwaitResult(r) => (r.ret, r.errno),
        other => panic!("expected WorkerSyscallAwaitResult, got {}", other.kind()),
    }
}

#[test]
#[cfg(target_arch = "x86_64")]
fn worker_async_syscall_begins_and_awaits_in_the_worker_process() {
    let (mut child, mut control) = spawn_worker();

    // Begin getpid async → get a handle; the worker runs it on a thread.
    let h1 = worker_begin(&mut control, NR_GETPID);
    // Begin a second one before awaiting the first — concurrent handles.
    let h2 = worker_begin(&mut control, NR_GETPPID);
    assert_ne!(h1, h2, "each async syscall gets a distinct handle");

    let (worker_pid, _) = worker_await(&mut control, h1);
    let (worker_ppid, _) = worker_await(&mut control, h2);
    assert_eq!(
        worker_pid,
        child.id() as i64,
        "async getpid ran in the worker process"
    );
    assert_eq!(
        worker_ppid,
        std::process::id() as i64,
        "async getppid is this test process (the worker's parent)"
    );

    // A sync syscall still works after async ones on the same channel.
    let (again, _) = worker_syscall(&mut control, NR_GETPID);
    assert_eq!(again, worker_pid, "sync + async share the worker process");

    // Awaiting an unknown handle is an error, not a hang.
    let bogus = HostMessage::WorkerSyscallAwait(WorkerSyscallAwaitArgs {
        handle: WorkerHandle::new(1),
        async_id: 9999,
    });
    write_frame(&mut control, &bogus, DEFAULT_MAX_FRAME_BYTES).expect("write bogus await");
    match read_frame(&mut control, DEFAULT_MAX_FRAME_BYTES).expect("read bogus reply") {
        AgentMessage::AgentError(_) => {}
        other => panic!("expected AgentError for unknown handle, got {}", other.kind()),
    }

    drop(control);
    let status = child.wait().expect("reap worker");
    assert!(status.success(), "worker exits 0 on EOF, got {status:?}");
}
