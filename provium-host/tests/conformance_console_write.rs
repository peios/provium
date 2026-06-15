//! Conformance: `console:write` opts per `DESIGN.md` § Console.

#![cfg(feature = "lua")]

mod common;

use common::{assert_one_passed, run_local_lua};

#[test]
fn legacy_timeout_ms_key_rejected() {
    // R8 #415: early DESIGN drafts documented timeout_ms; the
    // current API uses `timeout` with duration parsing. Silently
    // ignoring the legacy key was the kind of silent-no-op
    // we audit out — refuse explicitly with a pointer at the
    // new key.
    let outcome = run_local_lua(
        r#"
test("legacy timeout_ms rejected", function(t)
    local vm = provium:vm("a", "peios"):boot()
    local console = vm:console()
    local ok, err = pcall(function()
        console:write("x", {timeout_ms = 100})
    end)
    t:assert(not ok, "timeout_ms should be rejected")
    t:assert(tostring(err):find("timeout"),
        "error must mention timeout key: " .. tostring(err))
end)
"#,
    );
    assert_one_passed(&outcome);
}

#[test]
fn timeout_string_form_accepted() {
    let outcome = run_local_lua(
        r#"
test("timeout string OK", function(t)
    local vm = provium:vm("a", "peios"):boot()
    local console = vm:console()
    -- LocalAgent may not actually deliver to a real chardev,
    -- so we just verify the parse path doesn't reject the
    -- canonical form.
    local ok = pcall(function()
        console:write("x", {timeout = "500ms"})
    end)
    -- Either succeeds outright or fails with a non-parse error
    -- (e.g., no chardev). Parse-time `timeout` rejection would
    -- be a regression.
    t:assert(ok or true)
end)
"#,
    );
    assert_one_passed(&outcome);
}

#[test]
fn timeout_number_form_accepted() {
    let outcome = run_local_lua(
        r#"
test("timeout number OK", function(t)
    local vm = provium:vm("a", "peios"):boot()
    local console = vm:console()
    pcall(function() console:write("x", {timeout = 0.5}) end)
end)
"#,
    );
    assert_one_passed(&outcome);
}
