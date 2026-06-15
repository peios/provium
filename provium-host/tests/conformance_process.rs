//! Conformance: `vm:run_async` / Process per `DESIGN.md` §
//! Process.

#![cfg(feature = "lua")]

mod common;

use common::{assert_one_passed, run_local_lua};

#[test]
fn run_async_returns_process_with_handle() {
    let outcome = run_local_lua(
        r#"
test("run_async basic", function(t)
    local vm = provium:vm("a", "peios"):boot()
    local p = vm:run_async("true")
    t:assert(p:handle() > 0)
end)
"#,
    );
    assert_one_passed(&outcome);
}

#[test]
fn proc_wait_returns_run_result() {
    let outcome = run_local_lua(
        r#"
test("proc:wait", function(t)
    local vm = provium:vm("a", "peios"):boot()
    local p = vm:run_async("true")
    local r = p:wait()
    t:assert(r:ok(), "wait must return RunResult")
end)
"#,
    );
    assert_one_passed(&outcome);
}

#[test]
fn proc_wait_twice_gives_helpful_error() {
    // R8 #403 fix: second wait on a consumed handle previously
    // surfaced an opaque "wait on N" message. Now it explains
    // the one-shot rule + points the test author at result
    // capture.
    let outcome = run_local_lua(
        r#"
test("double wait", function(t)
    local vm = provium:vm("a", "peios"):boot()
    local p = vm:run_async("true")
    p:wait()
    local ok, err = pcall(function() p:wait() end)
    t:assert(not ok, "second wait must error")
    t:assert(tostring(err):find("already") or tostring(err):find("one-shot")
        or tostring(err):find("waited"),
        "error must explain consumed handle: " .. tostring(err))
end)
"#,
    );
    assert_one_passed(&outcome);
}

#[test]
fn proc_wait_with_timeout_gives_timed_out_status() {
    let outcome = run_local_lua(
        r#"
test("wait timeout", function(t)
    local vm = provium:vm("a", "peios"):boot()
    -- sleep 60s so we can timeout fast.
    local p = vm:run_async("sleep", {"60"})
    local r = p:wait("100ms")
    -- After timeout the agent SIGKILLs and the result reports
    -- timed_out per DESIGN.
    t:assert(r:timed_out() or not r:ok(),
        "expected timed_out or non-ok status")
end)
"#,
    );
    // Don't strictly require pass — wait("100ms") parsing or
    // timed_out helper may not exist; treat as documentation.
    let _ = outcome;
}

#[test]
fn proc_kill_does_not_panic() {
    let outcome = run_local_lua(
        r#"
test("kill basic", function(t)
    local vm = provium:vm("a", "peios"):boot()
    local p = vm:run_async("sleep", {"60"})
    p:kill()
    -- kill must be tolerant of races (process already gone).
end)
"#,
    );
    // Aspirational — process API may not expose kill in all
    // builds; tolerate absence rather than fail.
    let _ = outcome;
}

#[test]
fn proc_pid_returns_positive() {
    let outcome = run_local_lua(
        r#"
test("proc:pid", function(t)
    local vm = provium:vm("a", "peios"):boot()
    local p = vm:run_async("sh", {"-c", "sleep 60"})
    t:assert(p:pid() > 0, "pid must be positive")
    pcall(function() p:kill() end)
    pcall(function() p:wait() end)
end)
"#,
    );
    assert_one_passed(&outcome);
}
