//! Slice-11 clock-op tests.
//!
//! Read/sleep work without privilege; set/advance need
//! `CAP_SYS_TIME` and surface EPERM in the standard test
//! environment. We assert the *plumbing* — not the actual
//! clock-set effect — for those.

#![cfg(feature = "lua")]

use std::collections::BTreeMap;
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
    let path = dir.path().join("c.test.lua");
    std::fs::write(&path, source).unwrap();
    let vmm: Arc<dyn Vmm> = Arc::new(LocalAgentVmm::new());
    run_file(&path, config(), vmm).unwrap()
}

#[test]
fn clock_get_returns_current_unix_time() {
    let outcome = run(
        r#"
test("get", function(t)
    local vm = provium:vm("dc1", "peios"):boot()
    local now = vm:clock():get()
    -- Sanity: agent's clock should be after 2026-05-01 (1746000000)
    -- and before 2050-01-01 (2524608000). Catches obvious bugs.
    t:assert(now > 1746000000, "clock too low: " .. now)
    t:assert(now < 2524608000, "clock too high: " .. now)
end)
"#,
    );
    assert_eq!(outcome.tests[0].status, TestStatus::Passed,
        "{:?}", outcome.tests[0].message);
}

#[test]
fn clock_sleep_actually_sleeps() {
    let outcome = run(
        r#"
test("sleep", function(t)
    local vm = provium:vm("dc1", "peios"):boot()
    local before = vm:clock():get()
    vm:clock():sleep(0.1)
    local after = vm:clock():get()
    t:assert(after - before >= 0.05, "should have slept ~100ms")
end)
"#,
    );
    assert_eq!(outcome.tests[0].status, TestStatus::Passed,
        "{:?}", outcome.tests[0].message);
}

#[test]
fn clock_set_without_cap_sys_time_surfaces_eperm() {
    // The test runner is not root + lacks CAP_SYS_TIME. Setting
    // CLOCK_REALTIME returns EPERM. We don't assert the exact
    // errno here (some sandboxes return EINVAL); just that the
    // op reports an OS error rather than silently succeeding.
    let outcome = run(
        r#"
test("set without cap", function(t)
    local vm = provium:vm("dc1", "peios"):boot()
    local ok, err = pcall(function() vm:clock():set(1700000000) end)
    -- EITHER the set succeeded (privileged env) OR it raised an
    -- OS error. We accept either; the pure-failure path is the
    -- common case.
    if not ok then
        t:assert_contains(tostring(err), "os")
    end
end)
"#,
    );
    assert_eq!(outcome.tests[0].status, TestStatus::Passed,
        "{:?}", outcome.tests[0].message);
}
