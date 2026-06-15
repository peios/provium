//! Conformance: within-file behavior per `DESIGN.md` § Within-file
//! behavior. Tests run sequentially, share VMs, and inherit
//! prior state by default.

#![cfg(feature = "lua")]

mod common;

use common::{assert_all_passed, run_local_lua};

#[test]
fn tests_run_sequentially() {
    // No interleaving: test-1 must finish before test-2 starts.
    let outcome = run_local_lua(
        r#"
local order = {}

test("first", function(t)
    table.insert(order, "first")
end)

test("second", function(t)
    table.insert(order, "second")
    t:assert_eq(order[1], "first")
    t:assert_eq(order[2], "second")
end)
"#,
    );
    assert_all_passed(&outcome);
}

#[test]
fn tests_share_vms_across_blocks() {
    let outcome = run_local_lua(
        r#"
local vm = provium:vm("shared", "peios"):boot()

test("first writes file", function(t)
    vm:write_file("/tmp/marker", "from-first")
end)

test("second reads same file", function(t)
    local body = vm:read_file("/tmp/marker")
    t:assert_eq(body, "from-first",
        "default state inheritance broken: " .. body)
end)
"#,
    );
    assert_all_passed(&outcome);
}

#[test]
fn no_auto_reset_by_default() {
    // Confirm reset_between_tests defaults to false.
    let outcome = run_local_lua(
        r#"
local vm = provium:vm("a", "peios"):boot()

test("write then expect persistence", function(t)
    vm:write_file("/tmp/x", "persistent")
end)

test("file still there", function(t)
    -- If auto-reset were on by default, /tmp/x would be gone.
    local body = vm:read_file("/tmp/x")
    t:assert_eq(body, "persistent")
end)
"#,
    );
    assert_all_passed(&outcome);
}
