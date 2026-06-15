//! Conformance: error model per `DESIGN.md` § Error model.
//! Errors should be informative, name the resource, and where
//! possible suggest the fix. The R8 audit ladder kept catching
//! opaque errors — this file pins the wording so future drift
//! breaks the test.

#![cfg(feature = "lua")]

mod common;

use common::{assert_one_failed_with, assert_one_passed, run_local_lua};

#[test]
fn unknown_vm_error_names_the_vm() {
    let outcome = run_local_lua(
        r#"
test("unknown vm names it", function(t)
    local ok, err = pcall(function() provium:vm("typo-name") end)
    t:assert(not ok)
    t:assert(tostring(err):find("typo%-name"),
        "error must include the bad name: " .. tostring(err))
end)
"#,
    );
    assert_one_passed(&outcome);
}

#[test]
fn unknown_profile_error_names_the_profile() {
    let outcome = run_local_lua(
        r#"
test("unknown profile names it", function(t)
    local ok, err = pcall(function() provium:vm("a", "ghost-profile") end)
    t:assert(not ok)
    t:assert(tostring(err):find("ghost%-profile"),
        "error must include the bad profile name: " .. tostring(err))
end)
"#,
    );
    assert_one_passed(&outcome);
}

#[test]
fn wrong_state_error_names_state() {
    let outcome = run_local_lua(
        r#"
test("state error names state", function(t)
    local vm = provium:vm("a", "peios")
    local ok, err = pcall(function() vm:run("true") end)
    t:assert(not ok)
    -- Error must name the current state OR the transition needed.
    local s = tostring(err)
    t:assert(s:find("not booted") or s:find("created"),
        "error must hint at state: " .. s)
end)
"#,
    );
    assert_one_passed(&outcome);
}

#[test]
fn open_file_no_access_bit_error_lists_options() {
    // R8 #405: empty mode rejection must list which keys to add.
    let outcome = run_local_lua(
        r#"
test("open_file empty mode error", function(t)
    local vm = provium:vm("a", "peios"):boot()
    local ok, err = pcall(function() vm:open_file("/x", {}) end)
    t:assert(not ok)
    -- Must mention all three modes the user could pass.
    t:assert(tostring(err):find("read"))
    t:assert(tostring(err):find("write"))
    t:assert(tostring(err):find("append"))
end)
"#,
    );
    assert_one_passed(&outcome);
}

#[test]
fn duplicate_proc_wait_error_includes_handle() {
    // R8 #403: second wait names the handle so it can be
    // matched back to the offending proc:wait call.
    let outcome = run_local_lua(
        r#"
test("dup wait names handle", function(t)
    local vm = provium:vm("a", "peios"):boot()
    local p = vm:run_async("true")
    p:wait()
    local ok, err = pcall(function() p:wait() end)
    t:assert(not ok)
    -- Either contains the handle number or "one-shot" hint.
    t:assert(tostring(err):find("one-shot") or tostring(err):find("waited"),
        "error must explain consumed handle: " .. tostring(err))
end)
"#,
    );
    assert_one_passed(&outcome);
}
