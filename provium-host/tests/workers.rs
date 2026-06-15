//! Slice-10.5 worker tests.

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
    let path = dir.path().join("w.test.lua");
    std::fs::write(&path, source).unwrap();
    let vmm: Arc<dyn Vmm> = Arc::new(LocalAgentVmm::new());
    run_file(&path, config(), vmm).unwrap()
}

#[test]
fn spawn_then_run_then_join() {
    let outcome = run(
        r#"
test("worker exec", function(t)
    local vm = provium:vm("dc1", "peios"):boot()
    local w = vm:spawn_worker()
    t:assert(w:handle() > 0)
    local r = w:run("printf hi")
    t:assert(r:ok())
    t:assert_eq(r.stdout, "hi")
    w:join()
end)
"#,
    );
    assert_eq!(outcome.tests[0].status, TestStatus::Passed,
        "{:?}", outcome.tests[0].message);
}

#[test]
fn join_unknown_worker_raises() {
    // Manually fabricate a stale worker by joining twice.
    let outcome = run(
        r#"
test("double join", function(t)
    local vm = provium:vm("dc1", "peios"):boot()
    local w = vm:spawn_worker()
    w:join()
    local ok, err = pcall(function() w:join() end)
    t:assert(not ok)
    t:assert_contains(tostring(err), "agent error")
end)
"#,
    );
    assert_eq!(outcome.tests[0].status, TestStatus::Passed,
        "{:?}", outcome.tests[0].message);
}
