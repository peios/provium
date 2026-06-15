//! Layer-0 `ioctl(2)`. Variable-args wrapping is more involved than
//! plain syscall: ioctls take `(fd, cmd, void* arg)` and the agent
//! has to materialise `arg` as raw bytes the kernel can read/write.
//!
//! Slice 11.6 minimum: a single optional in/out byte buffer.
//! Pointer-typed args (e.g. `arg` itself being a struct holding
//! pointers) are out of scope — typical config-file ioctls are
//! flat structs the agent hands to the kernel.

use serde::{Deserialize, Serialize};

use crate::handle::FileHandle;

use super::OpResult;

/// Arguments for `Ioctl`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct IoctlArgs {
    /// Open-file handle the ioctl targets.
    pub handle: FileHandle,
    /// `cmd` argument (the ioctl request code).
    pub cmd: u64,
    /// Optional in/out byte buffer used as the third arg to
    /// `ioctl(fd, cmd, &data[0])`. Returned as `out_data`.
    #[serde(default, with = "serde_bytes")]
    pub data: Vec<u8>,
    /// Additional in/out byte buffers — design's `ptrs` model. Each
    /// buffer's address is spliced into a parallel `ptr_offsets[i]`
    /// byte offset within the primary `data` buffer, so structs
    /// holding pointer fields can be marshalled in one shot.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub bufs: Vec<Vec<u8>>,
    /// Byte offsets within `data` to overwrite with `bufs[i]`'s
    /// address. Same length as `bufs`. Each offset is treated as
    /// `usize`-aligned.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub ptr_offsets: Vec<u32>,
}

/// `Ioctl` success-payload.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct IoctlOk {
    /// Raw kernel return value.
    pub ret: i64,
    /// Buffer post-ioctl. Same length as the input.
    #[serde(with = "serde_bytes")]
    pub out_data: Vec<u8>,
    /// Auxiliary buffer contents post-ioctl, paired by index with
    /// `IoctlArgs::bufs`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub out_bufs: Vec<Vec<u8>>,
}

/// `Ioctl` payload.
pub type IoctlResult = OpResult<IoctlOk>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip() {
        let a = IoctlArgs {
            handle: FileHandle::new(7),
            cmd: 0x4000_0000,
            data: vec![1, 2, 3],
            bufs: Vec::new(),
            ptr_offsets: Vec::new(),
        };
        let b: IoctlArgs = rmp_serde::from_slice(&rmp_serde::to_vec_named(&a).unwrap()).unwrap();
        assert_eq!(a, b);
    }
}
