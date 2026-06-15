//! Conformance: test-context `t` object per `DESIGN.md` §
//! Test context (`t`).

#![cfg(feature = "lua")]

mod common;

use common::{assert_one_failed_with, assert_one_passed, run_local_lua};

#[test]
fn assert_truthy_passes() {
    let outcome = run_local_lua(
        r#"
test("t:assert truthy", function(t)
    t:assert(true)
    t:assert(1)
    t:assert("nonempty")
end)
"#,
    );
    assert_one_passed(&outcome);
}

#[test]
fn assert_falsy_fails_with_message() {
    let outcome = run_local_lua(
        r#"
test("t:assert false", function(t)
    t:assert(false, "intentional failure")
end)
"#,
    );
    assert_one_failed_with(&outcome, "intentional failure");
}

#[test]
fn assert_eq_passes_when_equal() {
    let outcome = run_local_lua(
        r#"
test("assert_eq", function(t)
    t:assert_eq(1, 1)
    t:assert_eq("x", "x")
end)
"#,
    );
    assert_one_passed(&outcome);
}

#[test]
fn assert_eq_fails_with_diff() {
    let outcome = run_local_lua(
        r#"
test("assert_eq diff", function(t)
    t:assert_eq("apple", "orange")
end)
"#,
    );
    assert_one_failed_with(&outcome, "apple");
}

#[test]
fn assert_neq_passes_when_different() {
    let outcome = run_local_lua(
        r#"
test("assert_neq", function(t)
    t:assert_neq(1, 2)
    t:assert_neq("a", "b")
end)
"#,
    );
    assert_one_passed(&outcome);
}

#[test]
fn assert_neq_fails_when_equal() {
    let outcome = run_local_lua(
        r#"
test("assert_neq match", function(t)
    t:assert_neq(7, 7)
end)
"#,
    );
    assert_one_failed_with(&outcome, "7");
}

#[test]
fn skip_marks_test_skipped() {
    let outcome = run_local_lua(
        r#"
test("skipped", function(t)
    t:skip("not relevant in this run")
    -- Code past skip should not execute, but a fail wouldn't
    -- count anyway.
end)
"#,
    );
    assert_eq!(outcome.tests.len(), 1);
    assert_eq!(
        outcome.tests[0].status,
        provium_host::lua::TestStatus::Skipped,
    );
}

#[test]
fn log_does_not_affect_outcome() {
    let outcome = run_local_lua(
        r#"
test("log", function(t)
    t:log("informational")
    t:log("with formatting: " .. tostring(42))
    -- Test still passes.
end)
"#,
    );
    assert_one_passed(&outcome);
}
