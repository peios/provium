//! Network bridge resource.
//!
//! Slice 9 ships the API surface + graph tracking — `bridge:attach`,
//! `bridge:partition`, impairments — recorded in [`BridgeInner`].
//! Actual host networking (TAP devices, `tc netem`, NAT uplink)
//! is the slice 9.5 follow-up; the recorded state is enough to
//! exercise the API in tests today and is what the real impl
//! will consume when it lands.
//!
//! Bridges live in [`crate::lab::Lab`] alongside VMs and sub-labs.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex};

/// One bridge.
///
/// Cheap to clone — mutable state lives behind an `Arc<Mutex<…>>`.
pub struct Bridge {
    name: String,
    inner: Arc<Mutex<BridgeInner>>,
    /// Live capture-stream count. Incremented by
    /// [`Self::register_capture`] and decremented when the
    /// returned [`CaptureGuard`] drops. Read by
    /// [`Self::active_captures`] for the snapshot precondition.
    captures: Arc<std::sync::atomic::AtomicUsize>,
}

#[derive(Default)]
#[allow(clippy::struct_excessive_bools)]
struct BridgeInner {
    /// VM names attached to this bridge (set, not list — duplicate
    /// attaches are no-ops).
    attached: BTreeSet<String>,
    /// Symmetric partition rules: pair (lo, hi) means a↔b is cut.
    partitions: BTreeSet<(String, String)>,
    /// Whole-bridge latency (ms). 0 = none.
    latency_ms: u32,
    /// Whole-bridge drop rate, percent (0–100).
    drop_rate_pct: u32,
    /// Bandwidth limit, bps. 0 = none.
    bandwidth_bps: u64,
    /// Per-pair directional impairments. Map from (from, to) to
    /// the impairment values; absence = none.
    directional: BTreeMap<(String, String), DirectionalImpairment>,
    /// `true` once `partition_all` has cut the whole bridge.
    fully_partitioned: bool,
    /// L3 routes — bridges this one is allowed to reach.
    routed_to: BTreeSet<String>,
    /// VMs in solitary on this bridge.
    isolated: BTreeSet<String>,
    /// Host-side realisation state. Tracks whether `ip link add` has
    /// run for this bridge, which TAPs have been created, and whether
    /// netem has been applied.
    realized: bool,
    /// VM names whose TAPs have been added on the host.
    realized_taps: BTreeSet<String>,
    /// Whole-bridge netem already applied. Stored as `(latency_ms,
    /// drop_pct)`; `None` means none applied.
    realized_netem: Option<(u32, u32)>,
    /// Per-tap shaping already applied for directional impairments.
    /// Maps `from` VM name → its currently-applied `(latency_ms,
    /// drop_pct, bandwidth_bps)`. Each entry corresponds to either
    /// a `tc qdisc add … root netem` (when bandwidth_bps is 0) or
    /// an HTB-root + class + optional netem-child chain (when
    /// bandwidth_bps is non-zero) on the from-VM's TAP.
    realized_directional: BTreeMap<String, (u32, u32, u64)>,
    /// Asymmetric partition rules — `(from, to)` means block
    /// `from→to` only. Reverse direction stays open unless a
    /// matching `(to, from)` entry exists.
    directional_partitions: BTreeSet<(String, String)>,
    /// `true` once the NAT uplink has been installed on the host.
    uplink_realized: bool,
    /// User intent for uplink — drives realisation in
    /// [`Bridge::refresh_uplink`].
    uplink_enabled: bool,
}

/// Directional impairment configured per (from, to) pair.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DirectionalImpairment {
    /// Latency in ms.
    pub latency_ms: u32,
    /// Drop rate (0–100).
    pub drop_rate_pct: u32,
    /// Bandwidth limit (bps). 0 = unset.
    pub bandwidth_bps: u64,
}

impl std::fmt::Debug for Bridge {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let inner = self.inner.lock().unwrap();
        f.debug_struct("Bridge")
            .field("name", &self.name)
            .field("attached", &inner.attached)
            .field("partitions", &inner.partitions)
            .field("latency_ms", &inner.latency_ms)
            .field("drop_rate_pct", &inner.drop_rate_pct)
            .finish()
    }
}

impl Clone for Bridge {
    fn clone(&self) -> Self {
        Self {
            name: self.name.clone(),
            inner: Arc::clone(&self.inner),
            captures: Arc::clone(&self.captures),
        }
    }
}

/// RAII guard returned by [`Bridge::register_capture`]. Increments
/// the bridge's active-capture counter on construction and
/// decrements it on drop. Held by [`crate::lua::capture_ud::CaptureUd`]
/// for the lifetime of the underlying tcpdump child so the
/// snapshot precondition can see the live capture.
#[derive(Debug)]
pub struct CaptureGuard {
    counter: Arc<std::sync::atomic::AtomicUsize>,
}

impl Drop for CaptureGuard {
    fn drop(&mut self) {
        self.counter
            .fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
    }
}

impl Bridge {
    /// Build a new bridge with the given name.
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            inner: Arc::new(Mutex::new(BridgeInner::default())),
            captures: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        }
    }

    /// Bridge name.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Number of live capture streams attached to this bridge.
    /// Used by the snapshot precondition so an in-flight tcpdump
    /// child blocks the snapshot the same way a tail-file stream
    /// would.
    pub fn active_captures(&self) -> usize {
        self.captures
            .load(std::sync::atomic::Ordering::SeqCst)
    }

    /// Register a new capture stream — bumps the active-capture
    /// counter and returns a guard whose drop decrements it.
    pub fn register_capture(&self) -> CaptureGuard {
        self.captures
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        CaptureGuard {
            counter: Arc::clone(&self.captures),
        }
    }

    // -----------------------------------------------------------------
    // Membership
    // -----------------------------------------------------------------

    /// Attach a VM by name. No-op if already attached.
    pub fn attach(&self, vm_name: impl Into<String>) {
        self.inner.lock().unwrap().attached.insert(vm_name.into());
    }

    /// Detach a VM by name. Refreshes host nft state so any
    /// partition / isolation rules referencing the detached VM's
    /// TAP are removed; tears down the TAP itself if the bridge
    /// is realised. Without this, leftover DROP rules and a
    /// stale `realized_taps` entry would survive.
    pub fn detach(&self, vm_name: &str) {
        let (was_realized, had_tap) = {
            let mut inner = self.inner.lock().unwrap();
            inner.attached.remove(vm_name);
            inner.isolated.remove(vm_name);
            // Drop pair partitions + directional pairs that
            // reference this VM — otherwise refresh_partitions
            // would still emit rules for a TAP we just removed.
            inner
                .partitions
                .retain(|(a, b)| a != vm_name && b != vm_name);
            inner
                .directional_partitions
                .retain(|(a, b)| a != vm_name && b != vm_name);
            inner
                .directional
                .retain(|(a, b), _| a != vm_name && b != vm_name);
            let was_realized = inner.realized;
            let had_tap = inner.realized_taps.remove(vm_name);
            (was_realized, had_tap)
        };
        // Tear down the TAP interface if it was created for this
        // VM. Best-effort: failures (no CAP_NET_ADMIN, missing
        // device) are silently ignored.
        if was_realized && had_tap {
            let bridge_name = self.name.clone();
            let tap = crate::bridge_realize::tap_name_for_vm_on_bridge(
                vm_name,
                &bridge_name,
            );
            let _ = run_ip(&["link", "del", "dev", &tap]);
        }
        let _ = self.refresh_partitions();
    }

    /// Snapshot of the attached-VM set.
    pub fn members(&self) -> Vec<String> {
        self.inner
            .lock()
            .unwrap()
            .attached
            .iter()
            .cloned()
            .collect()
    }

    // -----------------------------------------------------------------
    // Partitions
    // -----------------------------------------------------------------

    /// Partition `a` and `b` symmetrically. Idempotent. Refreshes
    /// the host-side nft rules so the partition is enforced if
    /// the bridge has been realised.
    pub fn partition(&self, a: &str, b: &str) {
        let pair = canonical_pair(a, b);
        self.inner.lock().unwrap().partitions.insert(pair);
        let _ = self.refresh_partitions();
    }

    /// Heal a previous symmetric partition. Idempotent.
    pub fn unpartition(&self, a: &str, b: &str) {
        let pair = canonical_pair(a, b);
        self.inner.lock().unwrap().partitions.remove(&pair);
        let _ = self.refresh_partitions();
    }

    /// Directional partition: block `from→to` traffic only. The
    /// reverse direction stays open. Per `DESIGN.md` § Bridge.
    pub fn partition_directional(&self, from: &str, to: &str) {
        self.inner
            .lock()
            .unwrap()
            .directional_partitions
            .insert((from.to_owned(), to.to_owned()));
        let _ = self.refresh_partitions();
    }

    /// Heal a directional partition.
    pub fn unpartition_directional(&self, from: &str, to: &str) {
        self.inner
            .lock()
            .unwrap()
            .directional_partitions
            .remove(&(from.to_owned(), to.to_owned()));
        let _ = self.refresh_partitions();
    }

    /// `true` if `a` and `b` are currently partitioned.
    pub fn is_partitioned(&self, a: &str, b: &str) -> bool {
        let pair = canonical_pair(a, b);
        let inner = self.inner.lock().unwrap();
        inner.fully_partitioned || inner.partitions.contains(&pair)
    }

    /// Cut the whole bridge.
    pub fn partition_all(&self) {
        self.inner.lock().unwrap().fully_partitioned = true;
        let _ = self.refresh_partitions();
    }

    /// Heal the whole bridge.
    pub fn restore_all(&self) {
        self.inner.lock().unwrap().fully_partitioned = false;
        let _ = self.refresh_partitions();
    }

    /// Drop every impairment + partition. Per DESIGN.md § Bridge,
    /// `bridge:reset()` is scoped to "impairments + partitions" —
    /// it does NOT clear topology config (`routed_to`,
    /// `uplink_enabled`, isolation set). Refreshes the host nft /
    /// tc / netem state so the in-memory reset propagates to the
    /// kernel.
    pub fn reset(&self) {
        {
            let mut inner = self.inner.lock().unwrap();
            inner.partitions.clear();
            inner.directional_partitions.clear();
            inner.latency_ms = 0;
            inner.drop_rate_pct = 0;
            inner.bandwidth_bps = 0;
            inner.directional.clear();
            inner.fully_partitioned = false;
            // Do NOT pre-clear `realized_netem` — refresh_netem
            // uses it as the signal that a kernel qdisc EXISTS
            // and must be torn down. R9 found that pre-clearing
            // tripped the early-exit guard (realized_netem ==
            // None && lat == 0 && drop == 0 → return) and left
            // the actual qdisc in place. refresh_netem itself
            // will reset realized_netem to None after issuing
            // the `tc qdisc del`.
            // Preserve routed_to + uplink_enabled + isolated —
            // those are topology, not impairments. A test calling
            // reset() to clean up after a partition or netem run
            // would otherwise inadvertently drop NAT and L3 routes.
        }
        let _ = self.refresh_partitions();
        let _ = self.refresh_netem();
        let _ = self.refresh_bandwidth();
        let _ = self.refresh_directional();
    }

    /// L3 route: this bridge can reach `other`. Records the
    /// relationship for slice-9.6 nftables rules to consume.
    pub fn route(&self, other: &str) {
        self.inner
            .lock()
            .unwrap()
            .routed_to
            .insert(other.to_owned());
    }

    /// Snapshot of the routed-to bridges.
    pub fn routes(&self) -> Vec<String> {
        self.inner.lock().unwrap().routed_to.iter().cloned().collect()
    }

    /// Isolate `vm` from everyone else on the bridge. Mutually
    /// exclusive with `unisolate`; latest call wins.
    pub fn isolate(&self, vm: &str) {
        self.inner.lock().unwrap().isolated.insert(vm.to_owned());
        let _ = self.refresh_partitions();
    }

    /// Lift the isolation on `vm`.
    pub fn unisolate(&self, vm: &str) {
        self.inner.lock().unwrap().isolated.remove(vm);
        let _ = self.refresh_partitions();
    }

    /// `true` if `vm` is currently isolated.
    pub fn is_isolated(&self, vm: &str) -> bool {
        self.inner.lock().unwrap().isolated.contains(vm)
    }

    // -----------------------------------------------------------------
    // Whole-bridge impairments
    // -----------------------------------------------------------------

    /// Set whole-bridge latency in ms. Pushes the new value to the
    /// host's `tc netem` immediately if the bridge is realised —
    /// without that the change would only land at the next VM
    /// boot's realize_for_vm call.
    pub fn add_latency(&self, ms: u32) {
        self.inner.lock().unwrap().latency_ms = ms;
        let _ = self.refresh_netem();
    }

    /// Set whole-bridge drop rate as a percent (0-100). Pushes to
    /// the host's `tc netem` immediately if realised.
    pub fn drop_rate(&self, pct: u32) {
        self.inner.lock().unwrap().drop_rate_pct = pct.min(100);
        let _ = self.refresh_netem();
    }

    /// Set whole-bridge bandwidth limit in bits/sec. `0` clears.
    /// Refreshes host-side `tc tbf` configuration.
    pub fn bandwidth_limit(&self, bps: u64) {
        self.inner.lock().unwrap().bandwidth_bps = bps;
        let _ = self.refresh_bandwidth();
    }

    /// Whole-bridge latency.
    pub fn latency_ms(&self) -> u32 {
        self.inner.lock().unwrap().latency_ms
    }

    /// Whole-bridge drop percent.
    pub fn drop_rate_pct(&self) -> u32 {
        self.inner.lock().unwrap().drop_rate_pct
    }

    /// Whole-bridge bandwidth limit in bits/sec. `0` means
    /// unlimited.
    pub fn bandwidth_bps(&self) -> u64 {
        self.inner.lock().unwrap().bandwidth_bps
    }

    /// Snapshot of every directional impairment as
    /// `((from, to), DirectionalImpairment)` triples. Used by
    /// `lab:snapshot` so a restore can reconstruct per-pair tc
    /// shaping.
    pub fn directional_pairs(&self) -> Vec<((String, String), DirectionalImpairment)> {
        self.inner
            .lock()
            .unwrap()
            .directional
            .iter()
            .map(|((a, b), v)| ((a.clone(), b.clone()), *v))
            .collect()
    }

    /// Snapshot of the directional partition set. Each entry is a
    /// one-way drop `from → to`. Reverse direction stays open
    /// unless a separate `(to, from)` entry exists.
    pub fn directional_partitions(&self) -> Vec<(String, String)> {
        self.inner
            .lock()
            .unwrap()
            .directional_partitions
            .iter()
            .cloned()
            .collect()
    }

    /// Snapshot of every symmetric pair partition.
    pub fn symmetric_partitions(&self) -> Vec<(String, String)> {
        self.inner.lock().unwrap().partitions.iter().cloned().collect()
    }

    /// Snapshot of the isolated-vm set.
    pub fn isolated_vms(&self) -> Vec<String> {
        self.inner.lock().unwrap().isolated.iter().cloned().collect()
    }

    /// `true` if the bridge is in the "everyone partitioned from
    /// everyone" state set by [`Self::partition_all`].
    pub fn is_fully_partitioned(&self) -> bool {
        self.inner.lock().unwrap().fully_partitioned
    }

    // -----------------------------------------------------------------
    // Directional impairments
    // -----------------------------------------------------------------

    /// Set directional latency from `a` to `b`. If the bridge has
    /// already been realised on the host, pushes the change to `tc`
    /// immediately; otherwise the value is recorded and applied at
    /// the next `realize_for_vm`.
    pub fn add_directional_latency(&self, from: &str, to: &str, ms: u32) {
        {
            let mut inner = self.inner.lock().unwrap();
            let entry = inner
                .directional
                .entry((from.to_owned(), to.to_owned()))
                .or_default();
            entry.latency_ms = ms;
            // R9 lab-Mi2: GC zero-valued entries so the
            // directional map doesn't grow unbounded across a
            // long test session that toggles impairments.
            gc_directional_entry(&mut inner.directional, from, to);
        }
        let _ = self.refresh_directional();
    }

    /// Set directional bandwidth limit from `a` to `b` in bits per
    /// second. Realized via an HTB root qdisc on the source TAP
    /// (`tap-<from>`), combined with a `netem` child when latency
    /// or drop is also set on the same source. Idempotent and
    /// best-effort — missing TAPs (VM not yet booted) are picked
    /// up at the next `realize_for_vm`.
    ///
    /// Like `add_directional_latency` / `directional_drop_rate`,
    /// the `to` is graph-recorded but does not select a destination
    /// in the realized qdisc: HTB on the source TAP rate-limits
    /// *every* packet leaving that source, regardless of
    /// destination. When a source has multiple directional
    /// bandwidth entries (`from=A → to=B`, `from=A → to=C`,
    /// each with a different `bps`), the realized rate is the
    /// *maximum* — pick the loosest cap so no recorded pair is
    /// over-shaped.
    pub fn set_directional_bandwidth(&self, from: &str, to: &str, bps: u64) {
        {
            let mut inner = self.inner.lock().unwrap();
            let entry = inner
                .directional
                .entry((from.to_owned(), to.to_owned()))
                .or_default();
            entry.bandwidth_bps = bps;
            gc_directional_entry(&mut inner.directional, from, to);
        }
        let _ = self.refresh_directional();
    }

    /// Set directional drop rate from `a` to `b`. Same realisation
    /// semantics as [`Self::add_directional_latency`].
    pub fn directional_drop_rate(&self, from: &str, to: &str, pct: u32) {
        {
            let mut inner = self.inner.lock().unwrap();
            let entry = inner
                .directional
                .entry((from.to_owned(), to.to_owned()))
                .or_default();
            entry.drop_rate_pct = pct.min(100);
            gc_directional_entry(&mut inner.directional, from, to);
        }
        let _ = self.refresh_directional();
    }

    /// Look up directional impairment (returns Default if unset).
    pub fn directional(&self, from: &str, to: &str) -> DirectionalImpairment {
        self.inner
            .lock()
            .unwrap()
            .directional
            .get(&(from.to_owned(), to.to_owned()))
            .copied()
            .unwrap_or_default()
    }

    // -----------------------------------------------------------------
    // Host realisation
    // -----------------------------------------------------------------

    /// Realise this bridge on the host: create the bridge interface
    /// (if not already present), set it up, apply whole-bridge netem,
    /// and create the TAP for `vm_name` attached to the bridge.
    ///
    /// Idempotent — repeated calls for the same `vm_name` only add
    /// missing pieces. Requires `CAP_NET_ADMIN`; failures bubble up
    /// as [`std::io::Error`].
    ///
    /// Returns the TAP interface name created for the VM (matches the
    /// QEMU `-netdev tap,ifname=…` argument).
    pub fn realize_for_vm(&self, vm_name: &str) -> std::io::Result<String> {
        let bridge_name = self.name.clone();
        let tap = crate::bridge_realize::tap_name_for_vm_on_bridge(vm_name, &bridge_name);

        let (need_bridge, current_netem, target_netem, need_tap) = {
            let inner = self.inner.lock().unwrap();
            (
                !inner.realized,
                inner.realized_netem,
                if inner.latency_ms > 0 || inner.drop_rate_pct > 0 {
                    Some((inner.latency_ms, inner.drop_rate_pct))
                } else {
                    None
                },
                !inner.realized_taps.contains(vm_name),
            )
        };

        if need_bridge {
            // Use `ip link add … type bridge`. EEXIST is OK — another
            // VM may have realized concurrently.
            let _ = run_ip(&["link", "add", &bridge_name, "type", "bridge"]);
            run_ip(&["link", "set", "dev", &bridge_name, "up"])?;
        }
        if current_netem != target_netem {
            if let Some((lat, drop)) = target_netem {
                let mut args: Vec<String> = vec![
                    "qdisc".into(),
                    if current_netem.is_some() {
                        "change".into()
                    } else {
                        "add".into()
                    },
                    "dev".into(),
                    bridge_name.clone(),
                    "root".into(),
                    "netem".into(),
                ];
                if lat > 0 {
                    args.push("delay".into());
                    args.push(format!("{lat}ms"));
                }
                if drop > 0 {
                    args.push("loss".into());
                    args.push(format!("{drop}%"));
                }
                let argv: Vec<&str> = args.iter().map(String::as_str).collect();
                run_tc(&argv)?;
            } else if current_netem.is_some() {
                // Clear netem.
                let _ = run_tc(&["qdisc", "del", "dev", &bridge_name, "root"]);
            }
        }
        if need_tap {
            let _ = run_ip(&["tuntap", "add", "dev", &tap, "mode", "tap"]);
            run_ip(&["link", "set", "dev", &tap, "master", &bridge_name])?;
            run_ip(&["link", "set", "dev", &tap, "up"])?;
        }

        // Apply directional impairments on this VM's TAP. Each
        // (from=vm_name, to=*) collapses into a single per-tap qdisc
        // chain — Linux netem/HTB are per-interface, so multiple
        // directionals from the same VM merge into the *worst-case*
        // (highest latency, highest drop, highest bandwidth ceiling).
        // This is the same model the slice-9.5 plan_for emits.
        //
        // Realization shape depends on which axes are non-zero:
        // - bandwidth only      → `htb default 10` root + one class
        // - latency/drop only   → `netem` root (legacy path)
        // - bandwidth + netem   → `htb default 10` root + class +
        //                          `netem` child on the class
        // - all zero            → no qdisc (or delete existing root)
        //
        // Whenever the realization shape changes (bandwidth toggled
        // on/off, axes flipped), we tear down the existing root
        // first — `tc change` cannot convert a netem qdisc to an
        // htb one (different kind). The teardown is cheap and the
        // alternative is brittle dispatch on every transition.
        let directional_target = self.collapsed_directional_for(vm_name);
        let directional_current = self
            .inner
            .lock()
            .unwrap()
            .realized_directional
            .get(vm_name)
            .copied();
        if directional_current != directional_target {
            // Always start with a clean slate for direction-changes
            // — `tc qdisc del` on a missing root is harmless.
            if directional_current.is_some() {
                let _ = run_tc(&["qdisc", "del", "dev", &tap, "root"]);
            }
            if let Some((lat, drop, bps)) = directional_target {
                if bps > 0 {
                    install_htb_with_optional_netem(&tap, lat, drop, bps)?;
                } else {
                    install_netem_root(&tap, lat, drop)?;
                }
            }
        }

        let mut inner = self.inner.lock().unwrap();
        inner.realized = true;
        inner.realized_netem = target_netem;
        inner.realized_taps.insert(vm_name.to_owned());
        match directional_target {
            Some(v) => {
                inner.realized_directional.insert(vm_name.to_owned(), v);
            }
            None => {
                inner.realized_directional.remove(vm_name);
            }
        }
        Ok(tap)
    }

    /// Apply / clear the whole-bridge `tc tbf` qdisc to enforce
    /// the bandwidth ceiling. Best-effort: if netem already owns
    /// the root qdisc, we replace with a tbf+netem chain via
    /// classful htb. v1 simplification: tbf as root when no netem,
    /// or no-op when netem is also configured (rare combination).
    /// Re-apply the whole-bridge `tc netem` (latency + drop) to
    /// the realised bridge. Mirrors [`Self::refresh_bandwidth`]
    /// for the netem axis so `bridge:add_latency()` /
    /// `bridge:drop_rate()` take effect immediately when the
    /// bridge is already up. Idempotent: deletes the existing
    /// netem qdisc and reinstalls one when at least one knob is
    /// non-zero. Returns silently when the bridge isn't realised.
    pub fn refresh_netem(&self) -> std::io::Result<()> {
        let (lat, drop, realized, realized_netem) = {
            let inner = self.inner.lock().unwrap();
            (
                inner.latency_ms,
                inner.drop_rate_pct,
                inner.realized,
                inner.realized_netem,
            )
        };
        if !realized {
            return Ok(());
        }
        // Skip the `tc qdisc del` shell-out when the bridge has
        // never had a netem qdisc installed AND nothing new is
        // being asked for. Without this guard we run a tc
        // command that always returns non-zero (no qdisc to
        // delete) for every NIC counter poll on an unshaped
        // bridge — noisy for strace-based test diagnostics.
        if realized_netem.is_none() && lat == 0 && drop == 0 {
            return Ok(());
        }
        // Delete first so re-apply is idempotent. Missing qdisc
        // isn't an error — `tc qdisc del` returns non-zero in
        // that case but we don't care.
        let _ = run_cmd("tc", &["qdisc", "del", "dev", &self.name, "root"]);
        if lat == 0 && drop == 0 {
            // Nothing to install — the delete already cleared.
            self.inner.lock().unwrap().realized_netem = None;
            return Ok(());
        }
        let mut args: Vec<String> = vec![
            "qdisc".into(),
            "add".into(),
            "dev".into(),
            self.name.clone(),
            "root".into(),
            "netem".into(),
        ];
        if lat > 0 {
            args.push("delay".into());
            args.push(format!("{lat}ms"));
        }
        if drop > 0 {
            args.push("loss".into());
            args.push(format!("{drop}%"));
        }
        let argv: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
        run_cmd("tc", &argv)?;
        Ok(())
    }

    pub fn refresh_bandwidth(&self) -> std::io::Result<()> {
        let (bps, lat, drop, realized) = {
            let inner = self.inner.lock().unwrap();
            (
                inner.bandwidth_bps,
                inner.latency_ms,
                inner.drop_rate_pct,
                inner.realized,
            )
        };
        if !realized {
            return Ok(());
        }
        // Replace the root qdisc. tc replace is idempotent.
        if bps == 0 {
            // Clear by deleting the root. If latency / drop is
            // still set, refresh_netem reinstalls the netem-only
            // qdisc — without it, clearing bandwidth would also
            // silently strip latency/drop from the kernel even
            // though the in-memory state still records them.
            let _ = run_cmd("tc", &["qdisc", "del", "dev", &self.name, "root"]);
            if lat > 0 || drop > 0 {
                let _ = self.refresh_netem();
            }
            return Ok(());
        }
        if lat > 0 || drop > 0 {
            // Combined netem+tbf needs class-based htb — out of
            // scope for v1. Document by leaving the netem qdisc
            // alone; bandwidth is graph-state only in this case.
            return Ok(());
        }
        // Plain tbf. burst sized at bps/100 to allow ~10ms bursts.
        let burst = (bps / 800).max(1500);
        let limit = (bps / 8).max(1500);
        let bps_str = format!("{}bit", bps);
        let burst_str = format!("{}", burst);
        let limit_str = format!("{}", limit);
        run_cmd(
            "tc",
            &[
                "qdisc", "replace", "dev", &self.name, "root", "handle", "1:",
                "tbf", "rate", &bps_str, "burst", &burst_str, "limit",
                &limit_str,
            ],
        )?;
        Ok(())
    }

    /// Re-apply partition + isolation rules to the host nftables
    /// `bridge` family. Idempotent and best-effort — the bridge
    /// family table is wiped per-bridge and rebuilt to match
    /// current graph state. Returns silently when nft isn't
    /// available or the bridge hasn't been realised yet.
    pub fn refresh_partitions(&self) -> std::io::Result<()> {
        let bridge_name = self.name.clone();
        let (taps, partitions, directional, isolated, fully_partitioned, realized) = {
            let inner = self.inner.lock().unwrap();
            let taps: Vec<String> = inner
                .realized_taps
                .iter()
                .map(|vm| crate::bridge_realize::tap_name_for_vm_on_bridge(vm, &bridge_name))
                .collect();
            (
                taps,
                inner.partitions.clone(),
                inner.directional_partitions.clone(),
                inner.isolated.clone(),
                inner.fully_partitioned,
                inner.realized,
            )
        };
        if !realized {
            return Ok(());
        }
        let table_name = format!("provium_{}", sanitize_nft_name(&self.name));
        // Wipe existing — `nft delete table` errors if absent;
        // ignore.
        let _ = run_cmd("nft", &["delete", "table", "bridge", &table_name]);

        // Skip table creation if there's nothing to enforce.
        // The directional set must be checked too — without it a
        // bridge with only `bridge:partition({from=,to=})` calls
        // would short-circuit here and leave the directional rules
        // unreached, silently un-enforcing the partition.
        let nothing_to_do = !fully_partitioned
            && partitions.is_empty()
            && directional.is_empty()
            && isolated.is_empty();
        if nothing_to_do {
            return Ok(());
        }
        run_cmd("nft", &["add", "table", "bridge", &table_name])?;
        run_cmd(
            "nft",
            &[
                "add", "chain", "bridge", &table_name, "forward",
                "{ type filter hook forward priority 0 ; }",
            ],
        )?;

        if fully_partitioned {
            // Drop everything between any two members on this bridge.
            for a in &taps {
                for b in &taps {
                    if a == b {
                        continue;
                    }
                    let rule = format!("iifname \"{a}\" oifname \"{b}\" drop");
                    run_cmd("nft", &["add", "rule", "bridge", &table_name, "forward", &rule])?;
                }
            }
            return Ok(());
        }
        // Symmetric pair partitions: install drop rules in both
        // directions.
        for (va, vb) in &partitions {
            let ta = crate::bridge_realize::tap_name_for_vm_on_bridge(va, &bridge_name);
            let tb = crate::bridge_realize::tap_name_for_vm_on_bridge(vb, &bridge_name);
            for (i, o) in [(&ta, &tb), (&tb, &ta)] {
                let rule = format!("iifname \"{i}\" oifname \"{o}\" drop");
                run_cmd("nft", &["add", "rule", "bridge", &table_name, "forward", &rule])?;
            }
        }
        // Directional partitions: drop only `from→to`; reverse
        // stays open unless a separate (to, from) entry exists.
        for (from, to) in &directional {
            let tf = crate::bridge_realize::tap_name_for_vm_on_bridge(from, &bridge_name);
            let tt = crate::bridge_realize::tap_name_for_vm_on_bridge(to, &bridge_name);
            let rule = format!("iifname \"{tf}\" oifname \"{tt}\" drop");
            run_cmd("nft", &["add", "rule", "bridge", &table_name, "forward", &rule])?;
        }
        // Isolation: for each isolated VM, drop traffic to/from
        // every other member.
        for v in &isolated {
            let ti = crate::bridge_realize::tap_name_for_vm_on_bridge(v, &bridge_name);
            for other in &taps {
                if *other == ti {
                    continue;
                }
                for (i, o) in [(&ti, other), (other, &ti)] {
                    let rule = format!("iifname \"{i}\" oifname \"{o}\" drop");
                    run_cmd("nft", &["add", "rule", "bridge", &table_name, "forward", &rule])?;
                }
            }
        }
        Ok(())
    }

    /// Re-apply directional impairments for every realised TAP. Call
    /// this after `add_directional_latency` / `directional_drop_rate`
    /// changes the recorded state to push the new values to `tc`.
    ///
    /// Idempotent and best-effort — a missing TAP (VM not yet booted)
    /// is silently skipped because `realize_for_vm` will pick up the
    /// recorded state at boot.
    pub fn refresh_directional(&self) -> std::io::Result<()> {
        // Guard on `realized` for symmetry with refresh_netem /
        // refresh_bandwidth — when the bridge isn't realised on
        // the host (e.g. CAP_NET_ADMIN missing, or before any
        // boot), there's nothing to push to tc and we must not
        // run realize_for_vm against a non-existent interface.
        let (realized, vm_names) = {
            let inner = self.inner.lock().unwrap();
            (
                inner.realized,
                inner.realized_taps.iter().cloned().collect::<Vec<_>>(),
            )
        };
        if !realized {
            return Ok(());
        }
        for vm in &vm_names {
            self.realize_for_vm(vm)?;
        }
        Ok(())
    }

    /// Mark the bridge as wanting uplink (NAT to the host's primary
    /// interface). Realised immediately if the bridge already exists
    /// on the host; otherwise picked up at the next
    /// [`Self::realize_for_vm`].
    pub fn enable_uplink(&self) -> std::io::Result<()> {
        self.inner.lock().unwrap().uplink_enabled = true;
        self.refresh_uplink()
    }

    /// Lift the uplink intent and tear down any installed NAT rules.
    pub fn disable_uplink(&self) -> std::io::Result<()> {
        self.inner.lock().unwrap().uplink_enabled = false;
        self.refresh_uplink()
    }

    /// `true` if the bridge currently wants an uplink.
    pub fn uplink_enabled(&self) -> bool {
        self.inner.lock().unwrap().uplink_enabled
    }

    fn refresh_uplink(&self) -> std::io::Result<()> {
        let (want, have, bridge) = {
            let inner = self.inner.lock().unwrap();
            (inner.uplink_enabled, inner.uplink_realized, self.name.clone())
        };
        if want == have {
            return Ok(());
        }
        let upstream = match default_upstream_iface() {
            Some(s) => s,
            None => {
                // No default route — nothing to NAT to. Treat as
                // success so tests on isolated hosts don't error.
                let mut inner = self.inner.lock().unwrap();
                inner.uplink_realized = want;
                return Ok(());
            }
        };
        // Per-bridge nft table so disabling one bridge's uplink
        // doesn't tear down every other bridge's NAT. The previous
        // shared `ip provium` table meant `bridge_a:disable_uplink()`
        // silently dropped `bridge_b`'s masquerade.
        let table = uplink_table_for(&bridge);

        if want {
            // Enable IPv4 forwarding (best-effort — sysctl may be
            // read-only in containerised hosts).
            let _ = std::fs::write("/proc/sys/net/ipv4/ip_forward", "1");
            // Install a per-bridge masquerade rule. nftables form
            // preferred; fall back to iptables if nft isn't on PATH.
            if run_cmd("nft", &[
                "add", "table", "ip", &table,
            ]).is_ok()
                && run_cmd("nft", &[
                    "add", "chain", "ip", &table, "postrouting",
                    "{ type nat hook postrouting priority 100 ; }",
                ])
                .is_ok()
            {
                let rule = format!(
                    "oifname \"{upstream}\" iifname \"{bridge}\" masquerade"
                );
                run_cmd("nft", &["add", "rule", "ip", &table, "postrouting", &rule])?;
            } else {
                // iptables POSTROUTING does NOT accept `-i` —
                // input-interface tracking only exists in
                // pre-routing chains. Match purely on the
                // outbound interface; this is slightly broader
                // than the nftables `iifname` rule (which can
                // scope to this bridge), but matches the
                // canonical iptables MASQUERADE pattern. nft is
                // tried first so this fallback only fires when
                // nft is missing, where overscoping is the lesser
                // evil vs erroring out.
                run_cmd(
                    "iptables",
                    &[
                        "-t", "nat", "-A", "POSTROUTING", "-o", &upstream,
                        "-j", "MASQUERADE",
                    ],
                )?;
            }
        } else {
            // Best-effort delete; missing rule isn't an error.
            let _ = run_cmd(
                "iptables",
                &[
                    "-t", "nat", "-D", "POSTROUTING", "-o", &upstream,
                    "-j", "MASQUERADE",
                ],
            );
            let _ = run_cmd("nft", &["delete", "table", "ip", &table]);
        }
        self.inner.lock().unwrap().uplink_realized = want;
        Ok(())
    }

    /// Collapse all `(from=vm, to=*)` directional rules into a single
    /// `(latency_ms, drop_pct)` to apply to `vm`'s TAP. Per-pair
    /// shaping needs class-based htb + filter — out of scope for v1.
    /// Public alias of the worst-case directional collapse —
    /// used by [`crate::bridge_realize::plan_for`] so plan
    /// generation and live realisation produce identical netem
    /// args (was inconsistent: plan picked the FIRST partner's
    /// values, realize used the max).
    pub fn collapsed_directional_for_pub(
        &self,
        vm: &str,
    ) -> Option<(u32, u32, u64)> {
        self.collapsed_directional_for(vm)
    }

    /// Collapse every `(vm, *)` directional impairment into a single
    /// `(latency_ms, drop_rate_pct, bandwidth_bps)` tuple by taking
    /// the worst case (max) of each axis. Linux netem and HTB are
    /// per-interface — the source TAP can only carry one set of
    /// values, so we install the most restrictive of any declared
    /// pair. Reordering pairs doesn't matter; max is associative.
    ///
    /// Returns `None` when all three axes are zero.
    fn collapsed_directional_for(&self, vm: &str) -> Option<(u32, u32, u64)> {
        let inner = self.inner.lock().unwrap();
        let mut acc: Option<(u32, u32, u64)> = None;
        for ((from, _), d) in &inner.directional {
            if from != vm {
                continue;
            }
            let cur = acc.unwrap_or((0, 0, 0));
            acc = Some((
                cur.0.max(d.latency_ms),
                cur.1.max(d.drop_rate_pct),
                cur.2.max(d.bandwidth_bps),
            ));
        }
        acc.filter(|(l, d, b)| *l > 0 || *d > 0 || *b > 0)
    }

    /// `true` once any VM TAP has been realised on the host.
    pub fn is_realized(&self) -> bool {
        self.inner.lock().unwrap().realized
    }

    /// `true` if the host TAP for `vm_name` on this bridge has
    /// been created (i.e. `realize_for_vm(vm_name)` succeeded).
    /// Per-VM equivalent of [`Self::is_realized`] — used by
    /// `nic:capture` to gate tcpdump on the actual TAP existing,
    /// not just the bridge having any TAP at all.
    pub fn has_realized_tap(&self, vm_name: &str) -> bool {
        self.inner.lock().unwrap().realized_taps.contains(vm_name)
    }

    /// Tear down host-side state — best-effort delete of every TAP,
    /// the bridge interface, the partition nft table, and the
    /// uplink NAT rules. Errors are returned as the *first*
    /// failure but teardown continues regardless. Used at scope/
    /// file end.
    pub fn unrealize(&self) -> std::io::Result<()> {
        // Tearing down the bridge interface invalidates every
        // active per-tap tcpdump — the kernel closes their
        // sockets and the streams emit StreamEnd::Eof mid-read.
        // Warn once so test authors notice the truncation
        // instead of debugging a phantom-length capture.
        let active = self.active_captures();
        if active > 0 {
            eprintln!(
                "provium: bridge `{}` unrealize with {active} active capture(s) — \
                 captures will end at this point. Close streams before unrealize \
                 to avoid truncation.",
                self.name,
            );
        }
        let (taps, was_realized, uplink_was_realized) = {
            let mut inner = self.inner.lock().unwrap();
            let taps: Vec<String> = inner.realized_taps.iter().cloned().collect();
            inner.realized_taps.clear();
            let was = inner.realized;
            let uplink_was = inner.uplink_realized;
            inner.realized = false;
            inner.uplink_realized = false;
            inner.realized_netem = None;
            inner.realized_directional.clear();
            (taps, was, uplink_was)
        };
        let mut first_err: Option<std::io::Error> = None;

        // 1. Tear down per-bridge nft partition table (was created by
        //    refresh_partitions). Best-effort: missing table is fine.
        let table_name = format!("provium_{}", sanitize_nft_name(&self.name));
        let _ = run_cmd("nft", &["delete", "table", "bridge", &table_name]);

        // 2. Tear down NAT uplink (per-bridge nft table or matching
        //    iptables rule) if it was installed. Per-bridge so we
        //    only touch this bridge's table.
        if uplink_was_realized {
            let uplink_table = uplink_table_for(&self.name);
            let _ = run_cmd("nft", &["delete", "table", "ip", &uplink_table]);
            // iptables fallback — silently ignore failure.
            if let Some(upstream) = default_upstream_iface() {
                // Mirror the iptables ADD form in `enable_uplink`
                // — `-i` is invalid on POSTROUTING, so the
                // delete must match the bare `-o`-only rule.
                let _ = run_cmd(
                    "iptables",
                    &[
                        "-t", "nat", "-D", "POSTROUTING", "-o", &upstream,
                        "-j", "MASQUERADE",
                    ],
                );
            }
        }

        // 3. TAPs.
        for vm_name in &taps {
            let tap = crate::bridge_realize::tap_name_for_vm_on_bridge(vm_name, &self.name);
            if let Err(e) = run_ip(&["link", "del", "dev", &tap]) {
                first_err.get_or_insert(e);
            }
        }

        // 4. Bridge interface itself (also implicitly removes any
        //    remaining tc qdiscs).
        if was_realized {
            if let Err(e) = run_ip(&["link", "del", "dev", &self.name]) {
                first_err.get_or_insert(e);
            }
        }

        match first_err {
            Some(e) => Err(e),
            None => Ok(()),
        }
    }
}

fn run_ip(args: &[&str]) -> std::io::Result<()> {
    run_cmd("ip", args)
}

fn run_tc(args: &[&str]) -> std::io::Result<()> {
    run_cmd("tc", args)
}

/// Install a netem-only root qdisc on `tap` with `lat`ms delay
/// and `drop`% loss. At least one must be non-zero — passing
/// (0, 0) emits a degenerate `netem` qdisc with no clauses, which
/// the kernel accepts but is pointless. Callers gate on
/// `directional_target` being `Some` before invoking.
fn install_netem_root(tap: &str, lat: u32, drop: u32) -> std::io::Result<()> {
    let mut args: Vec<String> = vec![
        "qdisc".into(),
        "add".into(),
        "dev".into(),
        tap.to_string(),
        "root".into(),
        "netem".into(),
    ];
    if lat > 0 {
        args.push("delay".into());
        args.push(format!("{lat}ms"));
    }
    if drop > 0 {
        args.push("loss".into());
        args.push(format!("{drop}%"));
    }
    let argv: Vec<&str> = args.iter().map(String::as_str).collect();
    run_tc(&argv)
}

/// Install an HTB root qdisc on `tap` with one default class
/// rate-limited to `bps_bits` bits per second. If `lat`/`drop` is
/// also non-zero, chain a `netem` qdisc onto the class so packets
/// are first rate-limited, then delayed/dropped.
///
/// HTB layout:
/// - `root handle 1: htb default 10` — packets matching no class
///   fall to classid `1:10`.
/// - `parent 1: classid 1:10 htb rate <r>` — the one and only class.
/// - `parent 1:10 handle 10: netem …` — optional child for
///   latency/drop.
///
/// `burst` is sized at ~10ms of throughput (`rate / 800` bytes),
/// floored at 1500 bytes to match the smallest practical MTU.
/// Matches the convention the whole-bridge tbf path already uses.
fn install_htb_with_optional_netem(
    tap: &str,
    lat: u32,
    drop: u32,
    bps_bits: u64,
) -> std::io::Result<()> {
    run_tc(&[
        "qdisc", "add", "dev", tap, "root", "handle", "1:", "htb",
        "default", "10",
    ])?;
    let rate_str = format!("{bps_bits}bit");
    let burst = (bps_bits / 800).max(1500);
    let burst_str = burst.to_string();
    run_tc(&[
        "class", "add", "dev", tap, "parent", "1:", "classid", "1:10",
        "htb", "rate", &rate_str, "ceil", &rate_str, "burst", &burst_str,
    ])?;
    if lat > 0 || drop > 0 {
        let mut args: Vec<String> = vec![
            "qdisc".into(),
            "add".into(),
            "dev".into(),
            tap.to_string(),
            "parent".into(),
            "1:10".into(),
            "handle".into(),
            "10:".into(),
            "netem".into(),
        ];
        if lat > 0 {
            args.push("delay".into());
            args.push(format!("{lat}ms"));
        }
        if drop > 0 {
            args.push("loss".into());
            args.push(format!("{drop}%"));
        }
        let argv: Vec<&str> = args.iter().map(String::as_str).collect();
        run_tc(&argv)?;
    }
    Ok(())
}

/// nft table names accept letters/digits/underscore only — sanitize.
/// nft table name for a bridge's uplink masquerade rule. Per-bridge
/// so disabling one bridge's uplink can't tear down another's.
fn uplink_table_for(bridge: &str) -> String {
    format!("provium_uplink_{}", sanitize_nft_name(bridge))
}

fn sanitize_nft_name(s: &str) -> String {
    s.chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect()
}

/// Best-effort lookup of the host's primary outgoing interface.
/// Used by NAT uplink. Returns `None` when no default route exists
/// or `ip` isn't on `PATH`.
fn default_upstream_iface() -> Option<String> {
    let out = std::process::Command::new("ip")
        .args(["-4", "route", "show", "default"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let line = String::from_utf8_lossy(&out.stdout);
    // Format: "default via X.Y.Z.W dev <iface> ..."
    let mut tokens = line.split_whitespace();
    while let Some(t) = tokens.next() {
        if t == "dev" {
            return tokens.next().map(String::from);
        }
    }
    None
}

fn run_cmd(program: &str, args: &[&str]) -> std::io::Result<()> {
    let output = std::process::Command::new(program).args(args).output()?;
    if !output.status.success() {
        return Err(std::io::Error::other(format!(
            "{program} {}: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    Ok(())
}

/// Drop a (from, to) entry from the directional impairment map
/// when every axis is back to zero. Without this the map grows
/// unbounded across long test sessions that toggle impairments
/// off-and-on. R9 lab-Mi2.
fn gc_directional_entry(
    map: &mut std::collections::BTreeMap<(String, String), DirectionalImpairment>,
    from: &str,
    to: &str,
) {
    let key = (from.to_owned(), to.to_owned());
    let drop_it = map
        .get(&key)
        .map(|d| d.latency_ms == 0 && d.drop_rate_pct == 0 && d.bandwidth_bps == 0)
        .unwrap_or(false);
    if drop_it {
        map.remove(&key);
    }
}

fn canonical_pair(a: &str, b: &str) -> (String, String) {
    if a <= b {
        (a.to_owned(), b.to_owned())
    } else {
        (b.to_owned(), a.to_owned())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn attach_then_members_lists_in_lex_order() {
        let b = Bridge::new("lan");
        b.attach("zeta");
        b.attach("alpha");
        assert_eq!(b.members(), vec!["alpha", "zeta"]);
    }

    #[test]
    fn partition_is_symmetric_and_canonical() {
        let b = Bridge::new("lan");
        b.partition("a", "b");
        assert!(b.is_partitioned("a", "b"));
        assert!(b.is_partitioned("b", "a"));
        b.unpartition("b", "a");
        assert!(!b.is_partitioned("a", "b"));
    }

    #[test]
    fn partition_all_implies_pair_partitions() {
        let b = Bridge::new("lan");
        b.partition_all();
        assert!(b.is_partitioned("any", "thing"));
        b.restore_all();
        assert!(!b.is_partitioned("any", "thing"));
    }

    #[test]
    fn reset_clears_impairments() {
        let b = Bridge::new("lan");
        b.add_latency(50);
        b.drop_rate(10);
        b.partition("a", "b");
        b.reset();
        assert_eq!(b.latency_ms(), 0);
        assert_eq!(b.drop_rate_pct(), 0);
        assert!(!b.is_partitioned("a", "b"));
    }

    #[test]
    fn directional_impairment_is_separate_from_whole_bridge() {
        let b = Bridge::new("lan");
        b.add_directional_latency("a", "b", 100);
        let d = b.directional("a", "b");
        assert_eq!(d.latency_ms, 100);
        // Reverse direction is unset.
        assert_eq!(b.directional("b", "a").latency_ms, 0);
    }

    #[test]
    fn directional_bandwidth_is_recorded_in_pairs() {
        let b = Bridge::new("lan");
        b.set_directional_bandwidth("a", "b", 1_000_000);
        assert_eq!(b.directional("a", "b").bandwidth_bps, 1_000_000);
        assert_eq!(b.directional("b", "a").bandwidth_bps, 0);
        // Round-trip survives `directional_pairs` (used by
        // LabSnapshotDirectional).
        let pairs = b.directional_pairs();
        assert!(pairs.iter().any(|((f, t), d)| {
            f == "a" && t == "b" && d.bandwidth_bps == 1_000_000
        }));
    }

    #[test]
    fn directional_bandwidth_collapses_with_max() {
        let b = Bridge::new("lan");
        b.set_directional_bandwidth("a", "b", 1_000_000);
        b.set_directional_bandwidth("a", "c", 5_000_000);
        let (_lat, _drop, bps) = b
            .collapsed_directional_for_pub("a")
            .expect("collapsed value");
        assert_eq!(bps, 5_000_000);
    }

    #[test]
    fn directional_bandwidth_zero_clears_the_pair() {
        // Setting to 0 with no other axes set on the pair GCs
        // the entry — directional map shouldn't grow unbounded
        // across a long session that toggles caps on/off.
        let b = Bridge::new("lan");
        b.set_directional_bandwidth("a", "b", 1_000_000);
        b.set_directional_bandwidth("a", "b", 0);
        assert_eq!(b.directional("a", "b").bandwidth_bps, 0);
        assert!(b.collapsed_directional_for_pub("a").is_none());
    }

    #[test]
    fn directional_bandwidth_preserves_latency_on_same_pair() {
        let b = Bridge::new("lan");
        b.add_directional_latency("a", "b", 50);
        b.set_directional_bandwidth("a", "b", 1_000_000);
        let d = b.directional("a", "b");
        assert_eq!(d.latency_ms, 50);
        assert_eq!(d.bandwidth_bps, 1_000_000);
    }
}
