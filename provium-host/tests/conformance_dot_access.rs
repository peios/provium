//! Conformance: dot-access on labs per `DESIGN.md` § Lab.
//! `lab.<name>` resolves to the named VM, bridge, or sub-lab.
//! Reserved keys (vm_fixture/lab_fixture/pack/unpack) are
//! refused at create time so they can never silently shadow
//! the helpers.

#![cfg(feature = "lua")]

mod common;

use common::{assert_one_passed, run_local_lua};

#[test]
fn dot_access_resolves_vm() {
    let outcome = run_local_lua(
        r#"
test("provium.web", function(t)
    provium:vm("web", "peios"):boot()
    local v = provium.web
    t:assert(v ~= nil, "dot access must resolve")
    t:assert_eq(v:state(), "booted")
end)
"#,
    );
    assert_one_passed(&outcome);
}

#[test]
fn dot_access_resolves_bridge() {
    let outcome = run_local_lua(
        r#"
test("provium.lan", function(t)
    provium:bridge("lan", {})
    local b = provium.lan
    t:assert(b ~= nil)
    t:assert_eq(b:name(), "lan")
end)
"#,
    );
    assert_one_passed(&outcome);
}

#[test]
fn dot_access_resolves_sub_lab() {
    let outcome = run_local_lua(
        r#"
test("provium.dc1", function(t)
    provium:lab("dc1")
    local sub = provium.dc1
    t:assert(sub ~= nil)
end)
"#,
    );
    assert_one_passed(&outcome);
}

#[test]
fn dot_access_returns_method_for_documented_helpers() {
    // The documented helpers must remain callable via dot
    // access — that's the whole point of reserving the names.
    let outcome = run_local_lua(
        r#"
test("helpers callable", function(t)
    -- vm_fixture / lab_fixture are methods, not VMs.
    t:assert(type(provium.vm_fixture) == "function")
    t:assert(type(provium.lab_fixture) == "function")
end)
"#,
    );
    assert_one_passed(&outcome);
}
