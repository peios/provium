//! Conformance: vm:reset / vm:power_button per `DESIGN.md` § VM
//! state machine. Both ops are state-checked Booted-only; calls
//! from other states should error cleanly.

#![cfg(feature = "lua")]

mod common;

use common::{assert_one_passed, run_local_lua};

#[test]
fn suspend_query_requires_booted_state() {
    let outcome = run_local_lua(
        r#"
test("suspend query checks lifecycle state", function(t)
    local vm = provium:vm("suspend-query", "peios")
    local function rejected(state)
        local ok, err = pcall(vm.is_suspended, vm)
        t:assert(not ok)
        t:assert(tostring(err):find(state), "state error: " .. tostring(err))
    end
    rejected("not booted")
    vm:boot()
    vm:shutdown()
    rejected("shutdown")
end)
"#,
    );
    assert_one_passed(&outcome);
}

#[test]
fn suspend_query_does_not_invent_state_for_local_agent() {
    let outcome = run_local_lua(
        r#"
test("a backend without ACPI reports unsupported", function(t)
    local vm = provium:vm("suspend-query", "peios"):boot()
    local ok, err = pcall(vm.is_suspended, vm)
    t:assert(not ok)
    t:assert(tostring(err):find("backend is_suspended"), tostring(err))
    t:assert_eq(vm:state(), "booted", "unsupported is not a dead VM")
end)
"#,
    );
    assert_one_passed(&outcome);
}

#[test]
fn reset_from_created_state_errors() {
    let outcome = run_local_lua(
        r#"
test("reset before boot", function(t)
    local vm = provium:vm("a", "peios")
    local ok, err = pcall(function() vm:reset() end)
    t:assert(not ok, "reset on Created VM must error")
    t:assert(tostring(err):find("not booted") or tostring(err):find("created"),
        "error must mention state: " .. tostring(err))
end)
"#,
    );
    assert_one_passed(&outcome);
}

#[test]
fn power_button_from_created_state_errors() {
    let outcome = run_local_lua(
        r#"
test("power_button before boot", function(t)
    local vm = provium:vm("a", "peios")
    local ok, err = pcall(function() vm:power_button() end)
    t:assert(not ok)
end)
"#,
    );
    assert_one_passed(&outcome);
}

#[test]
fn reset_from_shutdown_errors() {
    let outcome = run_local_lua(
        r#"
test("reset after shutdown", function(t)
    local vm = provium:vm("a", "peios"):boot()
    vm:shutdown()
    local ok = pcall(function() vm:reset() end)
    t:assert(not ok, "reset on Shutdown VM must error")
end)
"#,
    );
    assert_one_passed(&outcome);
}

#[test]
// The assertion below is deliberately weak — it is what a VM with no
// machine can say. Real coverage, against a booted guest, is in
// `tests/kvm_lifecycle.rs`; this `vm:state()` check stayed true
// throughout all three of PEI-1112's reset faults (PEI-1119).
#[ignore = "LocalAgentVmm has no machine to reset; real coverage is tests/kvm_lifecycle.rs"]
fn reset_from_booted_succeeds() {
    let outcome = run_local_lua(
        r#"
test("reset booted", function(t)
    local vm = provium:vm("a", "peios"):boot()
    vm:reset()
    -- Post-reset the VM should still be Booted (reset is a
    -- warm reboot, not a state transition).
    t:assert_eq(vm:state(), "booted")
end)
"#,
    );
    assert_one_passed(&outcome);
}

#[test]
// As above: real coverage is in `tests/kvm_lifecycle.rs`.
#[ignore = "LocalAgentVmm has no machine to power off; real coverage is tests/kvm_lifecycle.rs"]
fn power_button_from_booted_succeeds() {
    let outcome = run_local_lua(
        r#"
test("power_button booted", function(t)
    local vm = provium:vm("a", "peios"):boot()
    vm:power_button()
    -- power_button transitions to Shutdown (graceful).
    t:assert_eq(vm:state(), "shutdown")
end)
"#,
    );
    assert_one_passed(&outcome);
}
