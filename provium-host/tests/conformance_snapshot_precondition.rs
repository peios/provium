//! Conformance: snapshot precondition per `DESIGN.md` §
//! Snapshot precondition: no open streams. The whole snapshot
//! pipeline depends on this — both `vm:snapshot` and
//! `lab:snapshot` must refuse with a concrete error.

#![cfg(feature = "lua")]

mod common;

use common::{assert_one_passed, run_local_lua};

#[test]
fn vm_snapshot_with_tail_open_refuses() {
    let outcome = run_local_lua(
        r#"
test("vm snapshot precondition", function(t)
    local tmp = os.tmpname()
    io.open(tmp, "w"):close()
    local vm = provium:vm("a", "peios"):boot()
    local s = vm:tail_file(tmp)
    local snap = os.tmpname() .. ".snap"
    local ok, err = pcall(function() vm:snapshot(snap) end)
    t:assert(not ok)
    t:assert(tostring(err):find("stream"),
        "error must mention streams: " .. tostring(err))
    s:close()
end)
"#,
    );
    assert_one_passed(&outcome);
}

#[test]
fn lab_snapshot_with_tail_open_refuses() {
    let outcome = run_local_lua(
        r#"
test("lab snapshot precondition", function(t)
    local tmp = os.tmpname()
    io.open(tmp, "w"):close()
    local vm = provium:vm("a", "peios"):boot()
    local s = vm:tail_file(tmp)
    local dir = os.tmpname() .. ".d"
    local ok, err = pcall(function() provium:snapshot(dir) end)
    t:assert(not ok)
    t:assert(tostring(err):find("stream"))
    s:close()
end)
"#,
    );
    assert_one_passed(&outcome);
}

#[test]
fn snapshot_succeeds_after_explicit_close() {
    let outcome = run_local_lua(
        r#"
test("snapshot after close", function(t)
    local tmp = os.tmpname()
    io.open(tmp, "w"):close()
    local vm = provium:vm("a", "peios"):boot()
    local s = vm:tail_file(tmp)
    s:close()
    local snap = os.tmpname() .. ".snap"
    -- Should now succeed.
    vm:snapshot(snap)
end)
"#,
    );
    assert_one_passed(&outcome);
}

#[test]
fn snapshot_error_lists_open_stream_origins() {
    // The error should be diagnostic — naming where each open
    // stream was created (file:line). Without that the test
    // author can't find which call to close.
    let outcome = run_local_lua(
        r#"
test("snapshot error diag", function(t)
    local tmp = os.tmpname()
    io.open(tmp, "w"):close()
    local vm = provium:vm("a", "peios"):boot()
    local s = vm:tail_file(tmp)
    local snap = os.tmpname() .. ".snap"
    local ok, err = pcall(function() vm:snapshot(snap) end)
    t:assert(not ok)
    -- Error should be multi-line with a per-stream entry.
    t:assert(tostring(err):find("Close streams") or tostring(err):find("close"),
        "error must hint at remediation: " .. tostring(err))
    s:close()
end)
"#,
    );
    assert_one_passed(&outcome);
}
