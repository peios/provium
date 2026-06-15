//! Resource container — VMs, sub-labs, and (in slice 9) bridges.
//!
//! Per `DESIGN.md` § Resource model: a [`Lab`] groups VMs that
//! share lifecycle, networking, and snapshot semantics. `provium`
//! itself is a Lab — see [`crate::lua`] for the Lua-side wiring.
//!
//! ## Slice 4 scope
//!
//! * Create + look-up of VMs and sub-labs.
//! * `lab.boot()` — atomic-batch boot of every unbooted VM in the
//!   tree (the design's "one resource acquisition" guarantee
//!   becomes a real claim-acquire under slice 5; for now it's a
//!   parallel-spawn).
//! * `lab.shutdown()`, `lab.pause()`, `lab.resume()` — parallel
//!   across members.
//! * `lab.lab()` — sub-lab; sub-lab membership recurses through
//!   batch operations.
//!
//! Bridges + `lab:claim` + `lab:snapshot`/`lab:restore` are slices
//! 5/8/9 territory.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::thread;

use thiserror::Error;

use provium_protocol::events::{Event, VmSpawned};

use crate::bridge::Bridge;
use crate::profile::Config;
use crate::scheduler::events::{EventSink, NullSink};
use crate::vm::{Vm, VmError, VmState};
use crate::vmm::{BootOpts, Vmm};

/// Failures specific to lab membership management.
#[derive(Debug, Error)]
pub enum LabError {
    /// Tried to register a VM under a name another VM already uses.
    #[error("vm name `{0}` already used in this lab")]
    DuplicateVmName(String),

    /// `lab:vm("name")` lookup form against a name that wasn't
    /// declared.
    #[error("no vm named `{0}` in this lab")]
    UnknownVm(String),

    /// `lab:vm(name, profile)` referenced a profile not in
    /// `provium.toml`.
    #[error("profile `{0}` not in provium.toml")]
    UnknownProfile(String),

    /// Bridge name conflict at creation.
    #[error("bridge name `{0}` already used in this lab")]
    DuplicateBridgeName(String),

    /// Lookup form against a bridge that wasn't declared.
    #[error("no bridge named `{0}` in this lab")]
    UnknownBridge(String),

    /// `lab:restore_vm(...)` failed at the VMM layer.
    #[error("restore vm `{0}` failed: {1}")]
    RestoreFailed(String, String),

    /// `lab:claim` called twice on the same lab.
    #[error("lab claim already held; one-shot per lab")]
    ClaimAlreadyHeld,

    /// `lab:claim` request exceeds the pool's total budget — no
    /// amount of waiting will satisfy it.
    #[error("lab claim exceeds total pool budget")]
    ClaimExceedsBudget,

    /// Caller used a name reserved for the dot-access surface
    /// (`vm_fixture` / `lab_fixture` / `pack` / `unpack`) per
    /// `DESIGN.md` § Lab dot-access. Allowing them would shadow
    /// the documented method names — refuse loudly.
    #[error("name `{0}` is reserved for the dot-access surface (vm_fixture, lab_fixture, pack, unpack); use lab:vm()/lab:bridge() with a different name")]
    ReservedName(String),

    /// Historic — kept for source compatibility with downstream
    /// matchers. `lab:boot()` is now idempotent: a call after
    /// `lab:include_vm(...)` reserves an additional slot for the
    /// newly-created members and leaves existing reservations
    /// alone. No code path emits this variant in v1.
    #[error("lab already booted; call shutdown() before boot() again")]
    AlreadyBooted,

    /// One of the parallel batch operations failed for one or more
    /// members. Carries a per-name error map so callers can surface
    /// every offender.
    #[error("batch `{action}` failed: {failures:?}")]
    BatchFailed {
        /// Action name — `boot`, `shutdown`, `pause`, `resume`.
        action: &'static str,
        /// Map from member name to its error rendering.
        failures: BTreeMap<String, String>,
    },
}

/// Container for VMs + sub-labs.
///
/// Cheap to clone — internal state is `Arc<Mutex<…>>`. Cloning the
/// handle does **not** duplicate the underlying members; it lets the
/// same lab be reachable from both Lua and any sibling Rust code.
pub struct Lab {
    name: String,
    inner: Arc<Mutex<LabInner>>,
    config: Arc<Config>,
    vmm: Arc<dyn Vmm>,
    /// Pool reference propagated to created VMs for solo-boot
    /// reservation. `None` for ad-hoc / REPL labs that don't
    /// share a global pool.
    pool: Option<Arc<crate::scheduler::Pool>>,
    /// Event sink for VM-lifecycle telemetry. [`Self::restore_vm`]
    /// emits `vm_spawned` through it so fixture resumes — including
    /// `lab_restore`'s parallel restores — show up on the event
    /// stream. Sub-labs inherit the parent's sink. Defaults to
    /// [`NullSink`] for ad-hoc / REPL / test labs.
    events: Arc<dyn EventSink>,
}

struct LabInner {
    vms: BTreeMap<String, Vm>,
    sub_labs: BTreeMap<String, Lab>,
    bridges: BTreeMap<String, Bridge>,
    /// Active file-scope reservation from the scheduler's pool.
    /// Held for the file's lifetime; one-shot per lab per the
    /// design.
    claim: Option<crate::scheduler::Reservation>,
    /// `true` once `lab:claim(...)` has been called, even when no
    /// pool was wired. Enforces the design's "one-shot per file"
    /// rule for REPL/ad-hoc paths that lack a pool.
    claim_taken: bool,
    /// Named barriers — re-entrant, identified by string name.
    barriers: BTreeMap<String, std::sync::Arc<BarrierState>>,
    /// Joint pool reservation held while `lab:boot()`-launched VMs
    /// are running. Without this, the joint reservation taken in
    /// `boot_with_pool` was dropped on function return — leaving
    /// the VMs running with no pool accounting (each VM's solo
    /// pool ref was cleared to avoid double-charge). Released by
    /// `lab.shutdown()`.
    /// Joint pool reservations from prior `lab:boot()` calls.
    /// Vec because `lab:include_vm()` can introduce new VMs
    /// after the initial boot — re-calling `lab:boot()` then
    /// reserves an *additional* slot for the newcomers without
    /// invalidating the existing one. All entries are released
    /// at `lab:shutdown()`.
    boot_reservations: Vec<crate::scheduler::Reservation>,
}

/// State for a [`Lab::barrier`] rendezvous.
struct BarrierState {
    inner: std::sync::Mutex<BarrierInner>,
    cond: std::sync::Condvar,
}

struct BarrierInner {
    arrived: usize,
    /// Rounds completed; bumped on every release. Acts as a
    /// generation counter so re-uses of the same name don't
    /// observe stale `arrived` counts.
    generation: u64,
    /// Expected target captured at first arrival. Locked-in so a
    /// later mismatching `arrive(name, count)` call is treated
    /// as a programmer error rather than silently changing the
    /// release threshold under in-flight waiters.
    expected: Option<usize>,
}

impl BarrierState {
    fn new(_target: usize) -> Self {
        Self {
            inner: std::sync::Mutex::new(BarrierInner {
                arrived: 0,
                generation: 0,
                expected: None,
            }),
            cond: std::sync::Condvar::new(),
        }
    }

    fn arrive(&self, target: usize, timeout: std::time::Duration) -> bool {
        let deadline = std::time::Instant::now() + timeout;
        let mut g = self.inner.lock().unwrap();
        // Lock in the count on first arrival; reject mismatches
        // afterwards so the barrier doesn't silently honour two
        // different `count` values across rounds. (R6 #19.)
        match g.expected {
            None => g.expected = Some(target),
            Some(prev) if prev != target => {
                eprintln!(
                    "provium: barrier count mismatch: previously {prev}, now {target}; \
                     keeping the original target. arrive() returns false."
                );
                return false;
            }
            Some(_) => {}
        }
        let my_generation = g.generation;
        g.arrived += 1;
        if g.arrived >= target {
            g.arrived = 0;
            g.generation = g.generation.wrapping_add(1);
            self.cond.notify_all();
            return true;
        }
        loop {
            let now = std::time::Instant::now();
            if now >= deadline {
                // Roll back the increment so a future round
                // doesn't see a phantom arrival from this caller.
                // Without this, an arrive(count, timeout=0) with
                // count > 1 leaves arrived stuck at 1 forever
                // and the next round releases one short.
                if g.generation == my_generation && g.arrived > 0 {
                    g.arrived -= 1;
                }
                return false;
            }
            let remaining = deadline - now;
            let (new_g, wait_result) =
                self.cond.wait_timeout(g, remaining).unwrap();
            g = new_g;
            if g.generation != my_generation {
                return true;
            }
            if wait_result.timed_out() {
                if g.generation == my_generation && g.arrived > 0 {
                    g.arrived -= 1;
                }
                return false;
            }
        }
    }
}

/// Persisted metadata for a [`Lab::lab_snapshot`]. Round-trips
/// through JSON for human inspectability; size is small.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct LabSnapshotMeta {
    /// One per VM.
    pub vms: Vec<LabSnapshotEntry>,
    /// One per bridge.
    pub bridges: Vec<LabSnapshotBridge>,
}

/// One member of [`LabSnapshotMeta`].
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct LabSnapshotEntry {
    /// VM name as it appeared in the lab.
    pub vm_name: String,
    /// Profile from `provium.toml`.
    pub profile: String,
    /// Path of the per-VM snapshot file.
    pub snapshot_path: std::path::PathBuf,
}

/// Bridge record inside [`LabSnapshotMeta`]. Carries the full
/// impairment + uplink state so `lab:restore` can reconstruct an
/// equivalent topology, per `DESIGN.md` § Lab snapshot model.
#[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize)]
pub struct LabSnapshotBridge {
    /// Bridge name.
    pub name: String,
    /// VM names attached at snapshot time.
    pub members: Vec<String>,
    /// Whole-bridge latency in ms.
    #[serde(default)]
    pub latency_ms: u32,
    /// Whole-bridge drop rate (0–100).
    #[serde(default)]
    pub drop_rate_pct: u32,
    /// Whole-bridge bandwidth in bits/sec. `0` = unlimited.
    #[serde(default)]
    pub bandwidth_bps: u64,
    /// Bridge had uplink enabled at snapshot time.
    #[serde(default)]
    pub uplink_enabled: bool,
    /// Routes to other bridges.
    #[serde(default)]
    pub routes: Vec<String>,
    /// Isolated VMs (`bridge:isolate(vm)`).
    #[serde(default)]
    pub isolated: Vec<String>,
    /// Symmetric partition pairs `(a, b)`.
    #[serde(default)]
    pub partitions: Vec<(String, String)>,
    /// Directional partition `(from, to)` pairs — `from → to` is
    /// dropped, reverse direction stays open unless an inverse
    /// entry exists.
    #[serde(default)]
    pub directional_partitions: Vec<(String, String)>,
    /// Per-pair directional impairments (latency + drop) keyed by
    /// `(from, to)`.
    #[serde(default)]
    pub directional_impairments: Vec<LabSnapshotDirectional>,
    /// `bridge:partition_all` was active at snapshot time.
    #[serde(default)]
    pub fully_partitioned: bool,
}

/// One entry in [`LabSnapshotBridge::directional_impairments`].
#[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize)]
pub struct LabSnapshotDirectional {
    /// Source VM.
    pub from: String,
    /// Destination VM.
    pub to: String,
    /// Per-pair latency in ms.
    pub latency_ms: u32,
    /// Per-pair drop percent (0–100).
    pub drop_rate_pct: u32,
    /// Per-pair bandwidth in bits/sec. `0` = unlimited. Carried
    /// through round-trip even though Lua currently rejects the
    /// directional bandwidth form (see bridge_ud.rs) — Rust
    /// callers can set it via `DirectionalImpairment` and the
    /// snapshot must not silently lose it.
    #[serde(default)]
    pub bandwidth_bps: u64,
}

impl std::fmt::Debug for Lab {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let inner = self.inner.lock().unwrap();
        f.debug_struct("Lab")
            .field("name", &self.name)
            .field("vms", &inner.vms.keys().collect::<Vec<_>>())
            .field("sub_labs", &inner.sub_labs.keys().collect::<Vec<_>>())
            .finish()
    }
}

impl Clone for Lab {
    fn clone(&self) -> Self {
        Self {
            name: self.name.clone(),
            inner: Arc::clone(&self.inner),
            config: Arc::clone(&self.config),
            vmm: Arc::clone(&self.vmm),
            pool: self.pool.clone(),
            events: Arc::clone(&self.events),
        }
    }
}

impl Lab {
    /// Construct a lab. The `provium` global is one of these; sub-labs
    /// are created via [`Self::sub_lab`].
    ///
    /// VM-lifecycle events are discarded ([`NullSink`]). The
    /// dispatcher uses [`Self::new_with_pool_and_events`] to wire a
    /// real sink; ad-hoc / REPL / test runs don't need one.
    pub fn new(name: impl Into<String>, config: Arc<Config>, vmm: Arc<dyn Vmm>) -> Self {
        Self::new_with_pool(name, config, vmm, None)
    }

    /// Construct with an explicit pool reference. Used by the
    /// dispatcher so created VMs can reserve from the pool on
    /// solo-boot. VM-lifecycle events are discarded — see
    /// [`Self::new_with_pool_and_events`] for the telemetry-wired
    /// variant.
    pub fn new_with_pool(
        name: impl Into<String>,
        config: Arc<Config>,
        vmm: Arc<dyn Vmm>,
        pool: Option<Arc<crate::scheduler::Pool>>,
    ) -> Self {
        Self::new_with_pool_and_events(name, config, vmm, pool, Arc::new(NullSink))
    }

    /// Construct with both a pool reference and an event sink.
    ///
    /// The sink receives `vm_spawned` from [`Self::restore_vm`] and
    /// is inherited by every sub-lab, so fixture resumes anywhere in
    /// the tree land on the event stream.
    pub fn new_with_pool_and_events(
        name: impl Into<String>,
        config: Arc<Config>,
        vmm: Arc<dyn Vmm>,
        pool: Option<Arc<crate::scheduler::Pool>>,
        events: Arc<dyn EventSink>,
    ) -> Self {
        Self {
            name: name.into(),
            pool,
            inner: Arc::new(Mutex::new(LabInner {
                vms: BTreeMap::new(),
                sub_labs: BTreeMap::new(),
                bridges: BTreeMap::new(),
                claim: None,
                claim_taken: false,
                barriers: BTreeMap::new(),
                boot_reservations: Vec::new(),
            })),
            config,
            vmm,
            events,
        }
    }

    /// Declare a new bridge under this lab.
    pub fn create_bridge(&self, name: impl Into<String>) -> Result<Bridge, LabError> {
        let name = name.into();
        check_reserved_name(&name)?;
        let bridge = Bridge::new(name.clone());
        let mut inner = self.inner.lock().unwrap();
        if inner.bridges.contains_key(&name) {
            return Err(LabError::DuplicateBridgeName(name));
        }
        inner.bridges.insert(name, bridge.clone());
        Ok(bridge)
    }

    /// Look up an existing bridge.
    pub fn get_bridge(&self, name: &str) -> Result<Bridge, LabError> {
        self.inner
            .lock()
            .unwrap()
            .bridges
            .get(name)
            .cloned()
            .ok_or_else(|| LabError::UnknownBridge(name.to_owned()))
    }

    /// Snapshot of declared-bridge names.
    pub fn bridge_names(&self) -> Vec<String> {
        self.inner.lock().unwrap().bridges.keys().cloned().collect()
    }

    /// Snapshot every member VM in parallel into `dir` and persist
    /// a metadata file describing the topology. Resumed via
    /// [`Self::lab_restore`].
    pub fn lab_snapshot(&self, dir: &std::path::Path) -> Result<LabSnapshotMeta, LabError> {
        std::fs::create_dir_all(dir)
            .map_err(|e| LabError::RestoreFailed("snapshot".into(), e.to_string()))?;

        // Per `DESIGN.md` § Lab snapshot model: confirm no open
        // streams across the whole tree before touching the VMM.
        let total_streams = self.open_stream_total();
        if total_streams > 0 {
            let mut detail = format!(
                "lab:snapshot: {total_streams} stream(s) still open\n"
            );
            for vm in self.vms() {
                for meta in vm.open_streams_meta() {
                    detail.push_str("  - ");
                    detail.push_str(&crate::vm::format_stream_meta(&meta));
                    detail.push('\n');
                }
            }
            detail.push_str("  Close streams before snapshotting.");
            return Err(LabError::RestoreFailed("lab".into(), detail));
        }

        let vms = self.collect_all_vms();

        // Atomic batch pause coordinated by a startup barrier so
        // every VM's QMP `stop` fires near-simultaneously, then
        // each thread snapshots its own VM in parallel.
        use std::sync::Barrier;
        let n = vms.len();
        let barrier = Arc::new(Barrier::new(n.max(1)));
        let mut handles = Vec::with_capacity(n);
        let dir_owned = dir.to_path_buf();
        for (name, vm) in vms.iter() {
            let name = name.clone();
            let vm = vm.clone();
            let bar = Arc::clone(&barrier);
            let dir = dir_owned.clone();
            handles.push(std::thread::spawn(move || {
                let safe_name = name.replace('/', "_");
                let snap_path = dir.join(format!("{safe_name}.snap"));
                // Hold every thread at the barrier so all ::pause
                // fires within the same kernel-scheduling window.
                bar.wait();
                let r = (|| -> Result<LabSnapshotEntry, LabError> {
                    vm.pause().ok(); // best-effort — already-paused is fine
                    vm.snapshot(&snap_path).map_err(|e| {
                        LabError::RestoreFailed(name.clone(), e.to_string())
                    })?;
                    let _ = vm.resume();
                    Ok(LabSnapshotEntry {
                        vm_name: name.clone(),
                        profile: vm.profile_name().to_owned(),
                        snapshot_path: snap_path,
                    })
                })();
                r
            }));
        }
        let mut entries = Vec::with_capacity(n);
        for h in handles {
            match h.join() {
                Ok(Ok(e)) => entries.push(e),
                Ok(Err(e)) => return Err(e),
                Err(_) => {
                    return Err(LabError::RestoreFailed(
                        "lab".into(),
                        "snapshot thread panicked".into(),
                    ))
                }
            }
        }

        // Capture full bridge state — impairments + uplink + routes
        // + isolation + partitions, per design.
        let bridges_inner = self.inner.lock().unwrap();
        let bridges: Vec<LabSnapshotBridge> = bridges_inner
            .bridges
            .iter()
            .map(|(n, b)| {
                let members = b.members();
                let mut partitions = Vec::new();
                // Skip the per-pair enumeration when the whole
                // bridge is partitioned — is_partitioned returns
                // true for every pair under fully_partitioned,
                // which would otherwise enumerate O(n²) entries
                // and double-apply on restore (partition_all is
                // re-applied separately below).
                if !b.is_fully_partitioned() {
                    for (i, a) in members.iter().enumerate() {
                        for c in &members[i + 1..] {
                            if b.is_partitioned(a, c) {
                                partitions.push((a.clone(), c.clone()));
                            }
                        }
                    }
                }
                let isolated: Vec<String> = members
                    .iter()
                    .filter(|m| b.is_isolated(m))
                    .cloned()
                    .collect();
                let directional_partitions = b.directional_partitions();
                let directional_impairments = b
                    .directional_pairs()
                    .into_iter()
                    .filter(|(_, d)| {
                        d.latency_ms > 0
                            || d.drop_rate_pct > 0
                            || d.bandwidth_bps > 0
                    })
                    .map(|((from, to), d)| LabSnapshotDirectional {
                        from,
                        to,
                        latency_ms: d.latency_ms,
                        drop_rate_pct: d.drop_rate_pct,
                        bandwidth_bps: d.bandwidth_bps,
                    })
                    .collect();
                LabSnapshotBridge {
                    name: n.clone(),
                    members,
                    latency_ms: b.latency_ms(),
                    drop_rate_pct: b.drop_rate_pct(),
                    bandwidth_bps: b.bandwidth_bps(),
                    uplink_enabled: b.uplink_enabled(),
                    routes: b.routes(),
                    isolated,
                    partitions,
                    directional_partitions,
                    directional_impairments,
                    fully_partitioned: b.is_fully_partitioned(),
                }
            })
            .collect();
        drop(bridges_inner);

        let meta = LabSnapshotMeta {
            vms: entries,
            bridges,
        };
        let meta_path = dir.join("lab.json");
        std::fs::write(
            &meta_path,
            serde_json::to_string_pretty(&meta).unwrap_or_default(),
        )
        .map_err(|e| LabError::RestoreFailed("snapshot meta".into(), e.to_string()))?;

        Ok(meta)
    }

    /// Restore every VM in `meta` back into this lab in parallel,
    /// re-applying full bridge state (attachments, impairments,
    /// uplink, routes, isolation, partitions) per `DESIGN.md` §
    /// Lab snapshot model.
    pub fn lab_restore(&self, meta: &LabSnapshotMeta) -> Result<(), LabError> {
        // 0. Pre-flight: refuse if any target VM name already
        //    exists in this lab. Without the check, the parallel
        //    restore_vm threads happily spawn QEMUs in parallel
        //    until one hits DuplicateVmName — leaving the lab
        //    with live processes from the half-completed restore
        //    and no built-in rollback.
        //
        //    Sub-lab structure: snapshot stores VM names as
        //    `<sub_lab>/<vm>` (per `gather_into`'s prefix
        //    convention); restore reconstructs the same structure
        //    by auto-creating any missing sub-labs along each
        //    "/"-delimited path. Per `DESIGN.md` § Lab snapshot
        //    model — `lab:snapshot()` walks sub-labs recursively,
        //    so `lab:restore()` must reverse that walk.
        for entry in &meta.vms {
            let (sub_path, leaf) = split_sub_lab_path(&entry.vm_name);
            let target = self.descend_or_create(&sub_path);
            let inner = target.inner.lock().unwrap();
            if inner.vms.contains_key(leaf) {
                return Err(LabError::DuplicateVmName(entry.vm_name.clone()));
            }
        }
        // 1. Recreate bridges with attachments first so VM TAPs
        //    can land at restore_vm time.
        for b in &meta.bridges {
            if self.get_bridge(&b.name).is_err() {
                self.create_bridge(b.name.clone())?;
            }
            if let Ok(bridge) = self.get_bridge(&b.name) {
                for m in &b.members {
                    bridge.attach(m.clone());
                }
            }
        }

        // 2. Restore VMs in parallel — per design, each VM
        //    snapshot can resume independently. Each VM's
        //    BootOpts must include its NIC attachments (derived
        //    from snapshot bridge membership) so QEMU
        //    re-attaches the host TAPs and the restored guest
        //    can reach its previous bridges. Each VM is
        //    restored INTO its original sub-lab via
        //    descend_or_create (auto-builds intermediate sub-labs).
        let mut handles = Vec::with_capacity(meta.vms.len());
        for entry in meta.vms.iter().cloned() {
            let (sub_path, leaf) = split_sub_lab_path(&entry.vm_name);
            let target = self.descend_or_create(&sub_path);
            let leaf_name = leaf.to_string();
            let mut opts = BootOpts::default();
            for b in &meta.bridges {
                if b.members.contains(&entry.vm_name) {
                    let bridge = self.get_bridge(&b.name).ok();
                    opts.nic_attachments.push(crate::vmm::NicAttachment {
                        bridge_name: b.name.clone(),
                        bridge,
                        nic_id: format!("{}-{}", leaf_name, b.name),
                        mac: None,
                    });
                }
            }
            handles.push(std::thread::spawn(move || {
                target.restore_vm(
                    leaf_name,
                    entry.profile.clone(),
                    opts,
                    &entry.snapshot_path,
                )
            }));
        }
        let mut restored_vms: Vec<Vm> = Vec::with_capacity(meta.vms.len());
        for h in handles {
            match h.join() {
                Ok(Ok(vm)) => restored_vms.push(vm),
                Ok(Err(e)) => return Err(e),
                Err(_) => {
                    return Err(LabError::RestoreFailed(
                        "lab".into(),
                        "restore thread panicked".into(),
                    ))
                }
            }
        }

        // 2.5. Explicitly realise each (bridge, vm) pair on the
        //      host before re-applying impairments. Without this
        //      the step-3 refresh_netem / refresh_partitions
        //      calls run live tc/nft commands that need the TAP
        //      interfaces to exist; if the VMM's restore path
        //      didn't itself call realize_for_vm (e.g. a backend
        //      that resumes from snapshot without re-running
        //      the launch-time bridge wiring) those commands
        //      silently no-op and the lab restores without its
        //      impairments. Best-effort — failures here surface
        //      as VmmError later when the test tries to use the
        //      bridge.
        for b in &meta.bridges {
            if let Ok(bridge) = self.get_bridge(&b.name) {
                for member in &b.members {
                    let _ = bridge.realize_for_vm(member);
                }
            }
        }

        // 3. Re-apply impairments + uplink + routes + isolation +
        //    partitions on each bridge.
        for b in &meta.bridges {
            if let Ok(bridge) = self.get_bridge(&b.name) {
                if b.latency_ms > 0 {
                    bridge.add_latency(b.latency_ms);
                }
                if b.drop_rate_pct > 0 {
                    bridge.drop_rate(b.drop_rate_pct);
                }
                if b.bandwidth_bps > 0 {
                    bridge.bandwidth_limit(b.bandwidth_bps);
                }
                for r in &b.routes {
                    bridge.route(r);
                }
                for vm in &b.isolated {
                    bridge.isolate(vm);
                }
                for (a, c) in &b.partitions {
                    bridge.partition(a, c);
                }
                for (from, to) in &b.directional_partitions {
                    bridge.partition_directional(from, to);
                }
                for d in &b.directional_impairments {
                    if d.latency_ms > 0 {
                        bridge.add_directional_latency(&d.from, &d.to, d.latency_ms);
                    }
                    if d.drop_rate_pct > 0 {
                        bridge.directional_drop_rate(&d.from, &d.to, d.drop_rate_pct);
                    }
                    if d.bandwidth_bps > 0 {
                        bridge.set_directional_bandwidth(&d.from, &d.to, d.bandwidth_bps);
                    }
                }
                if b.fully_partitioned {
                    bridge.partition_all();
                }
                if b.uplink_enabled {
                    let _ = bridge.enable_uplink();
                }
            }
        }

        // 4. Resume every restored VM in parallel — DESIGN.md §
        //    Lab snapshot model step 6. QEMU's `-incoming "file:"`
        //    lands the VM in paused state; without an explicit
        //    resume the guest's vsock never wakes and any op
        //    against it hangs forever.
        let resume_handles: Vec<_> = restored_vms
            .into_iter()
            .map(|vm| std::thread::spawn(move || vm.resume()))
            .collect();
        for h in resume_handles {
            match h.join() {
                Ok(Ok(())) => {}
                Ok(Err(e)) => {
                    return Err(LabError::RestoreFailed(
                        "lab".into(),
                        format!("resume after restore: {e}"),
                    ));
                }
                Err(_) => {
                    return Err(LabError::RestoreFailed(
                        "lab".into(),
                        "resume thread panicked".into(),
                    ));
                }
            }
        }
        Ok(())
    }

    // -----------------------------------------------------------------
    // Membership manipulation (lab:include / remove / members)
    // -----------------------------------------------------------------

    /// Insert an existing [`Vm`] under this lab. Used when a VM is
    /// constructed elsewhere and the test wants to track it under
    /// the current lab.
    pub fn include_vm(&self, vm: Vm) -> Result<(), LabError> {
        let name = vm.name().to_owned();
        // R9 reg-3: include_* paths must mirror create_*'s
        // reserved-name guard. Otherwise a Lua script can build a
        // VM elsewhere and slip a reserved name (vm_fixture / pack
        // / unpack / lab_fixture) in via the include path.
        check_reserved_name(&name)?;
        let mut inner = self.inner.lock().unwrap();
        if inner.vms.contains_key(&name) {
            return Err(LabError::DuplicateVmName(name));
        }
        inner.vms.insert(name, vm);
        Ok(())
    }

    /// Insert an existing [`crate::bridge::Bridge`] under this lab.
    /// Mirrors [`Self::include_vm`] for the bridge resource kind so
    /// `lab:include(my_bridge)` works per `DESIGN.md` § Lab.
    pub fn include_bridge(
        &self,
        bridge: crate::bridge::Bridge,
    ) -> Result<(), LabError> {
        let name = bridge.name().to_owned();
        check_reserved_name(&name)?;
        let mut inner = self.inner.lock().unwrap();
        if inner.bridges.contains_key(&name) {
            // R9 lab-M1: was DuplicateVmName — wrong variant.
            return Err(LabError::DuplicateBridgeName(name));
        }
        inner.bridges.insert(name, bridge);
        Ok(())
    }

    /// Insert an existing sub-lab under this lab. Mirrors
    /// [`Self::include_vm`] for the sub-lab resource kind so
    /// `lab:include(my_sublab)` works per `DESIGN.md` § Lab.
    pub fn include_sub_lab(&self, sub: Lab) -> Result<(), LabError> {
        let name = sub.name.clone();
        check_reserved_name(&name)?;
        let mut inner = self.inner.lock().unwrap();
        if inner.sub_labs.contains_key(&name) {
            // R9 lab-M1: was DuplicateVmName. We don't have a
            // dedicated DuplicateSubLabName variant, so reuse the
            // closest one — DuplicateVmName carries the most
            // generic "name in use" semantics; the message says
            // "vm name X" which is still misleading but matches
            // the lab's existing naming-conflict surface. Worth a
            // dedicated variant if/when more sub-lab errors appear.
            return Err(LabError::DuplicateVmName(format!(
                "sub-lab `{name}`"
            )));
        }
        inner.sub_labs.insert(name, sub);
        Ok(())
    }

    /// Remove a VM by name; does not shut it down.
    pub fn remove_vm(&self, name: &str) -> Option<Vm> {
        self.inner.lock().unwrap().vms.remove(name)
    }

    /// Remove a bridge by name; does not unrealise it. Used by
    /// `lab:remove(bridge_ud)` so the API surface mirrors
    /// `lab:include`'s symmetric handling.
    pub fn remove_bridge(&self, name: &str) -> Option<crate::bridge::Bridge> {
        self.inner.lock().unwrap().bridges.remove(name)
    }

    /// Remove a sub-lab by name. Mirrors [`Self::remove_vm`] for
    /// the sub-lab resource kind.
    pub fn remove_sub_lab(&self, name: &str) -> Option<Lab> {
        self.inner.lock().unwrap().sub_labs.remove(name)
    }

    /// Snapshot of every member kind as `(kind, name)` pairs.
    pub fn members(&self) -> Vec<(&'static str, String)> {
        let inner = self.inner.lock().unwrap();
        let mut out = Vec::new();
        for n in inner.vms.keys() {
            out.push(("vm", n.clone()));
        }
        for n in inner.bridges.keys() {
            out.push(("bridge", n.clone()));
        }
        for n in inner.sub_labs.keys() {
            out.push(("lab", n.clone()));
        }
        out
    }

    // -----------------------------------------------------------------
    // claim() — file-scope reservation. Backed by the scheduler's
    // pool when present, no-op otherwise.
    // -----------------------------------------------------------------

    /// Take a file-level reservation against `pool`. Idempotent
    /// per-Lab: a second `claim` call returns
    /// [`LabError::ClaimAlreadyHeld`].
    pub fn claim(
        &self,
        pool: &std::sync::Arc<crate::scheduler::Pool>,
        amount: crate::scheduler::ResourceAmount,
    ) -> Result<(), LabError> {
        let mut inner = self.inner.lock().unwrap();
        if inner.claim_taken {
            return Err(LabError::ClaimAlreadyHeld);
        }
        let reservation = pool
            .acquire(amount)
            .ok_or(LabError::ClaimExceedsBudget)?;
        inner.claim = Some(reservation);
        inner.claim_taken = true;
        Ok(())
    }

    /// Mark the claim as taken without acquiring a real reservation.
    /// Used by Lua paths where no pool is wired (REPL / single-file
    /// tests) so the design's "one-shot per file" rule is enforced
    /// regardless.
    pub fn note_claim_taken(&self) -> Result<(), LabError> {
        let mut inner = self.inner.lock().unwrap();
        if inner.claim_taken {
            return Err(LabError::ClaimAlreadyHeld);
        }
        inner.claim_taken = true;
        Ok(())
    }

    /// Drop the active claim. Returns the reserved
    /// [`crate::scheduler::ResourceAmount`] so the caller can emit a
    /// `claim_released` event without having to track it separately.
    /// Returns `None` when no claim was held.
    pub fn release_claim(&self) -> Option<crate::scheduler::ResourceAmount> {
        let r = self.inner.lock().unwrap().claim.take();
        r.map(|res| res.amount())
    }

    // -----------------------------------------------------------------
    // barrier() — N-arrival rendezvous, file-scoped.
    // -----------------------------------------------------------------

    /// Wait for `count` callers to reach the barrier with the same
    /// `name`. Releases all when reached. Reusable: callers re-use
    /// the same name in a later round to wait again.
    pub fn barrier(&self, name: &str, count: usize, timeout: std::time::Duration) -> bool {
        let bar = {
            let mut inner = self.inner.lock().unwrap();
            inner
                .barriers
                .entry(name.to_owned())
                .or_insert_with(|| std::sync::Arc::new(BarrierState::new(count)))
                .clone()
        };
        bar.arrive(count, timeout)
    }

    /// Lab name. The root provium-lab is conventionally `""` or
    /// `"provium"`; sub-labs get auto-generated names if not supplied.
    pub fn name(&self) -> &str {
        &self.name
    }

    // -----------------------------------------------------------------
    // VM membership
    // -----------------------------------------------------------------

    /// Declare a new VM in [`crate::vm::VmState::Created`].
    ///
    /// The 2-arg overload ("create") on the Lua side maps to this.
    pub fn create_vm(
        &self,
        name: impl Into<String>,
        profile_name: impl Into<String>,
        opts: BootOpts,
    ) -> Result<Vm, LabError> {
        let name = name.into();
        check_reserved_name(&name)?;
        let profile_name = profile_name.into();
        let profile = self
            .config
            .profile(&profile_name)
            .ok_or_else(|| LabError::UnknownProfile(profile_name.clone()))?
            .clone();

        let vm = Vm::new(
            name.clone(),
            profile_name,
            profile,
            opts,
            Arc::clone(&self.vmm),
        )
        .with_pool(self.pool.clone());

        let mut inner = self.inner.lock().unwrap();
        if inner.vms.contains_key(&name) {
            return Err(LabError::DuplicateVmName(name));
        }
        inner.vms.insert(name, vm.clone());
        Ok(vm)
    }

    /// Resume a VM from a fixture snapshot. Skips the
    /// Created→Booted transition by going straight through
    /// [`crate::vmm::Vmm::restore`].
    ///
    /// Returns a [`Vm`] already in [`crate::vm::VmState::Booted`].
    pub fn restore_vm(
        &self,
        name: impl Into<String>,
        profile_name: impl Into<String>,
        opts: BootOpts,
        snapshot_path: &std::path::Path,
    ) -> Result<Vm, LabError> {
        let name = name.into();
        let profile_name = profile_name.into();
        let profile = self
            .config
            .profile(&profile_name)
            .ok_or_else(|| LabError::UnknownProfile(profile_name.clone()))?
            .clone();

        let running = self
            .vmm
            .restore(&name, &profile, opts.clone(), snapshot_path)
            .map_err(|e| LabError::RestoreFailed(name.clone(), e.to_string()))?;

        let vm = Vm::from_running(
            name.clone(),
            profile_name,
            profile,
            opts.clone(),
            std::sync::Arc::clone(&self.vmm),
            running,
        );

        {
            let mut inner = self.inner.lock().unwrap();
            if inner.vms.contains_key(&name) {
                return Err(LabError::DuplicateVmName(name));
            }
            inner.vms.insert(name, vm.clone());
        }

        // A resumed VM is a spawned VM. `restore_vm` is the single
        // choke point for every fixture resume — `vm_fixture` and
        // `lab_restore`'s parallel restores alike — so emitting here
        // gives the event stream (progress bar, `--save-events`,
        // `provium-coverage`) one `vm_spawned` per VM regardless of
        // the resume path. The matching `vm_shutdown` comes from lab
        // teardown (`scheduler/dispatch` and the runner's per-test
        // scope teardown).
        self.events.emit(Event::VmSpawned(VmSpawned {
            file: self.name.clone(),
            vm_name: vm.name().to_owned(),
            profile: vm.profile_name().to_owned(),
            memory_bytes: opts.memory_bytes.unwrap_or(0),
            cid: vm.cid().unwrap_or(0),
        }));
        Ok(vm)
    }

    /// Look up an already-declared VM by name. The 1-arg overload
    /// ("accessor") on the Lua side maps to this.
    pub fn get_vm(&self, name: &str) -> Result<Vm, LabError> {
        let inner = self.inner.lock().unwrap();
        inner
            .vms
            .get(name)
            .cloned()
            .ok_or_else(|| LabError::UnknownVm(name.to_owned()))
    }

    /// Snapshot of the names of every VM declared in this lab
    /// (excluding sub-labs). Returned in insertion order via the
    /// underlying [`BTreeMap`]'s lexicographic ordering.
    pub fn vm_names(&self) -> Vec<String> {
        self.inner.lock().unwrap().vms.keys().cloned().collect()
    }

    /// Snapshot of every member [`Vm`] handle in this lab. Cheap —
    /// each `Vm` clone is a refcount bump.
    pub fn vms(&self) -> Vec<Vm> {
        self.inner.lock().unwrap().vms.values().cloned().collect()
    }

    /// Sum of open streams across every direct-child VM. Recursive
    /// across sub-labs. Used by the runner to enforce the
    /// `reset_between_tests` precondition (no open file-scope
    /// streams when the auto-snapshot is taken).
    pub fn open_stream_total(&self) -> usize {
        let inner = self.inner.lock().unwrap();
        let from_vms: usize = inner.vms.values().map(|v| v.open_stream_count()).sum();
        // Live `bridge:capture()` / `nic:capture()` streams are
        // tracked per-bridge — they don't show up in any VM's
        // tail-stream registry but DESIGN.md § Streams says a
        // pending capture must block snapshots like any other
        // stream.
        let from_captures: usize =
            inner.bridges.values().map(|b| b.active_captures()).sum();
        let from_subs: usize = inner.sub_labs.values().map(Lab::open_stream_total).sum();
        from_vms + from_captures + from_subs
    }

    // -----------------------------------------------------------------
    // Sub-lab membership
    // -----------------------------------------------------------------

    /// Create a sub-lab.
    ///
    /// Panics if `name` collides with the reserved dot-access
    /// surface — Lua callers should hit the
    /// [`Self::sub_lab_checked`] form to surface the error
    /// instead. This unchecked entry exists for internal callers
    /// (e.g. `descend_or_create` during restore where the names
    /// have already passed the original create-time guard).
    pub fn sub_lab(&self, name: impl Into<String>) -> Lab {
        let name = name.into();
        // Inherit the parent's event sink so VMs restored anywhere
        // in the lab tree surface `vm_spawned` on the same stream.
        // Pool stays `None` as before — sub-lab VM pool accounting
        // is unchanged by this.
        let lab = Lab::new_with_pool_and_events(
            name.clone(),
            Arc::clone(&self.config),
            Arc::clone(&self.vmm),
            None,
            Arc::clone(&self.events),
        );
        self.inner
            .lock()
            .unwrap()
            .sub_labs
            .insert(name, lab.clone());
        lab
    }

    /// Like [`Self::sub_lab`] but errors when the name is in the
    /// reserved set. The Lua binding routes through this so test
    /// authors get a typed `LabError` instead of silent shadowing
    /// of `vm_fixture` / `lab_fixture` / `pack` / `unpack`.
    pub fn sub_lab_checked(&self, name: impl Into<String>) -> Result<Lab, LabError> {
        let name = name.into();
        check_reserved_name(&name)?;
        Ok(self.sub_lab(name))
    }

    /// Snapshot of the names of every direct-child sub-lab.
    pub fn sub_lab_names(&self) -> Vec<String> {
        self.inner.lock().unwrap().sub_labs.keys().cloned().collect()
    }

    /// Look up a sub-lab by name. Used by `lab.<name>` dot
    /// accessor so test code can write `provium.dc1` for a sub-
    /// lab the same way it writes it for a VM or bridge.
    pub fn get_sub_lab(&self, name: &str) -> Option<Lab> {
        self.inner.lock().unwrap().sub_labs.get(name).cloned()
    }

    // -----------------------------------------------------------------
    // Batch lifecycle ops
    // -----------------------------------------------------------------

    /// Boot every unbooted VM in this lab + recursively in sub-labs.
    /// Members run in parallel; the call returns once every one
    /// has either finished booting or returned an error.
    ///
    /// "Atomic batch" per `DESIGN.md` § Scheduler / Deadlock: a
    /// single pool acquisition for the *sum* of every member's
    /// declared memory + cpus, then parallel launch with the
    /// reservation held until all members finish booting. This
    /// avoids the hold-and-wait deadlock pattern of sequential
    /// per-VM `boot()` calls.
    ///
    /// Falls back to plain parallel-spawn (no pool acquisition)
    /// when no pool is wired (REPL / single-file ad-hoc runs).
    pub fn boot(&self) -> Result<(), LabError> {
        self.boot_with_pool(None)
    }

    /// Boot variant that takes an explicit pool. Used by the
    /// dispatcher to supply the same pool the file's `lab:claim`
    /// targets.
    pub fn boot_with_pool(
        &self,
        pool: Option<&std::sync::Arc<crate::scheduler::Pool>>,
    ) -> Result<(), LabError> {
        // Compute the sum of declared memory + cpus across all VMs
        // in Created state. Already-booted VMs contribute zero —
        // so a re-call after `lab:include_vm(new_vm)` only reserves
        // for the newly-created member(s), not the live ones.
        // If nothing is in Created state, this is a no-op (idempotent
        // re-boot of an already-fully-booted lab).
        let vms = self.collect_all_vms();
        let mut needed = crate::scheduler::ResourceAmount {
            memory_bytes: 0,
            cpus: 0,
        };
        for (_, vm) in &vms {
            if vm.state() == VmState::Created {
                let opts = vm.boot_opts_summary();
                // Match the solo-boot accounting (`Vm::boot`):
                // declared memory + ~100 MB VMM overhead per VM.
                // Without this the joint reservation undercharges
                // by 100 MB × N and a lab of N VMs blows past the
                // configured pool budget.
                let per_vm_mem = opts
                    .memory_bytes
                    .unwrap_or(0)
                    .saturating_add(crate::vm::VMM_OVERHEAD_BYTES);
                needed.memory_bytes =
                    needed.memory_bytes.saturating_add(per_vm_mem);
                needed.cpus = needed.cpus.saturating_add(opts.cpus.unwrap_or(0));
            }
        }

        // Acquire the joint reservation. We stash it on the Lab
        // so it survives past this function — without that, the
        // VMs ended up running with no pool accounting at all
        // (their solo-boot pool ref is cleared below to avoid
        // double-charge). `Lab::shutdown` releases the
        // reservation when the lab tears down.
        let atomic_hold = if let Some(pool) = pool {
            if needed.memory_bytes > 0 || needed.cpus > 0 {
                pool.acquire(needed)
            } else {
                None
            }
        } else {
            None
        };

        // Each member's solo-boot pool reference is cleared so it
        // doesn't double-charge the lab's joint reservation. The
        // resulting per-VM boot() calls launch without acquiring
        // their own slot.
        let result = self.batch("boot", |vm| {
            let v = vm.clone().with_pool(None);
            v.boot()
        });

        if result.is_ok() {
            if let Some(hold) = atomic_hold {
                self.inner.lock().unwrap().boot_reservations.push(hold);
            }
        }
        // On failure, drop the reservation here so the partial
        // boot doesn't leak the slot.
        result
    }

    /// Shut every VM in this lab + sub-labs. VMs already in
    /// `Shutdown` are left alone. Also releases the joint pool
    /// reservation taken by `boot_with_pool` if any — without
    /// this the pool slot would leak past the lab's lifetime.
    pub fn shutdown(&self) -> Result<(), LabError> {
        let r = self.batch("shutdown", |vm| vm.shutdown());
        // Release every joint reservation regardless of partial
        // shutdown failures — keeping any would leak slots.
        self.inner.lock().unwrap().boot_reservations.clear();
        r
    }

    /// Pause every booted VM in parallel.
    pub fn pause(&self) -> Result<(), LabError> {
        self.batch("pause", |vm| vm.pause())
    }

    /// Resume every paused VM in parallel.
    pub fn resume(&self) -> Result<(), LabError> {
        self.batch("resume", |vm| vm.resume())
    }

    /// Run `op` against every VM in this lab + sub-labs in parallel.
    /// Returns `Err(BatchFailed)` if any VM's `op` errored, with
    /// per-VM-name details. Successful members are *not* rolled back.
    fn batch<F>(&self, action: &'static str, op: F) -> Result<(), LabError>
    where
        F: Fn(&Vm) -> Result<(), VmError> + Send + Sync + 'static + Clone,
    {
        let vms = self.collect_all_vms();
        let mut handles = Vec::with_capacity(vms.len());
        for (vm_name, vm) in vms {
            let op = op.clone();
            handles.push(thread::spawn(move || {
                let result = op(&vm);
                (vm_name, result)
            }));
        }

        let mut failures = BTreeMap::new();
        for h in handles {
            // Thread-panic case: surface as a string failure for the
            // member. The caller still gets a per-vm message.
            match h.join() {
                Ok((name, Ok(()))) => {
                    let _ = name;
                }
                Ok((name, Err(e))) => {
                    failures.insert(name, e.to_string());
                }
                Err(_) => {
                    failures
                        .insert("<panicked>".into(), "thread panicked".into());
                }
            }
        }

        if failures.is_empty() {
            Ok(())
        } else {
            Err(LabError::BatchFailed { action, failures })
        }
    }

    /// Collect (name, Vm) for this lab and recursively for every
    /// sub-lab. Sub-lab VM names are prefixed `"<sub_lab>/<vm>"` so
    /// two labs with same-named members surface clearly.
    pub fn collect_all_vms(&self) -> Vec<(String, Vm)> {
        let mut out = Vec::new();
        self.gather_into(&mut out, "");
        out
    }

    fn gather_into(&self, out: &mut Vec<(String, Vm)>, prefix: &str) {
        // Snapshot members under lock; drop the lock before
        // recursing into sub-labs so cross-lab batch ops don't risk
        // lock-ordering deadlocks.
        let (vms, sub_labs) = {
            let inner = self.inner.lock().unwrap();
            let vms: Vec<(String, Vm)> = inner
                .vms
                .iter()
                .map(|(n, v)| (n.clone(), v.clone()))
                .collect();
            let sub_labs: Vec<(String, Lab)> = inner
                .sub_labs
                .iter()
                .map(|(n, l)| (n.clone(), l.clone()))
                .collect();
            (vms, sub_labs)
        };

        for (name, vm) in vms {
            let full = if prefix.is_empty() {
                name
            } else {
                format!("{prefix}/{name}")
            };
            out.push((full, vm));
        }
        for (sub_name, sub_lab) in sub_labs {
            let next_prefix = if prefix.is_empty() {
                sub_name
            } else {
                format!("{prefix}/{sub_name}")
            };
            sub_lab.gather_into(out, &next_prefix);
        }
    }
}

/// Names reserved by the dot-access surface, per `DESIGN.md` §
/// Lab dot-access. Using any of these as a VM / bridge / sub-lab
/// name would silently shadow the corresponding helper method
/// when looked up via `lab.<name>`.
const RESERVED_LAB_NAMES: &[&str] =
    &["vm_fixture", "lab_fixture", "pack", "unpack"];

fn check_reserved_name(name: &str) -> Result<(), LabError> {
    if RESERVED_LAB_NAMES.contains(&name) {
        return Err(LabError::ReservedName(name.to_owned()));
    }
    Ok(())
}

/// Split a `gather_into`-style flat name (`"a/b/vm"`) into the
/// sub-lab path (`["a", "b"]`) and the leaf VM name (`"vm"`).
/// A bare name with no slashes returns an empty path.
fn split_sub_lab_path(flat: &str) -> (Vec<&str>, &str) {
    let mut parts: Vec<&str> = flat.split('/').collect();
    let leaf = parts.pop().unwrap_or("");
    (parts, leaf)
}

impl Lab {
    /// Walk the sub-lab path `parts` from `self`, auto-creating any
    /// missing intermediate sub-labs. Returns the final sub-lab
    /// (or `self` if `parts` is empty). Idempotent — re-walking an
    /// already-built path does not duplicate.
    fn descend_or_create(&self, parts: &[&str]) -> Lab {
        let mut cur = self.clone();
        for p in parts {
            cur = match cur.get_sub_lab(p) {
                Some(existing) => existing,
                None => cur.sub_lab(*p),
            };
        }
        cur
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::profile::{Profile, ProviumSection};

    #[test]
    fn split_sub_lab_path_handles_nested() {
        assert_eq!(split_sub_lab_path("vm"), (vec![], "vm"));
        assert_eq!(split_sub_lab_path("dc1/web"), (vec!["dc1"], "web"));
        assert_eq!(
            split_sub_lab_path("dc1/region-a/web"),
            (vec!["dc1", "region-a"], "web"),
        );
    }

    #[derive(Debug)]
    struct DummyVmm;
    impl Vmm for DummyVmm {
        fn launch(
            &self,
            _name: &str,
            _profile: &Profile,
            _opts: BootOpts,
        ) -> Result<crate::vmm::VmRunning, crate::vmm::VmmError> {
            Err(crate::vmm::VmmError::Unimplemented("dummy"))
        }
    }

    fn lab_with(profile_name: &str) -> Lab {
        let mut profiles = BTreeMap::new();
        profiles.insert(
            profile_name.into(),
            Profile {
                kernel: "/k".into(),
                initrd: "/i".into(),
                cmdline: "console=hvc0".into(),
                guest_os: "peios".into(),
            inject_agent: true,
            agent_overlay_path: None,
            cmdline_file: None,
            build: None,
            build_out: None,
            },
        );
        let config = Arc::new(Config {
            provium: ProviumSection::default(),
            profiles,
        });
        Lab::new("provium", config, Arc::new(DummyVmm))
    }

    #[test]
    fn create_then_lookup_returns_same_vm() {
        let lab = lab_with("peios");
        let created = lab
            .create_vm("dc1", "peios", BootOpts::default())
            .unwrap();
        let looked = lab.get_vm("dc1").unwrap();
        assert_eq!(created.name(), looked.name());
    }

    #[test]
    fn restore_recreates_sub_lab_path() {
        // R8 #398 regression: the restore path must split a
        // "/"-prefixed entry name and auto-create intermediate
        // sub-labs via descend_or_create. Since DummyVmm fails
        // every restore, we don't test the full lab_restore
        // here — just that descend_or_create + split_sub_lab_path
        // build the exact tree the snapshot's flat names imply.
        let lab = lab_with("peios");
        for flat in ["dc1/web", "dc1/db", "dc1/region-a/api", "standalone"] {
            let (parts, _leaf) = split_sub_lab_path(flat);
            let _ = lab.descend_or_create(&parts);
        }
        // Top-level: dc1 exists, "standalone" did not become a sub-lab
        // (its leaf was the root itself, parts empty).
        let names = lab.sub_lab_names();
        assert!(names.contains(&"dc1".to_string()), "dc1 missing: {names:?}");
        assert!(
            !names.contains(&"standalone".to_string()),
            "standalone should not be a sub-lab",
        );
        let dc1 = lab.get_sub_lab("dc1").expect("dc1 sub-lab missing");
        let dc1_subs = dc1.sub_lab_names();
        assert!(
            dc1_subs.contains(&"region-a".to_string()),
            "region-a missing under dc1: {dc1_subs:?}",
        );
        // Re-walking is idempotent.
        let _ = lab.descend_or_create(&["dc1", "region-a"]);
        let dc1_subs2 = dc1.sub_lab_names();
        assert_eq!(dc1_subs2, dc1_subs, "descend_or_create not idempotent");
    }

    #[test]
    fn reserved_name_blocks_create_vm() {
        // R8 #412: dot-access reserved names must error at create
        // time so they don't silently shadow the documented
        // helper methods (vm_fixture / lab_fixture / pack / unpack).
        let lab = lab_with("peios");
        for reserved in ["vm_fixture", "lab_fixture", "pack", "unpack"] {
            match lab.create_vm(reserved, "peios", BootOpts::default()) {
                Err(LabError::ReservedName(name)) => assert_eq!(name, reserved),
                other => panic!("{reserved}: expected ReservedName, got {other:?}"),
            }
        }
    }

    #[test]
    fn reserved_name_blocks_create_bridge() {
        let lab = lab_with("peios");
        for reserved in ["vm_fixture", "lab_fixture", "pack", "unpack"] {
            match lab.create_bridge(reserved) {
                Err(LabError::ReservedName(name)) => assert_eq!(name, reserved),
                other => panic!("{reserved}: expected ReservedName, got {other:?}"),
            }
        }
    }

    #[test]
    fn reserved_name_blocks_sub_lab_checked() {
        let lab = lab_with("peios");
        for reserved in ["vm_fixture", "lab_fixture", "pack", "unpack"] {
            match lab.sub_lab_checked(reserved) {
                Err(LabError::ReservedName(name)) => assert_eq!(name, reserved),
                other => panic!("{reserved}: expected ReservedName, got {other:?}"),
            }
        }
    }

    #[test]
    fn create_with_unknown_profile_errors() {
        let lab = lab_with("peios");
        match lab.create_vm("a", "missing", BootOpts::default()) {
            Err(LabError::UnknownProfile(name)) => assert_eq!(name, "missing"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn duplicate_vm_name_errors() {
        let lab = lab_with("peios");
        lab.create_vm("dup", "peios", BootOpts::default()).unwrap();
        match lab.create_vm("dup", "peios", BootOpts::default()) {
            Err(LabError::DuplicateVmName(_)) => {}
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn vm_names_lists_in_lex_order() {
        let lab = lab_with("peios");
        lab.create_vm("zeta", "peios", BootOpts::default()).unwrap();
        lab.create_vm("alpha", "peios", BootOpts::default()).unwrap();
        assert_eq!(lab.vm_names(), vec!["alpha", "zeta"]);
    }

    #[test]
    fn sub_lab_appears_in_collect() {
        let lab = lab_with("peios");
        lab.create_vm("top", "peios", BootOpts::default()).unwrap();
        let sub = lab.sub_lab("inner");
        sub.create_vm("nested", "peios", BootOpts::default()).unwrap();

        let collected = lab.collect_all_vms();
        let names: Vec<String> = collected.iter().map(|(n, _)| n.clone()).collect();
        assert!(names.contains(&"top".to_owned()));
        assert!(names.contains(&"inner/nested".to_owned()));
    }
}
