//! A boot that declares no `memory` or `cpus` is charged to the pool
//! at the size it will actually launch with (PEI-1111).
//!
//! Before this, the reservation used zero for anything undeclared
//! while the backend launched the VM with one vCPU and 512 MiB, so an
//! unclaimed testset of undeclared boots ran past the CPU budget by as
//! many VMs as it had files. The two cases below each give the pool
//! less than one default-sized VM on one axis and check that the boot
//! is refused as exceeding the pool — which it can only be if the
//! defaults were charged.
#![cfg(feature = "lua")]

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

use provium_host::profile::{Config, Profile, ProviumSection};
use provium_host::scheduler::{dispatch_files, DispatchOpts, FileTimeout, Pool, ResourceAmount};
use provium_host::vmm::local_agent::LocalAgentVmm;
use provium_host::vmm::{Vmm, DEFAULT_CPUS, DEFAULT_MEMORY_BYTES};

fn config() -> Arc<Config> {
    let mut profiles = BTreeMap::new();
    profiles.insert(
        "peios".into(),
        Profile {
            kernel: "/unused".into(),
            initrd: "/unused".into(),
            root: None,
            cmdline: "console=hvc0".into(),
            guest_os: "peios".into(),
            inject_agent: true,
            agent_overlay_path: None,
            agent_boot_timeout: None,
            disks: Vec::new(),
            cmdline_file: None,
            build: None,
            build_out: None,
            dir: None,
        },
    );
    Arc::new(Config {
        provium: ProviumSection::default(),
        profiles,
    })
}

fn undeclared_boot(dir: &std::path::Path) -> PathBuf {
    let path = dir.join("undeclared.test.lua");
    std::fs::write(
        &path,
        r#"
test("boots with nothing declared", function(t)
    local vm = provium:vm("v", "peios"):boot()
    t:assert(vm ~= nil)
end)
"#,
    )
    .unwrap();
    path
}

fn opts() -> DispatchOpts {
    DispatchOpts {
        per_file_overhead: ResourceAmount {
            memory_bytes: 0,
            cpus: 0,
        },
        timeout: FileTimeout::Disabled,
        ..Default::default()
    }
}

fn run_against(pool: Arc<Pool>) -> String {
    let dir = tempfile::tempdir().unwrap();
    let path = undeclared_boot(dir.path());
    let vmm: Arc<dyn Vmm> = Arc::new(LocalAgentVmm::new());
    let results = dispatch_files(vec![path], pool, config(), vmm, opts());
    assert_eq!(results.len(), 1);
    let r = &results[0];
    assert!(!r.passed(), "an undeclared boot must not fit a pool smaller than the defaults");
    r.outcome
        .chunk_error
        .clone()
        .or_else(|| r.outcome.tests.iter().find_map(|t| t.message.clone()))
        .unwrap_or_default()
}

#[test]
fn undeclared_boot_is_charged_a_default_vcpu() {
    // Memory for many default VMs, but not a single vCPU: the boot is
    // refused, which proves one vCPU was charged.
    let err = run_against(Pool::new(ResourceAmount {
        memory_bytes: 8 * DEFAULT_MEMORY_BYTES,
        cpus: DEFAULT_CPUS - 1,
    }));
    assert!(err.contains("exceeds pool total"), "got: {err}");
}

#[test]
fn undeclared_boot_is_charged_the_default_memory_plus_overhead() {
    // vCPUs to spare, but one byte less than the default memory: the
    // reservation is the default plus VMM overhead, so it cannot fit.
    let err = run_against(Pool::new(ResourceAmount {
        memory_bytes: DEFAULT_MEMORY_BYTES - 1,
        cpus: 4,
    }));
    assert!(err.contains("exceeds pool total"), "got: {err}");
}

#[test]
fn undeclared_boot_fits_a_pool_sized_for_it() {
    // The control: the default memory plus overhead and one vCPU is
    // enough, so the same file passes.
    let dir = tempfile::tempdir().unwrap();
    let path = undeclared_boot(dir.path());
    let pool = Pool::new(ResourceAmount {
        memory_bytes: DEFAULT_MEMORY_BYTES + 100 * 1024 * 1024,
        cpus: DEFAULT_CPUS,
    });
    let vmm: Arc<dyn Vmm> = Arc::new(LocalAgentVmm::new());
    let results = dispatch_files(vec![path], pool, config(), vmm, opts());
    assert!(results[0].passed(), "{:?}", results[0].outcome.chunk_error);
}
