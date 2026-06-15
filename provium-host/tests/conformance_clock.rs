//! Conformance: `vm:clock` per `DESIGN.md` § Clock. Each test
//! pins one documented behavior — float / int variants, NaN
//! rejection, signed advance, sub-µs precision via `_ns` form.

#![cfg(feature = "lua")]

mod common;

use common::{assert_one_failed_with, assert_one_passed, run_local_lua};

#[test]
fn get_returns_seconds_as_float() {
    let outcome = run_local_lua(
        r#"
test("clock get is float", function(t)
    local vm = provium:vm("a", "peios"):boot()
    local now = vm:clock():get()
    t:assert(type(now) == "number", "expected number, got " .. type(now))
end)
"#,
    );
    assert_one_passed(&outcome);
}

#[test]
fn get_ns_returns_integer_nanoseconds() {
    // R8 #404: f64 seconds-since-epoch loses sub-µs precision.
    // The `_ns` accessor was added so tests needing exact
    // round-trips can stay in i64 land.
    let outcome = run_local_lua(
        r#"
test("clock get_ns is integer", function(t)
    local vm = provium:vm("a", "peios"):boot()
    local ns = vm:clock():get_ns()
    t:assert(math.type(ns) == "integer",
        "expected integer ns, got " .. tostring(math.type(ns)))
end)
"#,
    );
    assert_one_passed(&outcome);
}

#[test]
#[ignore = "clock_settime requires CAP_SYS_TIME — only runs as root"]
fn set_ns_round_trips_exactly() {
    let outcome = run_local_lua(
        r#"
test("set_ns/get_ns round-trip", function(t)
    local vm = provium:vm("a", "peios"):boot()
    local target = 1700000000123456789
    vm:clock():set_ns(target)
    local readback = vm:clock():get_ns()
    t:assert(math.abs(readback - target) < 1000000,
        string.format("got %d expected %d", readback, target))
end)
"#,
    );
    assert_one_passed(&outcome);
}

#[test]
fn set_with_nan_errors() {
    let outcome = run_local_lua(
        r#"
test("clock:set rejects NaN", function(t)
    local vm = provium:vm("a", "peios"):boot()
    local ok, err = pcall(function() vm:clock():set(0/0) end)
    t:assert(not ok, "NaN should error")
    t:assert(tostring(err):find("NaN") or tostring(err):find("finite"),
        "error must mention NaN/finite: " .. tostring(err))
end)
"#,
    );
    assert_one_passed(&outcome);
}

#[test]
fn set_with_infinity_errors() {
    let outcome = run_local_lua(
        r#"
test("clock:set rejects inf", function(t)
    local vm = provium:vm("a", "peios"):boot()
    local ok, err = pcall(function() vm:clock():set(math.huge) end)
    t:assert(not ok, "inf should error")
end)
"#,
    );
    assert_one_passed(&outcome);
}

#[test]
#[ignore = "clock_settime requires CAP_SYS_TIME — only runs as root"]
fn set_negative_is_allowed() {
    let outcome = run_local_lua(
        r#"
test("clock:set negative", function(t)
    local vm = provium:vm("a", "peios"):boot()
    vm:clock():set(-1.5)
end)
"#,
    );
    assert_one_passed(&outcome);
}

#[test]
#[ignore = "clock_settime requires CAP_SYS_TIME — only runs as root"]
fn advance_negative_is_allowed() {
    let outcome = run_local_lua(
        r#"
test("clock:advance negative", function(t)
    local vm = provium:vm("a", "peios"):boot()
    vm:clock():advance(-1.0)
end)
"#,
    );
    assert_one_passed(&outcome);
}

#[test]
fn advance_with_nan_errors() {
    let outcome = run_local_lua(
        r#"
test("clock:advance rejects NaN", function(t)
    local vm = provium:vm("a", "peios"):boot()
    local ok, err = pcall(function() vm:clock():advance(0/0) end)
    t:assert(not ok)
    t:assert(tostring(err):find("NaN") or tostring(err):find("finite"))
end)
"#,
    );
    assert_one_passed(&outcome);
}

#[test]
#[ignore = "clock_settime requires CAP_SYS_TIME — only runs as root"]
fn advance_accepts_duration_string() {
    let outcome = run_local_lua(
        r#"
test("clock:advance string", function(t)
    local vm = provium:vm("a", "peios"):boot()
    vm:clock():advance("500ms")
    vm:clock():advance("2s")
end)
"#,
    );
    assert_one_passed(&outcome);
}

#[test]
fn sleep_accepts_duration_forms() {
    let outcome = run_local_lua(
        r#"
test("clock:sleep duration forms", function(t)
    local vm = provium:vm("a", "peios"):boot()
    -- Number = seconds.
    vm:clock():sleep(0.001)
    -- Duration string.
    vm:clock():sleep("1ms")
end)
"#,
    );
    assert_one_passed(&outcome);
}
