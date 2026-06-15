//! `exec` — run a command synchronously to completion.
//!
//! The composite "spawn + wait + collect output" op. Maps to
//! `vm:run(cmd, opts?)` in the Lua API. The async sibling
//! (`run_async` / `Process` resource) is a separate op family and lives
//! in `process.rs` (added incrementally).

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::error::OsError;

/// Arguments for the `Exec` op.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExecArgs {
    /// Program to run. Resolved against `PATH` if not absolute.
    pub cmd: String,

    /// Positional arguments. The agent does not split on whitespace —
    /// the host sends them already split.
    #[serde(default)]
    pub args: Vec<String>,

    /// Environment overrides. Merged onto the agent's default
    /// environment unless [`Self::env_clear`] is set.
    #[serde(default)]
    pub env: BTreeMap<String, String>,

    /// If true, start with an empty environment and apply only
    /// [`Self::env`]. Mirrors `std::process::Command::env_clear`.
    #[serde(default)]
    pub env_clear: bool,

    /// Bytes piped to stdin. Empty for no stdin input.
    #[serde(default, with = "serde_bytes")]
    pub stdin: Vec<u8>,

    /// Working directory. `None` inherits the agent's cwd.
    #[serde(default)]
    pub cwd: Option<String>,

    /// Wall-clock timeout in milliseconds. The agent kills the process
    /// (SIGKILL after a short SIGTERM grace) if it has not exited in
    /// this time and surfaces [`ExitStatus::TimedOut`]. `None` waits
    /// indefinitely.
    #[serde(default)]
    pub timeout_ms: Option<u64>,
}

/// Response payload for the `Exec` op.
///
/// Distinct from [`crate::wire::ops::OpResult`] because exec has a third
/// outcome — timeout — that isn't representable as an [`OsError`].
/// `Ok` covers every case where the process actually ran (whether it
/// exited cleanly, was killed by a signal, or hit the configured
/// timeout); `Err` covers the agent failing to start the command at all.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "outcome", content = "value", rename_all = "snake_case")]
pub enum ExecResult {
    /// The agent successfully launched the process; it has now finished
    /// in some state.
    Ok(ExecOk),
    /// The agent could not launch the process (typically `ENOENT` or
    /// `EACCES` from the underlying execve).
    Err(OsError),
}

/// Successful-launch payload — the process ran and finished, or was
/// killed for timeout.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExecOk {
    /// How the process terminated.
    pub status: ExitStatus,

    /// Bytes captured from the process's stdout. Truncated to
    /// `stdout_at_kill` if the process timed out.
    #[serde(with = "serde_bytes")]
    pub stdout: Vec<u8>,

    /// Bytes captured from the process's stderr.
    #[serde(with = "serde_bytes")]
    pub stderr: Vec<u8>,
}

/// How a process terminated.
///
/// All three outcomes are observable in `ExecOk` — the process *did*
/// run; this enum just describes what happened to it. A test that
/// expects "exited cleanly with code 0" matches `ExitStatus::Exited(0)`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "value", rename_all = "snake_case")]
pub enum ExitStatus {
    /// Normal exit; carries the exit code (0..=255 in practice).
    Exited(i32),
    /// Killed by a signal; carries the signal number (POSIX numbering).
    Signalled(i32),
    /// Killed by the agent because [`ExecArgs::timeout_ms`] elapsed.
    TimedOut,
}

impl ExitStatus {
    /// Returns the exit code if the process exited normally with a
    /// known code. Convenience for the common test pattern
    /// `result:assert_ok()` which checks `exit_code == 0`.
    pub fn exit_code(self) -> Option<i32> {
        match self {
            Self::Exited(code) => Some(code),
            _ => None,
        }
    }

    /// Returns `true` if the process exited normally with code 0.
    pub fn is_clean_exit(self) -> bool {
        self.exit_code() == Some(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn args_round_trip_with_minimal_payload() {
        let args = ExecArgs {
            cmd: "ls".into(),
            args: vec!["-la".into()],
            env: BTreeMap::new(),
            env_clear: false,
            stdin: vec![],
            cwd: None,
            timeout_ms: None,
        };
        let bytes = rmp_serde::to_vec_named(&args).unwrap();
        let decoded: ExecArgs = rmp_serde::from_slice(&bytes).unwrap();
        assert_eq!(args, decoded);
    }

    #[test]
    fn args_round_trip_with_full_payload() {
        let mut env = BTreeMap::new();
        env.insert("FOO".into(), "bar".into());
        let args = ExecArgs {
            cmd: "/usr/bin/cat".into(),
            args: vec!["/etc/hostname".into()],
            env,
            env_clear: true,
            stdin: vec![1, 2, 3],
            cwd: Some("/tmp".into()),
            timeout_ms: Some(5_000),
        };
        let bytes = rmp_serde::to_vec_named(&args).unwrap();
        let decoded: ExecArgs = rmp_serde::from_slice(&bytes).unwrap();
        assert_eq!(args, decoded);
    }

    #[test]
    fn result_ok_round_trips() {
        let r = ExecResult::Ok(ExecOk {
            status: ExitStatus::Exited(0),
            stdout: b"hello\n".to_vec(),
            stderr: vec![],
        });
        let bytes = rmp_serde::to_vec_named(&r).unwrap();
        let decoded: ExecResult = rmp_serde::from_slice(&bytes).unwrap();
        assert_eq!(r, decoded);
    }

    #[test]
    fn result_signalled_round_trips() {
        let r = ExecResult::Ok(ExecOk {
            status: ExitStatus::Signalled(9),
            stdout: vec![],
            stderr: b"killed".to_vec(),
        });
        let bytes = rmp_serde::to_vec_named(&r).unwrap();
        let decoded: ExecResult = rmp_serde::from_slice(&bytes).unwrap();
        assert_eq!(r, decoded);
    }

    #[test]
    fn result_timed_out_round_trips() {
        let r = ExecResult::Ok(ExecOk {
            status: ExitStatus::TimedOut,
            stdout: b"partial".to_vec(),
            stderr: vec![],
        });
        let bytes = rmp_serde::to_vec_named(&r).unwrap();
        let decoded: ExecResult = rmp_serde::from_slice(&bytes).unwrap();
        assert_eq!(r, decoded);
    }

    #[test]
    fn result_err_round_trips() {
        let r = ExecResult::Err(OsError::from_errno(2));
        let bytes = rmp_serde::to_vec_named(&r).unwrap();
        let decoded: ExecResult = rmp_serde::from_slice(&bytes).unwrap();
        assert_eq!(r, decoded);
    }

    #[test]
    fn exit_status_predicates() {
        assert!(ExitStatus::Exited(0).is_clean_exit());
        assert!(!ExitStatus::Exited(1).is_clean_exit());
        assert!(!ExitStatus::Signalled(9).is_clean_exit());
        assert!(!ExitStatus::TimedOut.is_clean_exit());

        assert_eq!(ExitStatus::Exited(7).exit_code(), Some(7));
        assert_eq!(ExitStatus::Signalled(9).exit_code(), None);
        assert_eq!(ExitStatus::TimedOut.exit_code(), None);
    }
}
