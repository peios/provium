//! Process-family op handlers: `RunAsync` / `Wait` / `Kill`.

use std::io::Read;
use std::os::unix::process::ExitStatusExt;
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use provium_protocol::wire::{
    AgentError, AgentErrorKind, AgentMessage, ExecOk, ExitStatus, GetPidArgs, KillArgs, OpResult,
    ProcStatusArgs, ProcStdinCloseArgs, ProcStdinWriteArgs, ProcStdinWriteOk, ProcessLiveStatus,
    RunAsyncArgs, WaitArgs,
};

use crate::state::{AgentState, ProcessSlot};

use super::os_error_from_io;

/// `RunAsync` — spawn a child + start drain threads.
pub fn run_async(args: RunAsyncArgs, state: &Arc<AgentState>) -> AgentMessage {
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

    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            return AgentMessage::RunAsyncResult(OpResult::Err(os_error_from_io(e)));
        }
    };

    let stdin_pipe = child.stdin.take();
    let stdout_pipe = child.stdout.take().expect("piped");
    let stderr_pipe = child.stderr.take().expect("piped");
    let stdout_buf = Arc::new(Mutex::new(Vec::new()));
    let stderr_buf = Arc::new(Mutex::new(Vec::new()));

    let stdout_join = {
        let buf = Arc::clone(&stdout_buf);
        thread::spawn(move || drain_into(stdout_pipe, buf))
    };
    let stderr_join = {
        let buf = Arc::clone(&stderr_buf);
        thread::spawn(move || drain_into(stderr_pipe, buf))
    };

    let slot = ProcessSlot {
        child: Some(child),
        stdout_buf,
        stderr_buf,
        stdout_join: Some(stdout_join),
        stderr_join: Some(stderr_join),
        stdin: stdin_pipe,
    };
    let handle = state.insert_process(slot);
    AgentMessage::RunAsyncResult(OpResult::Ok(handle))
}

/// `Wait` — block until the child exits (with optional timeout),
/// drain captured output, return as `ExecOk`.
pub fn wait(args: WaitArgs, state: &Arc<AgentState>) -> AgentMessage {
    let Some(mut slot) = state.take_process(args.handle) else {
        return AgentMessage::AgentError(AgentError {
            kind: AgentErrorKind::UnknownHandle,
            message: format!("wait on {}", args.handle),
        });
    };

    let mut child = match slot.child.take() {
        Some(c) => c,
        None => {
            return AgentMessage::AgentError(AgentError {
                kind: AgentErrorKind::Internal,
                message: "process slot has no child (already waited?)".into(),
            });
        }
    };

    let mut timed_out = false;
    let wait_status = match args.timeout_ms {
        Some(ms) => match wait_with_timeout(&mut child, Duration::from_millis(ms)) {
            WaitOutcome::Exited(s) => Ok(s),
            WaitOutcome::TimedOutKilled(s) => {
                timed_out = true;
                Ok(s)
            }
            WaitOutcome::Err(e) => Err(e),
        },
        None => child.wait(),
    };

    // Join drain threads to flush their buffers. They exit on
    // pipe-EOF, which happens when the process exits / is killed.
    if let Some(h) = slot.stdout_join.take() {
        let _ = h.join();
    }
    if let Some(h) = slot.stderr_join.take() {
        let _ = h.join();
    }
    let stdout = std::mem::take(&mut *slot.stdout_buf.lock().unwrap());
    let stderr = std::mem::take(&mut *slot.stderr_buf.lock().unwrap());

    let status = match wait_status {
        Ok(s) => {
            if timed_out {
                ExitStatus::TimedOut
            } else if let Some(code) = s.code() {
                ExitStatus::Exited(code)
            } else if let Some(sig) = s.signal() {
                ExitStatus::Signalled(sig)
            } else {
                ExitStatus::Exited(-1)
            }
        }
        Err(e) => {
            return AgentMessage::WaitResult(OpResult::Err(os_error_from_io(e)));
        }
    };

    AgentMessage::WaitResult(OpResult::Ok(ExecOk {
        status,
        stdout,
        stderr,
    }))
}

/// `Kill` — send `signal` to the tracked child. Caller still has
/// to call `Wait` to reap the process.
pub fn kill(args: KillArgs, state: &Arc<AgentState>) -> AgentMessage {
    let pid = state.with_process_mut(args.handle, |slot| {
        slot.child.as_ref().map(|c| c.id())
    });
    let pid = match pid {
        Some(Some(pid)) => pid,
        Some(None) => {
            return AgentMessage::AgentError(AgentError {
                kind: AgentErrorKind::Internal,
                message: format!("process {} has no child to signal", args.handle),
            });
        }
        None => {
            return AgentMessage::AgentError(AgentError {
                kind: AgentErrorKind::UnknownHandle,
                message: format!("kill on {}", args.handle),
            });
        }
    };
    // SAFETY: pid was the spawn id of an in-flight Child. ESRCH
    // (already-reaped) is acceptable and surfaces as Ok at the wire.
    let r = unsafe { libc::kill(pid as libc::pid_t, args.signal) };
    if r != 0 {
        let err = std::io::Error::last_os_error();
        if err.raw_os_error() == Some(libc::ESRCH) {
            return AgentMessage::KillResult(OpResult::Ok(()));
        }
        return AgentMessage::KillResult(OpResult::Err(os_error_from_io(err)));
    }
    AgentMessage::KillResult(OpResult::Ok(()))
}

/// `GetPid` — return the kernel PID of an in-flight async process.
/// `proc:pid()` on the host calls into this so test code sees the
/// actual process number rather than the provium handle counter.
pub fn get_pid(args: GetPidArgs, state: &Arc<AgentState>) -> AgentMessage {
    let pid = state.with_process_mut(args.handle, |slot| {
        slot.child.as_ref().map(|c| c.id())
    });
    match pid {
        Some(Some(p)) => AgentMessage::GetPidResult(OpResult::Ok(p)),
        Some(None) => AgentMessage::AgentError(AgentError {
            kind: AgentErrorKind::Internal,
            message: format!("get_pid: process {} already reaped", args.handle),
        }),
        None => AgentMessage::AgentError(AgentError {
            kind: AgentErrorKind::UnknownHandle,
            message: format!("get_pid on {}", args.handle),
        }),
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn drain_into<R: Read>(mut reader: R, sink: Arc<Mutex<Vec<u8>>>) {
    let mut buf = [0u8; 4096];
    loop {
        match reader.read(&mut buf) {
            Ok(0) => return,
            Ok(n) => sink.lock().unwrap().extend_from_slice(&buf[..n]),
            Err(_) => return,
        }
    }
}

enum WaitOutcome {
    Exited(std::process::ExitStatus),
    TimedOutKilled(std::process::ExitStatus),
    Err(std::io::Error),
}

fn wait_with_timeout(
    child: &mut std::process::Child,
    timeout: Duration,
) -> WaitOutcome {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(s)) => return WaitOutcome::Exited(s),
            Ok(None) => {
                if std::time::Instant::now() >= deadline {
                    let pid = child.id();
                    unsafe {
                        libc::kill(pid as libc::pid_t, libc::SIGKILL);
                    }
                    return match child.wait() {
                        Ok(s) => WaitOutcome::TimedOutKilled(s),
                        Err(e) => WaitOutcome::Err(e),
                    };
                }
                thread::sleep(Duration::from_millis(20));
            }
            Err(e) => return WaitOutcome::Err(e),
        }
    }
}

// ---------------------------------------------------------------------------
// stdin write / close + status
// ---------------------------------------------------------------------------

/// `ProcStdinWrite` — write to a tracked child's stdin.
pub fn proc_stdin_write(args: ProcStdinWriteArgs, state: &Arc<AgentState>) -> AgentMessage {
    let outcome = state.with_process_mut(args.handle, |slot| {
        let pipe = match slot.stdin.as_mut() {
            Some(p) => p,
            None => {
                return AgentMessage::AgentError(AgentError {
                    kind: AgentErrorKind::Internal,
                    message: format!("process {} has no live stdin", args.handle),
                });
            }
        };
        match std::io::Write::write(pipe, &args.data) {
            Ok(n) => AgentMessage::ProcStdinWriteResult(OpResult::Ok(ProcStdinWriteOk {
                written: n as u64,
            })),
            Err(e) => AgentMessage::ProcStdinWriteResult(OpResult::Err(os_error_from_io(e))),
        }
    });
    outcome.unwrap_or_else(|| {
        AgentMessage::AgentError(AgentError {
            kind: AgentErrorKind::UnknownHandle,
            message: format!("proc_stdin_write on {}", args.handle),
        })
    })
}

/// `ProcStdinClose` — drop the stdin pipe so the child sees EOF.
pub fn proc_stdin_close(args: ProcStdinCloseArgs, state: &Arc<AgentState>) -> AgentMessage {
    let outcome = state.with_process_mut(args.handle, |slot| {
        slot.stdin = None;
        AgentMessage::ProcStdinCloseResult(OpResult::Ok(()))
    });
    outcome.unwrap_or_else(|| {
        AgentMessage::AgentError(AgentError {
            kind: AgentErrorKind::UnknownHandle,
            message: format!("proc_stdin_close on {}", args.handle),
        })
    })
}

/// `ProcStatus` — non-destructive liveness query.
pub fn proc_status(args: ProcStatusArgs, state: &Arc<AgentState>) -> AgentMessage {
    let status = state.with_process_mut(args.handle, |slot| {
        let child = match slot.child.as_mut() {
            Some(c) => c,
            None => return ProcessLiveStatus::Unknown,
        };
        match child.try_wait() {
            Ok(None) => ProcessLiveStatus::Running,
            Ok(Some(_)) => ProcessLiveStatus::Exited,
            Err(_) => ProcessLiveStatus::Unknown,
        }
    });
    AgentMessage::ProcStatusResult(OpResult::Ok(status.unwrap_or(ProcessLiveStatus::Unknown)))
}
