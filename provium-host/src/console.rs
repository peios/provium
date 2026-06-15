//! `provium console <profile>` — boot a profile's VM and hand the
//! controlling terminal its serial console.
//!
//! Where [`crate::repl`] boots a VM and talks to the in-guest agent
//! over vsock to expose a Lua REPL, this command does the opposite:
//! it wires QEMU's serial port + monitor straight to the terminal and
//! gets out of the way, so you see exactly what a hand-rolled
//! `qemu-system-x86_64 -kernel … -initrd …` invocation would show.
//! Useful for poking at a profile's boot, watching kernel messages,
//! or dropping into whatever shell the guest's init starts.
//!
//! `Ctrl-A X` quits QEMU; `Ctrl-A C` toggles the QEMU monitor.
//!
//! The agent overlay is *opt-in* here (`inject_agent`): the whole
//! point is a qemu-direct boot, so by default the profile's initrd is
//! used as-is with no vsock plumbing.

use std::path::PathBuf;
use std::process::{Command, Stdio};

use crate::profile::Config;
use crate::vmm::qemu::{
    build_interactive_qemu_command, InteractiveLaunchPlan, DEFAULT_CPUS,
    DEFAULT_MEMORY_BYTES,
};

/// Options for one [`run`] invocation. Built from the `provium
/// console` CLI flags.
#[derive(Clone, Debug)]
pub struct ConsoleOpts {
    /// Profile name from `provium.toml`.
    pub profile_name: String,
    /// Guest memory cap in bytes. `None` uses the per-VM default.
    pub memory_bytes: Option<u64>,
    /// vCPU count. `None` uses the per-VM default.
    pub cpus: Option<u32>,
    /// Override the profile's kernel cmdline. `None` uses the profile.
    pub cmdline_override: Option<String>,
    /// Inject the provium agent overlay (so the vsock control plane is
    /// up alongside the console). Off by default — a bare boot.
    pub inject_agent: bool,
    /// QEMU binary to exec. `None` uses `qemu-system-x86_64` on PATH.
    pub qemu_binary: Option<PathBuf>,
    /// Emit `merge=on` on the memory backend (KSM-eligible).
    pub ksm_enabled: bool,
    /// Extra arguments forwarded verbatim to QEMU (after `--`).
    pub extra_args: Vec<String>,
    /// Print the assembled command line and exit without booting.
    pub print_command: bool,
}

/// Boot `opts.profile_name` and attach this process's terminal to the
/// guest serial console. Blocks until QEMU exits; returns QEMU's exit
/// code (`0` on a clean guest power-off or `Ctrl-A X`). With
/// `print_command` set, prints the assembled command line and returns
/// `0` without launching anything.
pub fn run(
    opts: ConsoleOpts,
    config: &Config,
) -> Result<i32, Box<dyn std::error::Error>> {
    let profile = config.profile(&opts.profile_name).ok_or_else(|| {
        let available: Vec<&str> =
            config.profiles.keys().map(String::as_str).collect();
        let have = if available.is_empty() {
            "none defined".to_string()
        } else {
            available.join(", ")
        };
        format!(
            "profile `{}` not found in provium.toml (have: {have})",
            opts.profile_name,
        )
    })?;

    // Fail with a clean diagnostic rather than an opaque QEMU exit if
    // the profile points at a kernel/initrd that isn't there.
    if !profile.kernel.exists() {
        return Err(format!(
            "profile `{}`: kernel `{}` does not exist",
            opts.profile_name,
            profile.kernel.display(),
        )
        .into());
    }
    if !profile.initrd.exists() {
        return Err(format!(
            "profile `{}`: initrd `{}` does not exist",
            opts.profile_name,
            profile.initrd.display(),
        )
        .into());
    }

    let memory_bytes = opts.memory_bytes.unwrap_or(DEFAULT_MEMORY_BYTES);
    let cpus = opts.cpus.unwrap_or(DEFAULT_CPUS);
    let cmdline = opts
        .cmdline_override
        .clone()
        .map(Ok)
        .unwrap_or_else(|| profile.resolve_cmdline())?;

    // Agent overlay is opt-in. When asked for, reuse the exact
    // overlay-inject path the test runner uses so the in-guest agent
    // comes up too — and allocate a CID so the builder wires vsock.
    let (initrd_path, cmdline, cid) = if opts.inject_agent {
        let scratch_root = std::env::temp_dir().join("provium-qemu");
        let prepared = crate::vmm::agent_overlay::prepare_initrd(
            profile,
            &cmdline,
            &scratch_root,
        )?;
        let cid = crate::cid::CidAllocator::new().allocate();
        (prepared.initrd_path, prepared.cmdline, Some(cid))
    } else {
        (profile.initrd.clone(), cmdline, None)
    };

    let qemu_binary = opts
        .qemu_binary
        .clone()
        .unwrap_or_else(|| PathBuf::from("qemu-system-x86_64"));

    let plan = InteractiveLaunchPlan {
        qemu_binary: &qemu_binary,
        kernel: &profile.kernel,
        initrd: &initrd_path,
        cmdline: &cmdline,
        memory_bytes,
        cpus,
        cid,
        ksm_enabled: opts.ksm_enabled,
        extra_args: &opts.extra_args,
    };
    let mut command = build_interactive_qemu_command(&plan);

    if opts.print_command {
        println!("{}", render_command(&command));
        return Ok(0);
    }

    eprintln!(
        "provium console — booting profile `{}` (Ctrl-A X quits, Ctrl-A C for monitor)",
        opts.profile_name,
    );

    // Inherit the terminal: QEMU itself puts it into raw mode and
    // restores it on exit, so the experience matches running qemu by
    // hand.
    command
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());

    // PR_SET_PDEATHSIG so a killed provium takes QEMU with it rather
    // than orphaning a VM that still owns the terminal. Mirrors the
    // QemuVmm launch path.
    //
    // SAFETY: the closure only calls prctl(2) with constant args and
    // reports failure via errno — async-signal-safe and standard for
    // a pre_exec hook.
    unsafe {
        use std::os::unix::process::CommandExt;
        command.pre_exec(|| {
            let r = libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL);
            if r != 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }

    let status = command.status().map_err(|e| {
        format!("failed to spawn `{}`: {e}", qemu_binary.display())
    })?;
    // Signal-terminated (no code) → treat as failure.
    Ok(status.code().unwrap_or(1))
}

/// Render a [`Command`] as a copy-pasteable shell-ish line for
/// `--print-command`. Arguments containing whitespace are
/// single-quoted so the printed line round-trips through a shell.
fn render_command(cmd: &Command) -> String {
    let mut parts = vec![cmd.get_program().to_string_lossy().into_owned()];
    for a in cmd.get_args() {
        let s = a.to_string_lossy();
        if s.contains(char::is_whitespace) {
            parts.push(format!("'{s}'"));
        } else {
            parts.push(s.into_owned());
        }
    }
    parts.join(" ")
}
