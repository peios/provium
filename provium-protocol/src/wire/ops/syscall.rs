//! Layer-0 raw syscall.
//!
//! `Syscall` is the maximally-portable lowest-level op. The agent
//! invokes the platform's `syscall()` with the given number + 6
//! integer arguments and reports the return value + errno.
//!
//! Slice-11.5 minimum: integer args only. The full design supports
//! input/output byte buffers and pointer-typed arguments — these
//! need careful wire marshalling and are slice-11.6.

use serde::{Deserialize, Serialize};

/// Arguments for a 6-arg integer-only syscall.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SyscallArgs {
    /// Syscall number (Linux ABI on the v1 Peios port).
    pub nr: i64,
    /// Six integer arguments. Unused trailing slots are `0`.
    pub args: [i64; 6],
    /// Optional input byte buffers. Each entry is paired with a
    /// `ptrs[i]` index pointing to the `args` slot whose value
    /// should be replaced with the buffer's address. Per
    /// `DESIGN.md` § VM / Layer-0 raw `vm:syscall(nr, args?, bufs?,
    /// ptrs?)`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub bufs: Vec<Vec<u8>>,
    /// Pointer-arg indices. `ptrs[i]` is the `args` slot for
    /// `bufs[i]`'s address.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub ptrs: Vec<u8>,
}

/// Outcome of a syscall. Mirrors POSIX: `ret >= 0` means success;
/// `ret < 0` (typically `-1`) means failure with the cause in
/// `errno`. Carries `out_bufs` for callers that supplied input
/// buffers — same length and order as `SyscallArgs::bufs`, with
/// post-syscall contents.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SyscallResult {
    /// Raw return value.
    pub ret: i64,
    /// `errno` value at the time of return; `0` when `ret >= 0`.
    pub errno: i32,
    /// Buffer contents post-syscall, paired by index with the
    /// request's `bufs`. Empty when no bufs were supplied.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub out_bufs: Vec<Vec<u8>>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn args_round_trip() {
        let a = SyscallArgs {
            nr: 42,
            args: [1, 2, 3, 0, 0, 0],
            bufs: Vec::new(),
            ptrs: Vec::new(),
        };
        let bytes = rmp_serde::to_vec_named(&a).unwrap();
        let back: SyscallArgs = rmp_serde::from_slice(&bytes).unwrap();
        assert_eq!(a, back);
    }

    #[test]
    fn result_round_trip() {
        let r = SyscallResult {
            ret: -1,
            errno: 13,
            out_bufs: Vec::new(),
        };
        let bytes = rmp_serde::to_vec_named(&r).unwrap();
        let back: SyscallResult = rmp_serde::from_slice(&bytes).unwrap();
        assert_eq!(r, back);
    }
}
