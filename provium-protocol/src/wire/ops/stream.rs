//! Streaming ops — the open-acknowledge half. The actual frame
//! payloads live in [`crate::wire::stream`] (`StreamFrame`,
//! `StreamEnd`).
//!
//! A streaming op is a two-phase exchange:
//!
//! 1. Host sends the open request (e.g. [`TailFileArgs`]).
//! 2. Agent sends the open-result. If [`OpResult::Ok`], the connection
//!    stays open and the agent pushes [`crate::wire::stream::StreamFrame`]
//!    messages until source EOF (followed by
//!    [`crate::wire::stream::StreamEnd`]) or the host disconnects.

use serde::{Deserialize, Serialize};

use super::OpResult;

// ---------------------------------------------------------------------------
// tail_file
// ---------------------------------------------------------------------------

/// Arguments for the `TailFile` op — the canonical streaming op.
///
/// Equivalent to `tail -f` with a configurable starting offset. The
/// agent opens the file, optionally seeks, then emits frames as bytes
/// are appended. Frame data is the raw file bytes; line buffering is
/// the host's job (mlua's stream:read_until / :next handle that).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TailFileArgs {
    /// Filesystem path. The file is opened read-only.
    pub path: String,
    /// Where to start reading from.
    #[serde(default)]
    pub start: TailStart,
}

/// Start position for a [`TailFileArgs`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "value", rename_all = "snake_case")]
pub enum TailStart {
    /// Start at byte 0; replay all existing content.
    Beginning,
    /// Start at the end-of-file at open time; emit only new appends.
    /// This is the default — matches the typical "watch a log file"
    /// expectation.
    #[default]
    End,
    /// Start at a specific byte offset.
    Offset(u64),
}

/// Open-acknowledge for [`TailFileArgs`]. Followed by stream frames if
/// `Ok`.
pub type TailFileResult = OpResult<()>;

// ---------------------------------------------------------------------------
// fd_stream
// ---------------------------------------------------------------------------

/// Arguments for `FdStream` — a streaming subscription to bytes
/// produced by an already-open file handle. Equivalent to a tight
/// `read(2)` loop until EOF.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FdStreamArgs {
    /// File handle from a prior [`super::OpenFileArgs`].
    pub handle: crate::handle::FileHandle,
}

/// Open-acknowledge for [`FdStreamArgs`]. Followed by frames if `Ok`.
pub type FdStreamResult = OpResult<()>;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::OsError;

    fn round_trip<T>(value: &T) -> T
    where
        T: serde::Serialize + serde::de::DeserializeOwned,
    {
        let bytes = rmp_serde::to_vec_named(value).unwrap();
        rmp_serde::from_slice(&bytes).unwrap()
    }

    #[test]
    fn tail_args_round_trip_with_each_start_variant() {
        for start in [
            TailStart::Beginning,
            TailStart::End,
            TailStart::Offset(4096),
        ] {
            let args = TailFileArgs {
                path: "/var/log/messages".into(),
                start,
            };
            assert_eq!(args, round_trip(&args));
        }
    }

    #[test]
    fn tail_start_defaults_to_end() {
        assert_eq!(TailStart::default(), TailStart::End);
    }

    #[test]
    fn tail_result_round_trips() {
        let ok: TailFileResult = OpResult::Ok(());
        assert_eq!(ok, round_trip(&ok));

        let err: TailFileResult = OpResult::Err(OsError::from_errno(2));
        assert_eq!(err, round_trip(&err));
    }
}
