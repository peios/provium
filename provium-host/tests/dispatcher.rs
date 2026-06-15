//! Multi-file dispatcher tests.
//!
//! Drives [`provium_host::scheduler::dispatch_files`] over a small
//! corpus of synthetic `.test.lua` files written into a tempdir.
//! Pool sizing is configured to force serialisation in a couple of
//! tests so we exercise the blocking-acquire path.

#![cfg(feature = "lua")]

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use provium_host::lab::Lab;
use provium_host::profile::{Config, Profile, ProviumSection};
use provium_host::scheduler::{
    dispatch_files, DispatchOpts, FileTimeout, FileTimeoutOutcome, Pool, ResourceAmount,
};
use provium_host::vmm::local_agent::LocalAgentVmm;
use provium_host::vmm::Vmm;

fn slice2_config() -> Arc<Config> {
    let mut profiles = BTreeMap::new();
    profiles.insert(
        "peios".into(),
        Profile {
            kernel: "/unused".into(),
            initrd: "/unused".into(),
            cmdline: "console=hvc0".into(),
            guest_os: "peios".into(),
            inject_agent: true,
            agent_overlay_path: None,
            cmdline_file: None,
            build: None,
            build_out: None,
        },
    );
    Arc::new(Config {
        provium: ProviumSection::default(),
        profiles,
    })
}

fn write_file(dir: &std::path::Path, name: &str, source: &str) -> PathBuf {
    let path = dir.join(name);
    std::fs::write(&path, source).unwrap();
    path
}

fn small_pool() -> Arc<Pool> {
    Pool::new(ResourceAmount {
        memory_bytes: 4 * 1024 * 1024 * 1024,
        cpus: 8,
    })
}

fn opts() -> DispatchOpts {
    DispatchOpts {
        per_file_overhead: ResourceAmount {
            memory_bytes: 50 * 1024 * 1024,
            cpus: 0,
        },
        timeout: FileTimeout::Wall(Duration::from_secs(30)),
        events: Arc::new(provium_host::scheduler::NullSink),
        pressure: None,
        fail_fast: false,
    }
}

fn dispatch(paths: Vec<PathBuf>) -> Vec<provium_host::scheduler::dispatch::DispatchedFile> {
    let pool = small_pool();
    let config = slice2_config();
    let vmm: Arc<dyn Vmm> = Arc::new(LocalAgentVmm::new());
    dispatch_files(paths, pool, config, vmm, opts())
}

// ---------------------------------------------------------------------------
// Basic multi-file
// ---------------------------------------------------------------------------

#[test]
fn dispatches_multiple_files_concurrently() {
    let dir = tempfile::tempdir().unwrap();
    let a = write_file(
        dir.path(),
        "a.test.lua",
        r#"
test("alpha", function(t)
    local vm = provium:vm("a", "peios"):boot()
    local r = vm:run("printf alpha")
    t:assert_eq(r.stdout, "alpha")
end)
"#,
    );
    let b = write_file(
        dir.path(),
        "b.test.lua",
        r#"
test("beta", function(t)
    local vm = provium:vm("b", "peios"):boot()
    local r = vm:run("printf beta")
    t:assert_eq(r.stdout, "beta")
end)
"#,
    );

    let results = dispatch(vec![a.clone(), b.clone()]);
    assert_eq!(results.len(), 2);
    assert!(results[0].passed());
    assert!(results[1].passed());
}

#[test]
fn dispatch_preserves_input_order() {
    let dir = tempfile::tempdir().unwrap();
    let mut paths = Vec::new();
    for i in 0..3 {
        paths.push(write_file(
            dir.path(),
            &format!("{i}.test.lua"),
            &format!(
                r#"test("tn", function(t) t:log("file{i}"); t:assert(true) end)"#
            ),
        ));
    }
    let results = dispatch(paths.clone());
    for (idx, r) in results.iter().enumerate() {
        assert_eq!(r.path, paths[idx]);
    }
}

#[test]
fn failing_test_in_one_file_does_not_taint_others() {
    let dir = tempfile::tempdir().unwrap();
    let pass = write_file(
        dir.path(),
        "pass.test.lua",
        r#"test("p", function(t) t:assert(true) end)"#,
    );
    let fail = write_file(
        dir.path(),
        "fail.test.lua",
        r#"test("f", function(t) t:fail("nope") end)"#,
    );
    let results = dispatch(vec![pass, fail]);
    assert!(results[0].passed());
    assert!(!results[1].passed());
    // The failed file's failure shouldn't be a chunk_error — it's a
    // per-test failure.
    assert!(results[1].outcome.chunk_error.is_none());
    assert_eq!(results[1].outcome.tests.len(), 1);
}

// ---------------------------------------------------------------------------
// Resource pool blocking
// ---------------------------------------------------------------------------

#[test]
fn pool_blocks_when_overhead_exceeds_remaining_budget() {
    let dir = tempfile::tempdir().unwrap();
    // Tiny pool: only fits one file's overhead at a time.
    let pool = Pool::new(ResourceAmount {
        memory_bytes: 60 * 1024 * 1024,
        cpus: 0,
    });
    let opts = DispatchOpts {
        per_file_overhead: ResourceAmount {
            memory_bytes: 50 * 1024 * 1024,
            cpus: 0,
        },
        timeout: FileTimeout::Wall(Duration::from_secs(30)),
        events: Arc::new(provium_host::scheduler::NullSink),
        pressure: None,
        fail_fast: false,
    };

    let mut paths = Vec::new();
    for i in 0..3 {
        paths.push(write_file(
            dir.path(),
            &format!("{i}.test.lua"),
            r#"test("t", function(t) t:assert(true) end)"#,
        ));
    }

    let started = std::time::Instant::now();
    let config = slice2_config();
    let vmm: Arc<dyn Vmm> = Arc::new(LocalAgentVmm::new());
    let results = dispatch_files(paths, pool, config, vmm, opts);
    let elapsed = started.elapsed();

    assert_eq!(results.len(), 3);
    for r in &results {
        assert!(r.passed(), "file should pass: {:?}", r.outcome.chunk_error);
    }
    // No specific timing assertion — just sanity that 3 files
    // serialised with one pool slot still completes promptly.
    assert!(elapsed < Duration::from_secs(10));
}

#[test]
fn per_file_overhead_exceeding_total_pool_is_caught_at_dispatch() {
    let dir = tempfile::tempdir().unwrap();
    let path = write_file(
        dir.path(),
        "x.test.lua",
        r#"test("t", function(t) t:assert(true) end)"#,
    );

    let pool = Pool::new(ResourceAmount {
        memory_bytes: 1024,
        cpus: 1,
    });
    let opts = DispatchOpts {
        per_file_overhead: ResourceAmount {
            memory_bytes: 1024 * 1024 * 1024, // 1 GiB
            cpus: 0,
        },
        timeout: FileTimeout::Disabled,
        events: Arc::new(provium_host::scheduler::NullSink),
        pressure: None,
        fail_fast: false,
    };
    let config = slice2_config();
    let vmm: Arc<dyn Vmm> = Arc::new(LocalAgentVmm::new());
    let results = dispatch_files(vec![path], pool, config, vmm, opts);

    let chunk_error = results[0]
        .outcome
        .chunk_error
        .as_deref()
        .unwrap_or("");
    assert!(chunk_error.contains("exceeds"));
}

// ---------------------------------------------------------------------------
// Catch-unwind plumbing
// ---------------------------------------------------------------------------
//
// The dispatcher wraps `run_file_with_lab` in `catch_unwind` so a
// Rust panic in mlua's userdata code can never bring the whole run
// down. We don't have a stable way to *trigger* such a panic from
// Lua (mlua converts most things to errors), so we don't have a
// dedicated test here — the catch_unwind structure is exercised on
// every dispatcher invocation.

// ---------------------------------------------------------------------------
// Timeout
// ---------------------------------------------------------------------------

#[test]
fn per_file_timeout_fires_and_marks_dispatched_file_timed_out() {
    // The watchdog's lab.shutdown() is a no-op for LocalAgentVmm
    // (no real VM to terminate; the agent's spawned children are
    // out of its reach in v1). What we *can* test under the test
    // VMM is the watchdog's bookkeeping: it fires, marks the
    // FileTimeoutOutcome, and the dispatcher tags the result
    // accordingly. Real-VMM timeout enforcement is exercised by
    // the slice-3c real-QEMU smoke harness where shutdown actually
    // tears the QEMU child down and the in-flight op fails.
    let dir = tempfile::tempdir().unwrap();
    let slow = write_file(
        dir.path(),
        "slow.test.lua",
        r#"
test("slow op", function(t)
    local vm = provium:vm("dc1", "peios"):boot()
    vm:run("sleep 3")
end)
"#,
    );

    let pool = small_pool();
    let opts = DispatchOpts {
        per_file_overhead: ResourceAmount {
            memory_bytes: 50 * 1024 * 1024,
            cpus: 0,
        },
        timeout: FileTimeout::Wall(Duration::from_millis(200)),
        events: Arc::new(provium_host::scheduler::NullSink),
        pressure: None,
        fail_fast: false,
    };
    let config = slice2_config();
    let vmm: Arc<dyn Vmm> = Arc::new(LocalAgentVmm::new());
    let started = std::time::Instant::now();
    let results = dispatch_files(vec![slow], pool, config, vmm, opts);
    let elapsed = started.elapsed();

    assert_eq!(results.len(), 1);
    assert_eq!(results[0].timeout, FileTimeoutOutcome::TimedOut);
    // Dispatch waits for the runner thread, which is blocked in
    // `sleep 3`. Generous bound so we don't flake on slow CI.
    assert!(
        elapsed < Duration::from_secs(10),
        "dispatch elapsed: {elapsed:?}"
    );
}

// ---------------------------------------------------------------------------
// Disabled timeout
// ---------------------------------------------------------------------------

#[test]
fn disabled_timeout_lets_short_files_complete() {
    let dir = tempfile::tempdir().unwrap();
    let path = write_file(
        dir.path(),
        "quick.test.lua",
        r#"test("q", function(t) t:assert(true) end)"#,
    );

    let pool = small_pool();
    let opts = DispatchOpts {
        per_file_overhead: ResourceAmount {
            memory_bytes: 50 * 1024 * 1024,
            cpus: 0,
        },
        timeout: FileTimeout::Disabled,
        events: Arc::new(provium_host::scheduler::NullSink),
        pressure: None,
        fail_fast: false,
    };
    let config = slice2_config();
    let vmm: Arc<dyn Vmm> = Arc::new(LocalAgentVmm::new());
    let results = dispatch_files(vec![path], pool, config, vmm, opts);
    assert_eq!(results[0].timeout, FileTimeoutOutcome::InTime);
    assert!(results[0].passed());
}

// Suppress unused-Lab warning — kept in scope for slice 5.5 when
// we'll wire claim()-style file-scope reservations through the lab.
#[allow(dead_code)]
fn _lab_use(l: Lab) -> Lab {
    l
}
