//! Cross-cutting helpers shared by op handlers.

use std::io;

use provium_protocol::OsError;

/// Map a [`std::io::Error`] to the wire-shaped [`OsError`].
///
/// The `io::Error` may have been built from a raw OS errno (typical) or
/// fabricated by std (`ErrorKind::InvalidInput`, etc.). For the latter,
/// errno is `0` — the host's error rendering knows to suppress the
/// "errno N" suffix when N is 0.
pub(crate) fn os_error_from_io(e: io::Error) -> OsError {
    OsError {
        errno: e.raw_os_error().unwrap_or(0),
        message: e.to_string(),
    }
}

/// Signal a spawned child *and* everything it spawned.
///
/// Children spawned for `Exec` and `RunAsync` are put in their own
/// process group (`Command::process_group(0)`), so the group id equals
/// the child's pid and `kill(-pid)` reaches the whole tree. That is
/// what a timeout needs: a shell that forks rather than execs its
/// command leaves the real worker running as our grandchild, and
/// signalling the pid alone leaves it holding the stdout/stderr pipes
/// open, so the capture threads never see EOF and the op blocks until
/// the runaway finishes on its own.
///
/// The direct pid is signalled too, so the kill still lands if the
/// child is not the leader of its group (it called `setsid` for
/// itself, say). `ESRCH` from an already-reaped pid is fine to ignore;
/// so is a group that no longer has members.
pub(crate) fn kill_process_group(pid: u32, signal: i32) {
    let pid = pid as libc::pid_t;
    // A pid of 0 would signal *our own* process group and 1 is init;
    // neither is ever a `Child::id()`, but the negation below makes
    // the mistake unrecoverable, so refuse it.
    if pid <= 1 {
        return;
    }
    // SAFETY: `pid` came from an unreaped `Child`, so it names a
    // process we own; a negative pid names that process's group.
    // `libc::kill` is documented to take either, and reports failures
    // (ESRCH in particular) through its return value rather than by
    // any unsafe effect.
    unsafe {
        libc::kill(-pid, signal);
        libc::kill(pid, signal);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_real_errno() {
        let e = io::Error::from_raw_os_error(2);
        let os = os_error_from_io(e);
        assert_eq!(os.errno, 2);
        assert!(!os.message.is_empty());
    }

    #[test]
    fn maps_synthesised_error() {
        let e = io::Error::new(io::ErrorKind::InvalidInput, "bad");
        let os = os_error_from_io(e);
        assert_eq!(os.errno, 0);
        assert_eq!(os.message, "bad");
    }
}
