//! Conformance: lab:barrier per `DESIGN.md` § Lab. Current
//! impl is a synchronous wait: `provium:barrier(name, count,
//! timeout)` blocks until `count` arrivers have called the same
//! barrier, then returns true; on timeout returns false.

#![cfg(feature = "lua")]

mod common;

use common::{assert_one_passed, run_local_lua};

#[test]
fn barrier_count_one_returns_true_immediately() {
    let outcome = run_local_lua(
        r#"
test("barrier solo", function(t)
    local arrived = provium:barrier("solo", 1, 0.5)
    t:assert(arrived, "single-count barrier must satisfy on first call")
end)
"#,
    );
    assert_one_passed(&outcome);
}

#[test]
fn barrier_times_out_when_count_unmet() {
    let outcome = run_local_lua(
        r#"
test("barrier timeout", function(t)
    -- count=2 with only one arriver — must time out.
    local arrived = provium:barrier("waits", 2, 0.05)
    t:assert(not arrived, "unmet count must time out (return false)")
end)
"#,
    );
    assert_one_passed(&outcome);
}

#[test]
fn barrier_count_must_be_integer() {
    let outcome = run_local_lua(
        r#"
test("barrier bad count", function(t)
    local ok, err = pcall(function() provium:barrier("X", "two", 0.1) end)
    t:assert(not ok)
    t:assert(tostring(err):find("count") or tostring(err):find("int"))
end)
"#,
    );
    assert_one_passed(&outcome);
}

#[test]
fn barrier_name_must_be_string() {
    let outcome = run_local_lua(
        r#"
test("barrier bad name", function(t)
    local ok, err = pcall(function() provium:barrier(42, 1, 0.1) end)
    t:assert(not ok)
end)
"#,
    );
    assert_one_passed(&outcome);
}

#[test]
fn barrier_default_timeout_when_omitted() {
    let outcome = run_local_lua(
        r#"
test("barrier default timeout", function(t)
    -- Omitted timeout uses the documented default (60s) but
    -- count=1 makes it return immediately.
    local arrived = provium:barrier("solo2", 1)
    t:assert(arrived)
end)
"#,
    );
    assert_one_passed(&outcome);
}
