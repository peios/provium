//! Slice-9 bridge tests — exercise the API + graph state. Real
//! TAP/tc integration is slice 9.5.

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
    let path = dir.path().join("b.test.lua");
    std::fs::write(&path, source).unwrap();
    let vmm: Arc<dyn Vmm> = Arc::new(LocalAgentVmm::new());
    run_file(&path, config(), vmm).unwrap()
}

#[test]
fn create_then_lookup_returns_same_bridge() {
    let outcome = run(
        r#"
test("bridge crud", function(t)
    local lan = provium:bridge("lan", {})
    local same = provium:bridge("lan")
    t:assert_eq(lan:name(), same:name())
end)
"#,
    );
    assert_eq!(outcome.tests[0].status, TestStatus::Passed,
        "{:?}", outcome.tests[0].message);
}

#[test]
fn attach_then_members_lists_all_attached_vms() {
    let outcome = run(
        r#"
test("attach", function(t)
    local lan = provium:bridge("lan", {})
    local v1 = provium:vm("dc1", "peios")
    local v2 = provium:vm("dc2", "peios")
    lan:attach(v1)
    lan:attach(v2)
    local members = lan:members()
    t:assert_eq(#members, 2)
    -- alphabetical
    t:assert_eq(members[1], "dc1")
    t:assert_eq(members[2], "dc2")
end)
"#,
    );
    assert_eq!(outcome.tests[0].status, TestStatus::Passed,
        "{:?}", outcome.tests[0].message);
}

#[test]
fn partition_is_symmetric_and_unparts_clean() {
    let outcome = run(
        r#"
test("partition", function(t)
    local lan = provium:bridge("lan", {})
    lan:partition("a", "b")
    t:assert(lan:is_partitioned("a", "b"))
    t:assert(lan:is_partitioned("b", "a"))
    lan:unpartition("a", "b")
    t:assert(not lan:is_partitioned("a", "b"))
end)
"#,
    );
    assert_eq!(outcome.tests[0].status, TestStatus::Passed);
}

#[test]
fn partition_all_then_restore() {
    let outcome = run(
        r#"
test("partition all", function(t)
    local lan = provium:bridge("lan", {})
    lan:partition_all()
    t:assert(lan:is_partitioned("anything", "anywhere"))
    lan:restore_all()
    t:assert(not lan:is_partitioned("anything", "anywhere"))
end)
"#,
    );
    assert_eq!(outcome.tests[0].status, TestStatus::Passed);
}

#[test]
fn impairments_round_trip_through_lua() {
    let outcome = run(
        r#"
test("impairments", function(t)
    local lan = provium:bridge("lan", {})
    lan:add_latency(50)
    lan:drop_rate(10)
    t:assert_eq(lan:latency_ms(), 50)
    t:assert_eq(lan:drop_rate_pct(), 10)
    lan:reset()
    t:assert_eq(lan:latency_ms(), 0)
    t:assert_eq(lan:drop_rate_pct(), 0)
end)
"#,
    );
    assert_eq!(outcome.tests[0].status, TestStatus::Passed);
}

#[test]
fn duplicate_bridge_name_errors() {
    let outcome = run(
        r#"
test("dup", function(t)
    provium:bridge("lan", {})
    local ok, err = pcall(function() provium:bridge("lan", {}) end)
    t:assert(not ok)
    t:assert_contains(tostring(err), "already used")
end)
"#,
    );
    assert_eq!(outcome.tests[0].status, TestStatus::Passed);
}
