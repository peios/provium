//! The host ↔ agent wire protocol over vsock.
//!
//! Two top-level message types serve as serde-tagged envelopes:
//!
//! * [`HostMessage`] — anything the host sends to the agent: handshake,
//!   op requests, stream-mode open requests.
//! * [`AgentMessage`] — anything the agent sends to the host: handshake
//!   replies, op results, stream frames, agent-level errors.
//!
//! Each variant's payload is a strongly-typed struct from a sibling
//! module — adding a new op is a localised change in [`ops`] plus one
//! variant on each enum. Both messages are encoded as msgpack frames
//! ([`crate::frame`]); the wire tag is a `kind` field so a peer using
//! a future protocol version can at least decode the envelope and
//! report the unfamiliar variant.
//!
//! # Connection lifecycle
//!
//! 1. Host opens a vsock connection.
//! 2. Host sends [`HostMessage::Hello`].
//! 3. Agent replies with [`AgentMessage::HelloOk`] or
//!    [`AgentMessage::HelloErr`]. On `HelloErr` the agent closes the
//!    socket and the host raises [`crate::ProtocolError::VersionMismatch`].
//! 4. Host sends one op request ([`HostMessage::Exec`], etc.).
//! 5. Either:
//!    - **Short op** — agent sends one matching result variant
//!      (`AgentMessage::ExecResult`, …); both sides close the connection.
//!    - **Stream op** — agent sends an open-acknowledge result; if
//!      `Ok`, frames flow until the source ends or the host closes the
//!      connection. See [`stream`] for the frame and end types.

use serde::{Deserialize, Serialize};

pub mod error;
pub mod hello;
pub mod ops;
pub mod stream;

pub use error::{AgentError, AgentErrorKind};
pub use hello::{AgentInfo, Hello, HelloErr, HelloOk};
pub use stream::{StreamEnd, StreamFrame};

// Re-export op types at the wire level for ergonomics.
pub use ops::{
    AdvanceClockArgs, AdvanceClockResult, BatchExecArgs, BatchExecResult, BatchOpArgs,
    BatchOpResult, ClockTime, CloseArgs,
    CloseResult, DirEntry, EntryType, ExecArgs, ExecOk, ExecResult, ExitStatus, FdStreamArgs,
    FdStreamResult, FileMetadata, GetTimeArgs, GetTimeResult, IoctlArgs, IoctlOk, IoctlResult,
    GetPidArgs, GetPidResult, KillArgs, KillResult, ListdirArgs, ListdirResult, MkdirArgs,
    MkdirResult, NestedPtr, OpResult,
    OpenFileArgs, OpenFileResult, OpenMode, ProcStatusArgs, ProcStatusResult, ProcStdinCloseArgs,
    ProcStdinCloseResult, ProcStdinWriteArgs, ProcStdinWriteOk, ProcStdinWriteResult,
    ProcStreamArgs, ProcStreamChannel, ProcStreamResult, ProcessLiveStatus, ReadArgs, ReadFileArgs,
    ReadFileResult, ReadMemArgs, ReadMemOk, ReadMemResult, ReadOk, ReadResult, RenameArgs,
    RenameResult, RunAsyncArgs, RunAsyncResult,
    SeekArgs, SeekResult, SeekWhence, SetTimeArgs, SetTimeResult, SleepClockArgs, SleepClockResult,
    SpawnWorkerArgs, SpawnWorkerResult, StatArgs, StatResult, SyscallArgs, SyscallResult,
    TailFileArgs, TailFileResult, TailStart, UnlinkArgs, UnlinkResult, WaitArgs, WaitResult,
    WorkerExecArgs, WorkerExecResult, WorkerJoinArgs, WorkerJoinPayload, WorkerJoinResult,
    WorkerKillArgs,
    WorkerKillResult, WorkerOpenFileArgs, WorkerOpenFileResult, WorkerRunAsyncArgs,
    WorkerRunAsyncResult, WorkerSyscallArgs, WorkerSyscallAwaitArgs, WorkerSyscallAwaitResult,
    WorkerSyscallBeginArgs, WorkerSyscallBeginResult, WorkerSyscallResult, WriteArgs, WriteFileArgs,
    WriteFileMode, WriteFileResult, WriteOk, WriteResult,
};

/// Anything the host sends to the agent.
///
/// Variants are namespaced under the `kind` tag on the wire — a buggy
/// or out-of-version peer can at least identify the unfamiliar variant
/// rather than producing an opaque decode error.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "data", rename_all = "snake_case")]
pub enum HostMessage {
    /// Connection handshake (always the first message).
    Hello(Hello),

    /// Run a command synchronously.
    Exec(ExecArgs),

    /// Open a file for subsequent [`HostMessage::Read`] / [`HostMessage::Write`].
    OpenFile(OpenFileArgs),

    /// Read up to N bytes from an open file.
    Read(ReadArgs),

    /// Write bytes to an open file.
    Write(WriteArgs),

    /// Close an open file.
    Close(CloseArgs),

    /// Read a file's full contents in one round trip.
    ReadFile(ReadFileArgs),

    /// Replace / append / create a file's contents in one round trip.
    WriteFile(WriteFileArgs),

    /// Stat a path.
    Stat(StatArgs),

    /// Open a streaming tail of a file. Agent replies with
    /// [`AgentMessage::TailFileResult`] then pushes
    /// [`AgentMessage::StreamFrame`]s until the host closes the
    /// connection.
    TailFile(TailFileArgs),

    /// Spawn a process asynchronously, returning a handle.
    RunAsync(RunAsyncArgs),

    /// Wait for an asynchronous process to exit.
    Wait(WaitArgs),

    /// Send a signal to a tracked process.
    Kill(KillArgs),

    /// Fetch the kernel-level PID of an in-flight async process —
    /// used by `proc:pid()` to return the guest's real pid rather
    /// than the provium handle counter.
    GetPid(crate::wire::ops::GetPidArgs),

    /// Read the agent's wall clock.
    GetTime(crate::wire::ops::GetTimeArgs),
    /// Set the agent's wall clock to a specific time.
    SetTime(crate::wire::ops::SetTimeArgs),
    /// Pause the agent for a duration.
    SleepClock(crate::wire::ops::SleepClockArgs),
    /// Bump the agent's wall clock by a relative amount.
    AdvanceClock(crate::wire::ops::AdvanceClockArgs),

    /// Layer-0 raw syscall.
    Syscall(crate::wire::ops::SyscallArgs),

    /// Read `len` bytes from the agent's own address space at `addr` —
    /// lets the host observe memory the agent mapped but never passed
    /// as a syscall buffer (e.g. an `mmap`'d KMES ring).
    ReadMem(crate::wire::ops::ReadMemArgs),

    /// Allocate a sub-agent worker.
    SpawnWorker(crate::wire::ops::SpawnWorkerArgs),
    /// Run an exec against a worker.
    WorkerExec(crate::wire::ops::WorkerExecArgs),
    /// Reap a worker.
    WorkerJoin(crate::wire::ops::WorkerJoinArgs),

    /// List a directory.
    Listdir(crate::wire::ops::ListdirArgs),
    /// Create a directory.
    Mkdir(crate::wire::ops::MkdirArgs),
    /// Remove a file or empty directory.
    Unlink(crate::wire::ops::UnlinkArgs),
    /// Rename / move a path.
    Rename(crate::wire::ops::RenameArgs),
    /// Seek an open file.
    Seek(crate::wire::ops::SeekArgs),

    /// Layer-0 `ioctl(2)`.
    Ioctl(crate::wire::ops::IoctlArgs),

    /// Write to the stdin of an async process.
    ProcStdinWrite(crate::wire::ops::ProcStdinWriteArgs),
    /// Close the stdin of an async process.
    ProcStdinClose(crate::wire::ops::ProcStdinCloseArgs),
    /// Query the live status of an async process.
    ProcStatus(crate::wire::ops::ProcStatusArgs),

    /// Subscribe to an async process's stdout or stderr; agent
    /// replies with [`AgentMessage::ProcStreamResult`] then pushes
    /// [`AgentMessage::StreamFrame`]s until the process exits and
    /// the buffer drains, or the host disconnects.
    ProcStream(crate::wire::ops::ProcStreamArgs),

    /// Stream all bytes from an already-open file handle until EOF
    /// or the host disconnects. Frames flow as for tail/proc
    /// streams.
    FdStream(crate::wire::ops::FdStreamArgs),

    /// Per-worker async-spawn — process handle lives in the worker's
    /// `AgentState`, not the parent's.
    WorkerRunAsync(crate::wire::ops::WorkerRunAsyncArgs),
    /// Per-worker open-file — file handle lives in the worker's
    /// `AgentState`.
    WorkerOpenFile(crate::wire::ops::WorkerOpenFileArgs),
    /// Per-worker raw syscall.
    WorkerSyscall(crate::wire::ops::WorkerSyscallArgs),
    /// Per-worker raw syscall, started async (non-blocking): the worker
    /// runs it on a background thread and returns an async handle.
    WorkerSyscallBegin(crate::wire::ops::WorkerSyscallBeginArgs),
    /// Collect a [`HostMessage::WorkerSyscallBegin`] result by its handle.
    WorkerSyscallAwait(crate::wire::ops::WorkerSyscallAwaitArgs),
    /// Broadcast a signal to every process in a worker.
    WorkerKill(crate::wire::ops::WorkerKillArgs),

    /// Batch of `Exec` ops — agent runs each in order and replies with
    /// matching [`AgentMessage::BatchExecResult`]. Per `DESIGN.md`
    /// § Performance / Op batching. Legacy single-kind form.
    BatchExec(crate::wire::ops::BatchExecArgs),

    /// Generic op batch — agent dispatches each inner message and
    /// returns [`AgentMessage::BatchOpResult`] paired by index.
    BatchOp(crate::wire::ops::BatchOpArgs),
}

/// Anything the agent sends to the host.
///
/// The discriminator name closely matches its [`HostMessage`] partner
/// for short ops (`Exec` ↔ `ExecResult`). Stream-mode messages
/// ([`AgentMessage::StreamFrame`], [`AgentMessage::StreamEnd`]) and
/// envelope errors ([`AgentMessage::AgentError`]) have no host
/// counterpart.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "data", rename_all = "snake_case")]
pub enum AgentMessage {
    /// Handshake success.
    HelloOk(HelloOk),
    /// Handshake failure — version mismatch. Connection closes after
    /// this frame.
    HelloErr(HelloErr),

    /// Result of [`HostMessage::Exec`].
    ExecResult(ExecResult),
    /// Result of [`HostMessage::OpenFile`].
    OpenFileResult(OpenFileResult),
    /// Result of [`HostMessage::Read`].
    ReadResult(ReadResult),
    /// Result of [`HostMessage::Write`].
    WriteResult(WriteResult),
    /// Result of [`HostMessage::Close`].
    CloseResult(CloseResult),
    /// Result of [`HostMessage::ReadFile`].
    ReadFileResult(ReadFileResult),
    /// Result of [`HostMessage::WriteFile`].
    WriteFileResult(WriteFileResult),
    /// Result of [`HostMessage::Stat`].
    StatResult(StatResult),
    /// Result of [`HostMessage::TailFile`] (the open-acknowledge).
    TailFileResult(TailFileResult),

    /// Result of [`HostMessage::RunAsync`].
    RunAsyncResult(RunAsyncResult),
    /// Result of [`HostMessage::Wait`].
    WaitResult(WaitResult),
    /// Result of [`HostMessage::Kill`].
    KillResult(KillResult),
    /// Result of [`HostMessage::GetPid`].
    GetPidResult(crate::wire::ops::GetPidResult),

    /// Result of [`HostMessage::GetTime`].
    GetTimeResult(crate::wire::ops::GetTimeResult),
    /// Result of [`HostMessage::SetTime`].
    SetTimeResult(crate::wire::ops::SetTimeResult),
    /// Result of [`HostMessage::SleepClock`].
    SleepClockResult(crate::wire::ops::SleepClockResult),
    /// Result of [`HostMessage::AdvanceClock`].
    AdvanceClockResult(crate::wire::ops::AdvanceClockResult),

    /// Result of [`HostMessage::Syscall`].
    SyscallResult(crate::wire::ops::SyscallResult),

    /// Result of [`HostMessage::ReadMem`].
    ReadMemResult(crate::wire::ops::ReadMemResult),

    /// Result of [`HostMessage::SpawnWorker`].
    SpawnWorkerResult(crate::wire::ops::SpawnWorkerResult),
    /// Result of [`HostMessage::WorkerExec`].
    WorkerExecResult(crate::wire::ops::WorkerExecResult),
    /// Result of [`HostMessage::WorkerJoin`].
    WorkerJoinResult(crate::wire::ops::WorkerJoinResult),

    /// Result of [`HostMessage::Listdir`].
    ListdirResult(crate::wire::ops::ListdirResult),
    /// Result of [`HostMessage::Mkdir`].
    MkdirResult(crate::wire::ops::MkdirResult),
    /// Result of [`HostMessage::Unlink`].
    UnlinkResult(crate::wire::ops::UnlinkResult),
    /// Result of [`HostMessage::Rename`].
    RenameResult(crate::wire::ops::RenameResult),
    /// Result of [`HostMessage::Seek`].
    SeekResult(crate::wire::ops::SeekResult),
    /// Result of [`HostMessage::Ioctl`].
    IoctlResult(crate::wire::ops::IoctlResult),
    /// Result of [`HostMessage::ProcStdinWrite`].
    ProcStdinWriteResult(crate::wire::ops::ProcStdinWriteResult),
    /// Result of [`HostMessage::ProcStdinClose`].
    ProcStdinCloseResult(crate::wire::ops::ProcStdinCloseResult),
    /// Result of [`HostMessage::ProcStatus`].
    ProcStatusResult(crate::wire::ops::ProcStatusResult),

    /// Open-ack for [`HostMessage::ProcStream`]. If `Ok`, the same
    /// connection then carries `StreamFrame`/`StreamEnd`.
    ProcStreamResult(crate::wire::ops::ProcStreamResult),

    /// Open-ack for [`HostMessage::FdStream`].
    FdStreamResult(crate::wire::ops::FdStreamResult),

    /// Result of [`HostMessage::WorkerRunAsync`].
    WorkerRunAsyncResult(crate::wire::ops::WorkerRunAsyncResult),
    /// Result of [`HostMessage::WorkerOpenFile`].
    WorkerOpenFileResult(crate::wire::ops::WorkerOpenFileResult),
    /// Result of [`HostMessage::WorkerSyscall`].
    WorkerSyscallResult(crate::wire::ops::WorkerSyscallResult),
    /// Result of [`HostMessage::WorkerSyscallBegin`] — the async handle.
    WorkerSyscallBeginResult(crate::wire::ops::WorkerSyscallBeginResult),
    /// Result of [`HostMessage::WorkerSyscallAwait`] — the syscall result.
    WorkerSyscallAwaitResult(crate::wire::ops::WorkerSyscallAwaitResult),
    /// Result of [`HostMessage::WorkerKill`].
    WorkerKillResult(crate::wire::ops::WorkerKillResult),

    /// Result of [`HostMessage::BatchExec`] — list of [`ExecResult`]
    /// paired by index with the request's `items`.
    BatchExecResult(crate::wire::ops::BatchExecResult),

    /// Result of [`HostMessage::BatchOp`].
    BatchOpResult(crate::wire::ops::BatchOpResult),

    /// Stream-mode frame following an `Ok` open-acknowledge.
    StreamFrame(StreamFrame),
    /// Terminal stream-mode marker — clean source EOF or mid-stream
    /// error. Connection closes after this frame.
    StreamEnd(StreamEnd),

    /// Envelope-level agent failure (malformed request, unknown
    /// handle, internal bug). Distinct from per-op [`crate::OsError`]
    /// returns; surfaces as a test-infrastructure failure on the host.
    AgentError(AgentError),
}

impl HostMessage {
    /// Stable, machine-friendly discriminator suitable for log lines.
    /// Mirrors what `serde` puts on the wire as the `kind` tag.
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Hello(_) => "hello",
            Self::Exec(_) => "exec",
            Self::OpenFile(_) => "open_file",
            Self::Read(_) => "read",
            Self::Write(_) => "write",
            Self::Close(_) => "close",
            Self::ReadFile(_) => "read_file",
            Self::WriteFile(_) => "write_file",
            Self::Stat(_) => "stat",
            Self::TailFile(_) => "tail_file",
            Self::RunAsync(_) => "run_async",
            Self::Wait(_) => "wait",
            Self::Kill(_) => "kill",
            Self::GetPid(_) => "get_pid",
            Self::GetTime(_) => "get_time",
            Self::SetTime(_) => "set_time",
            Self::SleepClock(_) => "sleep_clock",
            Self::AdvanceClock(_) => "advance_clock",
            Self::Syscall(_) => "syscall",
            Self::ReadMem(_) => "read_mem",
            Self::SpawnWorker(_) => "spawn_worker",
            Self::WorkerExec(_) => "worker_exec",
            Self::WorkerJoin(_) => "worker_join",
            Self::Listdir(_) => "listdir",
            Self::Mkdir(_) => "mkdir",
            Self::Unlink(_) => "unlink",
            Self::Rename(_) => "rename",
            Self::Seek(_) => "seek",
            Self::Ioctl(_) => "ioctl",
            Self::ProcStdinWrite(_) => "proc_stdin_write",
            Self::ProcStdinClose(_) => "proc_stdin_close",
            Self::ProcStatus(_) => "proc_status",
            Self::ProcStream(_) => "proc_stream",
            Self::FdStream(_) => "fd_stream",
            Self::WorkerRunAsync(_) => "worker_run_async",
            Self::WorkerOpenFile(_) => "worker_open_file",
            Self::WorkerSyscall(_) => "worker_syscall",
            Self::WorkerSyscallBegin(_) => "worker_syscall_begin",
            Self::WorkerSyscallAwait(_) => "worker_syscall_await",
            Self::WorkerKill(_) => "worker_kill",
            Self::BatchExec(_) => "batch_exec",
            Self::BatchOp(_) => "batch_op",
        }
    }
}

impl AgentMessage {
    /// Stable, machine-friendly discriminator suitable for log lines.
    pub fn kind(&self) -> &'static str {
        match self {
            Self::HelloOk(_) => "hello_ok",
            Self::HelloErr(_) => "hello_err",
            Self::ExecResult(_) => "exec_result",
            Self::OpenFileResult(_) => "open_file_result",
            Self::ReadResult(_) => "read_result",
            Self::WriteResult(_) => "write_result",
            Self::CloseResult(_) => "close_result",
            Self::ReadFileResult(_) => "read_file_result",
            Self::WriteFileResult(_) => "write_file_result",
            Self::StatResult(_) => "stat_result",
            Self::TailFileResult(_) => "tail_file_result",
            Self::RunAsyncResult(_) => "run_async_result",
            Self::WaitResult(_) => "wait_result",
            Self::KillResult(_) => "kill_result",
            Self::GetPidResult(_) => "get_pid_result",
            Self::GetTimeResult(_) => "get_time_result",
            Self::SetTimeResult(_) => "set_time_result",
            Self::SleepClockResult(_) => "sleep_clock_result",
            Self::AdvanceClockResult(_) => "advance_clock_result",
            Self::SyscallResult(_) => "syscall_result",
            Self::ReadMemResult(_) => "read_mem_result",
            Self::SpawnWorkerResult(_) => "spawn_worker_result",
            Self::WorkerExecResult(_) => "worker_exec_result",
            Self::WorkerJoinResult(_) => "worker_join_result",
            Self::ListdirResult(_) => "listdir_result",
            Self::MkdirResult(_) => "mkdir_result",
            Self::UnlinkResult(_) => "unlink_result",
            Self::RenameResult(_) => "rename_result",
            Self::SeekResult(_) => "seek_result",
            Self::IoctlResult(_) => "ioctl_result",
            Self::ProcStdinWriteResult(_) => "proc_stdin_write_result",
            Self::ProcStdinCloseResult(_) => "proc_stdin_close_result",
            Self::ProcStatusResult(_) => "proc_status_result",
            Self::ProcStreamResult(_) => "proc_stream_result",
            Self::FdStreamResult(_) => "fd_stream_result",
            Self::WorkerRunAsyncResult(_) => "worker_run_async_result",
            Self::WorkerOpenFileResult(_) => "worker_open_file_result",
            Self::WorkerSyscallResult(_) => "worker_syscall_result",
            Self::WorkerSyscallBeginResult(_) => "worker_syscall_begin_result",
            Self::WorkerSyscallAwaitResult(_) => "worker_syscall_await_result",
            Self::WorkerKillResult(_) => "worker_kill_result",
            Self::BatchExecResult(_) => "batch_exec_result",
            Self::BatchOpResult(_) => "batch_op_result",
            Self::StreamFrame(_) => "stream_frame",
            Self::StreamEnd(_) => "stream_end",
            Self::AgentError(_) => "agent_error",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::OsError;
    use crate::handle::FileHandle;

    fn round_trip<T>(value: &T) -> T
    where
        T: serde::Serialize + serde::de::DeserializeOwned,
    {
        let bytes = rmp_serde::to_vec_named(value).unwrap();
        rmp_serde::from_slice(&bytes).unwrap()
    }

    #[test]
    fn hello_envelope_round_trips() {
        let msg = HostMessage::Hello(Hello {
            protocol_version: crate::PROTOCOL_VERSION,
        });
        assert_eq!(msg, round_trip(&msg));
        assert_eq!(msg.kind(), "hello");
    }

    #[test]
    fn exec_envelope_round_trips() {
        let req = HostMessage::Exec(ExecArgs {
            cmd: "ls".into(),
            args: vec![],
            env: Default::default(),
            env_clear: false,
            stdin: vec![],
            cwd: None,
            timeout_ms: None,
        });
        assert_eq!(req, round_trip(&req));

        let resp = AgentMessage::ExecResult(ExecResult::Ok(ExecOk {
            status: ExitStatus::Exited(0),
            stdout: b"a\nb\n".to_vec(),
            stderr: vec![],
        }));
        assert_eq!(resp, round_trip(&resp));
        assert_eq!(resp.kind(), "exec_result");
    }

    #[test]
    fn open_then_read_then_close_envelope_chain() {
        let open = HostMessage::OpenFile(OpenFileArgs {
            path: "/etc/hostname".into(),
            mode: OpenMode {
                read: true,
                ..Default::default()
            },
            create_perm: None,
        });
        assert_eq!(open, round_trip(&open));

        let open_resp = AgentMessage::OpenFileResult(OpResult::Ok(FileHandle::new(7)));
        assert_eq!(open_resp, round_trip(&open_resp));

        let read = HostMessage::Read(ReadArgs {
            handle: FileHandle::new(7),
            max_bytes: 4096,
        });
        assert_eq!(read, round_trip(&read));

        let read_resp = AgentMessage::ReadResult(OpResult::Ok(ReadOk {
            data: b"localhost\n".to_vec(),
        }));
        assert_eq!(read_resp, round_trip(&read_resp));

        let close = HostMessage::Close(CloseArgs {
            handle: FileHandle::new(7),
        });
        assert_eq!(close, round_trip(&close));

        let close_resp = AgentMessage::CloseResult(OpResult::Ok(()));
        assert_eq!(close_resp, round_trip(&close_resp));
    }

    #[test]
    fn agent_error_envelope_round_trips() {
        let err = AgentMessage::AgentError(AgentError {
            kind: AgentErrorKind::UnknownHandle,
            message: "file#42 closed".into(),
        });
        assert_eq!(err, round_trip(&err));
        assert_eq!(err.kind(), "agent_error");
    }

    #[test]
    fn stream_messages_round_trip() {
        let frame = AgentMessage::StreamFrame(StreamFrame {
            data: b"line\n".to_vec(),
        });
        assert_eq!(frame, round_trip(&frame));

        let end_eof = AgentMessage::StreamEnd(StreamEnd::Eof);
        assert_eq!(end_eof, round_trip(&end_eof));

        let end_err = AgentMessage::StreamEnd(StreamEnd::Error(OsError::from_errno(5)));
        assert_eq!(end_err, round_trip(&end_err));
    }

    #[test]
    fn full_handshake_through_frame_codec() {
        // End-to-end: write Hello, read it back from a Cursor, send a
        // HelloOk, read it back. This exercises both the wire types
        // and the frame codec working together.
        use crate::frame::{read_frame, write_frame, DEFAULT_MAX_FRAME_BYTES};
        use std::io::Cursor;

        let mut buf = Vec::new();
        let hello = HostMessage::Hello(Hello {
            protocol_version: crate::PROTOCOL_VERSION,
        });
        write_frame(&mut buf, &hello, DEFAULT_MAX_FRAME_BYTES).unwrap();

        let hello_ok = AgentMessage::HelloOk(HelloOk {
            protocol_version: crate::PROTOCOL_VERSION,
            agent: AgentInfo {
                os: "peios".into(),
                agent_version: "0.1.0".into(),
                kernel: None,
                capabilities: vec![],
            },
        });
        write_frame(&mut buf, &hello_ok, DEFAULT_MAX_FRAME_BYTES).unwrap();

        let mut cursor = Cursor::new(&buf);
        let decoded_hello: HostMessage =
            read_frame(&mut cursor, DEFAULT_MAX_FRAME_BYTES).unwrap();
        assert_eq!(decoded_hello, hello);

        let decoded_ok: AgentMessage =
            read_frame(&mut cursor, DEFAULT_MAX_FRAME_BYTES).unwrap();
        assert_eq!(decoded_ok, hello_ok);
    }
}
