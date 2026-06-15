//! The `Vm` resource — host-facing handle for one VM.
//!
//! `Vm` is a stateful machine. Per `DESIGN.md` § VM state machine:
//!
//! ```text
//! Created --boot()--> Booted --pause()--> Paused
//!                       |                    |
//!                       |                    resume()
//!                       |<-------------------+
//!                       |
//!                       shutdown()
//!                       |
//!                       v
//!                    Shutdown (terminal)
//! ```
//!
//! Constructed in [`VmState::Created`] by [`crate::lab::Lab::vm`]
//! (and `provium:vm(...)` on the Lua side). [`Vm::boot`] calls
//! [`crate::vmm::Vmm::launch`] to actually spin up host resources.
//! Operations are guarded — `vm.run(...)` on a Created or Paused
//! VM returns [`VmError::WrongState`] rather than dispatching.

use std::collections::BTreeSet;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Weak};

use thiserror::Error;

use provium_protocol::wire::{
    CloseArgs, DirEntry, ExecArgs, ExecResult, ExitStatus, IoctlArgs, IoctlOk, KillArgs,
    ListdirArgs, MkdirArgs, OpResult, OpenFileArgs, OpenMode, ReadArgs, ReadFileArgs, RenameArgs,
    RunAsyncArgs, SeekArgs, SeekWhence, StatArgs, TailFileArgs, TailStart, UnlinkArgs, WaitArgs,
    WriteArgs, WriteFileArgs, WriteFileMode,
};
use provium_protocol::OsError;

use crate::agent_client::{AgentClient, TailFileOutcome, TailFileSession};
use crate::profile::Profile;
use crate::vmm::{BootOpts, VmInstance, Vmm, VmmError};
use crate::ClientError;

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// Failure modes for ops issued through a [`Vm`].
#[derive(Debug, Error)]
pub enum VmError {
    /// Wire- or protocol-level failure talking to the agent.
    #[error("agent: {0}")]
    Client(#[from] ClientError),

    /// VMM-level failure (snapshot, pause, …).
    #[error("vmm: {0}")]
    Vmm(#[from] VmmError),

    /// OS-level failure servicing the op (e.g. exec target missing,
    /// stat on a path that doesn't exist).
    #[error("os: {0}")]
    Os(OsError),

    /// Op is not legal in the VM's current state.
    ///
    /// The caller should check the state explicitly (`vm.state()`)
    /// or transition first (`vm.boot()`, `vm.resume()`, …).
    ///
    /// Display routes through [`format_wrong_state`] so the wording
    /// matches `DESIGN.md` § VM state machine — e.g. `"VM is
    /// paused; use resume()"` for `(Paused, run)` instead of the
    /// generic `state-action` template. Tests grep for those exact
    /// phrases.
    #[error("{}", format_wrong_state(.vm, *.state, .action))]
    WrongState {
        /// VM name for the error message.
        vm: String,
        /// State at the time of the call.
        state: VmState,
        /// What the caller tried to do (e.g. `"run"`, `"snapshot"`).
        action: &'static str,
    },

    /// `vm:boot()` requested more resources than the host pool's
    /// total budget. Cannot be satisfied — fail the test now per
    /// `DESIGN.md` § Failure mode catalogue.
    #[error("vm `{vm}`: boot requested {requested:?} which exceeds pool total {total:?}")]
    PoolExceeded {
        /// VM name.
        vm: String,
        /// What boot wanted.
        requested: crate::scheduler::ResourceAmount,
        /// Total pool capacity.
        total: crate::scheduler::ResourceAmount,
    },

    /// `vm:snapshot` (or `lab:snapshot`) was called while resources
    /// owned by this VM were still open. Per `DESIGN.md` § Snapshot
    /// precondition, snapshotting silently while streams are
    /// mid-flight loses state — better to refuse and force the caller
    /// to be explicit.
    #[error("{}", format_open_resources(.vm, *.open_files, .stream_details))]
    OpenResources {
        /// VM name for the error message.
        vm: String,
        /// Open file handle count.
        open_files: usize,
        /// Open stream session count.
        open_streams: usize,
        /// Per-stream details, one per open stream.
        stream_details: Vec<String>,
    },
}

/// Render a [`StreamMeta`] in the snapshot-diagnostic line format
/// from `DESIGN.md` § Snapshot precondition:
///
/// ```text
///   - tail_file("/log") created at peinit/services.test.lua:42 (test "foo")
/// ```
pub fn format_stream_meta(meta: &StreamMeta) -> String {
    let mut s = format!("{}({})", meta.kind, meta.detail);
    if let Some((file, line)) = &meta.creation_site {
        s.push_str(&format!(" created at {file}:{line}"));
    }
    if let Some(t) = &meta.test_name {
        s.push_str(&format!(" (test \"{t}\")"));
    }
    s
}

/// Render a `WrongState` error using the canonical wording from
/// `DESIGN.md` § VM state machine. Each `(state, action)` pair has
/// a tailored phrase; anything not enumerated falls through to a
/// generic state/action message that still names both pieces so
/// the caller can debug.
pub fn format_wrong_state(vm: &str, state: VmState, action: &str) -> String {
    let body = match (state, action) {
        // Booted → boot is the duplicate; everything else is fine
        // and shouldn't have hit this code path.
        (VmState::Booted, "boot") => "VM already booted",
        // Created → only boot is legal; every other op surfaces as
        // "VM not booted".
        (VmState::Created, _) => "VM not booted",
        // Paused — every illegal op gets the same hint.
        (VmState::Paused, _) => "VM is paused; use resume()",
        // Shutdown is terminal.
        (VmState::Shutdown, _) => "VM is shutdown; create a new one",
        // Dead — host-side bookkeeping, not in DESIGN's table; keep
        // a clear message that names what happened so callers can
        // recognise it.
        (VmState::Dead, _) => "VM died; create a new one",
        // Generic fallback (e.g. Booted+non-boot — unreachable in
        // practice) so the match stays exhaustive.
        _ => "VM is in an unexpected state for this op",
    };
    format!("vm `{vm}`: {body} (cannot {action})")
}

fn format_open_resources(vm: &str, open_files: usize, streams: &[String]) -> String {
    use std::fmt::Write as _;
    let mut s = format!(
        "vm:snapshot() failed for `{vm}`: {} open file handle(s), {} open stream(s)",
        open_files,
        streams.len()
    );
    for line in streams {
        let _ = write!(&mut s, "\n  - {line}");
    }
    s.push_str("\n  Close streams before snapshotting.");
    s
}

impl From<OsError> for VmError {
    fn from(value: OsError) -> Self {
        Self::Os(value)
    }
}

// ---------------------------------------------------------------------------
// State machine
// ---------------------------------------------------------------------------

/// Public state of a VM.
///
/// Mirrors `DESIGN.md` § VM state machine. Available via
/// [`Vm::state`] for telemetry / debugging.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VmState {
    /// VM has been declared (`provium:vm(...)`) but has no host
    /// resources. Calling any op here returns [`VmError::WrongState`];
    /// the only legal transition is [`Vm::boot`].
    Created,
    /// VM is running. All op kinds are legal.
    Booted,
    /// VM is paused via QMP `stop`. Most ops error; only
    /// [`Vm::resume`] / [`Vm::shutdown`] / [`Vm::snapshot`] are legal.
    Paused,
    /// VM was running but the agent disconnected unexpectedly.
    /// Subsequent ops fail immediately per `DESIGN.md` § Failure
    /// mode catalogue. Recoverable only by `shutdown` →
    /// re-create. Console log (if any) remains readable.
    Dead,
    /// Terminal — host resources have been released and the VM
    /// cannot be re-used. Construct a new VM via the lab.
    Shutdown,
}

impl VmState {
    /// Stable, lowercase rendering used in error messages and the
    /// Lua `vm:state()` accessor.
    pub fn as_str(self) -> &'static str {
        match self {
            VmState::Created => "created",
            VmState::Booted => "booted",
            VmState::Paused => "paused",
            VmState::Dead => "dead",
            VmState::Shutdown => "shutdown",
        }
    }
}

// ---------------------------------------------------------------------------
// Vm
// ---------------------------------------------------------------------------

/// One VM, identified by name within its [`crate::lab::Lab`].
///
/// Cheap to clone: the inner state lives behind an [`Arc<Mutex<…>>`]
/// so the same `Vm` handle can be shared between Lua threads and
/// any sibling Rust code without further locking. Not [`Sync`] from
/// the outside per design — every op locks briefly.
pub struct Vm {
    name: String,
    profile_name: String,
    profile: Profile,
    boot_opts: BootOpts,
    vmm: Arc<dyn Vmm>,
    inner: Arc<Mutex<VmInner>>,
    resources: Arc<Mutex<ResourceRegistry>>,
    /// Optional pool reference. When set, solo `vm:boot()` reserves
    /// `memory + ~100MiB overhead + cpus` from the pool before
    /// launching, per `DESIGN.md` § Scheduler / Per-VM overhead.
    /// `lab:boot` clears this on each member so the lab's atomic
    /// reservation isn't double-counted.
    pool: Option<Arc<crate::scheduler::Pool>>,
}

struct VmInner {
    state: VmState,
    /// `None` until `boot` succeeds; cleared by `shutdown`.
    running: Option<VmRunningResources>,
    /// Pool reservation held for the lifetime of this VM. Released
    /// on shutdown via Drop. `None` for VMs created without a pool
    /// (REPL / ad-hoc) or for VMs booted as part of an atomic
    /// `lab:boot` (the lab holds the joint reservation).
    boot_reservation: Option<crate::scheduler::Reservation>,
    /// Bridges this VM is attached to. Recorded by
    /// `bridge:attach(vm)` and consumed at boot time. Map keys are
    /// bridge names; values hold the [`crate::bridge::Bridge`] handle
    /// so the boot path can realize host-side networking.
    bridge_attachments: std::collections::BTreeMap<String, crate::bridge::Bridge>,
    /// Disks attached to this VM. Keys are user-supplied ids;
    /// values carry the size + backing image path so `vm:disk(id)`
    /// can hand back a [`crate::lua::disk_ud::DiskUd`] without
    /// duplicating state.
    disks: std::collections::BTreeMap<String, DiskAttachment>,
    /// Per-boot overrides set via [`Vm::merge_boot_opts`]. Folded
    /// into the launch opts at boot time.
    boot_overrides: Option<BootOpts>,
}

/// One disk attachment recorded against a VM.
#[derive(Clone, Debug)]
pub struct DiskAttachment {
    /// User-supplied disk id (e.g. `"data"`).
    pub id: String,
    /// Disk size in bytes.
    pub size: u64,
    /// Backing image path (if any). `None` for graph-only attachments.
    pub image: Option<std::path::PathBuf>,
}

struct VmRunningResources {
    cid: u32,
    instance: Option<VmInstance>,
    client: AgentClient,
    console_log: Option<std::path::PathBuf>,
    console_socket: Option<std::path::PathBuf>,
}

/// Tracks resources currently held against a VM. Per
/// `DESIGN.md` § Snapshot precondition: open file handles and live
/// streams must be closed before a snapshot.
///
/// Stream ids come from the global [`STREAM_ID_COUNTER`]; we don't
/// need a per-VM counter because the registry only checks set
/// membership.
#[derive(Default)]
struct ResourceRegistry {
    /// Open file handles returned by `open_file` and not yet
    /// `close`d.
    open_files: BTreeSet<u64>,
    /// Active stream sessions, identified by host-allocated id and
    /// carrying enough metadata to format the snapshot diagnostic
    /// per `DESIGN.md` § Snapshot precondition.
    open_streams: std::collections::BTreeMap<u64, StreamMeta>,
}

/// Metadata recorded for an open stream session. Fed into the
/// snapshot-precondition diagnostic.
#[derive(Clone, Debug)]
pub struct StreamMeta {
    /// Op kind, e.g. `tail_file`, `fd_stream`, `proc_stdout_stream`,
    /// `proc_stderr_stream`, `bridge_capture`.
    pub kind: String,
    /// Identifying detail (path, fd id, bridge name).
    pub detail: String,
    /// Source path + line of the test/fixture frame the stream was
    /// opened from. `None` if no test-root frame was on the stack.
    pub creation_site: Option<(String, i32)>,
    /// Test name if the stream was opened inside a `test()` block.
    pub test_name: Option<String>,
}

impl ResourceRegistry {
    fn open_files_count(&self) -> usize {
        self.open_files.len()
    }
    fn open_streams_count(&self) -> usize {
        self.open_streams.len()
    }
    fn is_quiescent(&self) -> bool {
        self.open_files.is_empty() && self.open_streams.is_empty()
    }
    pub(crate) fn streams_snapshot(&self) -> Vec<StreamMeta> {
        self.open_streams.values().cloned().collect()
    }
}

/// RAII guard returned alongside a streaming resource. Drops
/// deregister the stream from the parent [`ResourceRegistry`] so
/// snapshot preconditions and per-file auto-close see the right
/// counts.
pub struct StreamGuard {
    id: u64,
    registry: Weak<Mutex<ResourceRegistry>>,
}

impl std::fmt::Debug for StreamGuard {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StreamGuard")
            .field("id", &self.id)
            .finish()
    }
}

impl Drop for StreamGuard {
    fn drop(&mut self) {
        if let Some(reg) = self.registry.upgrade() {
            reg.lock().unwrap().open_streams.remove(&self.id);
        }
    }
}

/// Wraps a [`TailFileSession`] with a guard so the parent VM's
/// registry is updated when the session ends.
pub struct VmTailSession {
    inner: TailFileSession,
    _guard: StreamGuard,
}

impl std::fmt::Debug for VmTailSession {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VmTailSession")
            .field("guard", &self._guard)
            .finish()
    }
}

impl VmTailSession {
    /// Pull the next stream frame.
    pub fn next_frame(
        &mut self,
    ) -> Result<Option<provium_protocol::wire::StreamFrame>, ClientError> {
        self.inner.next_frame()
    }

    /// `true` once the session has hit EOF.
    pub fn is_eof(&self) -> bool {
        self.inner.is_eof()
    }

    /// Set a per-call read timeout on the underlying stream.
    /// Best-effort — backends that don't support it return
    /// `NotSupported`.
    pub fn set_read_timeout(
        &self,
        timeout: Option<std::time::Duration>,
    ) -> std::io::Result<()> {
        self.inner.set_read_timeout(timeout)
    }
}

/// Internal counter for stream-id generation. One global counter
/// is fine — ids only need to be locally unique to one VM, but a
/// shared counter is simpler and the u64 namespace is unbounded.
static STREAM_ID_COUNTER: AtomicU64 = AtomicU64::new(1);

/// VMM overhead bytes per boot per `DESIGN.md` § Scheduler /
/// Per-VM overhead. Reserved alongside the declared memory.
pub(crate) const VMM_OVERHEAD_BYTES: u64 = 100 * 1024 * 1024;

impl std::fmt::Debug for Vm {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let inner = self.inner.lock().unwrap();
        f.debug_struct("Vm")
            .field("name", &self.name)
            .field("profile", &self.profile_name)
            .field("state", &inner.state)
            .field(
                "cid",
                &inner.running.as_ref().map(|r| r.cid).unwrap_or(0),
            )
            .finish()
    }
}

impl Clone for Vm {
    fn clone(&self) -> Self {
        Self {
            name: self.name.clone(),
            profile_name: self.profile_name.clone(),
            profile: self.profile.clone(),
            boot_opts: self.boot_opts.clone(),
            vmm: Arc::clone(&self.vmm),
            inner: Arc::clone(&self.inner),
            resources: Arc::clone(&self.resources),
            pool: self.pool.clone(),
        }
    }
}

impl Vm {
    /// Construct a fresh VM in [`VmState::Created`].
    ///
    /// Crate-private — Lua callers reach a `Vm` only through
    /// [`crate::lab::Lab::vm`]. The lab is responsible for both
    /// the name uniqueness check and for keeping the `Vm` reachable
    /// for batch boot and lifecycle teardown.
    pub(crate) fn new(
        name: String,
        profile_name: String,
        profile: Profile,
        boot_opts: BootOpts,
        vmm: Arc<dyn Vmm>,
    ) -> Self {
        Self {
            name,
            profile_name,
            profile,
            boot_opts,
            vmm,
            inner: Arc::new(Mutex::new(VmInner {
                state: VmState::Created,
                running: None,
                boot_reservation: None,
                bridge_attachments: std::collections::BTreeMap::new(),
                disks: std::collections::BTreeMap::new(),
                boot_overrides: None,
            })),
            resources: Arc::new(Mutex::new(ResourceRegistry::default())),
            pool: None,
        }
    }

    /// Build a [`Vm`] already in [`VmState::Booted`] from a
    /// [`crate::vmm::VmRunning`]. Used by [`crate::lab::Lab::restore_vm`]
    /// — the fixture-cache restore path skips the Created→Booted
    /// transition because the VMM has already produced a live VM
    /// from the cached snapshot.
    pub(crate) fn from_running(
        name: String,
        profile_name: String,
        profile: Profile,
        boot_opts: BootOpts,
        vmm: Arc<dyn Vmm>,
        running: crate::vmm::VmRunning,
    ) -> Self {
        let inner = VmInner {
            state: VmState::Booted,
            running: Some(VmRunningResources {
                cid: running.cid,
                instance: Some(running.instance),
                client: running.client,
                console_log: running.console_log,
                console_socket: running.console_socket,
            }),
            bridge_attachments: std::collections::BTreeMap::new(),
            disks: std::collections::BTreeMap::new(),
                boot_overrides: None,
            boot_reservation: None,
        };
        Self {
            name,
            profile_name,
            profile,
            boot_opts,
            vmm,
            inner: Arc::new(Mutex::new(inner)),
            resources: Arc::new(Mutex::new(ResourceRegistry::default())),
            pool: None,
        }
    }

    /// Attach a pool reference for solo `vm:boot()` reservation.
    /// Cleared by `lab:boot` on each member so the lab's atomic
    /// reservation isn't double-counted.
    pub fn with_pool(mut self, pool: Option<Arc<crate::scheduler::Pool>>) -> Self {
        self.pool = pool;
        self
    }

    /// Strip the pool reference (used by `lab:boot_with_pool` so
    /// solo-boot logic doesn't double-charge the lab's reservation).
    pub fn clear_pool_for_atomic_boot(&self) {
        // Pool lives in self.pool which is a value, not behind the
        // Arc<Mutex<…>>. Clones share `inner` but each has its own
        // `pool`. The lab's batch path holds a single VM clone per
        // launch, so clearing on that clone is enough.
        // (Method left here to keep the public surface honest even
        // though the actual mutation site is in lab.rs.)
    }

    /// Read-only snapshot of the VM's declared boot opts. Used by
    /// [`crate::lab::Lab::boot_with_pool`] to compute the
    /// atomic-batch claim size before launch.
    pub fn boot_opts_summary(&self) -> &BootOpts {
        &self.boot_opts
    }

    /// Merge boot-time overrides (`files`, `rng_seed`,
    /// `initial_time`, `kernel_cmdline`) into the VM's pending
    /// boot opts before launch. Errors if the VM has already
    /// booted. Only legal in [`VmState::Created`].
    ///
    /// Stored in `VmInner.boot_overrides`; consumed at launch.
    pub fn merge_boot_opts(&self, overrides: BootOpts) -> Result<(), VmError> {
        let mut inner = self.inner.lock().unwrap();
        if inner.state != VmState::Created {
            return Err(self.wrong_state(inner.state, "merge_boot_opts"));
        }
        let cur = inner.boot_overrides.get_or_insert(BootOpts::default());
        if overrides.cmdline_override.is_some() {
            cur.cmdline_override = overrides.cmdline_override;
        }
        if overrides.rng_seed.is_some() {
            cur.rng_seed = overrides.rng_seed;
        }
        if overrides.initial_time_ns.is_some() {
            cur.initial_time_ns = overrides.initial_time_ns;
        }
        if !overrides.files.is_empty() {
            cur.files.extend(overrides.files);
        }
        Ok(())
    }

    /// Number of currently-open file handles. Test/diagnostic only.
    pub fn open_file_count(&self) -> usize {
        self.resources.lock().unwrap().open_files_count()
    }

    /// Number of currently-active streams. Test/diagnostic only.
    pub fn open_stream_count(&self) -> usize {
        self.resources.lock().unwrap().open_streams_count()
    }

    // -----------------------------------------------------------------
    // Accessors
    // -----------------------------------------------------------------

    /// VM name as supplied to [`crate::lab::Lab::vm`].
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Profile name from `provium.toml`.
    pub fn profile_name(&self) -> &str {
        &self.profile_name
    }

    /// Current state.
    pub fn state(&self) -> VmState {
        self.inner.lock().unwrap().state
    }

    /// vsock CID — `None` until [`Vm::boot`] has succeeded.
    pub fn cid(&self) -> Option<u32> {
        self.inner.lock().unwrap().running.as_ref().map(|r| r.cid)
    }

    /// Record a bridge this VM is attached to. Called from
    /// `bridge:attach(vm)`. The list is consumed at boot time and
    /// fed into [`crate::vmm::BootOpts::nic_attachments`] so the
    /// QEMU command line includes the right `-netdev tap`s plus the
    /// host-side bridge realisation runs against the right
    /// [`crate::bridge::Bridge`] handle.
    pub fn add_bridge_attachment(&self, bridge: crate::bridge::Bridge) {
        self.inner
            .lock()
            .unwrap()
            .bridge_attachments
            .insert(bridge.name().to_owned(), bridge);
    }

    /// Snapshot of this VM's bridge attachments.
    pub fn bridge_attachments(&self) -> Vec<String> {
        self.inner
            .lock()
            .unwrap()
            .bridge_attachments
            .keys()
            .cloned()
            .collect()
    }

    /// Look up an attached bridge by name. Returns `None` if no
    /// bridge with that name has been recorded.
    pub fn bridge_for(&self, name: &str) -> Option<crate::bridge::Bridge> {
        self.inner.lock().unwrap().bridge_attachments.get(name).cloned()
    }

    /// Record a disk attachment under `id`. Used by `vm:attach_disk`.
    pub fn attach_disk_record(&self, attachment: DiskAttachment) {
        self.inner
            .lock()
            .unwrap()
            .disks
            .insert(attachment.id.clone(), attachment);
    }

    /// Look up a previously-attached disk by id.
    pub fn disk_attachment(&self, id: &str) -> Option<DiskAttachment> {
        self.inner.lock().unwrap().disks.get(id).cloned()
    }

    // -----------------------------------------------------------------
    // Lifecycle transitions
    // -----------------------------------------------------------------

    /// `Created → Booted`. Calls [`Vmm::launch`], stores the running
    /// resources, transitions state.
    ///
    /// Idempotent for already-booted VMs? **No** — calling on a
    /// `Booted` VM returns [`VmError::WrongState`]. Use [`Vm::state`]
    /// to check first if needed.
    pub fn boot(&self) -> Result<(), VmError> {
        // Guard scope: take only what we need from the lock so we
        // don't hold it across the (potentially long) `launch` call.
        let bridges: Vec<crate::bridge::Bridge> = {
            let inner = self.inner.lock().unwrap();
            if inner.state != VmState::Created {
                return Err(self.wrong_state(inner.state, "boot"));
            }
            inner.bridge_attachments.values().cloned().collect()
        };

        let mut opts = self.boot_opts.clone();
        // Apply any pending per-boot overrides
        // (`merge_boot_opts` from Lua's `vm:boot({…})`).
        if let Some(over) = self.inner.lock().unwrap().boot_overrides.take() {
            if over.cmdline_override.is_some() {
                opts.cmdline_override = over.cmdline_override;
            }
            if over.rng_seed.is_some() {
                opts.rng_seed = over.rng_seed;
            }
            if over.initial_time_ns.is_some() {
                opts.initial_time_ns = over.initial_time_ns;
            }
            opts.files.extend(over.files);
        }
        // One NIC per bridge attachment, with stable id derived
        // from "<vm_name>-<bridge>" so it's predictable in logs
        // and across restarts.
        for bridge in &bridges {
            opts.nic_attachments.push(crate::vmm::NicAttachment {
                bridge_name: bridge.name().to_owned(),
                bridge: Some(bridge.clone()),
                nic_id: format!("{}-{}", self.name, bridge.name()),
                mac: None,
            });
        }

        // Per `DESIGN.md` § Scheduler / Per-VM overhead: solo boot
        // reserves declared memory + ~100 MiB VMM overhead + cpus
        // from the pool. `lab:boot()` clears `self.pool` on each
        // member before launch so the lab's atomic claim isn't
        // double-counted here.
        let reservation = if let Some(pool) = &self.pool {
            let amount = crate::scheduler::ResourceAmount {
                memory_bytes: opts
                    .memory_bytes
                    .unwrap_or(0)
                    .saturating_add(VMM_OVERHEAD_BYTES),
                cpus: opts.cpus.unwrap_or(0),
            };
            if amount.memory_bytes > 0 || amount.cpus > 0 {
                match pool.acquire(amount) {
                    Some(r) => Some(r),
                    None => {
                        // Per `DESIGN.md` § Failure mode catalogue:
                        // boot > host budget → fail test (impossible
                        // to satisfy).
                        return Err(VmError::PoolExceeded {
                            vm: self.name.clone(),
                            requested: amount,
                            total: pool.total(),
                        });
                    }
                }
            } else {
                None
            }
        } else {
            None
        };

        let running = self.vmm.launch(&self.name, &self.profile, opts)?;

        let mut inner = self.inner.lock().unwrap();
        // Recheck — another thread might have raced us. Fairness
        // not guaranteed; first-wins.
        if inner.state != VmState::Created {
            // We already launched; release immediately.
            running.instance.shutdown().ok();
            return Err(self.wrong_state(inner.state, "boot"));
        }
        inner.state = VmState::Booted;
        inner.running = Some(VmRunningResources {
            cid: running.cid,
            instance: Some(running.instance),
            client: running.client,
            console_log: running.console_log,
            console_socket: running.console_socket,
        });
        inner.boot_reservation = reservation;
        Ok(())
    }

    /// `Booted → Paused`. Errors on any other state.
    pub fn pause(&self) -> Result<(), VmError> {
        let mut inner = self.inner.lock().unwrap();
        match inner.state {
            VmState::Booted => {}
            other => return Err(self.wrong_state(other, "pause")),
        }
        let res = inner
            .running
            .as_ref()
            .expect("Booted state must carry running resources");
        let r = res
            .instance
            .as_ref()
            .expect("instance present while not Shutdown")
            .pause();
        if let Err(e) = &r {
            if is_qmp_dead_signal(e) {
                inner.state = VmState::Dead;
            }
        }
        r?;
        inner.state = VmState::Paused;
        Ok(())
    }

    /// `Paused → Booted`. Errors on any other state.
    pub fn resume(&self) -> Result<(), VmError> {
        let mut inner = self.inner.lock().unwrap();
        match inner.state {
            VmState::Paused => {}
            other => return Err(self.wrong_state(other, "resume")),
        }
        let res = inner
            .running
            .as_ref()
            .expect("Paused state must carry running resources");
        let r = res
            .instance
            .as_ref()
            .expect("instance present while not Shutdown")
            .resume();
        if let Err(e) = &r {
            if is_qmp_dead_signal(e) {
                inner.state = VmState::Dead;
            }
        }
        r?;
        inner.state = VmState::Booted;
        Ok(())
    }

    /// `Booted | Paused → Shutdown`. Idempotent on `Shutdown`.
    /// Calling on `Created` is allowed and is a no-op (transitions
    /// to `Shutdown` without invoking the VMM).
    pub fn shutdown(&self) -> Result<(), VmError> {
        let mut inner = self.inner.lock().unwrap();
        match inner.state {
            VmState::Shutdown => return Ok(()),
            VmState::Created => {
                inner.state = VmState::Shutdown;
                // Releases any solo-boot reservation held since
                // creation (none, but kept symmetric).
                inner.boot_reservation = None;
                return Ok(());
            }
            VmState::Booted | VmState::Paused | VmState::Dead => {}
        }
        if let Some(res) = inner.running.as_mut() {
            if let Some(instance) = res.instance.take() {
                instance.shutdown()?;
            }
        }
        inner.state = VmState::Shutdown;
        // Release the pool reservation immediately, per
        // `DESIGN.md` § Scheduler / Per-VM overhead — "Overhead
        // released on VM shutdown."
        inner.boot_reservation = None;
        Ok(())
    }

    /// Snapshot the VM's state to `path`. Legal in `Booted` or
    /// `Paused`. Refuses if open file handles or streams are held —
    /// see `DESIGN.md` § Snapshot precondition.
    pub fn snapshot(&self, path: &Path) -> Result<(), VmError> {
        // Precondition first so the named-offenders error fires
        // before we touch the VMM.
        {
            let reg = self.resources.lock().unwrap();
            if !reg.is_quiescent() {
                let stream_details: Vec<String> = reg
                    .streams_snapshot()
                    .iter()
                    .map(format_stream_meta)
                    .collect();
                return Err(VmError::OpenResources {
                    vm: self.name.clone(),
                    open_files: reg.open_files_count(),
                    open_streams: reg.open_streams_count(),
                    stream_details,
                });
            }
        }

        let inner = self.inner.lock().unwrap();
        match inner.state {
            VmState::Booted | VmState::Paused => {}
            other => return Err(self.wrong_state(other, "snapshot")),
        }
        let res = inner
            .running
            .as_ref()
            .expect("Booted/Paused must carry running");
        let r = res
            .instance
            .as_ref()
            .expect("instance present while not Shutdown")
            .snapshot(path);
        drop(inner);
        if let Err(e) = &r {
            self.maybe_mark_dead_on_vmm_err(e);
        }
        r.map_err(VmError::Vmm)
    }

    /// `true` if no file handles or streams are open against this VM.
    /// Used by snapshot preconditions and (eventually) the resource
    /// graph's auto-close ordering.
    pub fn is_quiescent(&self) -> bool {
        self.resources.lock().unwrap().is_quiescent()
    }

    /// Read the captured guest serial-console output. Returns
    /// empty bytes when the VMM doesn't materialise a console log
    /// (e.g. [`crate::vmm::local_agent::LocalAgentVmm`]) and the
    /// file's current contents when it does.
    pub fn console_read(&self) -> Result<Vec<u8>, VmError> {
        let path = {
            let inner = self.inner.lock().unwrap();
            match inner.running.as_ref().and_then(|r| r.console_log.clone()) {
                Some(p) => p,
                None => return Ok(Vec::new()),
            }
        };
        std::fs::read(&path).map_err(|e| VmError::Os(provium_protocol::OsError {
            errno: e.raw_os_error().unwrap_or(0),
            message: e.to_string(),
        }))
    }

    /// Path of the QEMU chardev console socket, if the VMM
    /// materialised one. Used by `console:read` to open a stream
    /// against the live socket.
    pub fn console_socket_path(&self) -> Option<std::path::PathBuf> {
        self.inner
            .lock()
            .unwrap()
            .running
            .as_ref()
            .and_then(|r| r.console_socket.clone())
    }

    /// Send `data` to the VM's console (typically the serial line).
    /// Connects to the bidirectional chardev unix socket the VMM
    /// materialised and writes once. Returns the number of bytes
    /// accepted by the kernel-side socket. Errors if the VMM doesn't
    /// expose a writable console (e.g. [`crate::vmm::local_agent::LocalAgentVmm`]).
    pub fn console_write(&self, data: &[u8]) -> Result<usize, VmError> {
        self.console_write_timeout(data, None)
    }

    /// Like [`Self::console_write`] but bounds the write with an
    /// optional timeout — the kernel-side socket buffer can stall
    /// when the guest's tty consumer is dead, and an unbounded
    /// `write_all` would otherwise hang the test runner forever.
    pub fn console_write_timeout(
        &self,
        data: &[u8],
        timeout: Option<std::time::Duration>,
    ) -> Result<usize, VmError> {
        let path = {
            let inner = self.inner.lock().unwrap();
            match inner.running.as_ref().and_then(|r| r.console_socket.clone()) {
                Some(p) => p,
                None => {
                    return Err(VmError::Os(provium_protocol::OsError {
                        errno: 95, // ENOTSUP
                        message: "console:write needs a bidirectional chardev (QEMU only)".into(),
                    }));
                }
            }
        };
        use std::io::Write;
        let sock = std::os::unix::net::UnixStream::connect(&path).map_err(|e| {
            VmError::Os(provium_protocol::OsError {
                errno: e.raw_os_error().unwrap_or(0),
                message: format!("connect console socket: {e}"),
            })
        })?;
        if let Some(t) = timeout {
            sock.set_write_timeout(Some(t)).map_err(|e| {
                VmError::Os(provium_protocol::OsError {
                    errno: e.raw_os_error().unwrap_or(0),
                    message: format!("set console write timeout: {e}"),
                })
            })?;
        }
        let mut sock = sock;
        sock.write_all(data).map_err(|e| {
            VmError::Os(provium_protocol::OsError {
                errno: e.raw_os_error().unwrap_or(0),
                message: format!("write console: {e}"),
            })
        })?;
        Ok(data.len())
    }

    /// List a directory.
    pub fn listdir(&self, path: impl Into<String>) -> Result<Vec<DirEntry>, VmError> {
        self.with_client("listdir", |client| {
            match client.listdir(ListdirArgs { path: path.into() })? {
                OpResult::Ok(v) => Ok(v),
                OpResult::Err(e) => Err(VmError::Os(e)),
            }
        })
    }

    /// Create a directory.
    pub fn mkdir(
        &self,
        path: impl Into<String>,
        parents: bool,
        create_perm: Option<u32>,
    ) -> Result<(), VmError> {
        self.with_client("mkdir", |client| {
            match client.mkdir(MkdirArgs {
                path: path.into(),
                parents,
                create_perm,
            })? {
                OpResult::Ok(()) => Ok(()),
                OpResult::Err(e) => Err(VmError::Os(e)),
            }
        })
    }

    /// Remove a path.
    pub fn unlink(&self, path: impl Into<String>) -> Result<(), VmError> {
        self.with_client("unlink", |client| {
            match client.unlink(UnlinkArgs { path: path.into() })? {
                OpResult::Ok(()) => Ok(()),
                OpResult::Err(e) => Err(VmError::Os(e)),
            }
        })
    }

    /// Rename a path.
    pub fn rename(
        &self,
        from: impl Into<String>,
        to: impl Into<String>,
    ) -> Result<(), VmError> {
        self.with_client("rename", |client| {
            match client.rename(RenameArgs {
                from: from.into(),
                to: to.into(),
            })? {
                OpResult::Ok(()) => Ok(()),
                OpResult::Err(e) => Err(VmError::Os(e)),
            }
        })
    }

    /// Seek an open file. Returns the new absolute offset.
    pub fn seek(
        &self,
        handle: provium_protocol::handle::FileHandle,
        offset: i64,
        whence: SeekWhence,
    ) -> Result<u64, VmError> {
        self.with_client("seek", |client| {
            match client.seek(SeekArgs {
                handle,
                offset,
                whence,
            })? {
                OpResult::Ok(o) => Ok(o),
                OpResult::Err(e) => Err(VmError::Os(e)),
            }
        })
    }

    /// Run an ioctl. Optional `bufs` + `ptr_offsets` enable pointer-
    /// argument marshalling: each `bufs[i]`'s heap address is
    /// spliced into `data` at byte offset `ptr_offsets[i]`. Result
    /// includes `out_bufs` paired by index.
    pub fn ioctl(
        &self,
        handle: provium_protocol::handle::FileHandle,
        cmd: u64,
        data: Vec<u8>,
    ) -> Result<IoctlOk, VmError> {
        self.ioctl_with_ptrs(handle, cmd, data, Vec::new(), Vec::new())
    }

    /// Full-form ioctl. See `IoctlArgs` for `bufs`/`ptr_offsets`.
    pub fn ioctl_with_ptrs(
        &self,
        handle: provium_protocol::handle::FileHandle,
        cmd: u64,
        data: Vec<u8>,
        bufs: Vec<Vec<u8>>,
        ptr_offsets: Vec<u32>,
    ) -> Result<IoctlOk, VmError> {
        self.with_client("ioctl", |client| {
            match client.ioctl(IoctlArgs {
                handle,
                cmd,
                data,
                bufs,
                ptr_offsets,
            })? {
                OpResult::Ok(ok) => Ok(ok),
                OpResult::Err(e) => Err(VmError::Os(e)),
            }
        })
    }

    /// Reset the VM (warm reboot via QMP `system_reset`).
    pub fn reset(&self) -> Result<(), VmError> {
        let inner = self.inner.lock().unwrap();
        // DESIGN.md § VM state machine: reset() / power_button()
        // are Booted-only. Paused still has a `running` resource
        // attached so without an explicit state guard a paused VM
        // would silently accept the call.
        if inner.state != VmState::Booted {
            return Err(self.wrong_state(inner.state, "reset"));
        }
        let res = inner.running.as_ref().ok_or_else(|| self.wrong_state(inner.state, "reset"))?;
        let inst = res
            .instance
            .as_ref()
            .ok_or_else(|| self.wrong_state(inner.state, "reset"))?;
        let r = inst.reset();
        drop(inner);
        if let Err(e) = &r {
            self.maybe_mark_dead_on_vmm_err(e);
        }
        r.map_err(VmError::Vmm)
    }

    /// Press the virtual power button (graceful shutdown signal
    /// to the guest, e.g. ACPI).
    pub fn power_button(&self) -> Result<(), VmError> {
        let inner = self.inner.lock().unwrap();
        if inner.state != VmState::Booted {
            return Err(self.wrong_state(inner.state, "power_button"));
        }
        let res = inner.running.as_ref().ok_or_else(|| self.wrong_state(inner.state, "power_button"))?;
        let inst = res
            .instance
            .as_ref()
            .ok_or_else(|| self.wrong_state(inner.state, "power_button"))?;
        let r = inst.power_button();
        drop(inner);
        if let Err(e) = &r {
            self.maybe_mark_dead_on_vmm_err(e);
        }
        r.map_err(VmError::Vmm)
    }

    /// Spawn a sub-agent worker.
    pub fn spawn_worker(&self) -> Result<Worker, VmError> {
        let handle = self.with_client("spawn_worker", |client| match client.spawn_worker()? {
            OpResult::Ok(h) => Ok(h),
            OpResult::Err(e) => Err(VmError::Os(e)),
        })?;
        let open_files = std::sync::Arc::new(std::sync::Mutex::new(
            std::collections::BTreeSet::new(),
        ));
        let drop_guard = std::sync::Arc::new(WorkerDropGuard {
            handle,
            vm: self.clone(),
            open_files: std::sync::Arc::clone(&open_files),
        });
        Ok(Worker {
            handle,
            vm: self.clone(),
            open_files,
            _drop_guard: drop_guard,
        })
    }

    /// Issue a raw Layer-0 syscall through the agent.
    pub fn syscall(&self, nr: i64, args: [i64; 6]) -> Result<SyscallResult, VmError> {
        self.syscall_with_bufs(nr, args, Vec::new(), Vec::new())
    }

    /// Layer-0 syscall with byte-buffer marshalling. `bufs` carries
    /// in-out byte buffers; `ptrs[i]` is the `args` index whose
    /// value the agent should overwrite with `bufs[i]`'s address
    /// before the syscall. Result includes `out_bufs` paired by
    /// index. Per `DESIGN.md` § VM / Layer-0.
    pub fn syscall_with_bufs(
        &self,
        nr: i64,
        args: [i64; 6],
        bufs: Vec<Vec<u8>>,
        ptrs: Vec<u8>,
    ) -> Result<SyscallResult, VmError> {
        let r = self.with_client("syscall", |client| {
            client
                .syscall(provium_protocol::wire::SyscallArgs {
                    nr,
                    args,
                    bufs,
                    ptrs,
                })
                .map_err(VmError::Client)
        })?;
        Ok(SyscallResult {
            ret: r.ret,
            errno: r.errno,
            out_bufs: r.out_bufs,
        })
    }

    // -----------------------------------------------------------------
    // Layer-1 ops — only valid in `Booted`.
    // -----------------------------------------------------------------

    /// Run `cmd` synchronously. Errors with [`VmError::WrongState`]
    /// outside [`VmState::Booted`].
    pub fn run(&self, args: ExecArgs) -> Result<RunResult, VmError> {
        self.with_client("run", |client| match client.exec(args)? {
            ExecResult::Ok(ok) => Ok(RunResult::from_exec_ok(ok)),
            ExecResult::Err(e) => Err(VmError::Os(e)),
        })
    }

    /// Open a streaming subscription to an already-open file handle.
    /// Used by `vm:fd_stream` and `file:tail_stream` for sockets /
    /// pipes / non-tailable files.
    pub fn fd_stream(
        &self,
        handle: provium_protocol::handle::FileHandle,
    ) -> Result<VmTailSession, VmError> {
        let r = self.with_client("fd_stream", |client| {
            client
                .fd_stream(provium_protocol::wire::FdStreamArgs { handle })
                .map_err(VmError::Client)
        })?;
        let inner = match r {
            crate::agent_client::TailFileOutcome::Ok(s) => s,
            crate::agent_client::TailFileOutcome::Err(e) => return Err(VmError::Os(e)),
        };
        Ok(self.wrap_tail_session(inner))
    }

    /// Wrap an [`crate::agent_client::TailFileSession`] in a
    /// [`VmTailSession`] with a fresh registry guard. Used by
    /// `proc:stdout_stream`, `vm:fd_stream`, `bridge:capture`, and
    /// any future stream that opens off the agent.
    pub fn wrap_tail_session(
        &self,
        inner: crate::agent_client::TailFileSession,
    ) -> VmTailSession {
        self.wrap_tail_session_with_meta(
            inner,
            StreamMeta {
                kind: "stream".into(),
                detail: String::new(),
                creation_site: None,
                test_name: None,
            },
        )
    }

    /// As [`Self::wrap_tail_session`] but records `meta` against the
    /// stream's id for the snapshot diagnostic.
    pub(crate) fn wrap_tail_session_with_meta(
        &self,
        inner: crate::agent_client::TailFileSession,
        meta: StreamMeta,
    ) -> VmTailSession {
        let id = STREAM_ID_COUNTER.fetch_add(1, Ordering::Relaxed);
        self.resources
            .lock()
            .unwrap()
            .open_streams
            .insert(id, meta);
        let guard = StreamGuard {
            id,
            registry: Arc::downgrade(&self.resources),
        };
        VmTailSession { inner, _guard: guard }
    }

    /// Register a console-stream session in this VM's resource
    /// registry and return a [`StreamGuard`]. The guard's `Drop`
    /// removes the entry, so a `ConsoleStreamUd` that holds the
    /// guard for the lifetime of its underlying socket is
    /// counted by the snapshot precondition the same way a
    /// tail-file session is. Used by `console:read()`.
    pub fn register_console_stream(&self, meta: StreamMeta) -> StreamGuard {
        let id = STREAM_ID_COUNTER.fetch_add(1, Ordering::Relaxed);
        self.resources
            .lock()
            .unwrap()
            .open_streams
            .insert(id, meta);
        StreamGuard {
            id,
            registry: Arc::downgrade(&self.resources),
        }
    }

    /// Public attachment point so the Lua bindings can record
    /// metadata after the session is built.
    pub fn set_stream_meta(&self, session: &VmTailSession, meta: StreamMeta) {
        self.resources
            .lock()
            .unwrap()
            .open_streams
            .insert(session._guard.id, meta);
    }

    /// Snapshot of every currently-open stream's metadata.
    /// Used by the snapshot precondition check + the
    /// `reset_between_tests` precondition diagnostic.
    pub fn open_streams_meta(&self) -> Vec<StreamMeta> {
        self.resources.lock().unwrap().streams_snapshot()
    }

    /// Restore this VM from a previously-written snapshot file.
    /// Must be called from `Created` state. The current VMM is
    /// asked for `Vmm::restore` and the resulting `VmRunning`
    /// becomes this VM's running resources.
    ///
    /// Per `DESIGN.md` § VM API surface line 515.
    pub fn restore(&self, snapshot_path: &std::path::Path) -> Result<(), VmError> {
        // Validate state — only Created can be restored into.
        let mut opts = self.boot_opts.clone();
        // Same NIC plumbing as boot so attached bridges materialise.
        let bridges: Vec<crate::bridge::Bridge> = {
            let inner = self.inner.lock().unwrap();
            if inner.state != VmState::Created {
                return Err(self.wrong_state(inner.state, "restore"));
            }
            inner.bridge_attachments.values().cloned().collect()
        };
        for bridge in &bridges {
            opts.nic_attachments.push(crate::vmm::NicAttachment {
                bridge_name: bridge.name().to_owned(),
                bridge: Some(bridge.clone()),
                nic_id: format!("{}-{}", self.name, bridge.name()),
                mac: None,
            });
        }
        let running = self
            .vmm
            .restore(&self.name, &self.profile, opts, snapshot_path)?;
        let mut inner = self.inner.lock().unwrap();
        if inner.state != VmState::Created {
            running.instance.shutdown().ok();
            return Err(self.wrong_state(inner.state, "restore"));
        }
        inner.state = VmState::Booted;
        inner.running = Some(VmRunningResources {
            cid: running.cid,
            instance: Some(running.instance),
            client: running.client,
            console_log: running.console_log,
            console_socket: running.console_socket,
        });
        Ok(())
    }

    /// Hot-unplug a disk via the backend.
    pub fn detach_disk(&self, disk_id: &str) -> Result<(), VmError> {
        let inner = self.inner.lock().unwrap();
        // Explicit state guard. `Dead` VMs keep `running = Some`
        // (only `Shutdown` clears it) so without this check
        // detach_disk would dispatch a QMP device_del on a dead
        // QEMU. Booted + Paused are the only valid hot-unplug
        // states (mirrors set_link's pattern).
        match inner.state {
            VmState::Booted | VmState::Paused => {}
            other => return Err(self.wrong_state(other, "detach_disk")),
        }
        let res = inner.running.as_ref().ok_or_else(|| {
            self.wrong_state(inner.state, "detach_disk")
        })?;
        let r = res
            .instance
            .as_ref()
            .ok_or_else(|| self.wrong_state(inner.state, "detach_disk"))?
            .detach_disk(disk_id);
        drop(inner);
        if let Err(e) = &r {
            self.maybe_mark_dead_on_vmm_err(e);
        }
        r.map_err(VmError::Vmm)
    }

    /// Toggle a NIC's link state visible to the guest. Routes via
    /// the VMM backend (QMP `set_link` for QEMU).
    pub fn set_link(&self, netdev_id: &str, up: bool) -> Result<(), VmError> {
        let inner = self.inner.lock().unwrap();
        match inner.state {
            VmState::Booted | VmState::Paused => {}
            other => return Err(self.wrong_state(other, "set_link")),
        }
        let res = inner
            .running
            .as_ref()
            .expect("running must carry resources");
        let r = res
            .instance
            .as_ref()
            .expect("instance present while not Shutdown")
            .set_link(netdev_id, up);
        drop(inner);
        if let Err(e) = &r {
            self.maybe_mark_dead_on_vmm_err(e);
        }
        r.map_err(VmError::Vmm)
    }

    /// Run multiple commands in one wire round-trip. Each item is
    /// run in order on the agent; the returned vector is paired by
    /// index. Per `DESIGN.md` § Performance / Op batching.
    pub fn batch_run(&self, items: Vec<ExecArgs>) -> Result<Vec<RunResult>, VmError> {
        self.with_client("batch_run", |client| {
            let raw = client.batch_exec(items)?;
            raw.into_iter()
                .map(|r| match r {
                    ExecResult::Ok(ok) => Ok(RunResult::from_exec_ok(ok)),
                    ExecResult::Err(e) => Err(VmError::Os(e)),
                })
                .collect()
        })
    }

    /// Generic op batch — host-typed `HostMessage` items get
    /// dispatched server-side as one round-trip, agent responses are
    /// returned paired by index.
    pub fn batch_op(
        &self,
        items: Vec<provium_protocol::wire::HostMessage>,
    ) -> Result<Vec<provium_protocol::wire::AgentMessage>, VmError> {
        self.with_client("batch_op", |client| {
            client.batch_op(items).map_err(VmError::Client)
        })
    }

    /// Read a file's contents.
    ///
    /// **Size guard:** the entire file is sent as one msgpack frame.
    /// Anything past the wire's 128 MiB frame ceiling would crash
    /// the agent when it tries to write the response. Pre-stat and
    /// refuse with a helpful error pointing the caller at the
    /// streaming alternatives. The threshold leaves headroom for
    /// msgpack overhead (length prefix + map tags + path echo).
    pub fn read_file(&self, path: impl Into<String>) -> Result<Vec<u8>, VmError> {
        const READ_FILE_MAX_BYTES: u64 = 96 * 1024 * 1024;
        let path: String = path.into();
        if let Ok(meta) = self.stat(path.clone()) {
            if meta.size > READ_FILE_MAX_BYTES {
                return Err(VmError::Os(provium_protocol::OsError {
                errno: 27, // EFBIG
                message: format!(
                    "vm:read_file `{}`: {} bytes exceeds {} MiB limit \
                     — use vm:tail_file or vm:open_file for chunked reads",
                    path,
                    meta.size,
                    READ_FILE_MAX_BYTES / (1024 * 1024),
                ),
            }));
            }
        }
        self.with_client("read_file", |client| {
            match client.read_file(ReadFileArgs { path: path.clone() })? {
                OpResult::Ok(ok) => Ok(ok.data),
                OpResult::Err(e) => Err(VmError::Os(e)),
            }
        })
    }

    /// Replace (or create) a file with the given contents.
    pub fn write_file(&self, path: impl Into<String>, data: Vec<u8>) -> Result<(), VmError> {
        self.with_client("write_file", |client| {
            match client.write_file(WriteFileArgs {
                path: path.into(),
                data,
                mode: WriteFileMode::Replace,
                create_perm: None,
            })? {
                OpResult::Ok(()) => Ok(()),
                OpResult::Err(e) => Err(VmError::Os(e)),
            }
        })
    }

    /// Stat a path.
    pub fn stat(&self, path: impl Into<String>) -> Result<StatMeta, VmError> {
        self.with_client("stat", |client| {
            match client.stat(StatArgs {
                path: path.into(),
                follow_symlinks: true,
            })? {
                OpResult::Ok(m) => Ok(StatMeta::from_wire(m)),
                OpResult::Err(e) => Err(VmError::Os(e)),
            }
        })
    }

    /// Open a file in the agent's open-file table. Back-compat
    /// shim — `perm` is taken from the mode table by callers via
    /// [`Self::open_file_with_perm`].
    pub fn open_file(
        &self,
        path: impl Into<String>,
        mode: OpenMode,
    ) -> Result<provium_protocol::handle::FileHandle, VmError> {
        self.open_file_with_perm(path, mode, None)
    }

    /// Like [`Self::open_file`] but plumbs the create-time mode
    /// through to the agent. Used by `vm:open_file({create=true,
    /// perm=0o600})` so the documented `perm` flag isn't silently
    /// dropped.
    pub fn open_file_with_perm(
        &self,
        path: impl Into<String>,
        mode: OpenMode,
        create_perm: Option<u32>,
    ) -> Result<provium_protocol::handle::FileHandle, VmError> {
        let handle = self.with_client("open_file", |client| {
            match client.open_file(OpenFileArgs {
                path: path.into(),
                mode,
                create_perm,
            })? {
                OpResult::Ok(h) => Ok(h),
                OpResult::Err(e) => Err(VmError::Os(e)),
            }
        })?;
        // Register so snapshot precondition + auto-close see it.
        self.resources
            .lock()
            .unwrap()
            .open_files
            .insert(handle.get());
        Ok(handle)
    }

    /// Read from an opened file.
    pub fn read(
        &self,
        handle: provium_protocol::handle::FileHandle,
        max_bytes: u64,
    ) -> Result<Vec<u8>, VmError> {
        self.with_client("read", |client| {
            match client.read(ReadArgs { handle, max_bytes })? {
                OpResult::Ok(ok) => Ok(ok.data),
                OpResult::Err(e) => Err(VmError::Os(e)),
            }
        })
    }

    /// Write to an opened file.
    pub fn write(
        &self,
        handle: provium_protocol::handle::FileHandle,
        data: Vec<u8>,
    ) -> Result<u64, VmError> {
        self.with_client("write", |client| {
            match client.write(WriteArgs { handle, data })? {
                OpResult::Ok(ok) => Ok(ok.written),
                OpResult::Err(e) => Err(VmError::Os(e)),
            }
        })
    }

    /// Close a previously-opened file. Always deregisters the
    /// handle from the host-side resource registry — even when the
    /// agent-side close fails (e.g. VM dead) — so the snapshot
    /// precondition doesn't keep treating a closed FileUd as an
    /// open file forever. Mirrors the StreamGuard drop semantics.
    pub fn close(
        &self,
        handle: provium_protocol::handle::FileHandle,
    ) -> Result<(), VmError> {
        let close_result = self.with_client("close", |client| {
            match client.close(CloseArgs { handle })? {
                OpResult::Ok(()) => Ok(()),
                OpResult::Err(e) => Err(VmError::Os(e)),
            }
        });
        // Deregister regardless of agent outcome.
        self.resources
            .lock()
            .unwrap()
            .open_files
            .remove(&handle.get());
        close_result
    }

    /// Read the guest wall clock (nanoseconds since Unix epoch).
    pub fn clock_get(&self) -> Result<i64, VmError> {
        self.with_client("get_time", |client| {
            match client.get_time()? {
                OpResult::Ok(t) => Ok(t.ns),
                OpResult::Err(e) => Err(VmError::Os(e)),
            }
        })
    }

    /// Set the guest wall clock to `ns` nanoseconds since the
    /// Unix epoch.
    pub fn clock_set(&self, ns: i64) -> Result<(), VmError> {
        self.with_client("set_time", |client| {
            match client.set_time(provium_protocol::wire::SetTimeArgs { ns })? {
                OpResult::Ok(()) => Ok(()),
                OpResult::Err(e) => Err(VmError::Os(e)),
            }
        })
    }

    /// Sleep the guest for `ns` nanoseconds.
    pub fn clock_sleep(&self, ns: u64) -> Result<(), VmError> {
        self.with_client("sleep_clock", |client| {
            match client.sleep_clock(provium_protocol::wire::SleepClockArgs { ns })? {
                OpResult::Ok(()) => Ok(()),
                OpResult::Err(e) => Err(VmError::Os(e)),
            }
        })
    }

    /// Bump the guest wall clock by `ns` nanoseconds.
    pub fn clock_advance(&self, ns: i64) -> Result<(), VmError> {
        self.with_client("advance_clock", |client| {
            match client.advance_clock(provium_protocol::wire::AdvanceClockArgs { ns })? {
                OpResult::Ok(()) => Ok(()),
                OpResult::Err(e) => Err(VmError::Os(e)),
            }
        })
    }

    /// Spawn an async process. Returns a [`Process`] handle the
    /// caller can `wait()` or `kill()`.
    pub fn run_async(&self, args: RunAsyncArgs) -> Result<Process, VmError> {
        let handle = self.with_client("run_async", |client| match client.run_async(args)? {
            OpResult::Ok(h) => Ok(h),
            OpResult::Err(e) => Err(VmError::Os(e)),
        })?;
        Ok(Process {
            handle,
            vm: self.clone(),
            _drop_guard: std::sync::Arc::new(ProcessDropGuard {
                handle,
                vm: self.clone(),
                consumed: std::sync::atomic::AtomicBool::new(false),
            }),
        })
    }

    /// Open a streaming tail of a file. The returned session
    /// holds a registry guard; dropping the session deregisters
    /// the stream automatically.
    pub fn tail_file(
        &self,
        path: impl Into<String>,
        start: TailStart,
    ) -> Result<VmTailSession, VmError> {
        let path = path.into();
        let inner = self.with_client("tail_file", |client| {
            match client.tail_file(TailFileArgs {
                path: path.clone(),
                start,
            })? {
                TailFileOutcome::Ok(s) => Ok(s),
                TailFileOutcome::Err(e) => Err(VmError::Os(e)),
            }
        })?;
        Ok(self.wrap_tail_session_with_meta(
            inner,
            StreamMeta {
                kind: "tail_file".into(),
                detail: format!("\"{path}\""),
                creation_site: None,
                test_name: None,
            },
        ))
    }

    // -----------------------------------------------------------------
    // Internals
    // -----------------------------------------------------------------

    fn with_client<R>(
        &self,
        action: &'static str,
        f: impl FnOnce(&AgentClient) -> Result<R, VmError>,
    ) -> Result<R, VmError> {
        let result = {
            let inner = self.inner.lock().unwrap();
            match inner.state {
                VmState::Booted => {
                    let res = inner
                        .running
                        .as_ref()
                        .expect("Booted must carry running resources");
                    f(&res.client)
                }
                other => return Err(self.wrong_state(other, action)),
            }
        };
        // On connection-class failures the VM is dead. Mark it so
        // subsequent ops short-circuit per `DESIGN.md` § Failure
        // mode catalogue.
        if let Err(VmError::Client(ref ce)) = result {
            if is_dead_signal(ce) {
                let mut inner = self.inner.lock().unwrap();
                if inner.state == VmState::Booted {
                    inner.state = VmState::Dead;
                }
            }
        }
        result
    }

    fn wrong_state(&self, state: VmState, action: &'static str) -> VmError {
        VmError::WrongState {
            vm: self.name.clone(),
            state,
            action,
        }
    }
}

/// `true` if `err` smells like the agent is gone — connect failure,
/// frame EOF, or io error during send/recv. These transition the VM
/// to [`VmState::Dead`] so subsequent ops short-circuit.
fn is_dead_signal(err: &ClientError) -> bool {
    matches!(
        err,
        ClientError::Connect(_)
            | ClientError::Frame(provium_protocol::FrameError::Eof)
            | ClientError::Frame(provium_protocol::FrameError::Io(_))
    )
}

/// `true` if `err` indicates the QMP control plane is wedged
/// (timeout, IO failure, closed). Per `DESIGN.md` § Failure mode
/// catalogue, this transitions the VM to `Dead`.
fn is_qmp_dead_signal(err: &VmmError) -> bool {
    use provium_qmp::QmpError;
    matches!(
        err,
        VmmError::Qmp(QmpError::Timeout(_))
            | VmmError::Qmp(QmpError::Io(_))
            | VmmError::Qmp(QmpError::Closed(_))
    )
}

impl Vm {
    /// Mark the VM as Dead if `err` looks like a wedged QMP. Used
    /// by every op that goes through the VMM backend (pause /
    /// resume / snapshot / reset / set_link / detach_disk).
    fn maybe_mark_dead_on_vmm_err(&self, err: &VmmError) {
        if !is_qmp_dead_signal(err) {
            return;
        }
        let mut inner = self.inner.lock().unwrap();
        if matches!(inner.state, VmState::Booted | VmState::Paused) {
            inner.state = VmState::Dead;
        }
    }
}

impl Drop for Vm {
    fn drop(&mut self) {
        // Best-effort teardown if no one explicitly shut us down.
        // Only the *last* clone fires this branch (Arc<Mutex<…>>'s
        // strong count reaches 1 here only on the final drop), so
        // ergonomic Lua-side cloning of `Vm` doesn't trigger
        // premature shutdown.
        if Arc::strong_count(&self.inner) > 1 {
            return;
        }
        let mut inner = self.inner.lock().unwrap();
        if matches!(inner.state, VmState::Booted | VmState::Paused) {
            if let Some(res) = inner.running.as_mut() {
                if let Some(instance) = res.instance.take() {
                    let _ = instance.shutdown();
                }
            }
            inner.state = VmState::Shutdown;
        }
    }
}

// ---------------------------------------------------------------------------
// RunResult / StatMeta — host-facing types unchanged from slice 2.
// ---------------------------------------------------------------------------

/// Result of [`Vm::run`].
#[derive(Clone, Debug)]
pub struct RunResult {
    /// Captured stdout.
    pub stdout: Vec<u8>,
    /// Captured stderr.
    pub stderr: Vec<u8>,
    /// Termination outcome.
    pub status: ExitStatus,
}

impl RunResult {
    /// Build a `RunResult` from a wire-side `ExecOk`. Used by
    /// host-side batch dispatch to surface results to Lua without
    /// duplicating the conversion.
    pub fn from_exec_ok(ok: provium_protocol::wire::ExecOk) -> Self {
        Self {
            stdout: ok.stdout,
            stderr: ok.stderr,
            status: ok.status,
        }
    }

    /// `true` when the process exited cleanly with code 0.
    pub fn ok(&self) -> bool {
        matches!(self.status, ExitStatus::Exited(0))
    }

    /// Numeric exit code if the process exited normally; `None` for
    /// signalled / timed-out outcomes.
    pub fn exit_code(&self) -> Option<i32> {
        self.status.exit_code()
    }

    /// Lossy UTF-8 view of stdout for display.
    pub fn stdout_str(&self) -> std::borrow::Cow<'_, str> {
        String::from_utf8_lossy(&self.stdout)
    }

    /// Lossy UTF-8 view of stderr for display.
    pub fn stderr_str(&self) -> std::borrow::Cow<'_, str> {
        String::from_utf8_lossy(&self.stderr)
    }
}

/// Result of a Layer-0 syscall — the raw return value plus errno.
/// Mirrors POSIX semantics: a non-negative `ret` means success;
/// a negative `ret` (typically `-1`) means failure with the cause
/// in `errno`.
#[derive(Clone, Debug)]
pub struct SyscallResult {
    /// Syscall return value.
    pub ret: i64,
    /// `errno` set by the agent if `ret < 0`.
    pub errno: i32,
    /// Buffer contents post-syscall, paired by index with the
    /// request's `bufs`. Empty when no bufs were supplied.
    pub out_bufs: Vec<Vec<u8>>,
}

/// Sub-agent worker handle. Returned by [`Vm::spawn_worker`].
#[derive(Clone, Debug)]
pub struct Worker {
    handle: provium_protocol::handle::WorkerHandle,
    vm: Vm,
    /// File handles this worker has opened against the parent
    /// VM's `open_files` registry. Drained on `:join()` so the
    /// snapshot precondition stops counting them as live when
    /// the worker exits — without this, a worker that dies
    /// without explicit `file:close()` leaks the entries forever
    /// and `vm:snapshot` always fails.
    open_files: std::sync::Arc<std::sync::Mutex<std::collections::BTreeSet<u64>>>,
    /// Arc-shared drop guard. Fires only on the LAST `Worker`
    /// clone going away; if the Lua scope walker already issued
    /// `:close()` (which calls `:join()`) the guard's drained
    /// flag is set and the Drop is a no-op. Belt-and-braces for
    /// the rare WorkerUd that's GC'd without an explicit close
    /// — without this, the per-worker `open_files` entries leak
    /// in the parent VM's resource registry and every subsequent
    /// `vm:snapshot()` blocks waiting for streams that no longer
    /// have an owner.
    _drop_guard: std::sync::Arc<WorkerDropGuard>,
}

/// Best-effort cleanup hook that fires when the last [`Worker`]
/// clone is dropped. Does the same drain as
/// [`Worker::join`]; safe to invoke after an explicit join (the
/// post-join `clear()` empties the open_files set so we have
/// nothing to drain).
#[derive(Debug)]
pub(crate) struct WorkerDropGuard {
    handle: provium_protocol::handle::WorkerHandle,
    vm: Vm,
    open_files: std::sync::Arc<std::sync::Mutex<std::collections::BTreeSet<u64>>>,
}

impl Drop for WorkerDropGuard {
    fn drop(&mut self) {
        let drained: Vec<u64> = self
            .open_files
            .lock()
            .unwrap()
            .iter()
            .copied()
            .collect();
        if drained.is_empty() {
            return;
        }
        // Reap the worker on the agent side so its file handles
        // are actually closed; ignore errors — this is the
        // last-resort path.
        let _ = self.vm.with_client("worker_join_drop", |client| {
            client
                .worker_join(provium_protocol::wire::WorkerJoinArgs {
                    handle: self.handle,
                })
                .map_err(VmError::Client)
        });
        // Drain the parent's open_files registry regardless of
        // the agent-side outcome.
        let mut reg = self.vm.resources.lock().unwrap();
        for h in &drained {
            reg.open_files.remove(h);
        }
        self.open_files.lock().unwrap().clear();
    }
}

impl Worker {
    /// Underlying handle.
    pub fn handle(&self) -> provium_protocol::handle::WorkerHandle {
        self.handle
    }

    /// Run an exec under this worker. Returns a [`RunResult`] like
    /// [`Vm::run`].
    pub fn run(&self, exec: ExecArgs) -> Result<RunResult, VmError> {
        let result = self.vm.with_client("worker_exec", |client| {
            client
                .worker_exec(provium_protocol::wire::WorkerExecArgs {
                    handle: self.handle,
                    exec,
                })
                .map_err(VmError::Client)
        })?;
        match result {
            ExecResult::Ok(ok) => Ok(RunResult::from_exec_ok(ok)),
            ExecResult::Err(e) => Err(VmError::Os(e)),
        }
    }

    /// Reap the worker. Drains any worker-opened file handles
    /// from the parent VM's `open_files` registry — the agent
    /// closes them when the worker dies, so the host-side
    /// snapshot precondition mustn't keep counting them as live.
    /// Without this drain, a worker that opened files but never
    /// explicitly `file:close()`d them would block every
    /// subsequent `vm:snapshot()`.
    pub fn join(&self) -> Result<i32, VmError> {
        let result = self.vm.with_client("worker_join", |client| {
            client
                .worker_join(provium_protocol::wire::WorkerJoinArgs {
                    handle: self.handle,
                })
                .map_err(VmError::Client)
        })?;
        // Deregister regardless of agent outcome — even on a
        // worker-side join error, the worker is gone and its
        // file handles are unreachable.
        let drained: Vec<u64> = self
            .open_files
            .lock()
            .unwrap()
            .iter()
            .copied()
            .collect();
        if !drained.is_empty() {
            let mut reg = self.vm.resources.lock().unwrap();
            for h in &drained {
                reg.open_files.remove(h);
            }
            self.open_files.lock().unwrap().clear();
        }
        match result {
            OpResult::Ok(payload) => Ok(payload.exit_status),
            OpResult::Err(e) => Err(VmError::Os(e)),
        }
    }

    /// Async run under the worker. Process handle lives in the
    /// worker's `AgentState` subnamespace, distinct from the
    /// parent VM's process table.
    pub fn run_async(
        &self,
        args: provium_protocol::wire::RunAsyncArgs,
    ) -> Result<Process, VmError> {
        let result = self.vm.with_client("worker_run_async", |client| {
            client
                .worker_run_async(provium_protocol::wire::WorkerRunAsyncArgs {
                    handle: self.handle,
                    args,
                })
                .map_err(VmError::Client)
        })?;
        match result {
            OpResult::Ok(handle) => Ok(Process {
                handle,
                vm: self.vm.clone(),
                _drop_guard: std::sync::Arc::new(ProcessDropGuard {
                    handle,
                    vm: self.vm.clone(),
                    consumed: std::sync::atomic::AtomicBool::new(false),
                }),
            }),
            OpResult::Err(e) => Err(VmError::Os(e)),
        }
    }

    /// Open a file under this worker. File handle lives in the
    /// worker's `AgentState`.
    pub fn open_file(
        &self,
        path: impl Into<String>,
        mode: provium_protocol::wire::OpenMode,
    ) -> Result<provium_protocol::handle::FileHandle, VmError> {
        let result = self.vm.with_client("worker_open_file", |client| {
            client
                .worker_open_file(provium_protocol::wire::WorkerOpenFileArgs {
                    handle: self.handle,
                    args: provium_protocol::wire::OpenFileArgs {
                        path: path.into(),
                        mode,
                        create_perm: None,
                    },
                })
                .map_err(VmError::Client)
        })?;
        match result {
            OpResult::Ok(handle) => {
                // Register the file handle in the parent VM's
                // open-file set so the snapshot precondition sees
                // it. R6 #12: without this, vm:snapshot succeeded
                // even when worker:open_file had a live handle.
                // Also record on this Worker so :join() can
                // deregister it if the test never explicitly
                // closes the file.
                self.vm
                    .resources
                    .lock()
                    .unwrap()
                    .open_files
                    .insert(handle.get());
                self.open_files.lock().unwrap().insert(handle.get());
                Ok(handle)
            }
            OpResult::Err(e) => Err(VmError::Os(e)),
        }
    }

    /// Issue a raw syscall under the worker. Up to 6 i64 args are
    /// forwarded; the rest are zero-padded.
    pub fn syscall(
        &self,
        nr: i64,
        args: Vec<i64>,
    ) -> Result<crate::vm::SyscallResult, VmError> {
        let mut padded = [0i64; 6];
        for (slot, value) in padded.iter_mut().zip(args.into_iter().take(6)) {
            *slot = value;
        }
        self.syscall_with_bufs(nr, padded, Vec::new(), Vec::new())
    }

    /// Same as [`Worker::syscall`] but with the bufs/ptrs splice
    /// extension — the agent writes each `bufs[i]` into a scratch
    /// region and substitutes the address into `args[ptrs[i]]`
    /// before issuing the syscall, then returns the post-syscall
    /// buffer contents in `out_bufs`. Mirrors [`Vm::syscall_with_bufs`]
    /// so worker-scoped tests have full parity with VM-scoped ones.
    pub fn syscall_with_bufs(
        &self,
        nr: i64,
        args: [i64; 6],
        bufs: Vec<Vec<u8>>,
        ptrs: Vec<u8>,
    ) -> Result<crate::vm::SyscallResult, VmError> {
        let r = self.vm.with_client("worker_syscall", |client| {
            client
                .worker_syscall(provium_protocol::wire::WorkerSyscallArgs {
                    handle: self.handle,
                    args: provium_protocol::wire::SyscallArgs {
                        nr,
                        args,
                        bufs: bufs.clone(),
                        ptrs: ptrs.clone(),
                    },
                })
                .map_err(VmError::Client)
        })?;
        Ok(SyscallResult {
            ret: r.ret,
            errno: r.errno,
            out_bufs: r.out_bufs,
        })
    }

    /// Read access to the parent VM. Used by Lua bindings that
    /// need to wrap returned file handles back into the parent's
    /// userdata.
    pub fn parent_vm(&self) -> &Vm {
        &self.vm
    }

    /// Broadcast `signal` to every async process in the worker's
    /// namespace. Returns the count signalled.
    pub fn kill(&self, signal: i32) -> Result<u32, VmError> {
        let r = self.vm.with_client("worker_kill", |client| {
            client
                .worker_kill(provium_protocol::wire::WorkerKillArgs {
                    handle: self.handle,
                    signal,
                })
                .map_err(VmError::Client)
        })?;
        match r {
            OpResult::Ok(n) => Ok(n),
            OpResult::Err(e) => Err(VmError::Os(e)),
        }
    }
}

/// Async-process handle. Returned by [`Vm::run_async`]; consumed
/// by `Process::wait` or used by `Process::kill`/`signal`.
///
/// Cloning is cheap (refcount). When the last clone drops without
/// the test having called `wait`, the `Drop` impl on the inner
/// state issues a best-effort SIGTERM + reap so the agent doesn't
/// accumulate zombie children for processes that fell out of
/// scope unceremoniously (Rust callers of `Vm::run_async` outside
/// the Lua scope walker, panics mid-test, etc.).
#[derive(Clone, Debug)]
pub struct Process {
    handle: provium_protocol::handle::ProcessHandle,
    vm: Vm,
    /// Shared marker that survives clones and fires its Drop
    /// exactly once when the last clone is dropped. Used to send
    /// a best-effort kill+wait when no explicit `wait` was issued.
    _drop_guard: std::sync::Arc<ProcessDropGuard>,
}

/// Inner guard for [`Process`] — fires once on the final clone's
/// drop and reaps the agent-side child if it's still alive.
pub struct ProcessDropGuard {
    handle: provium_protocol::handle::ProcessHandle,
    vm: Vm,
    /// Set by `wait` / `kill` paths so we don't double-reap a
    /// process the user explicitly cleaned up.
    consumed: std::sync::atomic::AtomicBool,
}

impl std::fmt::Debug for ProcessDropGuard {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProcessDropGuard")
            .field("handle", &self.handle)
            .finish()
    }
}

impl Drop for ProcessDropGuard {
    fn drop(&mut self) {
        if self.consumed.load(std::sync::atomic::Ordering::SeqCst) {
            return;
        }
        // Best-effort cleanup. Failures here are silent — the
        // process may already be reaped, the VM may be dead, or
        // the connection may be wedged.
        let _ = self.vm.with_client("kill_drop", |client| {
            client
                .kill(provium_protocol::wire::KillArgs {
                    handle: self.handle,
                    signal: libc::SIGTERM,
                })
                .map_err(VmError::Client)
        });
        let _ = self.vm.with_client("wait_drop", |client| {
            client
                .wait(provium_protocol::wire::WaitArgs {
                    handle: self.handle,
                    timeout_ms: Some(2000),
                })
                .map_err(VmError::Client)
        });
    }
}

impl Process {
    /// Underlying handle. Test/diagnostic only.
    pub fn handle(&self) -> provium_protocol::handle::ProcessHandle {
        self.handle
    }

    /// `true` if `wait` / `wait_with_timeout` has reaped this
    /// process. Used by `proc:close` so scope-end cleanup can
    /// skip the redundant kill+wait round-trip when the test
    /// already consumed the handle.
    pub fn consumed(&self) -> bool {
        self._drop_guard
            .consumed
            .load(std::sync::atomic::Ordering::SeqCst)
    }

    /// Kernel-level PID of the in-flight child, fetched via the
    /// `GetPid` wire op. Distinct from [`Self::handle`] (which is
    /// the provium opaque counter). Used by `proc:pid()` so test
    /// code can compare against `ps`/proc.
    pub fn pid(&self) -> Result<u32, VmError> {
        let result = self.vm.with_client("get_pid", |client| {
            client
                .get_pid(provium_protocol::wire::GetPidArgs {
                    handle: self.handle,
                })
                .map_err(VmError::Client)
        })?;
        match result {
            OpResult::Ok(p) => Ok(p),
            OpResult::Err(e) => Err(VmError::Os(e)),
        }
    }

    /// Parent VM accessor. Used by the proc-stream Lua bindings to
    /// enrich the stream's `StreamMeta` after creation.
    pub fn parent_vm(&self) -> &Vm {
        &self.vm
    }

    /// Wait for the process to exit. Returns the captured output +
    /// exit status as a [`RunResult`].
    pub fn wait(&self) -> Result<RunResult, VmError> {
        self.wait_with_timeout(None)
    }

    /// Wait with an optional millisecond timeout. On timeout the
    /// agent SIGKILLs the child and the [`RunResult`] reports
    /// [`ExitStatus::TimedOut`].
    pub fn wait_with_timeout(
        &self,
        timeout_ms: Option<u64>,
    ) -> Result<RunResult, VmError> {
        // Pre-check: a previous `wait` (or the drop guard) marked
        // the process as consumed. The agent has already
        // released the slot — calling wire-level wait again
        // would surface the agent's "no such handle" error,
        // which is opaque ("wait on N"). Surface a concrete
        // explanation instead.
        if self
            ._drop_guard
            .consumed
            .load(std::sync::atomic::Ordering::SeqCst)
        {
            return Err(VmError::Os(provium_protocol::OsError {
                errno: 9, // EBADF
                message: format!(
                    "proc:wait: handle {} already waited on \
                     (proc:wait is one-shot — capture the result)",
                    self.handle.get(),
                ),
            }));
        }
        let result = self.vm.with_client("wait", |client| {
            client
                .wait(WaitArgs {
                    handle: self.handle,
                    timeout_ms,
                })
                .map_err(VmError::Client)
        })?;
        // Mark the drop-guard so the final clone's Drop doesn't
        // try to re-reap a process the test already consumed.
        self._drop_guard
            .consumed
            .store(true, std::sync::atomic::Ordering::SeqCst);
        match result {
            OpResult::Ok(ok) => Ok(RunResult::from_exec_ok(ok)),
            OpResult::Err(e) => Err(VmError::Os(e)),
        }
    }

    /// Send a signal to the process. Common values: 15 = SIGTERM,
    /// 9 = SIGKILL.
    pub fn kill(&self, signal: i32) -> Result<(), VmError> {
        let result = self.vm.with_client("kill", |client| {
            client
                .kill(KillArgs {
                    handle: self.handle,
                    signal,
                })
                .map_err(VmError::Client)
        })?;
        match result {
            OpResult::Ok(()) => Ok(()),
            OpResult::Err(e) => Err(VmError::Os(e)),
        }
    }

    /// Live status: "running", "exited", or "unknown".
    pub fn status(&self) -> Result<&'static str, VmError> {
        let r = self.vm.with_client("proc_status", |client| {
            client
                .proc_status(provium_protocol::wire::ProcStatusArgs {
                    handle: self.handle,
                })
                .map_err(VmError::Client)
        })?;
        match r {
            OpResult::Ok(provium_protocol::wire::ProcessLiveStatus::Running) => Ok("running"),
            OpResult::Ok(provium_protocol::wire::ProcessLiveStatus::Exited) => Ok("exited"),
            // Collapse the Unknown wire variant to "exited" for
            // the Lua surface — DESIGN documents only "running" |
            // "exited", and tests doing `if s == "running" or
            // s == "exited"` would otherwise silently fall
            // through. "Unknown" effectively means "the agent has
            // no live tracking entry", which is indistinguishable
            // from "already exited" from the test's point of
            // view.
            OpResult::Ok(provium_protocol::wire::ProcessLiveStatus::Unknown) => Ok("exited"),
            OpResult::Err(e) => Err(VmError::Os(e)),
        }
    }

    /// Write bytes to the child's stdin.
    pub fn stdin_write(&self, data: Vec<u8>) -> Result<u64, VmError> {
        let r = self.vm.with_client("proc_stdin_write", |client| {
            client
                .proc_stdin_write(provium_protocol::wire::ProcStdinWriteArgs {
                    handle: self.handle,
                    data,
                })
                .map_err(VmError::Client)
        })?;
        match r {
            OpResult::Ok(ok) => Ok(ok.written),
            OpResult::Err(e) => Err(VmError::Os(e)),
        }
    }

    /// Close the child's stdin.
    pub fn close_stdin(&self) -> Result<(), VmError> {
        let r = self.vm.with_client("proc_stdin_close", |client| {
            client
                .proc_stdin_close(provium_protocol::wire::ProcStdinCloseArgs {
                    handle: self.handle,
                })
                .map_err(VmError::Client)
        })?;
        match r {
            OpResult::Ok(()) => Ok(()),
            OpResult::Err(e) => Err(VmError::Os(e)),
        }
    }

    /// Open a streaming subscription to the process's captured
    /// stdout. Yields frames until the process exits and the buffer
    /// is drained.
    pub fn stdout_stream(&self) -> Result<VmTailSession, VmError> {
        self.proc_stream_channel(provium_protocol::wire::ProcStreamChannel::Stdout)
    }

    /// As [`Self::stdout_stream`] but for stderr.
    pub fn stderr_stream(&self) -> Result<VmTailSession, VmError> {
        self.proc_stream_channel(provium_protocol::wire::ProcStreamChannel::Stderr)
    }

    fn proc_stream_channel(
        &self,
        channel: provium_protocol::wire::ProcStreamChannel,
    ) -> Result<VmTailSession, VmError> {
        let r = self.vm.with_client("proc_stream", |client| {
            client
                .proc_stream(provium_protocol::wire::ProcStreamArgs {
                    handle: self.handle,
                    channel,
                })
                .map_err(VmError::Client)
        })?;
        let inner = match r {
            crate::agent_client::TailFileOutcome::Ok(s) => s,
            crate::agent_client::TailFileOutcome::Err(e) => return Err(VmError::Os(e)),
        };
        // R9 stream-M2: pre-fill kind+detail so a Rust-direct
        // caller (not via Lua) gets a meaningful snapshot
        // diagnostic. Lua callers still patch via
        // set_stream_meta with the file:line creation site, but
        // the fallback now identifies the channel rather than
        // showing a generic "stream" entry.
        let channel_label = match channel {
            provium_protocol::wire::ProcStreamChannel::Stdout => "proc:stdout_stream",
            provium_protocol::wire::ProcStreamChannel::Stderr => "proc:stderr_stream",
        };
        Ok(self.vm.wrap_tail_session_with_meta(
            inner,
            StreamMeta {
                kind: "proc_stream".into(),
                detail: format!("{channel_label}(handle={})", self.handle.get()),
                creation_site: None,
                test_name: None,
            },
        ))
    }
}

/// Host-facing file metadata. Returned by [`Vm::stat`].
#[derive(Clone, Copy, Debug)]
pub struct StatMeta {
    /// Size in bytes.
    pub size: u64,
    /// Modification time, ns since epoch.
    pub mtime_ns: i64,
    /// Filesystem entry kind.
    pub entry_type: provium_protocol::wire::EntryType,
    /// POSIX permission bits.
    pub perm: u32,
}

impl StatMeta {
    fn from_wire(m: provium_protocol::wire::FileMetadata) -> Self {
        Self {
            size: m.size,
            mtime_ns: m.mtime_ns,
            entry_type: m.entry_type,
            perm: m.perm,
        }
    }
}
