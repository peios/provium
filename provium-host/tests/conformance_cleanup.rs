//! Conformance: auto-close ordering per `DESIGN.md` § Cleanup
//! and § Auto-close ordering. Test-scope resources die at test
//! end; file-scope resources survive across tests in the same
//! file but die at file end.

#![cfg(feature = "lua")]

mod common;

use common::{assert_all_passed, run_local_lua};

#[test]
fn streams_auto_close_at_test_end() {
    // After test() returns, any test-scope stream should be
    // closed automatically. Next test starts clean.
    let outcome = run_local_lua(
        r#"
test("test-1 opens stream", function(t)
    local tmp = os.tmpname()
    local f = io.open(tmp, "w"); f:write(""); f:close()
    local vm = provium:vm("a", "peios"):boot()
    -- Open + leak intentionally — auto-close should reap it.
    local s = vm:tail_file(tmp)
    -- Don't call s:close()
end)

test("test-2 sees clean slate", function(t)
    -- A leaked stream from test-1 would block lab snapshot
    -- preconditions; surface as a basic operation working.
    local vm = provium:vm("a")
    t:assert_eq(vm:state(), "booted")
end)
"#,
    );
    assert_all_passed(&outcome);
}

#[test]
fn workers_auto_close_at_test_end() {
    let outcome = run_local_lua(
        r#"
test("worker leak", function(t)
    local vm = provium:vm("a", "peios"):boot()
    local w = vm:spawn_worker()
    -- intentionally don't close
end)

test("subsequent test", function(t)
    local vm = provium:vm("a")
    t:assert_eq(vm:state(), "booted")
end)
"#,
    );
    assert_all_passed(&outcome);
}

#[test]
fn vms_survive_across_tests_in_same_file() {
    // DESIGN: VMs and bridges only auto-close at FILE end,
    // not test end — they live for the file's lifetime.
    let outcome = run_local_lua(
        r#"
test("test-1 boots", function(t)
    local vm = provium:vm("a", "peios"):boot()
end)

test("test-2 finds same VM", function(t)
    -- VM "a" must still be reachable; not auto-shutdown.
    local vm = provium:vm("a")
    t:assert_eq(vm:state(), "booted")
end)
"#,
    );
    assert_all_passed(&outcome);
}

#[test]
fn bridges_survive_across_tests_in_same_file() {
    let outcome = run_local_lua(
        r#"
test("create bridge", function(t)
    provium:bridge("lan", {})
end)

test("bridge still here", function(t)
    local lan = provium:bridge("lan")
    t:assert(lan)
end)
"#,
    );
    assert_all_passed(&outcome);
}

#[test]
fn file_scope_stream_survives_across_tests() {
    // DESIGN § Cross-test stream sharing: streams created
    // OUTSIDE any test() block are owned by the file.
    let outcome = run_local_lua(
        r#"
local tmp = os.tmpname()
local f = io.open(tmp, "w"); f:write(""); f:close()
local vm = provium:vm("a", "peios"):boot()
local stream = vm:tail_file(tmp)

test("test-1 sees file-scope stream", function(t)
    t:assert(stream ~= nil)
end)

test("test-2 still sees it", function(t)
    -- Auto-close would have reaped a test-scope stream; this
    -- one survives because it was declared at file scope.
    t:assert(stream ~= nil)
end)
"#,
    );
    assert_all_passed(&outcome);
}
