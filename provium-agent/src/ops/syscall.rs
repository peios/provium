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
    // Every buffer's stable heap address. We only mutate buffer
    // *contents* below (never resize), so these stay valid.
    let addrs: Vec<i64> =
        bufs_owned.iter().map(|b| b.as_ptr() as i64).collect();
    // Nested-pointer splices first: write a child buffer's address
    // into a parent buffer at a byte offset (8-byte native pointer).
    // This lets a struct argument point at another buffer (e.g. the
    // QUERY ioctl's output buffer, or kacs_access_check's sd_ptr).
    for n in &args.nested {
        if let Some(parent) = bufs_owned.get_mut(n.parent as usize) {
            let addr = addrs.get(n.child as usize).copied().unwrap_or(0);
            let bytes = (addr as u64).to_ne_bytes();
            let off = n.offset as usize;
            if off + bytes.len() <= parent.len() {
                parent[off..off + bytes.len()].copy_from_slice(&bytes);
            }
        }
    }
    // Then the arg-slot pointers: replace `args[ptrs[i]]` with
    // `bufs[i]`'s address.
    for (i, ptr_idx) in args.ptrs.iter().copied().enumerate() {
        if let (Some(slot), Some(addr)) =
            (args.args.get_mut(ptr_idx as usize), addrs.get(i))
        {
            *slot = *addr;
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

#[cfg(test)]
mod tests {
    use super::*;
    use provium_protocol::wire::{NestedPtr, SyscallArgs};

    /// A nested splice writes the child buffer's address into the parent
    /// buffer at the requested offset. Uses an invalid syscall number so
    /// the kernel returns ENOSYS and leaves the buffers untouched, letting
    /// us observe the splice in `out_bufs`.
    #[test]
    fn nested_splice_writes_child_address_into_parent() {
        let args = SyscallArgs {
            nr: 0x7fff_ffff, // not a real syscall → ENOSYS
            args: [0; 6],
            bufs: vec![vec![0u8; 16], vec![0u8; 8]],
            ptrs: vec![],
            nested: vec![NestedPtr { parent: 0, child: 1, offset: 4 }],
        };
        let AgentMessage::SyscallResult(r) = syscall(args) else {
            panic!("expected SyscallResult");
        };
        // Parent's bytes 0..4 stay zero; bytes 4..12 hold a nonzero
        // pointer to the child buffer.
        assert_eq!(&r.out_bufs[0][0..4], &[0, 0, 0, 0], "offset must be respected");
        assert!(
            r.out_bufs[0][4..12].iter().any(|&b| b != 0),
            "nested splice should write a nonzero pointer at the offset"
        );
    }
}
