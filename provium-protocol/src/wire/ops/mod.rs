//! Per-op request / response types.
//!
//! Each submodule covers a logically-related family of ops. Adding a new
//! op is a localised change: introduce request/response types here, add
//! variants to [`super::HostMessage`] and [`super::AgentMessage`], and
//! the rest of the build pulls the new types in automatically.
//!
//! The shared [`OpResult`] type is the canonical "succeeded with `T` /
//! failed with [`crate::OsError`]" envelope used by every op whose only
//! failure mode is an OS error. Ops with richer failure surfaces (e.g.
//! [`exec::ExecResult`], which also distinguishes timeout) define their
//! own response type.

use serde::{Deserialize, Serialize};

use crate::error::OsError;

pub mod batch;
pub mod clock;
pub mod exec;
pub mod file;
pub mod ioctl;
pub mod proc_io;
pub mod process;
pub mod read_mem;
pub mod stream;
pub mod syscall;
pub mod worker;

pub use batch::*;
pub use clock::*;
pub use exec::*;
pub use file::*;
pub use ioctl::*;
pub use proc_io::*;
pub use process::*;
pub use read_mem::*;
pub use stream::*;
pub use syscall::*;
pub use worker::*;

/// Outcome of a wire op whose only failure mode is an OS error.
///
/// On the wire this serializes with an `outcome` tag and a `value`
/// payload — `{"outcome": "ok", "value": ...}` or
/// `{"outcome": "err", "value": <OsError>}`. The shape is uniform across
/// every op that uses it, so host code converting to `Result<T, OsError>`
/// at the binding layer is a one-liner: [`OpResult::into_result`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "outcome", content = "value", rename_all = "snake_case")]
pub enum OpResult<T> {
    /// Op succeeded; carries the success-side payload.
    Ok(T),
    /// Op failed at the OS layer with the given errno + message.
    Err(OsError),
}

impl<T> OpResult<T> {
    /// Convert into a standard [`Result`] for ergonomic `?` propagation.
    pub fn into_result(self) -> Result<T, OsError> {
        match self {
            Self::Ok(t) => Ok(t),
            Self::Err(e) => Err(e),
        }
    }

    /// Returns `true` if this is the [`OpResult::Ok`] variant.
    #[inline]
    pub fn is_ok(&self) -> bool {
        matches!(self, Self::Ok(_))
    }
}

impl<T> From<Result<T, OsError>> for OpResult<T> {
    fn from(value: Result<T, OsError>) -> Self {
        match value {
            Ok(t) => Self::Ok(t),
            Err(e) => Self::Err(e),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn op_result_ok_round_trips() {
        let r: OpResult<u64> = OpResult::Ok(42);
        let bytes = rmp_serde::to_vec_named(&r).unwrap();
        let decoded: OpResult<u64> = rmp_serde::from_slice(&bytes).unwrap();
        assert_eq!(r, decoded);
    }

    #[test]
    fn op_result_err_round_trips() {
        let r: OpResult<u64> = OpResult::Err(OsError::from_errno(13));
        let bytes = rmp_serde::to_vec_named(&r).unwrap();
        let decoded: OpResult<u64> = rmp_serde::from_slice(&bytes).unwrap();
        assert_eq!(r, decoded);
    }

    #[test]
    fn op_result_converts_to_std_result() {
        let ok: OpResult<u64> = OpResult::Ok(7);
        assert_eq!(ok.into_result(), Ok(7));

        let err: OpResult<u64> = OpResult::Err(OsError::from_errno(2));
        assert!(err.into_result().is_err());
    }

    #[test]
    fn op_result_from_std_result() {
        let ok: OpResult<u64> = Result::Ok(7).into();
        assert!(ok.is_ok());

        let err: OpResult<u64> = Result::Err(OsError::from_errno(2)).into();
        assert!(!err.is_ok());
    }
}
