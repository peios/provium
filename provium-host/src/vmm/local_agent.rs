//! [`LocalAgentVmm`] — a Vmm impl that "boots" by spawning a real
//! `provium-agent` connection handler per-op on a [`std::os::unix::net::UnixStream`]
//! pair, instead of running QEMU.
//!
//! Used by the slice-2 Lua test driver (and any host-only unit testing
//! of scheduler / runner logic): it exercises the full host-↔-agent
//! code path without needing KVM, a guest kernel, or an initrd. Real
//! VMs come from [`super::qemu::QemuVmm`] once slice 3 lands.
//!
//! Feature-gated behind `local-agent` so production builds without
//! the test harness don't pull in `provium-agent` as a dependency.

use std::io;
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::sync::Arc;
use std::thread;

use provium_agent::AgentState;

use crate::agent_client::AgentClient;
use crate::cid::CidAllocator;
use crate::connector::{AgentStream, Connector};
use crate::profile::Profile;
use crate::vmm::{Backend, BootOpts, BootSummary, Vmm, VmInstance, VmRunning, VmmError};

const DEFAULT_LOCAL_MEMORY: u64 = 512 * 1024 * 1024;
const DEFAULT_LOCAL_CPUS: u32 = 1;

/// VMM that spawns an in-process agent thread per connection. See the
/// module-level docs.
#[derive(Debug)]
pub struct LocalAgentVmm {
    cids: Arc<CidAllocator>,
}

impl LocalAgentVmm {
    /// Build with a fresh [`CidAllocator`].
    pub fn new() -> Self {
        Self {
            cids: Arc::new(CidAllocator::new()),
        }
    }

    /// Build sharing an existing CID allocator with other VMM
    /// instances. Production code passes the scheduler's CID
    /// allocator here so all VMs across the run share a monotonic
    /// namespace.
    pub fn with_cid_allocator(cids: Arc<CidAllocator>) -> Self {
        Self { cids }
    }
}

impl Default for LocalAgentVmm {
    fn default() -> Self {
        Self::new()
    }
}

impl Vmm for LocalAgentVmm {
    fn restore(
        &self,
        name: &str,
        profile: &Profile,
        opts: BootOpts,
        _snapshot_path: &std::path::Path,
    ) -> Result<VmRunning, VmmError> {
        // LocalAgentVmm doesn't actually preserve state across
        // snapshot/restore — there's no real VM. Returning a fresh
        // launch is enough to exercise the fixture-cache plumbing
        // (lock contention, key reuse, LRU eviction) end-to-end on
        // CI without a guest kernel.
        self.launch(name, profile, opts)
    }

    fn launch(
        &self,
        _name: &str,
        profile: &Profile,
        opts: BootOpts,
    ) -> Result<VmRunning, VmmError> {
        let cid = self.cids.allocate();
        let memory_bytes = opts.memory_bytes.unwrap_or(DEFAULT_LOCAL_MEMORY);
        let cpus = opts.cpus.unwrap_or(DEFAULT_LOCAL_CPUS);

        // The agent's state table outlives every per-connection
        // thread — the Connector clones it on each `connect()`.
        let agent_state = Arc::new(AgentState::new());

        let connector = LocalAgentConnector {
            state: Arc::clone(&agent_state),
        };
        let client = AgentClient::new(connector);

        let summary = BootSummary {
            guest_os: profile.guest_os.clone(),
            memory_bytes,
            cpus,
        };
        let backend = Arc::new(LocalAgentBackend);
        let instance = VmInstance::new(cid, summary.clone(), backend);

        Ok(VmRunning {
            cid,
            summary,
            instance,
            client,
            console_log: None,
            console_socket: None,
        })
    }
}

#[allow(dead_code)] // Retained as a reference for future
                    // callers that need the profile name plumbed
                    // through; today BootSummary only carries
                    // guest_os (R9 M3).
fn name_or_profile(profile: &Profile) -> String {
    profile.guest_os.clone()
}

/// Backend wired to the `LocalAgentVmm`. All lifecycle ops are
/// no-ops: there is no real VM to pause / snapshot, and the
/// per-connection agent threads die naturally when their connections
/// close.
struct LocalAgentBackend;

impl Backend for LocalAgentBackend {
    fn pause(&self) -> Result<(), VmmError> {
        Err(VmmError::Unimplemented(
            "LocalAgentVmm pause (no real VM; use QemuVmm)",
        ))
    }
    fn resume(&self) -> Result<(), VmmError> {
        Err(VmmError::Unimplemented(
            "LocalAgentVmm resume (no real VM; use QemuVmm)",
        ))
    }
    fn snapshot(&self, path: &Path) -> Result<(), VmmError> {
        // No real VM state to capture, but write a placeholder so
        // the fixture-cache plumbing (key derivation, file moves,
        // LRU eviction) can be exercised end-to-end on CI without
        // a guest kernel. LocalAgentVmm::restore ignores the
        // contents and just spawns a fresh agent.
        std::fs::write(path, b"local-agent-placeholder").map_err(VmmError::Io)?;
        Ok(())
    }
    fn shutdown(&self) -> Result<(), VmmError> {
        // Threads exit when their per-connection handlers return.
        Ok(())
    }
}

/// Connector for [`LocalAgentVmm`]. Each `connect()` creates a fresh
/// `UnixStream::pair()` and spawns a `provium-agent` connection handler
/// on the agent half.
struct LocalAgentConnector {
    state: Arc<AgentState>,
}

impl Connector for LocalAgentConnector {
    fn connect(&self) -> io::Result<Box<dyn AgentStream>> {
        let (host_side, agent_side) = UnixStream::pair()?;
        let state = Arc::clone(&self.state);
        let mut reader = agent_side.try_clone()?;
        let mut writer = agent_side;
        thread::Builder::new()
            .name("local-agent".into())
            .spawn(move || {
                let _ = provium_agent::connection::handle_connection(
                    &mut reader,
                    &mut writer,
                    state,
                );
            })?;
        Ok(Box::new(host_side))
    }
}
