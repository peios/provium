//! Per-connection handler — handshake, then dispatch one op.
//!
//! Generic over [`Read`] and [`Write`] so the same code services a
//! production `vsock::VsockStream` and a unit-test paired
//! `std::os::unix::net::UnixStream`.

use std::io::{Read, Write};
use std::sync::Arc;

use provium_protocol::wire::{
    AgentInfo, AgentMessage, HelloErr, HelloOk, HostMessage,
};
use provium_protocol::PROTOCOL_VERSION;

use crate::error::AgentRuntimeError;
use crate::io::{read_host_message, write_agent_message};
use crate::ops;
use crate::state::AgentState;
use crate::AGENT_VERSION;

/// Result of handling one connection.
///
/// `Handled` is the normal return — the op completed and the caller
/// should close. `VersionRejected` is the same shape but distinguished
/// for telemetry: a host whose version doesn't match should typically
/// abort the whole run.
#[derive(Debug, PartialEq, Eq)]
pub enum ConnectionOutcome {
    /// Handshake succeeded and one op was serviced.
    Handled,
    /// Handshake failed; a [`HelloErr`] was sent and the connection
    /// should be closed.
    VersionRejected {
        /// Version the host advertised.
        host_version: u32,
    },
}

/// Service one accepted connection.
///
/// Reads the Hello, replies, then dispatches one op. Returns when the
/// op handler returns; the caller is responsible for closing the
/// stream. Stream-mode ops complete only when their source ends or
/// their writes start failing.
pub fn handle_connection<R, W>(
    reader: &mut R,
    writer: &mut W,
    state: Arc<AgentState>,
) -> Result<ConnectionOutcome, AgentRuntimeError>
where
    R: Read,
    W: Write,
{
    // -----------------------------------------------------------------
    // 1. Handshake
    // -----------------------------------------------------------------
    let first = read_host_message(reader)?;
    let hello = match first {
        HostMessage::Hello(h) => h,
        other => {
            return Err(AgentRuntimeError::Protocol(
                describe_handshake_violation(&other),
            ));
        }
    };

    if hello.protocol_version != PROTOCOL_VERSION {
        write_agent_message(
            writer,
            &AgentMessage::HelloErr(HelloErr {
                host_version: hello.protocol_version,
                agent_version: PROTOCOL_VERSION,
            }),
        )?;
        return Ok(ConnectionOutcome::VersionRejected {
            host_version: hello.protocol_version,
        });
    }

    write_agent_message(writer, &AgentMessage::HelloOk(hello_ok()))?;

    // -----------------------------------------------------------------
    // 2. Dispatch one op
    // -----------------------------------------------------------------
    let op = read_host_message(reader)?;
    dispatch(op, reader, writer, &state)?;
    Ok(ConnectionOutcome::Handled)
}

fn dispatch<R, W>(
    op: HostMessage,
    reader: &mut R,
    writer: &mut W,
    state: &Arc<AgentState>,
) -> Result<(), AgentRuntimeError>
where
    R: Read,
    W: Write,
{
    // Stream ops own their own write loop; everything else delegates
    // to dispatch_short and we encode the response here.
    let response: AgentMessage = match op {
        // The Hello variant is only valid as the first message —
        // a second Hello mid-connection is a protocol violation.
        HostMessage::Hello(_) => {
            return Err(AgentRuntimeError::Protocol(
                "received a second Hello after handshake completed",
            ));
        }

        HostMessage::TailFile(args) => return ops::stream::tail_file(args, reader, writer),
        HostMessage::ProcStream(args) => {
            return ops::stream::proc_stream(args, state, reader, writer);
        }
        HostMessage::FdStream(args) => {
            return ops::stream::fd_stream(args, state, reader, writer);
        }

        HostMessage::BatchOp(args) => {
            let mut responses = Vec::with_capacity(args.items.len());
            for item in args.items {
                responses.push(dispatch_short(item, state));
            }
            AgentMessage::BatchOpResult(
                provium_protocol::wire::ops::BatchOpResult { responses },
            )
        }

        other => dispatch_short(other, state),
    };

    write_agent_message(writer, &response)
}

/// Dispatch a single non-stream, non-handshake op against `state`.
/// Reused by both the per-connection dispatch loop and the
/// [`HostMessage::BatchOp`] inner-item handler.
fn dispatch_short(op: HostMessage, state: &Arc<AgentState>) -> AgentMessage {
    use provium_protocol::wire::{AgentError, AgentErrorKind};
    let kind_str = op.kind();
    match op {
        // These can't appear inside a batch — they need their own
        // connection. Surface a clear AgentError.
        HostMessage::Hello(_)
        | HostMessage::TailFile(_)
        | HostMessage::ProcStream(_)
        | HostMessage::FdStream(_)
        | HostMessage::BatchOp(_) => AgentMessage::AgentError(AgentError {
            kind: AgentErrorKind::BadRequest,
            message: format!("`{kind_str}` cannot appear inside a batch"),
        }),

        HostMessage::Exec(args) => AgentMessage::ExecResult(ops::exec::run(args)),
        HostMessage::OpenFile(args) => ops::file::open_file(args, state),
        HostMessage::Read(args) => ops::file::read(args, state),
        HostMessage::Write(args) => ops::file::write(args, state),
        HostMessage::Close(args) => ops::file::close(args, state),
        HostMessage::ReadFile(args) => ops::file::read_file(args),
        HostMessage::WriteFile(args) => ops::file::write_file(args),
        HostMessage::Stat(args) => ops::file::stat(args),
        HostMessage::RunAsync(args) => ops::process::run_async(args, state),
        HostMessage::Wait(args) => ops::process::wait(args, state),
        HostMessage::Kill(args) => ops::process::kill(args, state),
        HostMessage::GetPid(args) => ops::process::get_pid(args, state),
        HostMessage::GetTime(args) => ops::clock::get_time(args),
        HostMessage::SetTime(args) => ops::clock::set_time(args),
        HostMessage::SleepClock(args) => ops::clock::sleep_clock(args),
        HostMessage::AdvanceClock(args) => ops::clock::advance_clock(args),
        HostMessage::Syscall(args) => ops::syscall::syscall(args),
        HostMessage::SpawnWorker(args) => ops::worker::spawn(args, state),
        HostMessage::WorkerExec(args) => ops::worker::worker_exec(args, state),
        HostMessage::WorkerJoin(args) => ops::worker::worker_join(args, state),
        HostMessage::WorkerRunAsync(args) => ops::worker::worker_run_async(args, state),
        HostMessage::WorkerOpenFile(args) => ops::worker::worker_open_file(args, state),
        HostMessage::WorkerSyscall(args) => ops::worker::worker_syscall(args, state),
        HostMessage::WorkerKill(args) => ops::worker::worker_kill(args, state),
        HostMessage::Listdir(args) => ops::file::listdir(args),
        HostMessage::Mkdir(args) => ops::file::mkdir(args),
        HostMessage::Unlink(args) => ops::file::unlink(args),
        HostMessage::Rename(args) => ops::file::rename(args),
        HostMessage::Seek(args) => ops::file::seek(args, state),
        HostMessage::Ioctl(args) => ops::ioctl::ioctl(args, state),
        HostMessage::ProcStdinWrite(args) => ops::process::proc_stdin_write(args, state),
        HostMessage::ProcStdinClose(args) => ops::process::proc_stdin_close(args, state),
        HostMessage::ProcStatus(args) => ops::process::proc_status(args, state),
        HostMessage::BatchExec(args) => {
            let mut results = Vec::with_capacity(args.items.len());
            for item in args.items {
                results.push(ops::exec::run(item));
            }
            AgentMessage::BatchExecResult(
                provium_protocol::wire::ops::BatchExecResult { results },
            )
        }
    }
}

fn hello_ok() -> HelloOk {
    HelloOk {
        protocol_version: PROTOCOL_VERSION,
        agent: AgentInfo {
            os: target_os().into(),
            agent_version: AGENT_VERSION.into(),
            kernel: kernel_release(),
            capabilities: vec![],
        },
    }
}

#[inline]
fn target_os() -> &'static str {
    // The compile-time guest-OS identifier the agent advertises in
    // [`HelloOk.agent.os`]. Per-port builds change this; the v1
    // default agent is the Peios port. Linux-target builds report
    // `peios` (the v1 agent IS the Peios port — the build target
    // and the userspace identity are decoupled in DESIGN's
    // architecture but not yet in the build matrix). The host's
    // port-selection logic uses `Profile.guest_os` from
    // `provium.toml`, NOT this field, so a misadvertised value
    // here cannot trick the host into a different code path —
    // only humans reading logs are misled.
    if cfg!(target_os = "linux") {
        "peios"
    } else {
        "unknown"
    }
}

fn kernel_release() -> Option<String> {
    // Best-effort `uname -r`. Failures here are not fatal — the field
    // is only diagnostic.
    let output = std::process::Command::new("uname").arg("-r").output().ok()?;
    if !output.status.success() {
        return None;
    }
    let s = String::from_utf8(output.stdout).ok()?;
    let trimmed = s.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_owned())
    }
}

fn describe_handshake_violation(message: &HostMessage) -> &'static str {
    match message {
        HostMessage::Hello(_) => "handshake-state hello (unreachable)",
        HostMessage::Exec(_) => "received `exec` before handshake",
        HostMessage::OpenFile(_) => "received `open_file` before handshake",
        HostMessage::Read(_) => "received `read` before handshake",
        HostMessage::Write(_) => "received `write` before handshake",
        HostMessage::Close(_) => "received `close` before handshake",
        HostMessage::ReadFile(_) => "received `read_file` before handshake",
        HostMessage::WriteFile(_) => "received `write_file` before handshake",
        HostMessage::Stat(_) => "received `stat` before handshake",
        HostMessage::TailFile(_) => "received `tail_file` before handshake",
        HostMessage::RunAsync(_) => "received `run_async` before handshake",
        HostMessage::Wait(_) => "received `wait` before handshake",
        HostMessage::Kill(_) => "received `kill` before handshake",
        HostMessage::GetPid(_) => "received `get_pid` before handshake",
        HostMessage::GetTime(_) => "received `get_time` before handshake",
        HostMessage::SetTime(_) => "received `set_time` before handshake",
        HostMessage::SleepClock(_) => "received `sleep_clock` before handshake",
        HostMessage::AdvanceClock(_) => "received `advance_clock` before handshake",
        HostMessage::Syscall(_) => "received `syscall` before handshake",
        HostMessage::SpawnWorker(_) => "received `spawn_worker` before handshake",
        HostMessage::WorkerExec(_) => "received `worker_exec` before handshake",
        HostMessage::WorkerJoin(_) => "received `worker_join` before handshake",
        HostMessage::Listdir(_) => "received `listdir` before handshake",
        HostMessage::Mkdir(_) => "received `mkdir` before handshake",
        HostMessage::Unlink(_) => "received `unlink` before handshake",
        HostMessage::Rename(_) => "received `rename` before handshake",
        HostMessage::Seek(_) => "received `seek` before handshake",
        HostMessage::Ioctl(_) => "received `ioctl` before handshake",
        HostMessage::ProcStdinWrite(_) => "received `proc_stdin_write` before handshake",
        HostMessage::ProcStdinClose(_) => "received `proc_stdin_close` before handshake",
        HostMessage::ProcStatus(_) => "received `proc_status` before handshake",
        HostMessage::ProcStream(_) => "received `proc_stream` before handshake",
        HostMessage::FdStream(_) => "received `fd_stream` before handshake",
        HostMessage::WorkerRunAsync(_) => "received `worker_run_async` before handshake",
        HostMessage::WorkerOpenFile(_) => "received `worker_open_file` before handshake",
        HostMessage::WorkerSyscall(_) => "received `worker_syscall` before handshake",
        HostMessage::WorkerKill(_) => "received `worker_kill` before handshake",
        HostMessage::BatchExec(_) => "received `batch_exec` before handshake",
        HostMessage::BatchOp(_) => "received `batch_op` before handshake",
    }
}
