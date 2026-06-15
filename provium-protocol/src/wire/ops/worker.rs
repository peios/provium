//! Worker (sub-agent) ops — slice 10.5 minimum.
//!
//! `SpawnWorker` allocates a [`WorkerHandle`]; `WorkerExec` runs an
//! `Exec` op against that worker; `WorkerJoin` reaps the worker.
//!
//! Slice-10.5 minimum: workers exist as agent-side bookkeeping but
//! share the parent agent's state (open files, processes, …) for
//! ops other than `Exec`. Per-worker state isolation is slice 10.6.

use serde::{Deserialize, Serialize};

use crate::handle::WorkerHandle;

use super::exec::{ExecArgs, ExecResult};
use super::file::{
    OpenFileArgs, OpenFileResult,
};
use super::process::{RunAsyncArgs, RunAsyncResult};
use super::syscall::{SyscallArgs, SyscallResult};
use super::OpResult;

/// `SpawnWorker` — no inputs at slice-10.5 minimum.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SpawnWorkerArgs;

/// Returns the new worker's handle.
pub type SpawnWorkerResult = OpResult<WorkerHandle>;

/// `WorkerExec` — run an exec against the named worker.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkerExecArgs {
    /// Worker handle from a previous [`SpawnWorkerArgs`].
    pub handle: WorkerHandle,
    /// The exec args.
    pub exec: ExecArgs,
}

/// Result mirrors [`ExecResult`].
pub type WorkerExecResult = ExecResult;

/// `WorkerJoin` — reap the worker.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkerJoinArgs {
    /// Worker handle.
    pub handle: WorkerHandle,
}

/// Aggregate result of a `WorkerJoin`. The agent reaps every
/// in-flight async process inside the worker's namespace and
/// reports the worst (max) exit status — 0 when no children ran
/// or all exited cleanly. Mirrors the `worker:join() ->
/// exit_status` contract in DESIGN.md.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct WorkerJoinPayload {
    /// Highest exit status observed across the worker's reaped
    /// async processes. `0` when nothing was running or every
    /// process exited cleanly.
    pub exit_status: i32,
}

/// `WorkerJoin` payload.
pub type WorkerJoinResult = OpResult<WorkerJoinPayload>;

/// `WorkerRunAsync` — start an async process under the worker's
/// own [`crate::handle::ProcessHandle`] namespace. Process handles
/// returned here live in the worker's `AgentState`, not the parent's.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkerRunAsyncArgs {
    /// Worker handle.
    pub handle: WorkerHandle,
    /// Inner async-spawn args.
    pub args: RunAsyncArgs,
}

/// `WorkerRunAsync` payload.
pub type WorkerRunAsyncResult = RunAsyncResult;

/// `WorkerOpenFile` — open a file in the worker's own file table.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkerOpenFileArgs {
    /// Worker handle.
    pub handle: WorkerHandle,
    /// Inner open-file args.
    pub args: OpenFileArgs,
}

/// `WorkerOpenFile` payload.
pub type WorkerOpenFileResult = OpenFileResult;

/// `WorkerSyscall` — issue a raw Layer-0 syscall under the worker.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkerSyscallArgs {
    /// Worker handle.
    pub handle: WorkerHandle,
    /// Inner syscall args.
    pub args: SyscallArgs,
}

/// `WorkerSyscall` payload.
pub type WorkerSyscallResult = SyscallResult;

/// `WorkerKill` — broadcast a signal to every process in the
/// worker's namespace. Used by `worker:kill(sig)` to terminate
/// in-flight async children before `worker:join`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkerKillArgs {
    /// Worker handle.
    pub handle: WorkerHandle,
    /// Signal number — POSIX (`9` = SIGKILL, `15` = SIGTERM).
    pub signal: i32,
}

/// `WorkerKill` payload — count of signalled processes.
pub type WorkerKillResult = OpResult<u32>;

#[cfg(test)]
mod tests {
    use super::*;

    fn round_trip<T>(v: &T) -> T
    where
        T: Serialize + serde::de::DeserializeOwned,
    {
        let b = rmp_serde::to_vec_named(v).unwrap();
        rmp_serde::from_slice(&b).unwrap()
    }

    #[test]
    fn spawn_args_round_trips() {
        let a = SpawnWorkerArgs;
        assert_eq!(a, round_trip(&a));
    }

    #[test]
    fn exec_args_round_trips() {
        let e = WorkerExecArgs {
            handle: WorkerHandle::new(7),
            exec: ExecArgs {
                cmd: "ls".into(),
                args: vec![],
                env: Default::default(),
                env_clear: false,
                stdin: vec![],
                cwd: None,
                timeout_ms: None,
            },
        };
        assert_eq!(e, round_trip(&e));
    }

    #[test]
    fn join_args_round_trips() {
        let j = WorkerJoinArgs {
            handle: WorkerHandle::new(3),
        };
        assert_eq!(j, round_trip(&j));
    }
}
