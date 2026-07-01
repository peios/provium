//! Guest-memory read — copy `len` bytes from the agent process's own
//! address space at virtual address `addr`.
//!
//! The agent reads its *own* memory fault-safely (via `/proc/self/mem`),
//! so an unmapped or unreadable address returns an OS error rather than
//! crashing the agent. This lets the host observe memory the agent
//! mapped but never passed as a syscall argument buffer — e.g. the pages
//! of an `mmap`'d ring the kernel writes to directly (KMES).

use serde::{Deserialize, Serialize};

use super::OpResult;

/// `ReadMem` — read `len` bytes at agent-virtual address `addr`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReadMemArgs {
    /// Virtual address in the agent's own address space.
    pub addr: u64,
    /// Number of bytes to read.
    pub len: u32,
}

/// Bytes copied out of the agent's memory.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReadMemOk {
    /// The bytes read from `[addr, addr + len)`. Encoded as msgpack
    /// `bin` (see [`crate::wire::stream::StreamFrame`] for the same
    /// `serde_bytes` rationale).
    #[serde(with = "serde_bytes")]
    pub bytes: Vec<u8>,
}

/// `ReadMem` payload type. `Err` carries the OS error from the failed
/// memory read (e.g. `EIO`/`EFAULT` for an unmapped address).
pub type ReadMemResult = OpResult<ReadMemOk>;

#[cfg(test)]
mod tests {
    use super::*;

    fn round_trip<T>(v: &T) -> T
    where
        T: Serialize + serde::de::DeserializeOwned,
    {
        let bytes = rmp_serde::to_vec_named(v).unwrap();
        rmp_serde::from_slice(&bytes).unwrap()
    }

    #[test]
    fn read_mem_args_round_trips() {
        let a = ReadMemArgs { addr: 0xdead_beef_0000, len: 4096 };
        assert_eq!(a, round_trip(&a));
    }

    #[test]
    fn read_mem_ok_round_trips() {
        let r = ReadMemOk { bytes: vec![1, 2, 3, 4, 0xff] };
        assert_eq!(r, round_trip(&r));
    }
}
