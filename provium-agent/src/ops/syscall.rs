//! Layer-0 raw syscall dispatch.
//!
//! 6-arg integer form: hands `nr` + `args` to `libc::syscall`,
//! reports `(ret, errno)`. With `bufs` + `ptrs`, the agent
//! allocates each input buffer in heap-owned `Vec<u8>`, replaces
//! the indicated `args[ptrs[i]]` slot with the buffer's pointer,
//! invokes the syscall, then copies the buffer contents back into
//! `out_bufs` paired by index.

use provium_protocol::wire::{AgentMessage, SyscallArgs, SyscallResult};

/// Run the requested syscall and return its `(ret, errno, out_bufs)`.
pub fn syscall(mut args: SyscallArgs) -> AgentMessage {
    // Set up heap-owned buffers for any pointer-typed args. Each
    // buffer is held in `bufs_owned` until the syscall returns so
    // its address remains valid.
    let mut bufs_owned: Vec<Vec<u8>> = std::mem::take(&mut args.bufs);
    for (i, ptr_idx) in args.ptrs.iter().copied().enumerate() {
        if let (Some(slot), Some(buf)) = (
            args.args.get_mut(ptr_idx as usize),
            bufs_owned.get_mut(i),
        ) {
            *slot = buf.as_mut_ptr() as i64;
        }
    }
    // SAFETY: libc::syscall is the platform's documented entry
    // point for arbitrary syscalls. The caller is responsible for
    // passing valid arguments — this op is intentionally
    // unsafe-by-design.
    let ret = unsafe {
        libc::syscall(
            args.nr,
            args.args[0],
            args.args[1],
            args.args[2],
            args.args[3],
            args.args[4],
            args.args[5],
        )
    };
    let errno = if ret < 0 {
        std::io::Error::last_os_error()
            .raw_os_error()
            .unwrap_or(0)
    } else {
        0
    };
    AgentMessage::SyscallResult(SyscallResult {
        ret,
        errno,
        out_bufs: bufs_owned,
    })
}
