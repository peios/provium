//! Worker (sub-agent) op handlers — slice 10.5 minimum.

use std::sync::Arc;

use provium_protocol::wire::{
    AgentError, AgentErrorKind, AgentMessage, OpResult, SpawnWorkerArgs, WorkerExecArgs,
    WorkerJoinArgs, WorkerJoinPayload, WorkerKillArgs, WorkerOpenFileArgs, WorkerRunAsyncArgs,
    WorkerSyscallArgs,
};

use crate::state::AgentState;

use super::{exec, file, process, syscall};

/// `SpawnWorker` — allocate a worker handle plus its own
/// independent [`AgentState`] (separate file + process tables
/// from the parent).
pub fn spawn(_args: SpawnWorkerArgs, state: &Arc<AgentState>) -> AgentMessage {
    let handle = state.insert_worker();
    AgentMessage::SpawnWorkerResult(OpResult::Ok(handle))
}

/// `WorkerExec` — run an exec against the worker. Validates the
/// worker handle exists. Sync `exec` is stateless on the agent
/// side (no process handle returned) so isolation is by call-time
/// validation rather than per-worker process-table routing.
pub fn worker_exec(args: WorkerExecArgs, state: &Arc<AgentState>) -> AgentMessage {
    let Some(_worker_state) = state.worker_state(args.handle) else {
        return AgentMessage::AgentError(AgentError {
            kind: AgentErrorKind::UnknownHandle,
            message: format!("worker_exec on {}", args.handle),
        });
    };
    // exec is stateless — there's no per-worker process table to
    // route through for sync exec. Future async per-worker exec
    // already routes via `worker_run_async` → `worker_state`.
    AgentMessage::WorkerExecResult(exec::run(args.exec))
}

/// `WorkerJoin` — reap every in-flight async process bound to the
/// worker's namespace, drop the worker handle, and return the
/// worst exit status seen. A worker that ran no async children
/// (or whose children all exited cleanly) joins with `0`.
pub fn worker_join(args: WorkerJoinArgs, state: &Arc<AgentState>) -> AgentMessage {
    if !state.has_worker(args.handle) {
        return AgentMessage::AgentError(AgentError {
            kind: AgentErrorKind::UnknownHandle,
            message: format!("worker_join on {}", args.handle),
        });
    }
    let proc_handles = state.take_worker_processes(args.handle);
    let exit_status = state.reap_processes(&proc_handles);
    state.remove_worker(args.handle);
    AgentMessage::WorkerJoinResult(OpResult::Ok(WorkerJoinPayload { exit_status }))
}

/// `WorkerRunAsync` — async-spawn under the worker. The returned
/// process handle is allocated in the *parent* agent's process
/// table (not the worker's subnamespace) so subsequent `Wait` /
/// `Kill` / `ProcStdinWrite` / `ProcStream` / `GetPid` ops — which
/// only carry a [`ProcessHandle`] and dispatch through the
/// parent's process table — can find the slot. Per-worker
/// process isolation (the 10.6 plan) requires
/// `WorkerWait`/`WorkerKill`/etc. wire ops to preserve the
/// worker scope; until those land, the worker boundary is
/// enforced only at spawn-validation time, not at every op.
pub fn worker_run_async(
    args: WorkerRunAsyncArgs,
    state: &Arc<AgentState>,
) -> AgentMessage {
    if !state.has_worker(args.handle) {
        return AgentMessage::AgentError(AgentError {
            kind: AgentErrorKind::UnknownHandle,
            message: format!("worker_run_async on {}", args.handle),
        });
    }
    let result = process::run_async(args.args, state);
    if let AgentMessage::RunAsyncResult(OpResult::Ok(proc_handle)) = &result {
        // Track the process under the worker so worker_kill /
        // worker_join can target only that worker's children.
        state.bind_process_to_worker(args.handle, *proc_handle);
    }
    AgentMessage::WorkerRunAsyncResult(match result {
        AgentMessage::RunAsyncResult(r) => r,
        // process::run_async only returns RunAsyncResult on this
        // path; an Err arm keeps the match exhaustive.
        _ => OpResult::Err(provium_protocol::OsError {
            errno: 0,
            message: "worker_run_async: unexpected agent message shape".into(),
        }),
    })
}

/// `WorkerOpenFile` — open a file under the worker. The handle is
/// allocated in the *parent* agent's file table (not the worker's
/// own subnamespace) so subsequent `file:read` / `:write` /
/// `:close` ops — which carry only a [`FileHandle`] and route
/// through the parent's dispatch — can find the slot.
///
/// Per-worker file isolation (the original 10.6 plan) needs
/// dedicated `WorkerReadFile` / `WorkerWriteFile` / `WorkerCloseFile`
/// wire ops so the handle's worker scope is preserved over the
/// wire. Until those land, opening under the worker only validates
/// the worker handle exists; the file itself lives at parent
/// scope. Documented at `DESIGN.md` § NIC, Disk, Console,
/// Snapshot, Clock, Worker (the "v1 worker isolation" note).
pub fn worker_open_file(
    args: WorkerOpenFileArgs,
    state: &Arc<AgentState>,
) -> AgentMessage {
    if !state.has_worker(args.handle) {
        return AgentMessage::AgentError(AgentError {
            kind: AgentErrorKind::UnknownHandle,
            message: format!("worker_open_file on {}", args.handle),
        });
    }
    file::open_file(args.args, state)
}

/// `WorkerSyscall` — raw syscall under the worker (Layer-0 ops are
/// stateless from the agent's perspective, but we still validate
/// the handle exists for correctness).
pub fn worker_syscall(
    args: WorkerSyscallArgs,
    state: &Arc<AgentState>,
) -> AgentMessage {
    if !state.has_worker(args.handle) {
        return AgentMessage::AgentError(AgentError {
            kind: AgentErrorKind::UnknownHandle,
            message: format!("worker_syscall on {}", args.handle),
        });
    }
    // Re-wrap as the worker variant — `syscall::syscall`
    // returns the unscoped `SyscallResult` variant by default,
    // but the host's worker client expects `WorkerSyscallResult`
    // and rejects the bare variant as a protocol mismatch.
    match syscall::syscall(args.args) {
        AgentMessage::SyscallResult(payload) => {
            AgentMessage::WorkerSyscallResult(payload)
        }
        other => other,
    }
}

/// `WorkerKill` — broadcast `signal` to every process registered
/// under the worker's namespace. Returns the number of processes
/// signalled.
pub fn worker_kill(
    args: WorkerKillArgs,
    state: &Arc<AgentState>,
) -> AgentMessage {
    if !state.has_worker(args.handle) {
        return AgentMessage::AgentError(AgentError {
            kind: AgentErrorKind::UnknownHandle,
            message: format!("worker_kill on {}", args.handle),
        });
    }
    let handles = state.worker_process_handles(args.handle);
    let count = state.signal_processes(&handles, args.signal);
    AgentMessage::WorkerKillResult(OpResult::Ok(count))
}
