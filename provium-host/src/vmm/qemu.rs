//! QEMU implementation of [`crate::vmm::Vmm`].
//!
//! Spawns one QEMU child process per VM. The lifecycle is:
//!
//! 1. Allocate a CID via the host's [`CidAllocator`].
//! 2. Build a per-VM scratch directory for the QMP socket + console
//!    log.
//! 3. Validate that the profile's `kernel` / `initrd` actually exist
//!    on disk (clean diagnostic vs an opaque QEMU exit).
//! 4. Construct the QEMU command line via [`build_qemu_command`].
//! 5. Spawn the child.
//! 6. Wait for the QMP socket file to appear (filesystem readiness)
//!    then connect [`provium_qmp::Qmp`] (which negotiates capabilities
//!    + enables MIGRATION events).
//! 7. Build a [`VsockConnector`] for the allocated CID and retry-connect
//!    against the agent until the Hello handshake succeeds (the kernel
//!    + agent boot cycle takes ~150ms with the spike kernel).
//! 8. Wrap everything in a [`Vm`].
//!
//! Slice 3a delivers steps 1-6; the agent retry (7) is implemented but
//! requires a real agent inside the VM to exercise — that is slice 3b
//! once the initrd assembly story lands.

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use provium_qmp::{EventMark, Qmp};

use crate::agent_client::AgentClient;
use crate::cid::CidAllocator;
use crate::connector::VsockConnector;
use crate::profile::Profile;
use crate::vmm::{Backend, BootOpts, BootSummary, Vmm, VmInstance, VmRunning, VmmError};
use crate::ClientError;

// ---------------------------------------------------------------------------
// Defaults
// ---------------------------------------------------------------------------

const DEFAULT_QEMU_BINARY: &str = "qemu-system-x86_64";
const DEFAULT_AGENT_PORT: u32 = 1234;
/// Per-VM memory cap applied when neither the boot opts nor (for the
/// interactive console) the CLI override one. Public so the
/// `provium console` path defaults consistently with launch.
pub const DEFAULT_MEMORY_BYTES: u64 = 512 * 1024 * 1024;
/// Per-VM vCPU count applied when nothing overrides it.
pub const DEFAULT_CPUS: u32 = 1;
/// How long [`QemuVmm::boot`] waits for the QMP socket file to appear
/// after the QEMU child has been spawned. Generous — typical times
/// are <100ms.
const QMP_SOCKET_WAIT: Duration = Duration::from_secs(5);
/// How long to keep retrying the QMP *connect* after the socket file
/// appears. QEMU creates the socket at `bind()` (the file shows up) and
/// `listen()`s immediately after; a fast host can connect in that gap
/// and get `ECONNREFUSED`. Retry briefly to close the race.
const QMP_CONNECT_RETRY: Duration = Duration::from_secs(3);
/// Cadence of the QMP connect-retry loop.
const QMP_CONNECT_INTERVAL: Duration = Duration::from_millis(20);
/// How long the agent retry loop will keep dialling the in-VM agent
/// before giving up. Covers kernel boot + agent startup; the spike
/// kernel boots in ~150ms but production kernels are larger.
const AGENT_BOOT_TIMEOUT: Duration = Duration::from_secs(30);
/// Cadence of the agent retry loop.
const AGENT_RETRY_INTERVAL: Duration = Duration::from_millis(50);
/// Bound on how long QMP `quit` will block before we SIGKILL the
/// child as a backstop.
const SHUTDOWN_GRACE: Duration = Duration::from_secs(2);
/// Maximum time to wait for an inbound migration (`-incoming`) to
/// finish loading the snapshot before declaring the restore failed.
/// The migration is dominated by snapshot file I/O — fast on a
/// warm cache, slow on cold storage. 60s mirrors the outbound side
/// (`Backend::snapshot`) so the budgets stay symmetric.
const INCOMING_MIGRATION_TIMEOUT: Duration = Duration::from_secs(60);
/// Cadence of the incoming-migration status poll. A few ms is
/// fine — `query-status` is a single QMP round-trip.
const INCOMING_MIGRATION_POLL: Duration = Duration::from_millis(20);

// ---------------------------------------------------------------------------
// QemuVmm
// ---------------------------------------------------------------------------

/// QEMU-backed VMM. Construct one per provium run; share via
/// [`std::sync::Arc`] across the scheduler.
#[derive(Debug)]
pub struct QemuVmm {
    cids: Arc<CidAllocator>,
    config: QemuVmmConfig,
}

/// User-overridable parameters for [`QemuVmm`].
///
/// The defaults match the design's QEMU spike — `qemu-system-x86_64`
/// on `PATH`, agent port 1234, 512 MiB / 1 vCPU per VM. Production
/// callers override `scratch_root` to live under their own per-run
/// directory so multiple provium runs don't clobber each other.
#[derive(Clone, Debug)]
pub struct QemuVmmConfig {
    /// Path (or PATH-name) of the `qemu-system-*` binary.
    pub qemu_binary: PathBuf,
    /// vsock port the in-VM agent is listening on.
    pub agent_port: u32,
    /// Root under which per-VM scratch directories are created.
    pub scratch_root: PathBuf,
    /// How long to wait for the agent to come up after QMP is ready.
    pub agent_boot_timeout: Duration,
    /// Pass `merge=on` on the memory-backend so KSM can dedupe
    /// pages across VMs. `false` opts the per-VM backend out via
    /// `merge=off` (`--no-ksm` CLI flag).
    pub ksm_enabled: bool,
}

impl QemuVmmConfig {
    /// Defaults: `qemu-system-x86_64` on PATH, agent port 1234,
    /// scratch under `/tmp/provium-qemu`.
    pub fn defaults() -> Self {
        Self {
            qemu_binary: PathBuf::from(DEFAULT_QEMU_BINARY),
            agent_port: DEFAULT_AGENT_PORT,
            scratch_root: std::env::temp_dir().join("provium-qemu"),
            agent_boot_timeout: AGENT_BOOT_TIMEOUT,
            ksm_enabled: true,
        }
    }
}

impl Default for QemuVmmConfig {
    fn default() -> Self {
        Self::defaults()
    }
}

impl QemuVmm {
    /// Build with the default config and a fresh [`CidAllocator`].
    pub fn new() -> Self {
        Self::with_cid_allocator(Arc::new(CidAllocator::new()))
    }

    /// Build with the default config sharing `cids` across other VMMs.
    pub fn with_cid_allocator(cids: Arc<CidAllocator>) -> Self {
        Self {
            cids,
            config: QemuVmmConfig::defaults(),
        }
    }

    /// Build with a custom config.
    pub fn with_config(cids: Arc<CidAllocator>, config: QemuVmmConfig) -> Self {
        Self { cids, config }
    }

    /// Read-only access to the active config.
    pub fn config(&self) -> &QemuVmmConfig {
        &self.config
    }
}

impl Default for QemuVmm {
    fn default() -> Self {
        Self::new()
    }
}

impl Vmm for QemuVmm {
    fn launch(
        &self,
        name: &str,
        profile: &Profile,
        opts: BootOpts,
    ) -> Result<VmRunning, VmmError> {
        self.launch_or_restore(name, profile, opts, None)
    }

    fn restore(
        &self,
        name: &str,
        profile: &Profile,
        opts: BootOpts,
        snapshot_path: &std::path::Path,
    ) -> Result<VmRunning, VmmError> {
        // Per `DESIGN.md` line 2082: restore = launch with
        // `-incoming "file:<path>"` on a fresh QEMU process.
        // `launch_or_restore` handles waiting for the incoming
        // migration to complete and issuing `cont` before
        // returning a running VM.
        self.launch_or_restore(name, profile, opts, Some(snapshot_path))
    }
}

impl QemuVmm {
    /// Shared core for [`Vmm::launch`] and [`Vmm::restore`]. The two
    /// only differ in whether `-incoming` is set on the QEMU command
    /// line and the post-connect dance that drains the incoming
    /// migration before the guest is usable.
    ///
    /// `incoming_snapshot = None` → cold boot.
    /// `incoming_snapshot = Some(path)` → resume from `path`. The
    /// snapshot file was produced by an earlier
    /// [`Backend::snapshot`] call (QMP `migrate file:`). The
    /// restored guest inherits all device state, including the
    /// running agent process and any open vsock connections (per
    /// Spike 2 — `provium/spikes/ch-snapshot/RESULT.md`).
    ///
    /// Each restore allocates a fresh vsock CID, just like cold
    /// launch. QEMU's incoming-migration successfully retargets
    /// the vhost-vsock-pci device to the new CID even though the
    /// snapshot was taken with a different one (validated by the
    /// fixture-fanout spike: 1 snapshot → 8 parallel restores with
    /// distinct CIDs, all independent).
    fn launch_or_restore(
        &self,
        name: &str,
        profile: &Profile,
        opts: BootOpts,
        incoming_snapshot: Option<&std::path::Path>,
    ) -> Result<VmRunning, VmmError> {
        // 1. Validate profile paths up-front so we fail with a clean
        //    diagnostic instead of QEMU-exit-with-cryptic-stderr.
        validate_profile_paths(profile)?;

        // 1b. Realise host-side networking for any attached bridges.
        //     Best-effort: failures here propagate as VmmError::Io so
        //     the launch fails fast (rather than having QEMU fail
        //     opening a missing TAP). Tests that pass attachments
        //     without a bridge handle (graph-state-only) skip this.
        for nic in &opts.nic_attachments {
            if let Some(bridge) = &nic.bridge {
                bridge
                    .realize_for_vm(name)
                    .map_err(VmmError::Io)?;
            }
        }

        // 2. CID + scratch dir.
        let cid = self.cids.allocate();
        let scratch = self.config.scratch_root.join(format!("vm-{cid}"));
        std::fs::create_dir_all(&scratch)?;
        let qmp_socket = scratch.join("qmp.sock");
        let console_log = scratch.join("console.log");
        let console_socket = scratch.join("console.sock");

        let memory_bytes = opts.memory_bytes.unwrap_or(DEFAULT_MEMORY_BYTES);
        let cpus = opts.cpus.unwrap_or(DEFAULT_CPUS);
        let raw_cmdline = opts
            .cmdline_override
            .clone()
            .map(Ok)
            .unwrap_or_else(|| profile.resolve_cmdline())?;

        // 2b. Agent-overlay injection. Concatenates a small cpio
        //     containing /sbin/provium-agent onto the user's initrd
        //     and appends rdinit=/sbin/provium-agent to the cmdline
        //     so the kernel exec's the agent as PID 1; the agent
        //     itself forks the user's /init. Cached on the merged
        //     content hash so repeat boots are free. Skipped per
        //     `profile.inject_agent = false`.
        // Ensure the kernel's printk is captured. The serial port is
        // the only console QEMU's chardev mirrors into `console.log`,
        // so a cmdline that only sets `console=hvc0` (virtio-console)
        // produces a guest where SeaBIOS is visible but the kernel
        // itself prints to a device that doesn't exist or isn't
        // wired to a logfile. Add `console=ttyS0` if absent — the
        // kernel happily uses multiple consoles, so user-set values
        // are preserved alongside.
        let raw_cmdline = ensure_serial_console(&raw_cmdline);

        let prepared = crate::vmm::agent_overlay::prepare_initrd(
            profile,
            &raw_cmdline,
            &self.config.scratch_root,
        )?;
        if crate::verbosity::is_verbose() {
            if prepared.injected {
                eprintln!(
                    "provium: vm {name}: agent overlay injected (initrd {})",
                    prepared.initrd_path.display(),
                );
            } else {
                eprintln!(
                    "provium: vm {name}: agent overlay skipped (inject_agent = false)",
                );
            }
        }
        let initrd_for_qemu = prepared.initrd_path.clone();
        let cmdline = prepared.cmdline.clone();

        let plan = QemuLaunchPlan {
            qemu_binary: &self.config.qemu_binary,
            vm_name: name,
            kernel: &profile.kernel,
            initrd: &initrd_for_qemu,
            cmdline: &cmdline,
            cid,
            memory_bytes,
            cpus,
            qmp_socket: &qmp_socket,
            console_log: &console_log,
            console_socket: &console_socket,
            nics: &opts.nic_attachments,
            ksm_enabled: self.config.ksm_enabled,
            rng_seed: opts.rng_seed,
            initial_time_ns: opts.initial_time_ns,
            incoming_snapshot,
        };

        // 3. Spawn QEMU.
        let mut command = build_qemu_command(&plan);
        command
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .stdin(Stdio::null());
        // PR_SET_PDEATHSIG so QEMU dies if provium itself is
        // SIGKILLed / segfaults / OOM-killed. Without this the
        // QEMU children become orphans and survive the parent.
        // Per `DESIGN.md` § Failure mode catalogue ("All VM
        // children die").
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
        let child = command.spawn()?;

        // SCHED_BATCH per DESIGN.md § Scheduler. Failures are
        // logged + ignored — the design explicitly accepts this.
        // SAFETY: child.id() is the kernel-assigned pid that's
        // valid for the duration of `child`'s lifetime.
        unsafe {
            let policy = libc::SCHED_BATCH;
            let param = libc::sched_param { sched_priority: 0 };
            let r = libc::sched_setscheduler(
                child.id() as libc::pid_t,
                policy,
                &param,
            );
            if r != 0 {
                let e = std::io::Error::last_os_error();
                eprintln!(
                    "provium: SCHED_BATCH on QEMU pid {} skipped: {e}",
                    child.id()
                );
            }
        }

        // 4. Wait for the QMP socket file to appear, then connect.
        if let Err(e) = wait_for_path(&qmp_socket, QMP_SOCKET_WAIT) {
            // QEMU may have exited early (bad kernel, missing KVM, …).
            // Surface that as a clearer error than "socket never
            // appeared".
            let _ = kill_child(&child);
            let _ = std::fs::remove_dir_all(&scratch);
            return Err(VmmError::Io(e));
        }
        // The socket file exists (QEMU `bind()`ed it), but QEMU may not
        // be `listen()`ing yet — connecting in that window yields
        // ECONNREFUSED. Retry briefly so a fast host doesn't lose the race.
        let qmp = {
            let deadline = Instant::now() + QMP_CONNECT_RETRY;
            loop {
                match Qmp::connect(&qmp_socket) {
                    Ok(q) => break q,
                    Err(e) => {
                        if Instant::now() >= deadline {
                            let _ = kill_child(&child);
                            let _ = std::fs::remove_dir_all(&scratch);
                            return Err(VmmError::Qmp(e));
                        }
                        thread::sleep(QMP_CONNECT_INTERVAL);
                    }
                }
            }
        };

        // 4b. If we launched with `-incoming`, drain the inbound
        //     migration before talking to the guest. QEMU starts
        //     the vCPUs paused while the snapshot is being loaded;
        //     once `query-status` reports the VM has left the
        //     `inmigrate` phase we issue `cont` to resume execution.
        //     Polling query-status (rather than waiting for the
        //     MIGRATION event) sidesteps a race where the migration
        //     may already have completed by the time we connected
        //     QMP — events buffered before connect are not
        //     redelivered, but the status is always queryable.
        if incoming_snapshot.is_some() {
            if let Err(e) =
                wait_incoming_migration(&qmp, INCOMING_MIGRATION_TIMEOUT)
            {
                let console_tail = read_console_tail(&console_log, 4096);
                let _ = qmp.quit();
                let _ = kill_child(&child);
                let _ = std::fs::remove_dir_all(&scratch);
                let mut msg =
                    format!("incoming migration did not complete: {e}");
                if let Some(tail) = console_tail {
                    msg.push_str("\n--- guest console (last bytes) ---\n");
                    msg.push_str(&tail);
                    if !msg.ends_with('\n') {
                        msg.push('\n');
                    }
                    msg.push_str("--- end guest console ---");
                }
                return Err(VmmError::Io(std::io::Error::other(msg)));
            }
            if let Err(e) = qmp.cont() {
                let _ = qmp.quit();
                let _ = kill_child(&child);
                let _ = std::fs::remove_dir_all(&scratch);
                return Err(VmmError::Io(std::io::Error::other(format!(
                    "cont after incoming migration failed: {e}"
                ))));
            }
        }

        // 5. Wait for the in-VM agent to come up.
        let connector = VsockConnector::new(cid, self.config.agent_port);
        let client = AgentClient::new(connector);
        if let Err(e) = wait_for_agent(&client, self.config.agent_boot_timeout, name) {
            // Read the tail of the kernel console BEFORE wiping the
            // scratch dir — without this, the only thing the user
            // sees is "agent did not come up within 30s" with zero
            // info on what the kernel actually did. The console.log
            // captures everything from kernel decompression through
            // userspace exit.
            let console_tail = read_console_tail(&console_log, 4096);
            let _ = qmp.quit();
            let _ = kill_child(&child);
            let _ = std::fs::remove_dir_all(&scratch);
            let mut msg = format!(
                "agent did not come up within {:?}: {e}",
                self.config.agent_boot_timeout,
            );
            if let Some(tail) = console_tail {
                msg.push_str("\n--- guest console (last bytes) ---\n");
                msg.push_str(&tail);
                if !msg.ends_with('\n') {
                    msg.push('\n');
                }
                msg.push_str("--- end guest console ---");
            } else {
                msg.push_str(
                    "\n(guest console.log was empty or unreadable — \
                     QEMU may have failed to spawn, or the kernel \
                     printed nothing before timeout)",
                );
            }
            return Err(VmmError::Io(std::io::Error::other(msg)));
        }

        // 6. Wrap in VmRunning.
        let backend: Arc<dyn Backend> = Arc::new(QemuBackend {
            child: Mutex::new(Some(child)),
            qmp: Mutex::new(Some(qmp)),
            scratch,
        });
        let summary = BootSummary {
            guest_os: profile.guest_os.clone(),
            memory_bytes,
            cpus,
        };
        let instance = VmInstance::new(cid, summary.clone(), backend);
        Ok(VmRunning {
            cid,
            summary,
            instance,
            client,
            console_log: Some(console_log),
            console_socket: Some(console_socket),
        })
    }
}

// ---------------------------------------------------------------------------
// Command line construction
// ---------------------------------------------------------------------------

/// Frozen view of the inputs to [`build_qemu_command`]. Holding the
/// borrows in one struct keeps the function signature and tests
/// readable when fields grow (e.g. CPU model overrides in slice 3b+).
#[derive(Debug)]
pub struct QemuLaunchPlan<'a> {
    /// Path / PATH-name of the QEMU binary.
    pub qemu_binary: &'a Path,
    /// VM name — drives the TAP interface name.
    pub vm_name: &'a str,
    /// Kernel image for `-kernel`.
    pub kernel: &'a Path,
    /// Initramfs image for `-initrd`.
    pub initrd: &'a Path,
    /// Kernel command line.
    pub cmdline: &'a str,
    /// vsock CID for `vhost-vsock-pci,guest-cid=...`.
    pub cid: u32,
    /// VM memory cap in bytes.
    pub memory_bytes: u64,
    /// vCPU count.
    pub cpus: u32,
    /// Path the QMP socket should be created at.
    pub qmp_socket: &'a Path,
    /// Path the serial console output is redirected to.
    pub console_log: &'a Path,
    /// Path the bidirectional console socket lives at. Hosts
    /// connect to this for `console:write` / `console:read` —
    /// QEMU's chardev `logfile=` option keeps `console_log`
    /// up-to-date in parallel.
    pub console_socket: &'a Path,
    /// NIC attachments — each emits a `-netdev tap` /
    /// `-device virtio-net-pci` pair.
    pub nics: &'a [super::NicAttachment],
    /// `true` enables `merge=on` on the memory backend (KSM-eligible);
    /// `false` emits `merge=off` so VMs opt out per `--no-ksm`.
    pub ksm_enabled: bool,
    /// Optional RNG seed surfaced to the guest via fw_cfg under
    /// `opt/provium/rng-seed` (hex-encoded). The guest agent (or
    /// kernel patches) can pull this for deterministic entropy at
    /// boot. Per `DESIGN.md` § Boot opts.
    pub rng_seed: Option<u64>,
    /// Optional initial guest wall-clock time in nanoseconds since
    /// the Unix epoch. Translated to `-rtc base=<ISO>,clock=host`.
    /// Per `DESIGN.md` § Boot opts.
    pub initial_time_ns: Option<i64>,
    /// Path to a snapshot file produced by an earlier `migrate
    /// file:` invocation. When set, QEMU launches with
    /// `-incoming "file:<path>"`, loading guest memory + device
    /// state from the snapshot. The guest's vCPUs start paused
    /// (`postmigrate` state) — the caller is responsible for
    /// awaiting migration completion and issuing `cont`.
    /// Per `DESIGN.md` § QEMU snapshot/restore (line 2082).
    pub incoming_snapshot: Option<&'a Path>,
}

/// Construct the `qemu-system-*` command line for one VM.
///
/// Decisions baked in (per `DESIGN.md` § Performance / Resolved
/// decisions and the QEMU spike findings):
///
/// * `q35,accel=kvm` — modern chipset + KVM acceleration.
/// * `memory-backend-ram,merge=on` so KSM can deduplicate guest
///   pages across VMs.
/// * `vhost-vsock-pci,guest-cid=<cid>` for the agent transport.
/// * `-qmp unix:<sock>,server=on,wait=off` so the host can connect
///   the moment the socket file appears (no race against `wait=on`).
/// * `-serial file:<console_log>` so the kernel's printk path is
///   captured for post-mortem diagnostic regardless of whether the
///   guest's console is hvc0 or ttyS0.
/// * `-no-reboot` so a guest panic exits the VMM rather than rebooting
///   into the same broken state.
/// * `-nographic` — provium VMs are headless.
pub fn build_qemu_command(plan: &QemuLaunchPlan<'_>) -> Command {
    let mem = memory_to_qemu_arg(plan.memory_bytes);

    let mut cmd = Command::new(plan.qemu_binary);
    cmd.args([
        "-M",
        "q35,accel=kvm,memory-backend=mem",
        "-m",
        &mem,
        "-smp",
        &plan.cpus.to_string(),
        "-nodefaults",
        "-no-reboot",
        "-nographic",
    ]);
    cmd.args([
        "-kernel",
        path_arg(plan.kernel).as_str(),
        "-initrd",
        path_arg(plan.initrd).as_str(),
        "-append",
        plan.cmdline,
    ]);
    cmd.args([
        "-object",
        &format!(
            "memory-backend-ram,id=mem,size={mem},merge={}",
            if plan.ksm_enabled { "on" } else { "off" }
        ),
    ]);
    cmd.args([
        "-device",
        &format!("vhost-vsock-pci,guest-cid={}", plan.cid),
    ]);
    cmd.args([
        "-qmp",
        &format!(
            "unix:{},server=on,wait=off",
            path_arg(plan.qmp_socket)
        ),
    ]);
    // Bidirectional console: socket chardev with `logfile=` mirroring
    // the byte stream into `console_log` (so the existing file-based
    // read path keeps working). The socket allows `console:write` /
    // `console:read_stream` to send keystrokes / receive bytes
    // without an intermediary.
    cmd.args([
        "-chardev",
        &format!(
            "socket,id=conserial0,path={},server=on,wait=off,logfile={}",
            path_arg(plan.console_socket),
            path_arg(plan.console_log),
        ),
    ]);
    cmd.args(["-serial", "chardev:conserial0"]);
    // Initial guest wall-clock — `-rtc base=<ISO8601>,clock=host`
    // sets the emulated RTC. `clock=host` ties the guest's tick
    // source to the host clock so monotonic time still advances
    // normally; only the wall-clock origin moves.
    if let Some(ns) = plan.initial_time_ns {
        if let Some(iso) = format_unix_ns_iso8601(ns) {
            cmd.args(["-rtc", &format!("base={iso},clock=host")]);
        }
    }
    // RNG seed — exposed via fw_cfg under `opt/provium/rng-seed`
    // as a hex string. Guest-side consumption is the agent's job;
    // this side just plumbs the value so it's observable.
    if let Some(seed) = plan.rng_seed {
        cmd.args([
            "-fw_cfg",
            &format!("name=opt/provium/rng-seed,string={seed:016x}"),
        ]);
    }
    // Incoming-migration restore. Adding `-incoming` makes QEMU
    // load guest state from the snapshot file before starting
    // vCPUs (which remain paused in `postmigrate` until `cont`).
    // Per `DESIGN.md` line 2082: only `file:` is safe — the
    // `exec:` form has a flush race that produces truncated
    // snapshots.
    if let Some(snap) = plan.incoming_snapshot {
        cmd.args(["-incoming", &format!("file:{}", path_arg(snap))]);
    }
    // NICs — one `-netdev tap` + `-device virtio-net-pci` pair per
    // attachment. Uses the bridge_realize::tap_name_for_vm convention
    // so the host-side TAP created by Bridge::realize_for_vm matches.
    for nic in plan.nics {
        let tap_ifname = crate::bridge_realize::tap_name_for_vm_on_bridge(
            plan.vm_name,
            &nic.bridge_name,
        );
        cmd.args([
            "-netdev",
            &format!(
                "tap,id={id},ifname={tap},script=no,downscript=no",
                id = nic.nic_id,
                tap = tap_ifname
            ),
        ]);
        let mut device =
            format!("virtio-net-pci,netdev={id}", id = nic.nic_id);
        if let Some(mac) = &nic.mac {
            device.push_str(&format!(",mac={mac}"));
        }
        cmd.args(["-device", &device]);
    }
    cmd
}

/// Frozen inputs for [`build_interactive_qemu_command`].
///
/// Unlike [`QemuLaunchPlan`] this drives a *human-facing* boot: QEMU's
/// serial port and monitor are muxed onto the controlling terminal's
/// stdio (`-serial mon:stdio`) rather than a logged socket, and there
/// is no QMP control socket — the operator at the keyboard, not the
/// host process, drives the VM. The result is what a hand-rolled
/// `qemu-system-x86_64 -kernel … -initrd …` would give you:
/// `Ctrl-A X` quits, `Ctrl-A C` toggles the QEMU monitor.
#[derive(Debug)]
pub struct InteractiveLaunchPlan<'a> {
    /// Path / PATH-name of the QEMU binary.
    pub qemu_binary: &'a Path,
    /// Kernel image for `-kernel`.
    pub kernel: &'a Path,
    /// Initramfs image for `-initrd`.
    pub initrd: &'a Path,
    /// Kernel command line. The builder ensures `console=ttyS0` is
    /// present so the kernel log + any getty land on the serial QEMU
    /// muxes to the terminal.
    pub cmdline: &'a str,
    /// VM memory cap in bytes.
    pub memory_bytes: u64,
    /// vCPU count.
    pub cpus: u32,
    /// vsock CID. `Some` wires a `vhost-vsock-pci` device so an
    /// injected agent is reachable; `None` (the default for a bare
    /// boot) omits it, dropping the dependency on `/dev/vhost-vsock`.
    pub cid: Option<u32>,
    /// `true` emits `merge=on` on the memory backend (KSM-eligible);
    /// `false` opts out via `merge=off`.
    pub ksm_enabled: bool,
    /// Extra arguments appended verbatim after the generated ones
    /// (e.g. `-drive`, `-device …`). Forwarded straight through.
    pub extra_args: &'a [String],
}

/// Construct the `qemu-system-*` command line for an interactive
/// console boot — see [`InteractiveLaunchPlan`] for how this differs
/// from [`build_qemu_command`].
///
/// Decisions:
///
/// * `-display none` + `-nodefaults` — no graphical window, no
///   surprise default devices. (We avoid `-nographic`'s implicit
///   stdio serial because pairing it with an explicit `-serial
///   mon:stdio` can trip QEMU's "stdio used by multiple chardevs"
///   guard.)
/// * `-serial mon:stdio` — the one and only stdio chardev: guest
///   serial muxed with the QEMU monitor, with `Ctrl-A` escapes.
/// * `-no-reboot` so a guest panic drops back to the shell rather than
///   silently rebooting.
/// * `vhost-vsock-pci` only when a CID is supplied (i.e. the agent
///   overlay was injected) — a bare boot needs no vsock plumbing.
pub fn build_interactive_qemu_command(plan: &InteractiveLaunchPlan<'_>) -> Command {
    let mem = memory_to_qemu_arg(plan.memory_bytes);
    let cmdline = ensure_serial_console(plan.cmdline);

    let mut cmd = Command::new(plan.qemu_binary);
    cmd.args([
        "-M",
        "q35,accel=kvm,memory-backend=mem",
        "-m",
        &mem,
        "-smp",
        &plan.cpus.to_string(),
        "-nodefaults",
        "-no-reboot",
        "-display",
        "none",
    ]);
    cmd.args([
        "-object",
        &format!(
            "memory-backend-ram,id=mem,size={mem},merge={}",
            if plan.ksm_enabled { "on" } else { "off" }
        ),
    ]);
    cmd.args([
        "-kernel",
        path_arg(plan.kernel).as_str(),
        "-initrd",
        path_arg(plan.initrd).as_str(),
        "-append",
        cmdline.as_str(),
    ]);
    // Mux the guest serial + QEMU monitor onto our stdio so the user
    // gets a live, interactive console.
    cmd.args(["-serial", "mon:stdio"]);
    if let Some(cid) = plan.cid {
        cmd.args(["-device", &format!("vhost-vsock-pci,guest-cid={cid}")]);
    }
    for a in plan.extra_args {
        cmd.arg(a);
    }
    cmd
}

/// Format a Unix-epoch nanosecond timestamp as the ISO 8601 string
/// QEMU's `-rtc base=` accepts (`YYYY-MM-DDTHH:MM:SS`, UTC, no
/// fractional seconds — QEMU rounds to whole-second RTC anyway).
///
/// Returns `None` if the value is out of QEMU's representable range
/// (pre-1970 or beyond year 9999); in that case the caller falls back
/// to QEMU's default base (host time at spawn).
fn format_unix_ns_iso8601(ns: i64) -> Option<String> {
    if ns < 0 {
        return None;
    }
    let secs = (ns / 1_000_000_000) as i64;
    // Convert epoch seconds → broken-down UTC time via libc gmtime_r.
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    let t = secs as libc::time_t;
    let r = unsafe { libc::gmtime_r(&t as *const _, &mut tm) };
    if r.is_null() {
        return None;
    }
    let year = tm.tm_year + 1900;
    if !(1970..=9999).contains(&year) {
        return None;
    }
    Some(format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}",
        year,
        tm.tm_mon + 1,
        tm.tm_mday,
        tm.tm_hour,
        tm.tm_min,
        tm.tm_sec,
    ))
}

/// Convert bytes to QEMU's `-m` argument. QEMU accepts plain integers
/// (bytes), `K`/`M`/`G` suffixes; we use `M` because it strikes a
/// reasonable balance between precision and human-readability and
/// also matches the unit `memory-backend-ram` expects when paired.
fn memory_to_qemu_arg(bytes: u64) -> String {
    let mib = bytes / (1024 * 1024);
    let mib = mib.max(1);
    format!("{mib}M")
}

/// Render a path for use as a QEMU CLI argument. Lossy by intent —
/// non-UTF-8 paths in profile config are rejected at TOML-load time so
/// this is only stretched in genuinely pathological cases (a scratch
/// dir under a non-UTF-8 cwd, etc.) where there's nothing better to
/// do than render with replacement chars.
fn path_arg(p: &Path) -> String {
    p.to_string_lossy().into_owned()
}

fn validate_profile_paths(profile: &Profile) -> Result<(), VmmError> {
    if !profile.kernel.exists() {
        return Err(VmmError::MissingProfilePath {
            field: "kernel",
            path: profile.kernel.clone(),
        });
    }
    if !profile.initrd.exists() {
        return Err(VmmError::MissingProfilePath {
            field: "initrd",
            path: profile.initrd.clone(),
        });
    }
    if let Some(cmdline_file) = &profile.cmdline_file {
        if !cmdline_file.exists() {
            return Err(VmmError::MissingProfilePath {
                field: "cmdline_file",
                path: cmdline_file.clone(),
            });
        }
    }
    Ok(())
}

fn wait_for_path(path: &Path, timeout: Duration) -> std::io::Result<()> {
    let deadline = Instant::now() + timeout;
    while !path.exists() {
        if Instant::now() >= deadline {
            return Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                format!("path `{}` did not appear within {timeout:?}", path.display()),
            ));
        }
        thread::sleep(Duration::from_millis(20));
    }
    Ok(())
}

/// Poll QMP `query-status` until the VM has exited the `inmigrate`
/// state, indicating QEMU has finished loading the snapshot from
/// `-incoming`. After return, the guest is paused (`postmigrate` or
/// `paused`) — caller must issue `cont` to start the vCPUs.
///
/// We poll status rather than wait for the `MIGRATION` event:
/// inbound migration can complete before QMP is connected, in
/// which case the buffered event has already been drained. Status
/// is authoritative regardless of event timing.
fn wait_incoming_migration(qmp: &Qmp, timeout: Duration) -> Result<(), String> {
    let deadline = Instant::now() + timeout;
    loop {
        match qmp.query_status() {
            Ok(status) => {
                if status != "inmigrate" {
                    return Ok(());
                }
            }
            Err(e) => {
                return Err(format!("query-status failed: {e}"));
            }
        }
        if Instant::now() >= deadline {
            return Err(format!(
                "VM stuck in `inmigrate` state after {timeout:?}"
            ));
        }
        thread::sleep(INCOMING_MIGRATION_POLL);
    }
}

fn wait_for_agent(
    client: &AgentClient,
    timeout: Duration,
    vm_name: &str,
) -> Result<(), ClientError> {
    let started = Instant::now();
    let deadline = started + timeout;
    let mut last_err: Option<ClientError> = None;
    let mut attempts: u32 = 0;
    let mut last_heartbeat = started;
    let heartbeat_interval = Duration::from_secs(5);
    loop {
        attempts += 1;
        match client.ping() {
            Ok(()) => {
                if crate::verbosity::is_verbose() {
                    eprintln!(
                        "provium: vm {vm_name}: agent up after {:.2}s ({attempts} attempts)",
                        started.elapsed().as_secs_f64(),
                    );
                }
                return Ok(());
            }
            Err(ClientError::Connect(_)) | Err(ClientError::Frame(_)) => {
                // Either the socket isn't being accepted yet (agent
                // not up) or we lost the connection mid-handshake
                // (very early boot). Retry.
            }
            Err(other) => return Err(other),
        }
        let now = Instant::now();
        if now >= deadline {
            return Err(last_err.unwrap_or_else(|| {
                ClientError::Connect(std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    "agent did not respond",
                ))
            }));
        }
        if now.duration_since(last_heartbeat) >= heartbeat_interval {
            eprintln!(
                "provium: vm {vm_name}: still waiting for agent ({:.0}s elapsed, {attempts} attempts)",
                started.elapsed().as_secs_f64(),
            );
            last_heartbeat = now;
        }
        last_err = Some(ClientError::Connect(std::io::Error::new(
            std::io::ErrorKind::ConnectionRefused,
            "still booting",
        )));
        thread::sleep(AGENT_RETRY_INTERVAL);
    }
}

/// Append `console=ttyS0` to `cmdline` if no `console=ttyS0` token is
/// already present. The kernel accepts multiple `console=` directives
/// — it logs to all of them — so any user-set values (e.g.
/// `console=hvc0`) are preserved.
fn ensure_serial_console(cmdline: &str) -> String {
    let already_present = cmdline
        .split_ascii_whitespace()
        .any(|tok| tok == "console=ttyS0");
    if already_present {
        cmdline.to_owned()
    } else {
        format!("{} console=ttyS0", cmdline.trim_end())
    }
}

/// Read the last `max_bytes` of the guest console log into a String.
/// Returns `None` if the file is missing, empty, or unreadable.
fn read_console_tail(path: &Path, max_bytes: usize) -> Option<String> {
    let bytes = std::fs::read(path).ok()?;
    if bytes.is_empty() {
        return None;
    }
    let start = bytes.len().saturating_sub(max_bytes);
    Some(String::from_utf8_lossy(&bytes[start..]).into_owned())
}

fn kill_child(child: &Child) -> std::io::Result<()> {
    // SAFETY: passing a valid pid value to libc::kill is documented
    // behaviour; SIGKILL (9) is always defined. Errors are ignored —
    // this is a best-effort cleanup path.
    let pid = child.id();
    unsafe {
        libc::kill(pid as libc::pid_t, libc::SIGKILL);
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// QemuBackend — the [`Backend`] impl that lives inside [`VmInstance`].
// ---------------------------------------------------------------------------

struct QemuBackend {
    child: Mutex<Option<Child>>,
    qmp: Mutex<Option<Qmp>>,
    scratch: PathBuf,
}

impl Backend for QemuBackend {
    fn pause(&self) -> Result<(), VmmError> {
        let qmp = self.qmp.lock().unwrap();
        let qmp = qmp
            .as_ref()
            .ok_or(VmmError::Unimplemented("VM already shut down"))?;
        qmp.stop()?;
        Ok(())
    }

    fn resume(&self) -> Result<(), VmmError> {
        let qmp = self.qmp.lock().unwrap();
        let qmp = qmp
            .as_ref()
            .ok_or(VmmError::Unimplemented("VM already shut down"))?;
        qmp.cont()?;
        Ok(())
    }

    fn snapshot(&self, path: &Path) -> Result<(), VmmError> {
        let qmp = self.qmp.lock().unwrap();
        let qmp = qmp
            .as_ref()
            .ok_or(VmmError::Unimplemented("VM already shut down"))?;
        // Atomic write per `DESIGN.md` § Failure modes: write to
        // `<path>.tmp`, then rename. ENOSPC mid-migrate leaves the
        // .tmp behind which we delete; the cache never sees a
        // partial snapshot.
        let tmp = path.with_extension("snap.tmp");
        let mark: EventMark = qmp.event_mark();
        qmp.migrate(&tmp)?;
        let wait = qmp.wait_migration_completed(mark, Duration::from_secs(60));
        if let Err(e) = wait {
            let _ = std::fs::remove_file(&tmp);
            return Err(e.into());
        }
        if let Err(e) = std::fs::rename(&tmp, path) {
            let _ = std::fs::remove_file(&tmp);
            return Err(VmmError::Io(e));
        }
        Ok(())
    }

    fn reset(&self) -> Result<(), VmmError> {
        let qmp = self.qmp.lock().unwrap();
        let qmp = qmp
            .as_ref()
            .ok_or(VmmError::Unimplemented("VM already shut down"))?;
        qmp.execute("system_reset", serde_json::Value::Null)?;
        Ok(())
    }

    fn power_button(&self) -> Result<(), VmmError> {
        let qmp = self.qmp.lock().unwrap();
        let qmp = qmp
            .as_ref()
            .ok_or(VmmError::Unimplemented("VM already shut down"))?;
        qmp.execute("system_powerdown", serde_json::Value::Null)?;
        Ok(())
    }

    fn set_link(&self, netdev_id: &str, up: bool) -> Result<(), VmmError> {
        let qmp = self.qmp.lock().unwrap();
        let qmp = qmp
            .as_ref()
            .ok_or(VmmError::Unimplemented("VM already shut down"))?;
        let args = serde_json::json!({ "name": netdev_id, "up": up });
        qmp.execute("set_link", args)?;
        Ok(())
    }

    fn detach_disk(&self, disk_id: &str) -> Result<(), VmmError> {
        let qmp = self.qmp.lock().unwrap();
        let qmp = qmp
            .as_ref()
            .ok_or(VmmError::Unimplemented("VM already shut down"))?;
        // device_del is best-effort — disks attached only as
        // host-side bookkeeping never registered with QEMU and
        // surface a polite QMP error which we let through.
        let args = serde_json::json!({ "id": disk_id });
        qmp.execute("device_del", args)?;
        Ok(())
    }

    fn shutdown(&self) -> Result<(), VmmError> {
        // 1. Drop the QMP connection (sends `quit` + closes).
        if let Some(qmp) = self.qmp.lock().unwrap().take() {
            let _ = qmp.close();
        }

        // 2. Reap the child. Give it a moment, then SIGKILL backstop.
        if let Some(mut child) = self.child.lock().unwrap().take() {
            let deadline = Instant::now() + SHUTDOWN_GRACE;
            loop {
                match child.try_wait()? {
                    Some(_) => break,
                    None if Instant::now() >= deadline => {
                        let _ = kill_child(&child);
                        let _ = child.wait();
                        break;
                    }
                    None => thread::sleep(Duration::from_millis(20)),
                }
            }
        }

        // 3. Best-effort scratch cleanup. Errors here are logged but
        //    not propagated — orphaned scratch dirs are recoverable
        //    (a startup sweeper handles that in slice 4).
        let _ = std::fs::remove_dir_all(&self.scratch);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fake_plan<'a>(
        cid: u32,
        kernel: &'a Path,
        initrd: &'a Path,
        qmp: &'a Path,
        console: &'a Path,
    ) -> QemuLaunchPlan<'a> {
        QemuLaunchPlan {
            qemu_binary: Path::new("qemu-system-x86_64"),
            vm_name: "test",
            kernel,
            initrd,
            cmdline: "console=hvc0 quiet",
            cid,
            memory_bytes: 1024 * 1024 * 1024,
            cpus: 4,
            qmp_socket: qmp,
            console_log: console,
            console_socket: Path::new("/tmp/console.sock"),
            nics: &[],
            ksm_enabled: true,
            rng_seed: None,
            initial_time_ns: None,
            incoming_snapshot: None,
        }
    }

    fn collect_args(cmd: &Command) -> Vec<String> {
        cmd.get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect()
    }

    fn arg_after(args: &[String], flag: &str) -> Option<String> {
        let idx = args.iter().position(|a| a == flag)?;
        args.get(idx + 1).cloned()
    }

    #[test]
    fn incoming_snapshot_emits_incoming_arg() {
        let mut plan = fake_plan(
            42,
            Path::new("/k"),
            Path::new("/i"),
            Path::new("/qmp"),
            Path::new("/log"),
        );
        let snap = Path::new("/cache/abc.snap");
        plan.incoming_snapshot = Some(snap);
        let cmd = build_qemu_command(&plan);
        let args = collect_args(&cmd);
        assert_eq!(
            arg_after(&args, "-incoming").as_deref(),
            Some("file:/cache/abc.snap")
        );
    }

    #[test]
    fn no_incoming_arg_when_snapshot_absent() {
        let plan = fake_plan(
            42,
            Path::new("/k"),
            Path::new("/i"),
            Path::new("/qmp"),
            Path::new("/log"),
        );
        let cmd = build_qemu_command(&plan);
        let args = collect_args(&cmd);
        assert!(
            !args.iter().any(|a| a == "-incoming"),
            "cold launch should not pass -incoming, got {args:?}"
        );
    }

    #[test]
    fn cmdline_includes_kvm_and_q35() {
        let plan = fake_plan(
            100,
            Path::new("/k"),
            Path::new("/i"),
            Path::new("/qmp"),
            Path::new("/log"),
        );
        let cmd = build_qemu_command(&plan);
        let args = collect_args(&cmd);
        assert_eq!(arg_after(&args, "-M").as_deref(), Some("q35,accel=kvm,memory-backend=mem"));
    }

    #[test]
    fn cmdline_carries_cid_in_vhost_vsock_device() {
        let plan = fake_plan(
            537,
            Path::new("/k"),
            Path::new("/i"),
            Path::new("/qmp"),
            Path::new("/log"),
        );
        let cmd = build_qemu_command(&plan);
        let args = collect_args(&cmd);
        let device = args
            .iter()
            .position(|a| a == "-device")
            .map(|i| args[i + 1].clone())
            .unwrap();
        assert_eq!(device, "vhost-vsock-pci,guest-cid=537");
    }

    #[test]
    fn cmdline_uses_file_qmp_with_server_no_wait() {
        let plan = fake_plan(
            100,
            Path::new("/k"),
            Path::new("/i"),
            Path::new("/run/qmp.sock"),
            Path::new("/log"),
        );
        let cmd = build_qemu_command(&plan);
        let args = collect_args(&cmd);
        assert_eq!(
            arg_after(&args, "-qmp").as_deref(),
            Some("unix:/run/qmp.sock,server=on,wait=off")
        );
    }

    #[test]
    fn cmdline_includes_memory_backend_with_merge_on() {
        let plan = fake_plan(
            100,
            Path::new("/k"),
            Path::new("/i"),
            Path::new("/qmp"),
            Path::new("/log"),
        );
        let cmd = build_qemu_command(&plan);
        let args = collect_args(&cmd);
        let object = args
            .iter()
            .position(|a| a == "-object")
            .map(|i| args[i + 1].clone())
            .unwrap();
        assert!(
            object.contains("memory-backend-ram") && object.contains("merge=on"),
            "got: {object}"
        );
    }

    #[test]
    fn cmdline_redirects_serial_to_log_file() {
        let plan = fake_plan(
            100,
            Path::new("/k"),
            Path::new("/i"),
            Path::new("/qmp"),
            Path::new("/scratch/console.log"),
        );
        let cmd = build_qemu_command(&plan);
        let args = collect_args(&cmd);
        // Serial points at the chardev socket; the chardev has a
        // `logfile=` clause so the captured-output file path is
        // still kept in sync.
        assert_eq!(
            arg_after(&args, "-serial").as_deref(),
            Some("chardev:conserial0")
        );
        let chardev = args
            .iter()
            .position(|a| a == "-chardev")
            .map(|i| args[i + 1].clone())
            .unwrap();
        assert!(chardev.contains("logfile=/scratch/console.log"), "got: {chardev}");
    }

    #[test]
    fn cmdline_propagates_kernel_initrd_cmdline() {
        let plan = fake_plan(
            100,
            Path::new("/k/img"),
            Path::new("/i/img"),
            Path::new("/qmp"),
            Path::new("/log"),
        );
        let cmd = build_qemu_command(&plan);
        let args = collect_args(&cmd);
        assert_eq!(arg_after(&args, "-kernel").as_deref(), Some("/k/img"));
        assert_eq!(arg_after(&args, "-initrd").as_deref(), Some("/i/img"));
        assert_eq!(arg_after(&args, "-append").as_deref(), Some("console=hvc0 quiet"));
    }

    #[test]
    fn cmdline_includes_no_reboot_and_nographic() {
        let plan = fake_plan(
            100,
            Path::new("/k"),
            Path::new("/i"),
            Path::new("/qmp"),
            Path::new("/log"),
        );
        let cmd = build_qemu_command(&plan);
        let args = collect_args(&cmd);
        assert!(args.iter().any(|a| a == "-no-reboot"));
        assert!(args.iter().any(|a| a == "-nographic"));
        assert!(args.iter().any(|a| a == "-nodefaults"));
    }

    #[test]
    fn cmdline_propagates_initial_time_via_rtc_base() {
        let dir = tempfile::tempdir().unwrap();
        let kernel = dir.path().join("k");
        let initrd = dir.path().join("i");
        let qmp = dir.path().join("q");
        let log = dir.path().join("l");
        std::fs::write(&kernel, []).unwrap();
        std::fs::write(&initrd, []).unwrap();
        let mut plan = fake_plan(7, &kernel, &initrd, &qmp, &log);
        // 2024-01-02T03:04:05Z = 1704164645 sec.
        plan.initial_time_ns = Some(1_704_164_645_000_000_000);
        let cmd = build_qemu_command(&plan);
        let args = collect_args(&cmd);
        let rtc = arg_after(&args, "-rtc").expect("missing -rtc");
        assert!(rtc.contains("base=2024-01-02T03:04:05"), "got {rtc}");
        assert!(rtc.contains("clock=host"), "got {rtc}");
    }

    #[test]
    fn cmdline_omits_rtc_when_no_initial_time() {
        let dir = tempfile::tempdir().unwrap();
        let kernel = dir.path().join("k");
        let initrd = dir.path().join("i");
        let qmp = dir.path().join("q");
        let log = dir.path().join("l");
        std::fs::write(&kernel, []).unwrap();
        std::fs::write(&initrd, []).unwrap();
        let plan = fake_plan(7, &kernel, &initrd, &qmp, &log);
        let cmd = build_qemu_command(&plan);
        let args = collect_args(&cmd);
        assert!(!args.iter().any(|a| a == "-rtc"));
    }

    #[test]
    fn cmdline_propagates_rng_seed_via_fw_cfg() {
        let dir = tempfile::tempdir().unwrap();
        let kernel = dir.path().join("k");
        let initrd = dir.path().join("i");
        let qmp = dir.path().join("q");
        let log = dir.path().join("l");
        std::fs::write(&kernel, []).unwrap();
        std::fs::write(&initrd, []).unwrap();
        let mut plan = fake_plan(7, &kernel, &initrd, &qmp, &log);
        plan.rng_seed = Some(0xdead_beef_cafe_f00d);
        let cmd = build_qemu_command(&plan);
        let args = collect_args(&cmd);
        let fw = arg_after(&args, "-fw_cfg").expect("missing -fw_cfg");
        assert_eq!(fw, "name=opt/provium/rng-seed,string=deadbeefcafef00d");
    }

    #[test]
    fn format_unix_ns_iso8601_handles_known_values() {
        assert_eq!(
            format_unix_ns_iso8601(0).unwrap(),
            "1970-01-01T00:00:00"
        );
        assert_eq!(
            format_unix_ns_iso8601(1_704_164_645_000_000_000).unwrap(),
            "2024-01-02T03:04:05"
        );
        // Negative rejected.
        assert!(format_unix_ns_iso8601(-1).is_none());
    }

    #[test]
    fn memory_to_qemu_arg_uses_mib() {
        assert_eq!(memory_to_qemu_arg(2 * 1024 * 1024 * 1024), "2048M");
        assert_eq!(memory_to_qemu_arg(512 * 1024 * 1024), "512M");
    }

    #[test]
    fn memory_to_qemu_arg_clamps_below_mib() {
        // Sub-1MiB values get clamped to 1M — QEMU rejects 0M.
        assert_eq!(memory_to_qemu_arg(0), "1M");
        assert_eq!(memory_to_qemu_arg(1024), "1M");
    }

    #[test]
    fn validate_profile_paths_rejects_missing_kernel() {
        let profile = Profile {
            kernel: PathBuf::from("/no/such/kernel"),
            initrd: PathBuf::from("/no/such/initrd"),
            cmdline: "console=hvc0".into(),
            guest_os: "peios".into(),
            inject_agent: true,
            agent_overlay_path: None,
            cmdline_file: None,
            build: None,
            build_out: None,
        };
        match validate_profile_paths(&profile) {
            Err(VmmError::MissingProfilePath { field: "kernel", .. }) => {}
            other => panic!("expected MissingProfilePath kernel, got {other:?}"),
        }
    }

    #[test]
    fn validate_profile_paths_accepts_existing_paths() {
        let dir = tempfile::tempdir().unwrap();
        let kernel = dir.path().join("vmlinuz");
        let initrd = dir.path().join("initrd");
        std::fs::write(&kernel, b"not really a kernel").unwrap();
        std::fs::write(&initrd, b"not really an initrd").unwrap();

        let profile = Profile {
            kernel,
            initrd,
            cmdline: "console=hvc0".into(),
            guest_os: "peios".into(),
            inject_agent: true,
            agent_overlay_path: None,
            cmdline_file: None,
            build: None,
            build_out: None,
        };
        validate_profile_paths(&profile).unwrap();
    }

    #[test]
    fn qemu_vmm_config_default_uses_x86_64_binary_and_default_port() {
        let cfg = QemuVmmConfig::defaults();
        assert!(cfg.qemu_binary.ends_with("qemu-system-x86_64"));
        assert_eq!(cfg.agent_port, DEFAULT_AGENT_PORT);
    }

    // -- Interactive console builder -------------------------------------

    fn fake_interactive<'a>(
        kernel: &'a Path,
        initrd: &'a Path,
        extra: &'a [String],
    ) -> InteractiveLaunchPlan<'a> {
        InteractiveLaunchPlan {
            qemu_binary: Path::new("qemu-system-x86_64"),
            kernel,
            initrd,
            cmdline: "console=hvc0 quiet",
            memory_bytes: 1024 * 1024 * 1024,
            cpus: 2,
            cid: None,
            ksm_enabled: true,
            extra_args: extra,
        }
    }

    #[test]
    fn interactive_muxes_serial_and_monitor_to_stdio() {
        let plan = fake_interactive(Path::new("/k"), Path::new("/i"), &[]);
        let cmd = build_interactive_qemu_command(&plan);
        let args = collect_args(&cmd);
        assert_eq!(arg_after(&args, "-serial").as_deref(), Some("mon:stdio"));
    }

    #[test]
    fn interactive_disables_display_and_default_devices() {
        let plan = fake_interactive(Path::new("/k"), Path::new("/i"), &[]);
        let cmd = build_interactive_qemu_command(&plan);
        let args = collect_args(&cmd);
        assert_eq!(arg_after(&args, "-display").as_deref(), Some("none"));
        assert!(args.iter().any(|a| a == "-nodefaults"));
        assert!(args.iter().any(|a| a == "-no-reboot"));
    }

    #[test]
    fn interactive_has_no_qmp_or_incoming() {
        let plan = fake_interactive(Path::new("/k"), Path::new("/i"), &[]);
        let cmd = build_interactive_qemu_command(&plan);
        let args = collect_args(&cmd);
        assert!(!args.iter().any(|a| a == "-qmp"), "got {args:?}");
        assert!(!args.iter().any(|a| a == "-incoming"), "got {args:?}");
    }

    #[test]
    fn interactive_ensures_serial_console_on_cmdline() {
        let plan = fake_interactive(Path::new("/k"), Path::new("/i"), &[]);
        let cmd = build_interactive_qemu_command(&plan);
        let args = collect_args(&cmd);
        let append = arg_after(&args, "-append").unwrap();
        assert!(append.contains("console=hvc0"), "preserves user console: {append}");
        assert!(append.contains("console=ttyS0"), "adds serial console: {append}");
    }

    #[test]
    fn interactive_omits_vsock_without_cid_and_includes_it_with() {
        let plan = fake_interactive(Path::new("/k"), Path::new("/i"), &[]);
        let cmd = build_interactive_qemu_command(&plan);
        let args = collect_args(&cmd);
        assert!(
            !args.iter().any(|a| a.contains("vhost-vsock")),
            "bare boot should not wire vsock, got {args:?}"
        );

        let mut plan = fake_interactive(Path::new("/k"), Path::new("/i"), &[]);
        plan.cid = Some(77);
        let cmd = build_interactive_qemu_command(&plan);
        let args = collect_args(&cmd);
        assert!(args.iter().any(|a| a == "vhost-vsock-pci,guest-cid=77"), "got {args:?}");
    }

    #[test]
    fn interactive_appends_extra_args_verbatim() {
        let extra = vec!["-drive".to_string(), "file=disk.img,if=virtio".to_string()];
        let plan = fake_interactive(Path::new("/k"), Path::new("/i"), &extra);
        let cmd = build_interactive_qemu_command(&plan);
        let args = collect_args(&cmd);
        assert_eq!(arg_after(&args, "-drive").as_deref(), Some("file=disk.img,if=virtio"));
    }

    #[test]
    fn interactive_propagates_kernel_and_initrd() {
        let plan = fake_interactive(Path::new("/k/img"), Path::new("/i/img"), &[]);
        let cmd = build_interactive_qemu_command(&plan);
        let args = collect_args(&cmd);
        assert_eq!(arg_after(&args, "-kernel").as_deref(), Some("/k/img"));
        assert_eq!(arg_after(&args, "-initrd").as_deref(), Some("/i/img"));
    }
}
