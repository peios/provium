//! `Exec` — run a command synchronously to completion, capturing
//! stdout/stderr, applying an optional timeout.

use std::io::{Read, Write};
use std::os::unix::process::ExitStatusExt;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use provium_protocol::wire::{ExecArgs, ExecOk, ExecResult, ExitStatus};

use super::os_error_from_io;

/// Run `args.cmd` and collect its full output. Always returns an
/// [`AgentMessage`]-shaped result; never panics on guest-OS failures
/// (those convert to [`ExecResult::Err`]).
pub fn run(args: ExecArgs) -> ExecResult {
    let child = match spawn_child(&args) {
        Ok(c) => c,
        Err(e) => return ExecResult::Err(os_error_from_io(e)),
    };

    let outcome = wait_with_capture(child, &args);
    match outcome {
        Ok(ok) => ExecResult::Ok(ok),
        Err(e) => ExecResult::Err(os_error_from_io(e)),
    }
}

fn spawn_child(args: &ExecArgs) -> std::io::Result<Child> {
    let mut cmd = Command::new(&args.cmd);
    cmd.args(&args.args);
    if args.env_clear {
        cmd.env_clear();
    }
    for (k, v) in &args.env {
        cmd.env(k, v);
    }
    if let Some(cwd) = &args.cwd {
        cmd.current_dir(cwd);
    }
    cmd.stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    cmd.spawn()
}

fn wait_with_capture(mut child: Child, args: &ExecArgs) -> std::io::Result<ExecOk> {
    // Pipes — taken before any thread spawn so we can move them.
    let stdin_pipe = child.stdin.take();
    let stdout_pipe = child
        .stdout
        .take()
        .expect("stdout was configured Stdio::piped()");
    let stderr_pipe = child
        .stderr
        .take()
        .expect("stderr was configured Stdio::piped()");
    let pid = child.id();

    // Feed stdin in a background thread so a child blocking on stdout
    // (waiting for a stdin chunk we haven't sent yet) can't deadlock.
    let stdin_input = args.stdin.clone();
    let stdin_thread = thread::spawn(move || {
        if let Some(mut pipe) = stdin_pipe {
            // Best-effort write; closing the pipe is the important part
            // (so the child sees EOF on its stdin).
            let _ = pipe.write_all(&stdin_input);
            // Pipe drops here, closing stdin from the child's PoV.
        }
    });

    let stdout_thread = thread::spawn(move || drain(stdout_pipe));
    let stderr_thread = thread::spawn(move || drain(stderr_pipe));

    // Wait with optional timeout.
    let (wait_tx, wait_rx) = mpsc::channel::<std::io::Result<std::process::ExitStatus>>();
    let waiter = thread::spawn(move || {
        let _ = wait_tx.send(child.wait());
    });

    let mut timed_out = false;
    let wait_result = match args.timeout_ms {
        Some(ms) => match wait_rx.recv_timeout(Duration::from_millis(ms)) {
            Ok(r) => r,
            Err(mpsc::RecvTimeoutError::Timeout) => {
                kill(pid);
                timed_out = true;
                // The kill above guarantees waiter will produce
                // a status shortly; recv() must complete without
                // disconnect because waiter still owns the tx.
                wait_rx
                    .recv()
                    .expect("waiter must produce a status after kill")
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                return Err(std::io::Error::other(
                    "child wait disconnected before producing a status",
                ));
            }
        },
        None => match wait_rx.recv() {
            Ok(r) => r,
            Err(_) => {
                return Err(std::io::Error::other(
                    "child wait disconnected before producing a status",
                ));
            }
        },
    };

    let _ = stdin_thread.join();
    let _ = waiter.join();
    let stdout = stdout_thread.join().unwrap_or_else(|_| Vec::new());
    let stderr = stderr_thread.join().unwrap_or_else(|_| Vec::new());

    let status = wait_result?;
    let exit = if timed_out {
        ExitStatus::TimedOut
    } else if let Some(code) = status.code() {
        ExitStatus::Exited(code)
    } else if let Some(sig) = status.signal() {
        ExitStatus::Signalled(sig)
    } else {
        // Theoretically unreachable on Unix — every status is either
        // an exit code or a signal. Surface as an exit-code -1 so the
        // host has *something* to render.
        ExitStatus::Exited(-1)
    };

    Ok(ExecOk {
        status: exit,
        stdout,
        stderr,
    })
}

fn drain<R: Read>(mut r: R) -> Vec<u8> {
    let mut buf = Vec::new();
    let _ = r.read_to_end(&mut buf);
    buf
}

fn kill(pid: u32) {
    // SAFETY: passing a valid pid value to libc::kill is documented
    // behaviour; SIGKILL (9) is always defined. ESRCH from a
    // never-spawned-or-already-reaped pid is fine to ignore.
    unsafe {
        libc::kill(pid as libc::pid_t, libc::SIGKILL);
    }
}
