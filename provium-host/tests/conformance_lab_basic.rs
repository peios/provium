//! Conformance: lab CRUD per `DESIGN.md` § Lab.

#![cfg(feature = "lua")]

mod common;

use common::{assert_one_passed, run_local_lua};

#[test]
fn provium_is_root_lab() {
    let outcome = run_local_lua(
        r#"
test("provium is lab", function(t)
    t:assert(provium:name() ~= nil)
end)
"#,
    );
    assert_one_passed(&outcome);
}

#[test]
fn vm_lookup_form_returns_same_handle() {
    let outcome = run_local_lua(
        r#"
test("vm lookup form", function(t)
    local created = provium:vm("a", "peios")
    local found = provium:vm("a")
    t:assert_eq(created:name(), found:name())
end)
"#,
    );
    assert_one_passed(&outcome);
}

#[test]
fn vm_unknown_lookup_errors() {
    let outcome = run_local_lua(
        r#"
test("vm unknown lookup", function(t)
    local ok, err = pcall(function() provium:vm("ghost") end)
    t:assert(not ok)
end)
"#,
    );
    assert_one_passed(&outcome);
}

#[test]
fn duplicate_vm_name_errors_at_create() {
    let outcome = run_local_lua(
        r#"
test("dup vm", function(t)
    provium:vm("a", "peios")
    local ok, err = pcall(function() provium:vm("a", "peios") end)
    t:assert(not ok)
end)
"#,
    );
    assert_one_passed(&outcome);
}

#[test]
fn unknown_profile_errors() {
    let outcome = run_local_lua(
        r#"
test("unknown profile", function(t)
    local ok, err = pcall(function() provium:vm("a", "missing-profile") end)
    t:assert(not ok)
    t:assert(tostring(err):find("missing-profile") or tostring(err):find("profile"),
        "error must mention profile: " .. tostring(err))
end)
"#,
    );
    assert_one_passed(&outcome);
}

#[test]
fn sub_lab_creates_with_dot_access() {
    let outcome = run_local_lua(
        r#"
test("sub-lab dot", function(t)
    local dc1 = provium:lab("dc1")
    -- dc1 is itself a lab
    t:assert(dc1:name() ~= nil)
    -- VM created in sub-lab is reachable through it
    dc1:vm("web", "peios")
    local found = dc1:vm("web")
    t:assert(found:name() == "web")
end)
"#,
    );
    assert_one_passed(&outcome);
}

#[test]
fn vm_names_lists_lex_order() {
    let outcome = run_local_lua(
        r#"
test("vm_names sorted", function(t)
    provium:vm("c", "peios")
    provium:vm("a", "peios")
    provium:vm("b", "peios")
    local names = provium:vm_names()
    t:assert_eq(names[1], "a")
    t:assert_eq(names[2], "b")
    t:assert_eq(names[3], "c")
end)
"#,
    );
    assert_one_passed(&outcome);
}

#[test]
fn lab_remove_drops_member() {
    let outcome = run_local_lua(
        r#"
test("lab remove", function(t)
    provium:vm("a", "peios")
    provium:remove("a")
    local ok = pcall(function() provium:vm("a") end)
    t:assert(not ok, "removed VM should be gone")
end)
"#,
    );
    assert_one_passed(&outcome);
}
