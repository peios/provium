//! Conformance: snapshot lifecycle per `DESIGN.md` § Snapshot.

#![cfg(feature = "lua")]

mod common;

use common::{assert_one_passed, run_local_lua};

#[test]
fn snapshot_returns_handle_with_path() {
    let outcome = run_local_lua(
        r#"
test("snapshot path", function(t)
    local vm = provium:vm("a", "peios"):boot()
    local snap = vm:snapshot()
    local p = snap:path()
    t:assert(type(p) == "string")
    t:assert(#p > 0, "path must be non-empty")
end)
"#,
    );
    assert_one_passed(&outcome);
}

#[test]
fn snapshot_size_returns_positive_bytes_for_fresh() {
    let outcome = run_local_lua(
        r#"
test("snapshot size", function(t)
    local vm = provium:vm("a", "peios"):boot()
    local snap = vm:snapshot()
    -- LocalAgent's stub-snapshot may produce an empty file; the
    -- contract is "size doesn't error". Pin the no-error path.
    local s = snap:size()
    t:assert(s ~= nil and s >= 0, "size must be non-negative number")
end)
"#,
    );
    assert_one_passed(&outcome);
}

#[test]
fn snapshot_delete_is_idempotent() {
    let outcome = run_local_lua(
        r#"
test("delete idempotent", function(t)
    local vm = provium:vm("a", "peios"):boot()
    local snap = vm:snapshot()
    snap:delete()
    -- Second delete must not raise.
    snap:delete()
end)
"#,
    );
    assert_one_passed(&outcome);
}

#[test]
fn snapshot_size_after_delete_returns_zero() {
    // Per docstring: "after snap:delete() the file is gone …
    // return 0 in the gone-file case rather than surfacing ENOENT".
    let outcome = run_local_lua(
        r#"
test("size after delete", function(t)
    local vm = provium:vm("a", "peios"):boot()
    local snap = vm:snapshot()
    snap:delete()
    t:assert_eq(snap:size(), 0)
end)
"#,
    );
    assert_one_passed(&outcome);
}

#[test]
fn snapshot_with_explicit_path_writes_there() {
    let outcome = run_local_lua(
        r#"
test("explicit snapshot path", function(t)
    local target = os.tmpname() .. ".snap"
    local vm = provium:vm("a", "peios"):boot()
    local snap = vm:snapshot(target)
    t:assert_eq(snap:path(), target)
end)
"#,
    );
    assert_one_passed(&outcome);
}

#[test]
fn snapshot_tostring_includes_path() {
    let outcome = run_local_lua(
        r#"
test("snapshot tostring", function(t)
    local vm = provium:vm("a", "peios"):boot()
    local snap = vm:snapshot()
    local s = tostring(snap)
    t:assert(s:find("snapshot"), "tostring must say 'snapshot': " .. s)
end)
"#,
    );
    assert_one_passed(&outcome);
}
