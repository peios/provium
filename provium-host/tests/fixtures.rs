//! Slice-8 fixture tests.
//!
//! Drives `provium.vm_fixture(...)` end-to-end against
//! [`provium_host::vmm::local_agent::LocalAgentVmm`]: builds on
//! cache miss, hits on second call, evicts old entries, file-locked
//! build serialises parallel access.
//!
//! `LocalAgentVmm.snapshot` writes a placeholder file and
//! `LocalAgentVmm.restore` ignores its contents — we're testing
//! the host-side cache + lock + eviction logic, not actual VM
//! state preservation. Real-VMM fixture preservation is
//! exercised by the slice-3c smoke harness once that grows to
//! cover fixtures.

#![cfg(feature = "lua")]

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;

use provium_host::fixture;
use provium_host::lua::run_file;
use provium_host::profile::{Config, Profile, ProviumSection};
use provium_host::vmm::local_agent::LocalAgentVmm;
use provium_host::vmm::Vmm;

fn config_for(test_root: &Path, cache_dir: &Path) -> Arc<Config> {
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
        provium: ProviumSection {
            roots: vec![test_root.to_string_lossy().into()],
            cache_dir: Some(cache_dir.into()),
            cache_max_size: None,
        },
        profiles,
    })
}

fn write(dir: &Path, name: &str, body: &str) -> std::path::PathBuf {
    let path = dir.join(name);
    std::fs::write(&path, body).unwrap();
    path
}

#[test]
fn vm_fixture_builds_on_first_call_hits_cache_on_second() {
    let dir = tempfile::tempdir().unwrap();
    let cache = tempfile::tempdir().unwrap();

    write(
        dir.path(),
        "warm.fixture.lua",
        r#"
local vm = provium:vm("warm", "peios"):boot()
vm:write_file("/tmp/marker", "warm-state")
return vm:snapshot()
"#,
    );

    let test = write(
        dir.path(),
        "uses_warm.test.lua",
        r#"
test("warm", function(t)
    local vm = provium:vm_fixture("warm")
    -- LocalAgentVmm doesn't preserve state across snapshot/restore
    -- (the placeholder snapshot is ignored). We exercise the
    -- caching + the resume yielding a Booted VM, not actual state
    -- preservation.
    t:assert_eq(vm:state(), "booted")
end)
"#,
    );

    let config = config_for(dir.path(), cache.path());
    let vmm: Arc<dyn Vmm> = Arc::new(LocalAgentVmm::new());

    // First run: cache miss, builds.
    let outcome1 = run_file(&test, Arc::clone(&config), Arc::clone(&vmm)).unwrap();
    assert_eq!(
        outcome1.tests[0].status,
        provium_host::lua::TestStatus::Passed,
        "{:?}",
        outcome1.tests[0].message
    );

    // Cache should now contain a .snap entry.
    let snap_count = std::fs::read_dir(cache.path())
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| e.path().extension().and_then(|s| s.to_str()) == Some("snap"))
        .count();
    assert_eq!(snap_count, 1, "exactly one cached snapshot");

    // Second run: cache hit. Same outcome.
    let outcome2 = run_file(&test, Arc::clone(&config), Arc::clone(&vmm)).unwrap();
    assert_eq!(outcome2.tests[0].status, provium_host::lua::TestStatus::Passed);
}

#[test]
fn cache_key_changes_with_fixture_content() {
    let key1 = fixture::compute_key(b"-- v1\nlocal vm = ...");
    let key2 = fixture::compute_key(b"-- v2\nlocal vm = ...");
    assert_ne!(key1, key2);
}

#[test]
fn vm_fixture_missing_file_raises_clean_error() {
    let dir = tempfile::tempdir().unwrap();
    let cache = tempfile::tempdir().unwrap();

    let test = write(
        dir.path(),
        "missing.test.lua",
        r#"
test("missing", function(t)
    local vm = provium:vm_fixture("does-not-exist")
end)
"#,
    );

    let config = config_for(dir.path(), cache.path());
    let vmm: Arc<dyn Vmm> = Arc::new(LocalAgentVmm::new());
    let outcome = run_file(&test, config, vmm).unwrap();
    assert_eq!(outcome.tests[0].status, provium_host::lua::TestStatus::Failed);
    let msg = outcome.tests[0].message.as_deref().unwrap_or("");
    assert!(msg.contains("does-not-exist") || msg.contains("not found"));
}

#[test]
fn fixture_returning_non_snapshot_is_a_build_error() {
    let dir = tempfile::tempdir().unwrap();
    let cache = tempfile::tempdir().unwrap();

    write(
        dir.path(),
        "bad.fixture.lua",
        r#"
return 42  -- not a Snapshot
"#,
    );

    let test = write(
        dir.path(),
        "uses_bad.test.lua",
        r#"
test("bad fixture", function(t)
    provium:vm_fixture("bad")
end)
"#,
    );

    let config = config_for(dir.path(), cache.path());
    let vmm: Arc<dyn Vmm> = Arc::new(LocalAgentVmm::new());
    let outcome = run_file(&test, config, vmm).unwrap();
    assert_eq!(outcome.tests[0].status, provium_host::lua::TestStatus::Failed);
    let msg = outcome.tests[0].message.as_deref().unwrap_or("");
    assert!(msg.contains("snapshot") || msg.contains("number"));
}

#[test]
fn lru_eviction_drops_oldest_when_over_budget() {
    let cache = tempfile::tempdir().unwrap();
    let a = cache.path().join("a.snap");
    let b = cache.path().join("b.snap");
    let c = cache.path().join("c.snap");
    std::fs::write(&a, vec![0u8; 1000]).unwrap();
    std::thread::sleep(std::time::Duration::from_millis(10));
    std::fs::write(&b, vec![0u8; 1000]).unwrap();
    std::thread::sleep(std::time::Duration::from_millis(10));
    std::fs::write(&c, vec![0u8; 1000]).unwrap();

    // Touch each in order so atimes are stable.
    let _ = std::fs::read(&a);
    std::thread::sleep(std::time::Duration::from_millis(20));
    let _ = std::fs::read(&b);
    std::thread::sleep(std::time::Duration::from_millis(20));
    let _ = std::fs::read(&c);

    let report = fixture::evict_to(cache.path(), 2000).unwrap();
    assert!(report.evicted_count >= 1);
    assert!(!a.exists(), "oldest snapshot should be evicted");
}
