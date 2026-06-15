//! Virtual-machine-monitor abstraction.
//!
//! The [`Vmm`] trait describes the surface every VMM implementation
//! provides for spawning and controlling VMs. Per `DESIGN.md`
//! Architecture §, QEMU is impl 1; the trait stays pluggable so a
//! second backend (cloud-hypervisor if upstream fixes its
//! snapshot/restore bugs, Firecracker, a custom KVM-direct VMM) can
//! land later by implementing [`Vmm`] without touching scheduler /
//! Lua-binding code.
//!
//! ## Slice 1 scope
//!
//! Trait definition + a no-op stub. The QEMU implementation is in
//! [`qemu`] but the methods all return [`VmmError::Unimplemented`] —
//! see slice 2.
//!
//! ## Resource ownership
//!
//! Each [`VmInstance`] owns the VMM-side resources for one VM. Drop
//! the instance to tear the VM down (the [`Drop`] impl makes a
//! best-effort `shutdown`). The Lua bindings surface `vm:shutdown()`
//! / `vm:close()` for explicit ordering.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use thiserror::Error;

use crate::profile::Profile;

pub mod agent_overlay;
pub mod qemu;

/// Boot-time runtime configuration for a VM. Mirrors the Lua API's
/// `vm:boot(boot_opts?)` second argument.
#[derive(Clone, Debug, Default)]
pub struct BootOpts {
    /// Memory cap in bytes. `None` defers to the profile / scheduler
    /// default.
    pub memory_bytes: Option<u64>,
    /// vCPU count. `None` defers to default.
    pub cpus: Option<u32>,
    /// Override the profile's `cmdline`. `None` uses the profile.
    pub cmdline_override: Option<String>,
    /// Files to inject into the VM at boot — currently unused, slot
    /// reserved for the slice-2 file-injection mechanism.
    pub files: Vec<InjectedFile>,
    /// Optional RNG seed for deterministic guest entropy.
    pub rng_seed: Option<u64>,
    /// Optional initial guest wall-clock time in nanoseconds since
    /// the Unix epoch.
    pub initial_time_ns: Option<i64>,
    /// NIC attachments. QemuVmm consumes these to configure
    /// host-side TAPs and `-netdev tap`/`-device virtio-net-pci`
    /// arguments. LocalAgentVmm ignores them.
    pub nic_attachments: Vec<NicAttachment>,
}

/// One NIC attachment.
#[derive(Clone, Debug)]
pub struct NicAttachment {
    /// Host bridge name.
    pub bridge_name: String,
    /// Bridge handle. `None` means "graph-state only, do not realise"
    /// — used by tests that exercise the QEMU command-line shape
    /// without privileged TAP creation.
    pub bridge: Option<crate::bridge::Bridge>,
    /// Stable NIC id used in the QEMU `-netdev id=…` flag.
    pub nic_id: String,
    /// MAC override; `None` lets QEMU pick.
    pub mac: Option<String>,
}

/// One file to inject into the guest at boot.
#[derive(Clone, Debug)]
pub struct InjectedFile {
    /// Path inside the guest filesystem.
    pub guest_path: PathBuf,
    /// Bytes to write.
    pub content: Vec<u8>,
}

/// VMM-layer errors.
#[derive(Debug, Error)]
pub enum VmmError {
    /// Slice 2 hasn't filled this in yet.
    #[error("VMM operation not yet implemented: {0}")]
    Unimplemented(&'static str),

    /// I/O failure (process spawn, socket I/O, …).
    #[error("VMM I/O: {0}")]
    Io(#[from] std::io::Error),

    /// QMP control-plane failure.
    #[error("QMP: {0}")]
    Qmp(#[from] provium_qmp::QmpError),

    /// The kernel or initrd path in the profile does not exist.
    #[error("missing path `{path}` (profile field `{field}`)")]
    MissingProfilePath {
        /// Which field — `kernel` or `initrd`.
        field: &'static str,
        /// The offending path.
        path: PathBuf,
    },

    /// Agent-overlay injection failed — the overlay file was not
    /// findable, or the user's cmdline already pinned a conflicting
    /// `rdinit=` value, or the cache write failed.
    #[error("agent overlay: {0}")]
    AgentOverlay(String),
}

/// One running VM owned by a VMM.
///
/// Type-erased over the backing implementation via a small inner
/// trait ([`Backend`]) so the host's higher layers don't have to be
/// generic over the VMM choice.
pub struct VmInstance {
    /// vsock CID assigned to this VM. The agent listens on this CID +
    /// the profile's agent port.
    cid: u32,
    /// Snapshot of the boot-time resources this VM was assigned;
    /// surfaced to test-event consumers via the scheduler.
    boot: BootSummary,
    /// Backend (typed-erased VMM-specific impl). Held in an
    /// [`Arc<dyn Backend>`] so the instance is `Send + Sync` without
    /// being generic.
    backend: Arc<dyn Backend>,
}

impl std::fmt::Debug for VmInstance {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VmInstance")
            .field("cid", &self.cid)
            .field("boot", &self.boot)
            .finish()
    }
}

/// Read-only summary of how a [`VmInstance`] was booted.
#[derive(Clone, Debug)]
pub struct BootSummary {
    /// Guest-OS port identifier from the profile (e.g. `"peios"`).
    /// Drives agent-port selection on the host. R9 M3 renamed
    /// from `profile` because the field was carrying `guest_os`
    /// while its doc claimed "profile name from provium.toml" —
    /// the user-visible profile name is recorded separately on
    /// the `Vm` and surfaced via the `VmSpawned` event.
    pub guest_os: String,
    /// Memory cap in bytes (post-default-resolution).
    pub memory_bytes: u64,
    /// vCPU count (post-default-resolution).
    pub cpus: u32,
}

impl VmInstance {
    /// Construct from a backend. Used by [`Vmm::boot`] implementations
    /// — the QEMU impl will land in slice 2.
    #[allow(dead_code)] // wired up in slice 2 by QemuVmm::boot
    pub(crate) fn new(cid: u32, boot: BootSummary, backend: Arc<dyn Backend>) -> Self {
        Self { cid, boot, backend }
    }

    /// Reset the guest (warm reboot via QMP `system_reset`).
    pub fn reset(&self) -> Result<(), VmmError> {
        self.backend.reset()
    }

    /// Press the power button (`system_powerdown`).
    pub fn power_button(&self) -> Result<(), VmmError> {
        self.backend.power_button()
    }

    /// Toggle a NIC's link state via the backend.
    pub fn set_link(&self, netdev_id: &str, up: bool) -> Result<(), VmmError> {
        self.backend.set_link(netdev_id, up)
    }

    /// Hot-unplug a disk via the backend.
    pub fn detach_disk(&self, disk_id: &str) -> Result<(), VmmError> {
        self.backend.detach_disk(disk_id)
    }

    /// vsock CID assigned at boot.
    pub fn cid(&self) -> u32 {
        self.cid
    }

    /// Read-only boot summary.
    pub fn boot_summary(&self) -> &BootSummary {
        &self.boot
    }

    /// Pause the guest (`stop` over QMP).
    pub fn pause(&self) -> Result<(), VmmError> {
        self.backend.pause()
    }

    /// Resume the guest (`cont` over QMP).
    pub fn resume(&self) -> Result<(), VmmError> {
        self.backend.resume()
    }

    /// Save state to `path` via `migrate file:<path>`.
    pub fn snapshot(&self, path: &Path) -> Result<(), VmmError> {
        self.backend.snapshot(path)
    }

    /// Tear the VM down (graceful where possible, kill on timeout).
    pub fn shutdown(self) -> Result<(), VmmError> {
        self.backend.shutdown()
    }
}

impl Drop for VmInstance {
    fn drop(&mut self) {
        // Best-effort shutdown so a panicking test doesn't leak a
        // QEMU child. Errors here can't be surfaced — drop().
        let _ = self.backend.shutdown();
    }
}

/// Internal trait implemented by VMM-specific backends. Hidden behind
/// the public [`VmInstance`] surface so the host's higher layers
/// stay non-generic.
pub(crate) trait Backend: Send + Sync {
    fn pause(&self) -> Result<(), VmmError>;
    fn resume(&self) -> Result<(), VmmError>;
    fn snapshot(&self, path: &Path) -> Result<(), VmmError>;
    fn shutdown(&self) -> Result<(), VmmError>;
    /// Default-impl: unsupported. QemuBackend overrides via QMP
    /// `system_reset`; LocalAgentBackend leaves the default.
    fn reset(&self) -> Result<(), VmmError> {
        Err(VmmError::Unimplemented("backend reset"))
    }
    /// Default-impl: unsupported. QemuBackend overrides via QMP
    /// `system_powerdown`.
    fn power_button(&self) -> Result<(), VmmError> {
        Err(VmmError::Unimplemented("backend power_button"))
    }
    /// Toggle a NIC's link state visible to the guest. Default-impl
    /// is a no-op so non-QEMU backends silently ignore.
    fn set_link(&self, _netdev_id: &str, _up: bool) -> Result<(), VmmError> {
        Ok(())
    }

    /// Hot-unplug a previously-attached disk. Default-impl is a
    /// no-op so non-QEMU backends silently ignore.
    fn detach_disk(&self, _disk_id: &str) -> Result<(), VmmError> {
        Ok(())
    }
}

/// Things a VMM-as-a-service can do.
///
/// `launch` is the low-level "spin up the host-side resources for one
/// VM" primitive — the [`crate::vm::Vm`] state machine wraps it to
/// gate transitions (Created → Booted, etc.) and to give callers a
/// stateful handle. `restore` is the parallel for resuming a saved
/// VM state from disk.
pub trait Vmm: Send + Sync {
    /// Spawn a new VM and return the host-side resources for it.
    ///
    /// Returns once the VM is running and the agent has answered
    /// `Hello`, so the contained [`crate::agent_client::AgentClient`]
    /// is immediately usable for ops.
    fn launch(
        &self,
        name: &str,
        profile: &Profile,
        opts: BootOpts,
    ) -> Result<VmRunning, VmmError>;

    /// Resume a VM from a snapshot file produced by an earlier
    /// [`VmInstance::snapshot`]. Backends should override this with
    /// a real implementation if they participate in the fixture
    /// cache.
    ///
    /// Current state:
    /// - [`local_agent::LocalAgentVmm`] — placeholder that re-launches
    ///   (no real VM state to preserve); enough to exercise the
    ///   fixture-cache plumbing on CI without a guest kernel.
    /// - [`qemu::QemuVmm`] — full implementation. Spawns a fresh
    ///   QEMU process with `-incoming "file:<path>"`, polls
    ///   `query-status` until the inbound migration completes,
    ///   then `cont`s the vCPUs. Per `DESIGN.md` line 2082.
    ///
    /// The trait default returns [`VmmError::Unimplemented`] —
    /// reached only by future backends that haven't been wired up
    /// yet.
    fn restore(
        &self,
        _name: &str,
        _profile: &Profile,
        _opts: BootOpts,
        _snapshot_path: &std::path::Path,
    ) -> Result<VmRunning, VmmError> {
        Err(VmmError::Unimplemented("Vmm::restore"))
    }
}

/// Bag of host-side resources for one running VM. Returned by
/// [`Vmm::launch`]; consumed by [`crate::vm::Vm`]'s `boot` transition.
#[derive(Debug)]
pub struct VmRunning {
    /// vsock CID assigned at launch.
    pub cid: u32,
    /// Resource summary for telemetry.
    pub summary: BootSummary,
    /// VMM-side lifecycle handle.
    pub instance: VmInstance,
    /// Wire to the in-VM agent.
    pub client: crate::agent_client::AgentClient,
    /// Path to the host-side log capturing the guest's serial
    /// console — `None` for VMMs that don't materialise one
    /// (e.g. [`local_agent::LocalAgentVmm`]).
    pub console_log: Option<PathBuf>,
    /// Path of the bidirectional console chardev socket. `None`
    /// for VMMs that don't materialise one. Hosts connect to write
    /// keystrokes (`console:write`).
    pub console_socket: Option<PathBuf>,
}

#[cfg(feature = "local-agent")]
pub mod local_agent;
