//! Conformance: the pool's hold-and-wait verdict and the claim that
//! prevents it (PEI-810).
//!
//! The shape that stalled a real run: every file boots one VM at file
//! scope, then a test boots a second one. Enough files at once hold
//! the whole CPU budget between them while every one of them waits,
//! and nothing is ever released. These tests drive that shape through
//! the dispatcher with the local-agent backend and a two-cpu pool.

#![cfg(feature = "lua")]

mod common;

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use provium_host::lua::TestStatus;
use provium_host::profile::{Config, Profile, ProviumSection};
use provium_host::scheduler::dispatch::{
    dispatch_files, DispatchOpts, DispatchedFile, FileTimeout, FileTimeoutOutcome,
};
use provium_host::scheduler::pool::{Pool, ResourceAmount};
use provium_host::vmm::local_agent::LocalAgentVmm;
use provium_host::vmm::Vmm;

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

fn write_test(dir: &std::path::Path, name: &str, body: &str) -> PathBuf {
    let p = dir.join(name);
    std::fs::write(&p, body).unwrap();
    p
}

/// Two cpus, and memory for four of the VMs below: the CPU axis is
/// the one that binds, as it was on the real host.
fn two_cpu_pool() -> Arc<Pool> {
    Pool::new(ResourceAmount {
        memory_bytes: 4 * 1024 * 1024 * 1024,
        cpus: 2,
    })
}

fn vmm() -> Arc<dyn Vmm> {
    Arc::new(LocalAgentVmm::new())
}

fn opts(timeout: FileTimeout) -> DispatchOpts {
    DispatchOpts {
        timeout,
        ..Default::default()
    }
}

/// A file that holds one VM from file scope and boots a second inside
/// its only test, after a pause long enough for its sibling file to
/// have booted its first. `prelude` goes at the top of the file.
fn hold_and_wait_file(prelude: &str) -> String {
    format!(
        r#"{prelude}
local first = provium:vm("first", "peios", {{ memory = "1M", cpus = 1 }})
first:boot()

test("second vm", function(t)
    local s = os.clock()
    while os.clock() - s < 0.3 do end
    local second = provium:vm("second", "peios", {{ memory = "1M", cpus = 1 }})
    second:boot()
    second:shutdown()
end)
"#
    )
}

fn failures(outs: &[DispatchedFile]) -> Vec<String> {
    outs.iter()
        .flat_map(|f| f.outcome.tests.iter())
        .filter(|t| t.status == TestStatus::Failed)
        .map(|t| t.message.clone().unwrap_or_default())
        .collect()
}

/// Two unclaimed files in the PEI-810 shape: the second boot that
/// would close the cycle fails at once, with a message that names the
/// deadlock and the remedy, and the other file goes through. Neither
/// waits for a watchdog.
#[test]
fn an_unclaimed_hold_and_wait_cycle_is_refused_not_stalled() {
    let dir = tempfile::tempdir().unwrap();
    let a = write_test(dir.path(), "a.test.lua", &hold_and_wait_file(""));
    let b = write_test(dir.path(), "b.test.lua", &hold_and_wait_file(""));

    let started = Instant::now();
    let outs = dispatch_files(
        vec![a, b],
        two_cpu_pool(),
        config(),
        vmm(),
        opts(FileTimeout::Wall(Duration::from_secs(60))),
    );
    let elapsed = started.elapsed();

    for f in &outs {
        assert_eq!(f.outcome.chunk_error, None, "{}", f.path.display());
        assert_eq!(f.timeout, FileTimeoutOutcome::InTime, "no watchdog should fire");
    }
    let failed = failures(&outs);
    assert_eq!(failed.len(), 1, "exactly one boot closes the cycle: {failed:?}");
    assert!(
        failed[0].contains("the pool is deadlocked") && failed[0].contains("provium:claim"),
        "the failure must name the deadlock and the remedy: {}",
        failed[0]
    );
    assert!(
        elapsed < Duration::from_secs(10),
        "the verdict is immediate, not a timeout: took {elapsed:?}"
    );
}

/// The same two files with a claim each: the claim is the file's whole
/// VM budget, so the second boot never touches the pool. The files
/// serialise at the claim and both pass.
#[test]
fn a_claim_covers_the_files_boots_so_the_same_shape_passes() {
    let dir = tempfile::tempdir().unwrap();
    let claim = r#"provium:claim({ memory = "512M", cpus = 2 })"#;
    let a = write_test(dir.path(), "a.test.lua", &hold_and_wait_file(claim));
    let b = write_test(dir.path(), "b.test.lua", &hold_and_wait_file(claim));

    let pool = two_cpu_pool();
    let outs = dispatch_files(
        vec![a, b],
        Arc::clone(&pool),
        config(),
        vmm(),
        opts(FileTimeout::Wall(Duration::from_secs(60))),
    );

    for f in &outs {
        assert_eq!(f.outcome.chunk_error, None, "{}", f.path.display());
        assert!(f.passed(), "{}: {:?}", f.path.display(), failures(&outs));
    }
    assert_eq!(pool.available(), pool.total(), "every claim is released at file end");
}

/// A boot the claim cannot cover fails at once, telling the author to
/// raise the claim — it does not fall back to the pool and wait.
#[test]
fn a_boot_past_the_claim_fails_fast_and_says_so() {
    let dir = tempfile::tempdir().unwrap();
    let file = hold_and_wait_file(r#"provium:claim({ memory = "512M", cpus = 1 })"#);
    let a = write_test(dir.path(), "a.test.lua", &file);

    let outs = dispatch_files(
        vec![a],
        two_cpu_pool(),
        config(),
        vmm(),
        opts(FileTimeout::Wall(Duration::from_secs(60))),
    );

    assert_eq!(outs[0].outcome.chunk_error, None);
    let failed = failures(&outs);
    assert_eq!(failed.len(), 1, "{failed:?}");
    assert!(
        failed[0].contains("the file's claim is") && failed[0].contains("raise provium:claim"),
        "{}",
        failed[0]
    );
}

/// Time a file spends queued at its claim is not the file's own work,
/// so it does not count against the file's timeout — the whole
/// timeout is still available once the file gets its turn.
#[test]
fn queueing_at_the_claim_is_not_charged_to_the_file_timeout() {
    let dir = tempfile::tempdir().unwrap();
    // Each file claims the whole pool and then works for 200ms. The
    // second file queues ~200ms behind the first, then works 200ms of
    // its own: 400ms of wall clock against a 300ms timeout, which is
    // only in time if the queueing is excused.
    let body = r#"provium:claim({ memory = "512M", cpus = 2 })
test("work", function(t)
    local s = os.clock()
    while os.clock() - s < 0.2 do end
end)
"#;
    let a = write_test(dir.path(), "a.test.lua", body);
    let b = write_test(dir.path(), "b.test.lua", body);

    let outs = dispatch_files(
        vec![a, b],
        two_cpu_pool(),
        config(),
        vmm(),
        opts(FileTimeout::Wall(Duration::from_millis(300))),
    );

    for f in &outs {
        assert_eq!(f.outcome.chunk_error, None, "{}", f.path.display());
        assert_eq!(
            f.timeout,
            FileTimeoutOutcome::InTime,
            "{} was killed for queueing",
            f.path.display()
        );
        assert!(f.passed(), "{}: {:?}", f.path.display(), failures(&outs));
    }
}
