//! Conformance: VM state machine per `DESIGN.md` § VM state
//! machine. The full transition table is below — one test per
//! row so a future refactor can't quietly drop a transition.

#![cfg(feature = "lua")]

mod common;

use common::{assert_one_passed, run_local_lua};

#[test]
fn created_boot_transitions_to_booted() {
    let outcome = run_local_lua(
        r#"
test("Created → Booted", function(t)
    local vm = provium:vm("a", "peios")
    t:assert_eq(vm:state(), "created")
    vm:boot()
    t:assert_eq(vm:state(), "booted")
end)
"#,
    );
    assert_one_passed(&outcome);
}

#[test]
fn created_run_errors_with_not_booted() {
    let outcome = run_local_lua(
        r#"
test("Created run errors", function(t)
    local vm = provium:vm("a", "peios")
    local ok, err = pcall(function() vm:run("true") end)
    t:assert(not ok)
    t:assert(tostring(err):find("not booted"),
        "error must say 'not booted': " .. tostring(err))
end)
"#,
    );
    assert_one_passed(&outcome);
}

#[test]
fn booted_boot_errors_with_already_booted() {
    let outcome = run_local_lua(
        r#"
test("double boot errors", function(t)
    local vm = provium:vm("a", "peios"):boot()
    local ok, err = pcall(function() vm:boot() end)
    t:assert(not ok)
    t:assert(tostring(err):find("already booted"),
        "error must say 'already booted': " .. tostring(err))
end)
"#,
    );
    assert_one_passed(&outcome);
}

#[test]
#[ignore = "LocalAgentVmm doesn't implement pause; needs QemuVmm"]
fn booted_pause_transitions_to_paused() {
    let outcome = run_local_lua(
        r#"
test("Booted → Paused", function(t)
    local vm = provium:vm("a", "peios"):boot()
    vm:pause()
    t:assert_eq(vm:state(), "paused")
end)
"#,
    );
    assert_one_passed(&outcome);
}

#[test]
#[ignore = "LocalAgentVmm doesn't implement pause; needs QemuVmm"]
fn paused_resume_returns_to_booted() {
    let outcome = run_local_lua(
        r#"
test("Paused → Booted", function(t)
    local vm = provium:vm("a", "peios"):boot()
    vm:pause()
    vm:resume()
    t:assert_eq(vm:state(), "booted")
end)
"#,
    );
    assert_one_passed(&outcome);
}

#[test]
#[ignore = "LocalAgentVmm doesn't implement pause; needs QemuVmm"]
fn paused_run_errors_with_use_resume() {
    let outcome = run_local_lua(
        r#"
test("Paused run errors", function(t)
    local vm = provium:vm("a", "peios"):boot()
    vm:pause()
    local ok, err = pcall(function() vm:run("true") end)
    t:assert(not ok)
    t:assert(tostring(err):find("paused") or tostring(err):find("resume"),
        "error must mention paused/resume: " .. tostring(err))
end)
"#,
    );
    assert_one_passed(&outcome);
}

#[test]
#[ignore = "LocalAgentVmm doesn't implement pause; needs QemuVmm"]
fn paused_boot_errors() {
    let outcome = run_local_lua(
        r#"
test("Paused boot errors", function(t)
    local vm = provium:vm("a", "peios"):boot()
    vm:pause()
    local ok = pcall(function() vm:boot() end)
    t:assert(not ok, "boot from Paused should error")
end)
"#,
    );
    assert_one_passed(&outcome);
}

#[test]
fn shutdown_transitions_to_shutdown_state() {
    let outcome = run_local_lua(
        r#"
test("Booted → Shutdown", function(t)
    local vm = provium:vm("a", "peios"):boot()
    vm:shutdown()
    t:assert_eq(vm:state(), "shutdown")
end)
"#,
    );
    assert_one_passed(&outcome);
}

#[test]
fn shutdown_boot_errors_create_new() {
    let outcome = run_local_lua(
        r#"
test("Shutdown reboot rejected", function(t)
    local vm = provium:vm("a", "peios"):boot()
    vm:shutdown()
    local ok, err = pcall(function() vm:boot() end)
    t:assert(not ok)
    t:assert(tostring(err):find("create a new") or tostring(err):find("shutdown"),
        "error must point at create-new: " .. tostring(err))
end)
"#,
    );
    assert_one_passed(&outcome);
}

#[test]
fn shutdown_is_idempotent() {
    // Common-case ergonomic: calling shutdown twice should be
    // a no-op rather than erroring (test cleanup paths often
    // shutdown defensively).
    let outcome = run_local_lua(
        r#"
test("shutdown idempotent", function(t)
    local vm = provium:vm("a", "peios"):boot()
    vm:shutdown()
    -- Second call may succeed silently or raise — accept either,
    -- but it must not panic the host.
    pcall(function() vm:shutdown() end)
    t:assert_eq(vm:state(), "shutdown")
end)
"#,
    );
    assert_one_passed(&outcome);
}
