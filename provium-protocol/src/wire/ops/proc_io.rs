//! Stdin / status ops for async processes.

use serde::{Deserialize, Serialize};

use crate::handle::ProcessHandle;

use super::OpResult;

/// Arguments for `ProcStdinWrite`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProcStdinWriteArgs {
    /// Async-process handle from `RunAsync`.
    pub handle: ProcessHandle,
    /// Bytes to write to the child's stdin.
    #[serde(with = "serde_bytes")]
    pub data: Vec<u8>,
}

/// `ProcStdinWrite` success — bytes actually written.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProcStdinWriteOk {
    /// Bytes successfully written.
    pub written: u64,
}

/// `ProcStdinWrite` payload.
pub type ProcStdinWriteResult = OpResult<ProcStdinWriteOk>;

/// Arguments for `ProcStdinClose`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProcStdinCloseArgs {
    /// Process handle.
    pub handle: ProcessHandle,
}

/// `ProcStdinClose` payload.
pub type ProcStdinCloseResult = OpResult<()>;

/// Arguments for `ProcStatus`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProcStatusArgs {
    /// Process handle.
    pub handle: ProcessHandle,
}

/// Process state at the time of the query.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProcessLiveStatus {
    /// Process is still running.
    Running,
    /// Process has exited (or been killed) — wait for the actual
    /// status code via `Wait`.
    Exited,
    /// Process slot is unknown (already waited / never existed).
    Unknown,
}

/// `ProcStatus` payload.
pub type ProcStatusResult = OpResult<ProcessLiveStatus>;

/// Which stream to open against the process.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProcStreamChannel {
    /// stdout
    Stdout,
    /// stderr
    Stderr,
}

/// Arguments for `ProcStream` — open a streaming subscription to a
/// running process's captured stdout or stderr.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProcStreamArgs {
    /// Process handle from `RunAsync`.
    pub handle: ProcessHandle,
    /// Which channel to subscribe to.
    pub channel: ProcStreamChannel,
}

/// Open-acknowledge for [`ProcStreamArgs`]. Followed by stream frames
/// (the same `StreamFrame` / `StreamEnd` pair as [`super::TailFileArgs`])
/// if `Ok`.
pub type ProcStreamResult = OpResult<()>;

#[cfg(test)]
mod tests {
    use super::*;

    fn rt<T>(v: &T) -> T
    where
        T: Serialize + serde::de::DeserializeOwned,
    {
        rmp_serde::from_slice(&rmp_serde::to_vec_named(v).unwrap()).unwrap()
    }

    #[test]
    fn args_round_trip() {
        let a = ProcStdinWriteArgs {
            handle: ProcessHandle::new(3),
            data: vec![1, 2, 3],
        };
        assert_eq!(a, rt(&a));
        let b = ProcStdinCloseArgs { handle: ProcessHandle::new(5) };
        assert_eq!(b, rt(&b));
        let c = ProcStatusArgs { handle: ProcessHandle::new(7) };
        assert_eq!(c, rt(&c));
        let s = ProcessLiveStatus::Running;
        assert_eq!(s, rt(&s));
    }
}
