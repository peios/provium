//! PID-1 init duties.
//!
//! When the agent is launched as `/init` from an initrd it inherits
//! PID 1, which carries Linux-specific responsibilities the kernel
//! does not perform on its own:
//!
//! * Mount the pseudo-filesystems guests expect (`/proc`, `/sys`,
//!   `/dev`, `/tmp`).
//! * (Future) Reap orphaned grandchildren via `SIGCHLD` —
//!   `std::process::Child::wait` covers immediate descendants but
//!   anything daemonised by a `vm:run` shell becomes our problem.
//!   Deferred: tests don't currently exercise that path.
//!
//! When the agent is *not* PID 1 — running on a real Peios install
//! where `peinit` is PID 1 and forks us, or on Linux with a normal
//! init system — these duties have already been done by someone
//! else, so [`run_if_init`] is a no-op.
//!
//! ## Overlay-injection chain
//!
//! When provium injects this binary as `/sbin/provium-agent` into a
//! user-supplied initrd via `rdinit=/sbin/provium-agent`, we end up
//! as PID 1 alongside the user's original `/init`. After mounts we
//! `fork()`: the child returns to the agent listener loop, the
//! parent `exec`s `/init` so the user's init logic runs as PID 1
//! and we run as a child. Without the chain, the user's userspace
//! never starts and the only thing alive in the VM is the agent.

use std::ffi::CString;
use std::io;
use std::path::Path;

/// If we're PID 1, perform the init mount sequence and (when running
/// from an injected overlay alongside a user `/init`) chain to it.
/// Errors are logged to stderr but never abort the agent — a missing
/// pseudo-FS or a missing `/init` is inconvenient but not fatal.
pub fn run_if_init() {
    // SAFETY: getpid is async-signal-safe and always defined on the
    // platforms we target (Linux). The result is a plain integer.
    let pid = unsafe { libc::getpid() };
    if pid != 1 {
        return;
    }
    // Two distinct PID-1 scenarios:
    //
    // * **Chain mode** — a real downstream init exists at `/init`
    //   (overlay-injection alongside prelude/peinit). That init owns
    //   the mount setup and may `MS_MOVE`/pivot the kernel vfs into a
    //   new root. If we pre-mount `/proc` etc. here, our plain mounts
    //   stack on the mountpoints and the downstream init's `MS_MOVE`
    //   fails `EBUSY`, halting the boot. So we mount *nothing* and hand
    //   straight off — the real init does it correctly, and because
    //   we share its mount namespace the listener sees the result.
    //
    // * **Standalone mode** — no user `/init` (agent-only initrd). We
    //   *are* the whole system, so we must perform init's mount duties
    //   ourselves before dropping into the listener.
    if user_init_present() {
        chain_to_user_init();
    } else {
        perform_init_mounts();
    }
}

/// Whether a real downstream init exists at `/init` that we should
/// chain to. False when `/init` is absent, or when it canonicalises to
/// the agent binary itself (the pre-overlay layout placed the agent at
/// `/init` — chaining to ourselves would be an exec loop).
fn user_init_present() -> bool {
    let init_path = Path::new("/init");
    if !init_path.exists() {
        return false;
    }
    if let (Ok(self_path), Ok(init_canon)) =
        (std::env::current_exe(), init_path.canonicalize())
    {
        if self_path == init_canon {
            return false;
        }
    }
    true
}

/// Fork and hand off to the user's `/init`: the child returns from this
/// function (and continues into the agent's listener loop), the parent
/// `exec`s `/init`, taking over PID 1. The caller must have confirmed
/// via [`user_init_present`] that `/init` is a real, non-self init.
fn chain_to_user_init() {
    // SAFETY: fork is async-signal-safe; result is a normal pid_t.
    let pid = unsafe { libc::fork() };
    if pid < 0 {
        eprintln!(
            "provium-agent (init): fork failed: {}",
            io::Error::last_os_error(),
        );
        return;
    }
    if pid == 0 {
        // Child: continue as the agent listener.
        return;
    }
    // Parent: exec /init. argv[0] = "/init", env preserved.
    let init_c = CString::new("/init").expect("/init has no NUL");
    let argv: [*const libc::c_char; 2] = [init_c.as_ptr(), std::ptr::null()];
    // SAFETY: argv is a NULL-terminated array of valid pointers to
    // NUL-terminated C strings; init_c lives until execv returns
    // (which it only does on failure).
    unsafe {
        libc::execv(init_c.as_ptr(), argv.as_ptr());
    }
    // execv only returns on failure. Don't return to caller — the
    // caller is the agent's main(), and falling back into the
    // listener loop while a child is already running it would leave
    // two listeners fighting for the vsock port. Better to abort
    // and let the harness see the boot fail with a clear signal.
    eprintln!(
        "provium-agent (init): exec /init failed: {}",
        io::Error::last_os_error(),
    );
    std::process::exit(1);
}

/// Mount each of the standard pseudo-FSes. Order matters slightly —
/// `/dev` is needed before processes spawn in case they expect
/// `/dev/null` etc., so we mount it early.
fn perform_init_mounts() {
    for spec in INIT_MOUNTS {
        if let Err(e) = mount(spec) {
            eprintln!(
                "provium-agent (init): mount {} -> {}: {e}",
                spec.fs_type, spec.target
            );
        }
    }
}

struct MountSpec {
    /// Source argument to `mount(2)` — for pseudo-FSes this is just
    /// a label.
    source: &'static str,
    /// Target mountpoint inside the initrd.
    target: &'static str,
    /// Filesystem type.
    fs_type: &'static str,
    /// `mount(2)` flags. 0 = defaults.
    flags: libc::c_ulong,
}

const INIT_MOUNTS: &[MountSpec] = &[
    MountSpec {
        source: "devtmpfs",
        target: "/dev",
        fs_type: "devtmpfs",
        flags: 0,
    },
    MountSpec {
        source: "proc",
        target: "/proc",
        fs_type: "proc",
        flags: 0,
    },
    MountSpec {
        source: "sysfs",
        target: "/sys",
        fs_type: "sysfs",
        flags: 0,
    },
    MountSpec {
        source: "tmpfs",
        target: "/tmp",
        fs_type: "tmpfs",
        flags: 0,
    },
];

fn mount(spec: &MountSpec) -> io::Result<()> {
    let source = CString::new(spec.source).expect("source has no NUL");
    let target = CString::new(spec.target).expect("target has no NUL");
    let fs_type = CString::new(spec.fs_type).expect("fs_type has no NUL");

    // SAFETY: every pointer is a valid NUL-terminated C string for
    // the duration of the call (the CStrings live to the end of this
    // scope). The data argument is null which is valid for every
    // pseudo-FS we list above.
    let r = unsafe {
        libc::mount(
            source.as_ptr(),
            target.as_ptr(),
            fs_type.as_ptr(),
            spec.flags,
            std::ptr::null(),
        )
    };
    if r != 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}
