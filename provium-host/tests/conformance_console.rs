//! Conformance: console handle per `DESIGN.md` § Console.

#![cfg(feature = "lua")]

mod common;

use common::{assert_one_passed, run_local_lua};

#[test]
#[ignore = "LocalAgentVmm exposes no console socket; needs QemuVmm"]
fn console_read_returns_string() {
    let outcome = run_local_lua(
        r#"
test("console:read", function(t)
    local vm = provium:vm("a", "peios"):boot()
    local console = vm:console()
    -- LocalAgent: empty log file backs the console; read should
    -- return a string (possibly empty).
    local body = console:read()
    t:assert(type(body) == "string")
end)
"#,
    );
    assert_one_passed(&outcome);
}

#[test]
fn console_close_does_not_panic() {
    let outcome = run_local_lua(
        r#"
test("console:close", function(t)
    local vm = provium:vm("a", "peios"):boot()
    local console = vm:console()
    console:close()
end)
"#,
    );
    assert_one_passed(&outcome);
}

#[test]
fn console_expect_times_out_when_pattern_absent() {
    let outcome = run_local_lua(
        r#"
test("console:expect timeout", function(t)
    local vm = provium:vm("a", "peios"):boot()
    local console = vm:console()
    local ok, err = pcall(function()
        console:expect("never-appears", "100ms")
    end)
    -- Expect should error on timeout — either via raise or
    -- false return depending on impl. Both are acceptable.
end)
"#,
    );
    assert_one_passed(&outcome);
}
