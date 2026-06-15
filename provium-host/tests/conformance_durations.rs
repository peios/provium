//! Conformance: duration-form acceptance per `DESIGN.md` §
//! Time and timeouts. Number-of-seconds OR `"500ms"`/`"5s"`/...

#![cfg(feature = "lua")]

mod common;

use common::{assert_one_passed, run_local_lua};

#[test]
fn ms_suffix_parsed() {
    let outcome = run_local_lua(
        r#"
test("ms suffix", function(t)
    local vm = provium:vm("a", "peios"):boot()
    -- clock:sleep accepts both forms.
    vm:clock():sleep("10ms")
end)
"#,
    );
    assert_one_passed(&outcome);
}

#[test]
fn seconds_suffix_parsed() {
    let outcome = run_local_lua(
        r#"
test("s suffix", function(t)
    local vm = provium:vm("a", "peios"):boot()
    vm:clock():sleep("0s")
end)
"#,
    );
    assert_one_passed(&outcome);
}

#[test]
fn plain_number_treated_as_seconds() {
    let outcome = run_local_lua(
        r#"
test("number seconds", function(t)
    local vm = provium:vm("a", "peios"):boot()
    -- 0 seconds — no actual sleep, just parse.
    vm:clock():sleep(0)
end)
"#,
    );
    assert_one_passed(&outcome);
}

#[test]
fn float_treated_as_seconds() {
    let outcome = run_local_lua(
        r#"
test("float seconds", function(t)
    local vm = provium:vm("a", "peios"):boot()
    vm:clock():sleep(0.001)  -- 1ms
end)
"#,
    );
    assert_one_passed(&outcome);
}

#[test]
fn unknown_suffix_rejected() {
    let outcome = run_local_lua(
        r#"
test("bad suffix", function(t)
    local vm = provium:vm("a", "peios"):boot()
    local ok, err = pcall(function() vm:clock():sleep("5fortnights") end)
    t:assert(not ok, "unknown duration suffix should error")
end)
"#,
    );
    assert_one_passed(&outcome);
}
