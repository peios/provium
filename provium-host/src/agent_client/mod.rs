//! Host-side counterpart to [`provium_agent::connection::handle_connection`].
//!
//! Per `DESIGN.md`, short ops are connectionless: every [`AgentClient`]
//! method opens a fresh connection via the configured
//! [`crate::connector::Connector`], drives the Hello handshake, sends
//! one op, reads one response, closes. Stream ops (currently
//! `tail_file`) keep the connection open and return a
//! [`tail::TailFileSession`] that owns it.
//!
//! ## Concurrency
//!
//! [`AgentClient`] is `Send + Sync` because every connector is
//! required to be (the trait bound). Each op opens its own
//! connection, so the client itself owns no shared mutable state and
//! can be wrapped in [`std::sync::Arc`] and shared across the host's
//! runner threads without further locking.

use provium_protocol::frame::{read_frame, write_frame, DEFAULT_MAX_FRAME_BYTES};
use provium_protocol::wire::{
    AdvanceClockArgs, AdvanceClockResult, AgentMessage, CloseArgs, CloseResult, ExecArgs,
    ExecResult, GetPidArgs, GetPidResult, GetTimeArgs, GetTimeResult, Hello, HostMessage, KillArgs,
    KillResult, OpenFileArgs,
    OpenFileResult, ReadArgs, ReadFileArgs, ReadFileResult, ReadResult, RunAsyncArgs,
    RunAsyncResult, SetTimeArgs, SetTimeResult, SleepClockArgs, SleepClockResult, StatArgs,
    StatResult, SyscallArgs, SyscallResult, TailFileArgs, WaitArgs, WaitResult, WriteArgs,
    WriteFileArgs, WriteFileResult, WriteResult,
};
use provium_protocol::PROTOCOL_VERSION;

use crate::connector::{AgentStream, Connector};
use crate::ClientError;

mod tail;
pub use tail::TailFileSession;

/// Typed wrapper around the host-↔-agent wire protocol.
///
/// Non-generic over the connector — the connector is held behind a
/// `Box<dyn Connector>` so [`crate::vm::Vm`] (and its mlua
/// `UserData` impl) can be a single concrete type regardless of
/// backend.
pub struct AgentClient {
    connector: Box<dyn Connector>,
}

impl std::fmt::Debug for AgentClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AgentClient").finish()
    }
}

impl AgentClient {
    /// Wrap a connector in a typed client.
    pub fn new(connector: impl Connector + 'static) -> Self {
        Self {
            connector: Box::new(connector),
        }
    }

    /// Build from an already-boxed connector — useful for adapters
    /// that compose an existing `Box<dyn Connector>`.
    pub fn from_boxed(connector: Box<dyn Connector>) -> Self {
        Self { connector }
    }

    /// Open a connection, complete the Hello handshake, and immediately
    /// close. Useful as a liveness probe — the
    /// [`crate::vmm::qemu::QemuVmm`] retry loop calls it to detect
    /// when the in-VM agent has finished booting.
    ///
    /// `Ok(())` means the handshake worked (so the agent process is
    /// up, the protocol versions match, and at least one connection
    /// is being accepted). The agent's connection-handler thread will
    /// observe an EOF on its op-read and log it once, which is
    /// expected and harmless during boot retries.
    pub fn ping(&self) -> Result<(), ClientError> {
        let _stream = self.open_with_handshake()?;
        Ok(())
    }

    // -----------------------------------------------------------------
    // Short ops
    // -----------------------------------------------------------------

    /// Run a command synchronously to completion.
    pub fn exec(&self, args: ExecArgs) -> Result<ExecResult, ClientError> {
        let mut stream = self.open_with_handshake()?;
        send(&mut stream, HostMessage::Exec(args))?;
        match recv(&mut stream)? {
            AgentMessage::ExecResult(r) => Ok(r),
            AgentMessage::AgentError(e) => Err(e.into()),
            other => Err(unexpected("exec_result", &other)),
        }
    }

    /// Batched exec — agent runs each item in order, returns paired
    /// results. One handshake + one round-trip for the whole batch.
    pub fn batch_exec(
        &self,
        items: Vec<ExecArgs>,
    ) -> Result<Vec<ExecResult>, ClientError> {
        let mut stream = self.open_with_handshake()?;
        send(
            &mut stream,
            HostMessage::BatchExec(provium_protocol::wire::ops::BatchExecArgs {
                items,
            }),
        )?;
        match recv(&mut stream)? {
            AgentMessage::BatchExecResult(r) => Ok(r.results),
            AgentMessage::AgentError(e) => Err(e.into()),
            other => Err(unexpected("batch_exec_result", &other)),
        }
    }

    /// Generic batch — agent dispatches each item in order against
    /// the same `AgentState`. One handshake + one round-trip.
    /// Stream / handshake / nested-batch items are rejected by the
    /// agent with an `AgentError` for that index.
    pub fn batch_op(
        &self,
        items: Vec<HostMessage>,
    ) -> Result<Vec<AgentMessage>, ClientError> {
        let mut stream = self.open_with_handshake()?;
        send(
            &mut stream,
            HostMessage::BatchOp(provium_protocol::wire::ops::BatchOpArgs { items }),
        )?;
        match recv(&mut stream)? {
            AgentMessage::BatchOpResult(r) => Ok(r.responses),
            AgentMessage::AgentError(e) => Err(e.into()),
            other => Err(unexpected("batch_op_result", &other)),
        }
    }

    /// Open a file in the agent's open-file table and return its handle.
    pub fn open_file(&self, args: OpenFileArgs) -> Result<OpenFileResult, ClientError> {
        let mut stream = self.open_with_handshake()?;
        send(&mut stream, HostMessage::OpenFile(args))?;
        match recv(&mut stream)? {
            AgentMessage::OpenFileResult(r) => Ok(r),
            AgentMessage::AgentError(e) => Err(e.into()),
            other => Err(unexpected("open_file_result", &other)),
        }
    }

    /// Read up to `max_bytes` from a previously-opened file.
    pub fn read(&self, args: ReadArgs) -> Result<ReadResult, ClientError> {
        let mut stream = self.open_with_handshake()?;
        send(&mut stream, HostMessage::Read(args))?;
        match recv(&mut stream)? {
            AgentMessage::ReadResult(r) => Ok(r),
            AgentMessage::AgentError(e) => Err(e.into()),
            other => Err(unexpected("read_result", &other)),
        }
    }

    /// Write bytes to a previously-opened file.
    pub fn write(&self, args: WriteArgs) -> Result<WriteResult, ClientError> {
        let mut stream = self.open_with_handshake()?;
        send(&mut stream, HostMessage::Write(args))?;
        match recv(&mut stream)? {
            AgentMessage::WriteResult(r) => Ok(r),
            AgentMessage::AgentError(e) => Err(e.into()),
            other => Err(unexpected("write_result", &other)),
        }
    }

    /// Drop a handle from the agent's open-file table.
    pub fn close(&self, args: CloseArgs) -> Result<CloseResult, ClientError> {
        let mut stream = self.open_with_handshake()?;
        send(&mut stream, HostMessage::Close(args))?;
        match recv(&mut stream)? {
            AgentMessage::CloseResult(r) => Ok(r),
            AgentMessage::AgentError(e) => Err(e.into()),
            other => Err(unexpected("close_result", &other)),
        }
    }

    /// Composite read-to-end on a path.
    pub fn read_file(&self, args: ReadFileArgs) -> Result<ReadFileResult, ClientError> {
        let mut stream = self.open_with_handshake()?;
        send(&mut stream, HostMessage::ReadFile(args))?;
        match recv(&mut stream)? {
            AgentMessage::ReadFileResult(r) => Ok(r),
            AgentMessage::AgentError(e) => Err(e.into()),
            other => Err(unexpected("read_file_result", &other)),
        }
    }

    /// Composite create/replace/append at a path.
    pub fn write_file(&self, args: WriteFileArgs) -> Result<WriteFileResult, ClientError> {
        let mut stream = self.open_with_handshake()?;
        send(&mut stream, HostMessage::WriteFile(args))?;
        match recv(&mut stream)? {
            AgentMessage::WriteFileResult(r) => Ok(r),
            AgentMessage::AgentError(e) => Err(e.into()),
            other => Err(unexpected("write_file_result", &other)),
        }
    }

    /// Stat a path.
    pub fn stat(&self, args: StatArgs) -> Result<StatResult, ClientError> {
        let mut stream = self.open_with_handshake()?;
        send(&mut stream, HostMessage::Stat(args))?;
        match recv(&mut stream)? {
            AgentMessage::StatResult(r) => Ok(r),
            AgentMessage::AgentError(e) => Err(e.into()),
            other => Err(unexpected("stat_result", &other)),
        }
    }

    /// Spawn an asynchronous process; returns its handle.
    pub fn run_async(&self, args: RunAsyncArgs) -> Result<RunAsyncResult, ClientError> {
        let mut stream = self.open_with_handshake()?;
        send(&mut stream, HostMessage::RunAsync(args))?;
        match recv(&mut stream)? {
            AgentMessage::RunAsyncResult(r) => Ok(r),
            AgentMessage::AgentError(e) => Err(e.into()),
            other => Err(unexpected("run_async_result", &other)),
        }
    }

    /// Wait for an async process to exit.
    pub fn wait(&self, args: WaitArgs) -> Result<WaitResult, ClientError> {
        let mut stream = self.open_with_handshake()?;
        send(&mut stream, HostMessage::Wait(args))?;
        match recv(&mut stream)? {
            AgentMessage::WaitResult(r) => Ok(r),
            AgentMessage::AgentError(e) => Err(e.into()),
            other => Err(unexpected("wait_result", &other)),
        }
    }

    /// Send a signal to an async process.
    pub fn kill(&self, args: KillArgs) -> Result<KillResult, ClientError> {
        let mut stream = self.open_with_handshake()?;
        send(&mut stream, HostMessage::Kill(args))?;
        match recv(&mut stream)? {
            AgentMessage::KillResult(r) => Ok(r),
            AgentMessage::AgentError(e) => Err(e.into()),
            other => Err(unexpected("kill_result", &other)),
        }
    }

    /// Fetch the kernel-level PID of an in-flight async process.
    pub fn get_pid(&self, args: GetPidArgs) -> Result<GetPidResult, ClientError> {
        let mut stream = self.open_with_handshake()?;
        send(&mut stream, HostMessage::GetPid(args))?;
        match recv(&mut stream)? {
            AgentMessage::GetPidResult(r) => Ok(r),
            AgentMessage::AgentError(e) => Err(e.into()),
            other => Err(unexpected("get_pid_result", &other)),
        }
    }

    /// Read the agent's wall clock.
    pub fn get_time(&self) -> Result<GetTimeResult, ClientError> {
        let mut stream = self.open_with_handshake()?;
        send(&mut stream, HostMessage::GetTime(GetTimeArgs))?;
        match recv(&mut stream)? {
            AgentMessage::GetTimeResult(r) => Ok(r),
            AgentMessage::AgentError(e) => Err(e.into()),
            other => Err(unexpected("get_time_result", &other)),
        }
    }

    /// Set the agent's wall clock.
    pub fn set_time(&self, args: SetTimeArgs) -> Result<SetTimeResult, ClientError> {
        let mut stream = self.open_with_handshake()?;
        send(&mut stream, HostMessage::SetTime(args))?;
        match recv(&mut stream)? {
            AgentMessage::SetTimeResult(r) => Ok(r),
            AgentMessage::AgentError(e) => Err(e.into()),
            other => Err(unexpected("set_time_result", &other)),
        }
    }

    /// Pause the agent for a duration.
    pub fn sleep_clock(&self, args: SleepClockArgs) -> Result<SleepClockResult, ClientError> {
        let mut stream = self.open_with_handshake()?;
        send(&mut stream, HostMessage::SleepClock(args))?;
        match recv(&mut stream)? {
            AgentMessage::SleepClockResult(r) => Ok(r),
            AgentMessage::AgentError(e) => Err(e.into()),
            other => Err(unexpected("sleep_clock_result", &other)),
        }
    }

    /// Bump the agent's wall clock by a relative amount.
    pub fn advance_clock(
        &self,
        args: AdvanceClockArgs,
    ) -> Result<AdvanceClockResult, ClientError> {
        let mut stream = self.open_with_handshake()?;
        send(&mut stream, HostMessage::AdvanceClock(args))?;
        match recv(&mut stream)? {
            AgentMessage::AdvanceClockResult(r) => Ok(r),
            AgentMessage::AgentError(e) => Err(e.into()),
            other => Err(unexpected("advance_clock_result", &other)),
        }
    }

    /// Issue a Layer-0 raw syscall.
    pub fn syscall(&self, args: SyscallArgs) -> Result<SyscallResult, ClientError> {
        let mut stream = self.open_with_handshake()?;
        send(&mut stream, HostMessage::Syscall(args))?;
        match recv(&mut stream)? {
            AgentMessage::SyscallResult(r) => Ok(r),
            AgentMessage::AgentError(e) => Err(e.into()),
            other => Err(unexpected("syscall_result", &other)),
        }
    }

    /// Spawn a sub-agent worker.
    pub fn spawn_worker(
        &self,
    ) -> Result<provium_protocol::wire::SpawnWorkerResult, ClientError> {
        let mut stream = self.open_with_handshake()?;
        send(
            &mut stream,
            HostMessage::SpawnWorker(provium_protocol::wire::SpawnWorkerArgs),
        )?;
        match recv(&mut stream)? {
            AgentMessage::SpawnWorkerResult(r) => Ok(r),
            AgentMessage::AgentError(e) => Err(e.into()),
            other => Err(unexpected("spawn_worker_result", &other)),
        }
    }

    /// Run an exec against a worker.
    pub fn worker_exec(
        &self,
        args: provium_protocol::wire::WorkerExecArgs,
    ) -> Result<provium_protocol::wire::WorkerExecResult, ClientError> {
        let mut stream = self.open_with_handshake()?;
        send(&mut stream, HostMessage::WorkerExec(args))?;
        match recv(&mut stream)? {
            AgentMessage::WorkerExecResult(r) => Ok(r),
            AgentMessage::AgentError(e) => Err(e.into()),
            other => Err(unexpected("worker_exec_result", &other)),
        }
    }

    /// Reap a worker.
    pub fn worker_join(
        &self,
        args: provium_protocol::wire::WorkerJoinArgs,
    ) -> Result<provium_protocol::wire::WorkerJoinResult, ClientError> {
        let mut stream = self.open_with_handshake()?;
        send(&mut stream, HostMessage::WorkerJoin(args))?;
        match recv(&mut stream)? {
            AgentMessage::WorkerJoinResult(r) => Ok(r),
            AgentMessage::AgentError(e) => Err(e.into()),
            other => Err(unexpected("worker_join_result", &other)),
        }
    }

    /// List a directory.
    pub fn listdir(
        &self,
        args: provium_protocol::wire::ListdirArgs,
    ) -> Result<provium_protocol::wire::ListdirResult, ClientError> {
        let mut stream = self.open_with_handshake()?;
        send(&mut stream, HostMessage::Listdir(args))?;
        match recv(&mut stream)? {
            AgentMessage::ListdirResult(r) => Ok(r),
            AgentMessage::AgentError(e) => Err(e.into()),
            other => Err(unexpected("listdir_result", &other)),
        }
    }

    /// Create a directory.
    pub fn mkdir(
        &self,
        args: provium_protocol::wire::MkdirArgs,
    ) -> Result<provium_protocol::wire::MkdirResult, ClientError> {
        let mut stream = self.open_with_handshake()?;
        send(&mut stream, HostMessage::Mkdir(args))?;
        match recv(&mut stream)? {
            AgentMessage::MkdirResult(r) => Ok(r),
            AgentMessage::AgentError(e) => Err(e.into()),
            other => Err(unexpected("mkdir_result", &other)),
        }
    }

    /// Remove a path.
    pub fn unlink(
        &self,
        args: provium_protocol::wire::UnlinkArgs,
    ) -> Result<provium_protocol::wire::UnlinkResult, ClientError> {
        let mut stream = self.open_with_handshake()?;
        send(&mut stream, HostMessage::Unlink(args))?;
        match recv(&mut stream)? {
            AgentMessage::UnlinkResult(r) => Ok(r),
            AgentMessage::AgentError(e) => Err(e.into()),
            other => Err(unexpected("unlink_result", &other)),
        }
    }

    /// Rename a path.
    pub fn rename(
        &self,
        args: provium_protocol::wire::RenameArgs,
    ) -> Result<provium_protocol::wire::RenameResult, ClientError> {
        let mut stream = self.open_with_handshake()?;
        send(&mut stream, HostMessage::Rename(args))?;
        match recv(&mut stream)? {
            AgentMessage::RenameResult(r) => Ok(r),
            AgentMessage::AgentError(e) => Err(e.into()),
            other => Err(unexpected("rename_result", &other)),
        }
    }

    /// Seek an open file.
    pub fn seek(
        &self,
        args: provium_protocol::wire::SeekArgs,
    ) -> Result<provium_protocol::wire::SeekResult, ClientError> {
        let mut stream = self.open_with_handshake()?;
        send(&mut stream, HostMessage::Seek(args))?;
        match recv(&mut stream)? {
            AgentMessage::SeekResult(r) => Ok(r),
            AgentMessage::AgentError(e) => Err(e.into()),
            other => Err(unexpected("seek_result", &other)),
        }
    }

    /// Layer-0 ioctl.
    pub fn ioctl(
        &self,
        args: provium_protocol::wire::IoctlArgs,
    ) -> Result<provium_protocol::wire::IoctlResult, ClientError> {
        let mut stream = self.open_with_handshake()?;
        send(&mut stream, HostMessage::Ioctl(args))?;
        match recv(&mut stream)? {
            AgentMessage::IoctlResult(r) => Ok(r),
            AgentMessage::AgentError(e) => Err(e.into()),
            other => Err(unexpected("ioctl_result", &other)),
        }
    }

    /// Write to async-process stdin.
    pub fn proc_stdin_write(
        &self,
        args: provium_protocol::wire::ProcStdinWriteArgs,
    ) -> Result<provium_protocol::wire::ProcStdinWriteResult, ClientError> {
        let mut stream = self.open_with_handshake()?;
        send(&mut stream, HostMessage::ProcStdinWrite(args))?;
        match recv(&mut stream)? {
            AgentMessage::ProcStdinWriteResult(r) => Ok(r),
            AgentMessage::AgentError(e) => Err(e.into()),
            other => Err(unexpected("proc_stdin_write_result", &other)),
        }
    }

    /// Close async-process stdin.
    pub fn proc_stdin_close(
        &self,
        args: provium_protocol::wire::ProcStdinCloseArgs,
    ) -> Result<provium_protocol::wire::ProcStdinCloseResult, ClientError> {
        let mut stream = self.open_with_handshake()?;
        send(&mut stream, HostMessage::ProcStdinClose(args))?;
        match recv(&mut stream)? {
            AgentMessage::ProcStdinCloseResult(r) => Ok(r),
            AgentMessage::AgentError(e) => Err(e.into()),
            other => Err(unexpected("proc_stdin_close_result", &other)),
        }
    }

    /// Liveness query for an async process.
    pub fn proc_status(
        &self,
        args: provium_protocol::wire::ProcStatusArgs,
    ) -> Result<provium_protocol::wire::ProcStatusResult, ClientError> {
        let mut stream = self.open_with_handshake()?;
        send(&mut stream, HostMessage::ProcStatus(args))?;
        match recv(&mut stream)? {
            AgentMessage::ProcStatusResult(r) => Ok(r),
            AgentMessage::AgentError(e) => Err(e.into()),
            other => Err(unexpected("proc_status_result", &other)),
        }
    }

    // -----------------------------------------------------------------
    // Stream ops
    // -----------------------------------------------------------------

    /// Open a streaming tail of a file.
    ///
    /// On `Ok`, returns a [`TailFileSession`] that owns the connection
    /// and yields [`provium_protocol::wire::StreamFrame`]s via
    /// [`TailFileSession::next_frame`]. The session ends when the
    /// agent emits a [`provium_protocol::wire::StreamEnd`] or when the
    /// caller drops the session (closing the connection).
    pub fn tail_file(&self, args: TailFileArgs) -> Result<TailFileOutcome, ClientError> {
        let mut stream = self.open_with_handshake()?;
        send(&mut stream, HostMessage::TailFile(args))?;
        match recv(&mut stream)? {
            AgentMessage::TailFileResult(provium_protocol::wire::OpResult::Ok(())) => {
                Ok(TailFileOutcome::Ok(TailFileSession::new(stream)))
            }
            AgentMessage::TailFileResult(provium_protocol::wire::OpResult::Err(e)) => {
                Ok(TailFileOutcome::Err(e))
            }
            AgentMessage::AgentError(e) => Err(e.into()),
            other => Err(unexpected("tail_file_result", &other)),
        }
    }

    /// Open a streaming subscription to a tracked async-process's
    /// captured stdout or stderr. Same lifecycle as
    /// [`Self::tail_file`] — frames flow until process EOF or host
    /// disconnect.
    pub fn proc_stream(
        &self,
        args: provium_protocol::wire::ops::ProcStreamArgs,
    ) -> Result<TailFileOutcome, ClientError> {
        let mut stream = self.open_with_handshake()?;
        send(&mut stream, HostMessage::ProcStream(args))?;
        match recv(&mut stream)? {
            AgentMessage::ProcStreamResult(provium_protocol::wire::OpResult::Ok(())) => {
                Ok(TailFileOutcome::Ok(TailFileSession::new(stream)))
            }
            AgentMessage::ProcStreamResult(provium_protocol::wire::OpResult::Err(e)) => {
                Ok(TailFileOutcome::Err(e))
            }
            AgentMessage::AgentError(e) => Err(e.into()),
            other => Err(unexpected("proc_stream_result", &other)),
        }
    }

    /// Async-spawn under a worker. Process handle returned lives in
    /// the worker's `AgentState` subnamespace.
    pub fn worker_run_async(
        &self,
        args: provium_protocol::wire::WorkerRunAsyncArgs,
    ) -> Result<provium_protocol::wire::RunAsyncResult, ClientError> {
        let mut stream = self.open_with_handshake()?;
        send(&mut stream, HostMessage::WorkerRunAsync(args))?;
        match recv(&mut stream)? {
            AgentMessage::WorkerRunAsyncResult(r) => Ok(r),
            AgentMessage::AgentError(e) => Err(e.into()),
            other => Err(unexpected("worker_run_async_result", &other)),
        }
    }

    /// Open a file under a worker.
    pub fn worker_open_file(
        &self,
        args: provium_protocol::wire::WorkerOpenFileArgs,
    ) -> Result<provium_protocol::wire::OpenFileResult, ClientError> {
        let mut stream = self.open_with_handshake()?;
        send(&mut stream, HostMessage::WorkerOpenFile(args))?;
        match recv(&mut stream)? {
            AgentMessage::WorkerOpenFileResult(r) => Ok(r),
            AgentMessage::AgentError(e) => Err(e.into()),
            other => Err(unexpected("worker_open_file_result", &other)),
        }
    }

    /// Broadcast a signal to every process in a worker's namespace.
    pub fn worker_kill(
        &self,
        args: provium_protocol::wire::WorkerKillArgs,
    ) -> Result<provium_protocol::wire::WorkerKillResult, ClientError> {
        let mut stream = self.open_with_handshake()?;
        send(&mut stream, HostMessage::WorkerKill(args))?;
        match recv(&mut stream)? {
            AgentMessage::WorkerKillResult(r) => Ok(r),
            AgentMessage::AgentError(e) => Err(e.into()),
            other => Err(unexpected("worker_kill_result", &other)),
        }
    }

    /// Raw syscall under a worker. Validates the worker exists; the
    /// syscall itself is stateless.
    pub fn worker_syscall(
        &self,
        args: provium_protocol::wire::WorkerSyscallArgs,
    ) -> Result<provium_protocol::wire::SyscallResult, ClientError> {
        let mut stream = self.open_with_handshake()?;
        send(&mut stream, HostMessage::WorkerSyscall(args))?;
        match recv(&mut stream)? {
            AgentMessage::WorkerSyscallResult(r) => Ok(r),
            AgentMessage::AgentError(e) => Err(e.into()),
            other => Err(unexpected("worker_syscall_result", &other)),
        }
    }

    /// Open a streaming subscription to an already-open file handle.
    /// Used by `vm:fd_stream` for arbitrary fds (sockets, pipes, …).
    pub fn fd_stream(
        &self,
        args: provium_protocol::wire::FdStreamArgs,
    ) -> Result<TailFileOutcome, ClientError> {
        let mut stream = self.open_with_handshake()?;
        send(&mut stream, HostMessage::FdStream(args))?;
        match recv(&mut stream)? {
            AgentMessage::FdStreamResult(provium_protocol::wire::OpResult::Ok(())) => {
                Ok(TailFileOutcome::Ok(TailFileSession::new(stream)))
            }
            AgentMessage::FdStreamResult(provium_protocol::wire::OpResult::Err(e)) => {
                Ok(TailFileOutcome::Err(e))
            }
            AgentMessage::AgentError(e) => Err(e.into()),
            other => Err(unexpected("fd_stream_result", &other)),
        }
    }

    // -----------------------------------------------------------------
    // Internals
    // -----------------------------------------------------------------

    /// Open a connection and complete the Hello handshake. Returns a
    /// stream ready to carry one op (or stream-mode frames).
    fn open_with_handshake(&self) -> Result<Box<dyn AgentStream>, ClientError> {
        let mut stream = self.connector.connect().map_err(ClientError::Connect)?;
        send(
            &mut stream,
            HostMessage::Hello(Hello {
                protocol_version: PROTOCOL_VERSION,
            }),
        )?;
        match recv(&mut stream)? {
            AgentMessage::HelloOk(_) => Ok(stream),
            AgentMessage::HelloErr(err) => Err(ClientError::VersionMismatch {
                host: err.host_version,
                agent: err.agent_version,
            }),
            other => Err(unexpected("hello_ok", &other)),
        }
    }
}

/// Result of [`AgentClient::tail_file`].
///
/// Modelled as an explicit enum rather than `Result` so the host
/// distinguishes "wire-/protocol-level failure" (returned via
/// [`ClientError`]) from "the agent successfully reported that the
/// source could not be opened" (returned here as `Err`).
#[derive(Debug)]
pub enum TailFileOutcome {
    /// Stream opened — frames will follow.
    Ok(TailFileSession),
    /// The agent could not open the source. The session was never
    /// established; the connection is already closed.
    Err(provium_protocol::OsError),
}

// -----------------------------------------------------------------------
// Free helpers
// -----------------------------------------------------------------------

pub(crate) fn send(
    w: &mut Box<dyn AgentStream>,
    msg: HostMessage,
) -> Result<(), ClientError> {
    // `Box<T>` itself implements `Read`/`Write` when `T` does, so we
    // pass the box directly — no `as_mut()` deref to a `dyn` (which
    // would be `?Sized` and rejected by `read_frame`'s generic bound).
    write_frame(w, &msg, DEFAULT_MAX_FRAME_BYTES)?;
    Ok(())
}

pub(crate) fn recv(r: &mut Box<dyn AgentStream>) -> Result<AgentMessage, ClientError> {
    Ok(read_frame(r, DEFAULT_MAX_FRAME_BYTES)?)
}

pub(crate) fn unexpected(expected: &'static str, actual: &AgentMessage) -> ClientError {
    ClientError::UnexpectedMessage {
        expected,
        actual: actual.kind(),
    }
}
