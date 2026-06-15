//! `Ioctl` op handler — dispatches `ioctl(fd, cmd, arg)` against
//! a previously-opened file. The `arg` buffer is passed by-pointer
//! to the kernel and read back on return.

use std::os::unix::io::AsRawFd;
use std::sync::Arc;

use provium_protocol::wire::{
    AgentError, AgentErrorKind, AgentMessage, IoctlArgs, IoctlOk, OpResult,
};

use crate::state::AgentState;

use super::os_error_from_io;

/// Run an ioctl. The primary `data` buffer is shared with the
/// kernel for the duration of the call. Auxiliary `bufs` are
/// allocated on the heap; their addresses are spliced into `data`
/// at the byte offsets the host requested via `ptr_offsets`.
pub fn ioctl(args: IoctlArgs, state: &Arc<AgentState>) -> AgentMessage {
    let mut buf = args.data;
    let mut bufs_owned: Vec<Vec<u8>> = args.bufs;
    // Splice each auxiliary buffer's address into `buf` at the
    // requested offset. Host is responsible for getting the offset
    // right — this is Layer 0.
    for (i, &offset) in args.ptr_offsets.iter().enumerate() {
        let off = offset as usize;
        if let Some(aux) = bufs_owned.get_mut(i) {
            let ptr_bytes = (aux.as_mut_ptr() as usize).to_ne_bytes();
            if off + ptr_bytes.len() <= buf.len() {
                buf[off..off + ptr_bytes.len()].copy_from_slice(&ptr_bytes);
            }
        }
    }
    let outcome = state.with_file_mut(args.handle, |file| {
        let fd = file.as_raw_fd();
        // SAFETY: see slice-11.6 notes — Layer-0 op by design.
        let ret = unsafe {
            libc::ioctl(
                fd,
                args.cmd as libc::Ioctl,
                buf.as_mut_ptr() as *mut libc::c_void,
            )
        };
        if ret < 0 {
            let err = std::io::Error::last_os_error();
            return OpResult::Err(os_error_from_io(err));
        }
        OpResult::Ok(IoctlOk {
            ret: ret as i64,
            out_data: std::mem::take(&mut buf),
            out_bufs: std::mem::take(&mut bufs_owned),
        })
    });
    match outcome {
        Some(r) => AgentMessage::IoctlResult(r),
        None => AgentMessage::AgentError(AgentError {
            kind: AgentErrorKind::UnknownHandle,
            message: format!("ioctl on {}", args.handle),
        }),
    }
}
