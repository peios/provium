//! Slice-10 process tests: vm:run_async + proc:wait + proc:kill.

#![cfg(feature = "lua")]

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;

use provium_host::lua::{run_file, TestStatus};
use provium_host::profile::{Config, Profile, ProviumSection};
use provium_host::vmm::local_agent::LocalAgentVmm;
use provium_host::vmm::Vmm;

fn config() -> Arc<Config> {
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

fn run(source: &str) -> provium_host::lua::FileOutcome {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("p.test.lua");
    std::fs::write(&path, source).unwrap();
    let vmm: Arc<dyn Vmm> = Arc::new(LocalAgentVmm::new());
    run_file(&path, config(), vmm).unwrap()
}

#[test]
fn run_async_then_wait_returns_run_result() {
    let outcome = run(
        r#"
test("async echo", function(t)
    local vm = provium:vm("dc1", "peios"):boot()
    local proc = vm:run_async("printf", {"hello"})
    local r = proc:wait()
    t:assert(r:ok())
    t:assert_eq(r.stdout, "hello")
end)
"#,
    );
    assert_eq!(outcome.tests[0].status, TestStatus::Passed,
        "{:?}", outcome.tests[0].message);
}

#[test]
fn run_async_then_kill_then_wait_reports_signal() {
    let outcome = run(
        r#"
test("kill async", function(t)
    local vm = provium:vm("dc1", "peios"):boot()
    local proc = vm:run_async("sh", {"-c", "sleep 30"})
    proc:kill("kill")
    local r = proc:wait()
    -- After SIGKILL the agent's wait reports a signal-termination,
    -- not a clean exit. status field is "signalled".
    t:assert_eq(r.status, "signalled")
end)
"#,
    );
    assert_eq!(outcome.tests[0].status, TestStatus::Passed,
        "{:?}", outcome.tests[0].message);
}

#[test]
fn wait_with_timeout_kills_runaway_child() {
    let outcome = run(
        r#"
test("timeout kills", function(t)
    local vm = provium:vm("dc1", "peios"):boot()
    local proc = vm:run_async("sh", {"-c", "sleep 30"})
    -- Per DESIGN.md § Time and timeouts: numbers are seconds,
    -- string suffixes for sub-second.
    local r = proc:wait("150ms")
    t:assert(r.timed_out, "expected timed_out true")
end)
"#,
    );
    assert_eq!(outcome.tests[0].status, TestStatus::Passed,
        "{:?}", outcome.tests[0].message);
}

#[test]
fn run_async_invalid_binary_raises_os_error() {
    let outcome = run(
        r#"
test("missing binary", function(t)
    local vm = provium:vm("dc1", "peios"):boot()
    -- Pass an args table so we hit direct exec (not shell form).
    -- Direct exec of a missing binary returns ENOENT at spawn time.
    local ok, err = pcall(function() vm:run_async("/nonexistent/binary", {}) end)
    t:assert(not ok)
    t:assert_contains(tostring(err), "No such file")
end)
"#,
    );
    assert_eq!(outcome.tests[0].status, TestStatus::Passed,
        "{:?}", outcome.tests[0].message);
}

// keep imports referenced
#[allow(dead_code)]
fn _unused(_p: &Path) {}
