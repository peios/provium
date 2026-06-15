//! Slice-11.5 tests: console read + raw syscall.

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
fn console_read_returns_empty_on_local_agent() {
    // LocalAgentVmm doesn't materialise a console log file, so
    // vm:console():read returns empty bytes.
    let outcome = run(
        r#"
test("empty console", function(t)
    local vm = provium:vm("dc1", "peios"):boot()
    -- console:read returns a Stream per DESIGN.md; LocalAgentVmm
    -- doesn't materialise a console socket, so opening the stream
    -- errors and the legacy `read_log` form returns empty bytes.
    local content = vm:console():read_log()
    t:assert_eq(#content, 0)
end)
"#,
    );
    assert_eq!(outcome.tests[0].status, TestStatus::Passed,
        "{:?}", outcome.tests[0].message);
}

#[test]
fn syscall_getpid_returns_a_positive_pid() {
    // SYS_getpid on Linux x86_64 is 39. The agent's own pid is
    // returned (non-negative).
    let outcome = run(
        r#"
test("getpid", function(t)
    local vm = provium:vm("dc1", "peios"):boot()
    local r = vm:syscall(39)
    t:assert(r.ret > 0, "pid should be > 0; got " .. tostring(r.ret))
    t:assert_eq(r.errno, 0)
end)
"#,
    );
    assert_eq!(outcome.tests[0].status, TestStatus::Passed,
        "{:?}", outcome.tests[0].message);
}

#[test]
fn syscall_with_invalid_nr_reports_errno() {
    // A syscall number well past anything Linux defines returns
    // -1 with errno set to ENOSYS (38).
    let outcome = run(
        r#"
test("bad nr", function(t)
    local vm = provium:vm("dc1", "peios"):boot()
    local r = vm:syscall(99999)
    t:assert(r.ret < 0)
    t:assert_eq(r.errno, 38)  -- ENOSYS
end)
"#,
    );
    assert_eq!(outcome.tests[0].status, TestStatus::Passed,
        "{:?}", outcome.tests[0].message);
}
