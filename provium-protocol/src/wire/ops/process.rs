//! `RunAsync` / `Wait` / `Kill` — async process control.
//!
//! Per `DESIGN.md` § VM API: `vm:run_async(cmd, opts?)` returns a
//! Process resource that can be waited on, signalled, or written
//! to. Slice 10 ships the `RunAsync` / `Wait` / `Kill` triple;
//! `StdinWrite`, `StdinClose`, and stream-tap variants land later.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::handle::ProcessHandle;

use super::exec::ExecOk;
use super::OpResult;

/// Arguments for `RunAsync`. Mirrors [`super::exec::ExecArgs`]
/// minus the synchronous-only fields.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunAsyncArgs {
    /// Program to run.
    pub cmd: String,

    /// Positional arguments.
    #[serde(default)]
    pub args: Vec<String>,

    /// Environment overrides.
    #[serde(default)]
    pub env: BTreeMap<String, String>,

    /// Empty-environment + `env` overlay.
    #[serde(default)]
    pub env_clear: bool,

    /// Working directory. `None` inherits the agent's cwd.
    #[serde(default)]
    pub cwd: Option<String>,
}

/// Successful `RunAsync` returns a [`ProcessHandle`] usable in
/// subsequent `Wait` / `Kill` ops.
pub type RunAsyncResult = OpResult<ProcessHandle>;

/// Arguments for `Wait`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WaitArgs {
    /// Handle returned by an earlier `RunAsync`.
    pub handle: ProcessHandle,
    /// Wall-clock wait cap. The agent kills with SIGKILL on
    /// timeout, captures whatever stdout/stderr accumulated, and
    /// returns [`super::exec::ExitStatus::TimedOut`]. `None` waits
    /// forever.
    #[serde(default)]
    pub timeout_ms: Option<u64>,
}

/// `Wait` reuses [`ExecOk`] for the success payload — same
/// status/stdout/stderr triple as the synchronous `Exec`.
pub type WaitResult = OpResult<ExecOk>;

/// Arguments for `Kill`. Sends `signal` to the process.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct KillArgs {
    /// Handle from `RunAsync`.
    pub handle: ProcessHandle,
    /// POSIX signal number (e.g. 15 = SIGTERM, 9 = SIGKILL).
    pub signal: i32,
}

/// `Kill` carries no payload on success — the kill syscall returned.
pub type KillResult = OpResult<()>;

/// `GetPid` — fetch the kernel-level PID of an in-flight async
/// process. The returned PID is whatever `Child::id()` reports
/// inside the agent (i.e. the guest's view; the host can't see
/// guest pids directly). Used by `proc:pid()` so test code can
/// `assert_eq(proc:pid(), expected_pid_from_ps)` against the
/// actual process number rather than the provium handle counter.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct GetPidArgs {
    /// Handle from `RunAsync`.
    pub handle: ProcessHandle,
}

/// `GetPid` payload: the kernel PID, or [`crate::wire::OsError`]
/// if the handle is unknown / the child already reaped.
pub type GetPidResult = OpResult<u32>;

#[cfg(test)]
mod tests {
    use super::*;

    fn round_trip<T>(value: &T) -> T
    where
        T: Serialize + serde::de::DeserializeOwned,
    {
        let bytes = rmp_serde::to_vec_named(value).unwrap();
        rmp_serde::from_slice(&bytes).unwrap()
    }

    #[test]
    fn run_async_args_round_trips() {
        let args = RunAsyncArgs {
            cmd: "sleep".into(),
            args: vec!["10".into()],
            env: Default::default(),
            env_clear: false,
            cwd: None,
        };
        assert_eq!(args, round_trip(&args));
    }

    #[test]
    fn wait_args_round_trip() {
        let args = WaitArgs {
            handle: ProcessHandle::new(7),
            timeout_ms: Some(5000),
        };
        assert_eq!(args, round_trip(&args));
    }

    #[test]
    fn kill_args_round_trip() {
        let args = KillArgs {
            handle: ProcessHandle::new(3),
            signal: 9,
        };
        assert_eq!(args, round_trip(&args));
    }
}
