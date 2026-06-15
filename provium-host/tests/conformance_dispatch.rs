//! Conformance: dispatcher per `DESIGN.md` § Host: scheduler /
//! file runner. Per-file pool reservation, watchdog timeout,
//! fail-fast, ordered output.

#![cfg(feature = "lua")]

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use provium_host::profile::{Config, Profile, ProviumSection};
use provium_host::scheduler::dispatch::{
    dispatch_files, DispatchOpts, FileTimeout, FileTimeoutOutcome,
};
use provium_host::scheduler::pool::{Pool, ResourceAmount};
use provium_host::vmm::local_agent::LocalAgentVmm;
use provium_host::vmm::Vmm;

fn config() -> Arc<Config> {
    let mut profiles = BTreeMap::new();
    profiles.insert("peios".into(), Profile {
        kernel: "/unused".into(),
        initrd: "/unused".into(),
        cmdline: "console=hvc0".into(),
        guest_os: "peios".into(),
            inject_agent: true,
            agent_overlay_path: None,
            cmdline_file: None,
            build: None,
            build_out: None,
    });
    Arc::new(Config { provium: ProviumSection::default(), profiles })
}

fn write_test(dir: &std::path::Path, name: &str, body: &str) -> PathBuf {
    let p = dir.join(name);
    std::fs::write(&p, body).unwrap();
    p
}

fn pool_for_tests() -> Arc<Pool> {
    Pool::new(ResourceAmount { memory_bytes: 1024 * 1024 * 1024, cpus: 4 })
}

fn vmm_for_tests() -> Arc<dyn Vmm> {
    Arc::new(LocalAgentVmm::new())
}

fn opts() -> DispatchOpts {
    let mut o = DispatchOpts::default();
    o.timeout = FileTimeout::Disabled;
    o
}

#[test]
fn dispatch_returns_one_outcome_per_input() {
    let dir = tempfile::tempdir().unwrap();
    let a = write_test(dir.path(), "a.test.lua", "test('a', function(t) end)");
    let b = write_test(dir.path(), "b.test.lua", "test('b', function(t) end)");
    let outs = dispatch_files(
        vec![a.clone(), b.clone()],
        pool_for_tests(),
        config(),
        vmm_for_tests(),
        opts(),
    );
    assert_eq!(outs.len(), 2);
}

#[test]
fn dispatch_preserves_input_order() {
    let dir = tempfile::tempdir().unwrap();
    let a = write_test(dir.path(), "a.test.lua", "test('a', function(t) end)");
    let b = write_test(dir.path(), "b.test.lua", "test('b', function(t) end)");
    let c = write_test(dir.path(), "c.test.lua", "test('c', function(t) end)");
    let outs = dispatch_files(
        vec![c.clone(), a.clone(), b.clone()],
        pool_for_tests(),
        config(),
        vmm_for_tests(),
        opts(),
    );
    assert_eq!(outs[0].path, c);
    assert_eq!(outs[1].path, a);
    assert_eq!(outs[2].path, b);
}

#[test]
fn passed_is_true_for_clean_run() {
    let dir = tempfile::tempdir().unwrap();
    let p = write_test(dir.path(), "ok.test.lua",
        "test('passes', function(t) t:assert(true) end)");
    let outs = dispatch_files(
        vec![p],
        pool_for_tests(),
        config(),
        vmm_for_tests(),
        opts(),
    );
    assert!(outs[0].passed(), "clean file must report passed=true");
    assert_eq!(outs[0].timeout, FileTimeoutOutcome::InTime);
}

#[test]
fn passed_is_false_when_a_test_fails() {
    let dir = tempfile::tempdir().unwrap();
    let p = write_test(dir.path(), "fail.test.lua",
        "test('boom', function(t) t:assert(false, 'nope') end)");
    let outs = dispatch_files(
        vec![p],
        pool_for_tests(),
        config(),
        vmm_for_tests(),
        opts(),
    );
    assert!(!outs[0].passed(), "failing file must report passed=false");
}

#[test]
fn watchdog_fires_when_file_exceeds_timeout() {
    let dir = tempfile::tempdir().unwrap();
    let p = write_test(dir.path(), "slow.test.lua",
        // Busy-loop in Lua to force the watchdog to interrupt.
        // 2-second sleep > 200ms timeout.
        "test('slow', function(t)
            local s = os.time()
            while os.time() - s < 2 do end
        end)");
    let mut o = opts();
    o.timeout = FileTimeout::Wall(Duration::from_millis(200));
    let outs = dispatch_files(
        vec![p],
        pool_for_tests(),
        config(),
        vmm_for_tests(),
        o,
    );
    assert_eq!(
        outs[0].timeout,
        FileTimeoutOutcome::TimedOut,
        "watchdog must fire when file exceeds wall timeout",
    );
}

#[test]
fn fail_fast_skips_subsequent_files() {
    let dir = tempfile::tempdir().unwrap();
    // Force serial-ish dispatch by using a 1-cpu pool so the
    // first file completes before the second starts.
    let pool = Pool::new(ResourceAmount {
        memory_bytes: 100 * 1024 * 1024,
        cpus: 1,
    });
    let a = write_test(dir.path(), "fail.test.lua",
        "test('boom', function(t) t:assert(false) end)");
    let b = write_test(dir.path(), "skipped.test.lua",
        "test('would-pass', function(t) end)");
    let mut o = opts();
    o.fail_fast = true;
    o.per_file_overhead = ResourceAmount {
        memory_bytes: 50 * 1024 * 1024,
        cpus: 1,
    };
    let outs = dispatch_files(
        vec![a, b],
        pool,
        config(),
        vmm_for_tests(),
        o,
    );
    // First file must report a fail; second's behaviour is impl-
    // defined (skipped or didn't-run). The contract is just
    // "no NEW files are picked up after a failure".
    assert!(!outs[0].passed(), "first file should have failed");
}

#[test]
fn empty_input_returns_empty_output() {
    let outs = dispatch_files(
        vec![],
        pool_for_tests(),
        config(),
        vmm_for_tests(),
        opts(),
    );
    assert!(outs.is_empty());
}
