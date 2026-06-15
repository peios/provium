//! Conformance: vm:reset / vm:power_button per `DESIGN.md` § VM
//! state machine. Both ops are state-checked Booted-only; calls
//! from other states should error cleanly.

#![cfg(feature = "lua")]

mod common;

use common::{assert_one_passed, run_local_lua};

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
#[ignore = "LocalAgentVmm doesn't implement reset; needs QemuVmm"]
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
#[ignore = "LocalAgentVmm doesn't implement power_button; needs QemuVmm"]
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
