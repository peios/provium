//! Conformance: RunResult per `DESIGN.md` § RunResult.

#![cfg(feature = "lua")]

mod common;

use common::{assert_one_passed, run_local_lua};

#[test]
fn run_returns_run_result() {
    let outcome = run_local_lua(
        r#"
test("run basic", function(t)
    local vm = provium:vm("a", "peios"):boot()
    local r = vm:run("true")
    t:assert(r:ok(), "true must report ok")
end)
"#,
    );
    assert_one_passed(&outcome);
}

#[test]
fn run_result_exposes_exit_code() {
    let outcome = run_local_lua(
        r#"
test("exit_code", function(t)
    local vm = provium:vm("a", "peios"):boot()
    local r = vm:run("true")
    t:assert_eq(r.exit_code, 0)
end)
"#,
    );
    assert_one_passed(&outcome);
}

#[test]
fn run_result_captures_stdout() {
    let outcome = run_local_lua(
        r#"
test("stdout capture", function(t)
    local vm = provium:vm("a", "peios"):boot()
    local r = vm:run("printf hello")
    t:assert_eq(r.stdout, "hello")
end)
"#,
    );
    assert_one_passed(&outcome);
}

#[test]
fn run_result_status_is_exited_for_normal_exit() {
    let outcome = run_local_lua(
        r#"
test("status exited", function(t)
    local vm = provium:vm("a", "peios"):boot()
    local r = vm:run("true")
    t:assert_eq(r.status, "exited")
end)
"#,
    );
    assert_one_passed(&outcome);
}

#[test]
fn assert_ok_passes_for_zero_exit() {
    let outcome = run_local_lua(
        r#"
test("assert_ok pass", function(t)
    local vm = provium:vm("a", "peios"):boot()
    vm:run("true"):assert_ok()
end)
"#,
    );
    assert_one_passed(&outcome);
}

#[test]
fn assert_ok_raises_for_nonzero_exit() {
    let outcome = run_local_lua(
        r#"
test("assert_ok fail", function(t)
    local vm = provium:vm("a", "peios"):boot()
    local ok, err = pcall(function() vm:run("false"):assert_ok() end)
    t:assert(not ok, "false must fail assert_ok")
end)
"#,
    );
    assert_one_passed(&outcome);
}

#[test]
fn timed_out_returns_false_for_normal_exit() {
    let outcome = run_local_lua(
        r#"
test("timed_out false", function(t)
    local vm = provium:vm("a", "peios"):boot()
    local r = vm:run("true")
    -- Per DESIGN: timed_out is a field (bool).
    t:assert(not r.timed_out)
end)
"#,
    );
    assert_one_passed(&outcome);
}

#[test]
fn signal_field_nil_for_normal_exit() {
    let outcome = run_local_lua(
        r#"
test("signal nil", function(t)
    local vm = provium:vm("a", "peios"):boot()
    local r = vm:run("true")
    t:assert(r.signal == nil, "signal must be nil after normal exit")
end)
"#,
    );
    assert_one_passed(&outcome);
}
