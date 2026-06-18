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

/// One nested-pointer splice: write `bufs[child]`'s in-agent address
/// into `bufs[parent]` at byte `offset` (an 8-byte native-endian
/// pointer), applied *before* the arg-slot `ptrs`. This lets a buffer
/// carry a pointer to another buffer — e.g. a struct argument whose
/// field must point at a caller-supplied output buffer (the QUERY
/// ioctl, `kacs_access_check`, etc.). `parent`/`child` are 0-based
/// indices into [`SyscallArgs::bufs`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct NestedPtr {
    /// 0-based index into `bufs` of the buffer whose bytes are patched.
    pub parent: u8,
    /// 0-based index into `bufs` of the buffer whose address is written.
    pub child: u8,
    /// Byte offset within `bufs[parent]` to write the 8-byte pointer.
    pub offset: u32,
}

/// Arguments for a 6-arg syscall, optionally with pointer buffers.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
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
    /// Nested-pointer splices applied *before* `ptrs` — see
    /// [`NestedPtr`]. Lets one buffer point at another.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub nested: Vec<NestedPtr>,
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
            nested: Vec::new(),
        };
        let bytes = rmp_serde::to_vec_named(&a).unwrap();
        let back: SyscallArgs = rmp_serde::from_slice(&bytes).unwrap();
        assert_eq!(a, back);
    }

    #[test]
    fn args_round_trip_with_nested() {
        let a = SyscallArgs {
            nr: 16,
            args: [7, 0xC0104B00, 0, 0, 0, 0],
            bufs: vec![vec![0u8; 16], vec![0u8; 64]],
            ptrs: vec![2],
            nested: vec![NestedPtr { parent: 0, child: 1, offset: 8 }],
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
