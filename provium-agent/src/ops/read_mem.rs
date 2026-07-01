//! Guest-memory read handler — the agent-side counterpart of
//! [`provium_protocol::wire::ops::read_mem`].
//!
//! Reads the agent's *own* address space through `/proc/self/mem`. The
//! key property is fault-safety: `read_exact_at` is a `pread` against
//! the mem file (whose offset *is* the virtual address), so an unmapped
//! or unreadable range comes back as an `EIO`/`EFAULT` error instead of
//! delivering `SIGSEGV` to the agent. That lets a test point `read_mem`
//! at any address — e.g. a page it `mmap`'d for a KMES ring — without
//! risking the agent process.

use std::io;
use std::os::unix::fs::FileExt;

use provium_protocol::wire::{AgentMessage, OpResult, ReadMemArgs, ReadMemOk};

use super::os_error_from_io;

/// Upper bound on a single read, guarding against a bogus `len`
/// triggering a huge allocation. Comfortably above a default KMES ring
/// mapping (`8192 + 2 * 4 MiB`).
const MAX_READ_LEN: usize = 64 * 1024 * 1024;

/// `ReadMem` — copy `len` bytes from agent-virtual address `addr`.
pub fn read_mem(args: ReadMemArgs) -> AgentMessage {
    let len = args.len as usize;
    if len > MAX_READ_LEN {
        return AgentMessage::ReadMemResult(OpResult::Err(
            provium_protocol::OsError::from_errno(libc::EINVAL),
        ));
    }
    AgentMessage::ReadMemResult(match read_self_mem(args.addr, len) {
        Ok(bytes) => OpResult::Ok(ReadMemOk { bytes }),
        Err(e) => OpResult::Err(os_error_from_io(e)),
    })
}

/// `pread` `len` bytes at virtual address `addr` from `/proc/self/mem`.
fn read_self_mem(addr: u64, len: usize) -> io::Result<Vec<u8>> {
    let mem = std::fs::File::open("/proc/self/mem")?;
    let mut buf = vec![0u8; len];
    mem.read_exact_at(&mut buf, addr)?;
    Ok(buf)
}
