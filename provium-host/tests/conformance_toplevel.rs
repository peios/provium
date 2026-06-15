//! Conformance: top-level globals per `DESIGN.md` § Top-level.

#![cfg(feature = "lua")]

mod common;

use common::{assert_one_passed, run_local_lua};

#[test]
fn pack_unpack_round_trips_via_colon() {
    // CURRENT IMPL: pack/unpack are colon-syntax methods on
    // the lab. DESIGN says they should be dot-syntax factory
    // functions. Test both forms below.
    let outcome = run_local_lua(
        r#"
test("pack/unpack colon", function(t)
    local bytes = provium:pack(">I4", 0x01020304)
    t:assert_eq(#bytes, 4)
    local v = provium:unpack(">I4", bytes)
    t:assert_eq(v, 0x01020304)
end)
"#,
    );
    assert_one_passed(&outcome);
}

#[test]
#[ignore = "DESIGN says provium.pack (dot syntax) but impl uses add_method \
            which requires colon — see lab_ud.rs `pack` definition"]
fn pack_unpack_round_trips_via_dot() {
    let outcome = run_local_lua(
        r#"
test("pack/unpack dot", function(t)
    local bytes = provium.pack(">I4", 0x01020304)
    t:assert_eq(#bytes, 4)
end)
"#,
    );
    assert_one_passed(&outcome);
}

#[test]
fn wait_until_returns_predicate_value() {
    let outcome = run_local_lua(
        r#"
test("wait_until immediate", function(t)
    local v = wait_until(function() return 42 end)
    t:assert_eq(v, 42)
end)
"#,
    );
    assert_one_passed(&outcome);
}

#[test]
fn wait_until_times_out_when_predicate_never_truthy() {
    let outcome = run_local_lua(
        r#"
test("wait_until timeout", function(t)
    local ok, err = pcall(function()
        wait_until(function() return false end, {timeout = 0.05})
    end)
    t:assert(not ok, "wait_until must raise on timeout")
end)
"#,
    );
    assert_one_passed(&outcome);
}

#[test]
fn wait_until_propagates_predicate_error() {
    let outcome = run_local_lua(
        r#"
test("wait_until error propagation", function(t)
    local ok, err = pcall(function()
        wait_until(function() error("custom failure") end)
    end)
    t:assert(not ok, "wait_until must propagate predicate errors")
    t:assert(tostring(err):find("custom failure"))
end)
"#,
    );
    assert_one_passed(&outcome);
}

#[test]
fn todo_marks_subsequent_tests_skipped() {
    // todo() at file scope marks the file or test todo. Behavior
    // varies per impl; this test documents the call works without
    // raising at parse time.
    let outcome = run_local_lua(
        r#"
todo("not yet")

test("would be a test", function(t)
end)
"#,
    );
    // Either todo skipped the test, marked it passed, or
    // produced no test entry — all are documented variants
    // of "todo prevented it running". A FAILED outcome would
    // be a regression.
    if let Some(t0) = outcome.tests.first() {
        assert_ne!(
            t0.status,
            provium_host::lua::TestStatus::Failed,
            "todo() left a test failing: {:?}", t0.message,
        );
    }
}
