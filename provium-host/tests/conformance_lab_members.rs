//! Conformance: lab:include / lab:remove / lab:members per
//! `DESIGN.md` § Lab.

#![cfg(feature = "lua")]

mod common;

use common::{assert_one_passed, run_local_lua};

#[test]
fn include_vm_userdata_attaches() {
    let outcome = run_local_lua(
        r#"
test("include vm", function(t)
    local sub = provium:lab("dc1")
    local vm = provium:vm("a", "peios")
    sub:include(vm)
    -- VM is now in sub-lab.
    local found = sub:vm("a")
    t:assert(found ~= nil)
end)
"#,
    );
    assert_one_passed(&outcome);
}

#[test]
fn include_bridge_userdata_attaches() {
    let outcome = run_local_lua(
        r#"
test("include bridge", function(t)
    local sub = provium:lab("dc1")
    local b = provium:bridge("lan", {})
    sub:include(b)
    local found = sub:bridge("lan")
    t:assert(found ~= nil)
end)
"#,
    );
    assert_one_passed(&outcome);
}

#[test]
fn include_array_with_bad_element_errors() {
    let outcome = run_local_lua(
        r#"
test("include bad array", function(t)
    local sub = provium:lab("dc1")
    local vm = provium:vm("a", "peios")
    local ok, err = pcall(function()
        sub:include({vm, "not-userdata"})
    end)
    t:assert(not ok)
    t:assert(tostring(err):find("userdata"))
end)
"#,
    );
    assert_one_passed(&outcome);
}

#[test]
fn remove_via_string_drops_vm() {
    let outcome = run_local_lua(
        r#"
test("remove by name", function(t)
    provium:vm("a", "peios")
    provium:remove("a")
    local ok = pcall(function() provium:vm("a") end)
    t:assert(not ok, "vm should be gone after remove")
end)
"#,
    );
    assert_one_passed(&outcome);
}

#[test]
fn remove_via_userdata_drops_resource() {
    let outcome = run_local_lua(
        r#"
test("remove by userdata", function(t)
    local vm = provium:vm("a", "peios")
    provium:remove(vm)
    local ok = pcall(function() provium:vm("a") end)
    t:assert(not ok)
end)
"#,
    );
    assert_one_passed(&outcome);
}

#[test]
fn members_lists_attached_resources() {
    let outcome = run_local_lua(
        r#"
test("members", function(t)
    provium:vm("a", "peios")
    provium:bridge("lan", {})
    local members = provium:members()
    t:assert(type(members) == "table")
    -- Membership should include both kinds. Schema may flatten
    -- as {kind, name} or interleave by type.
    t:assert(#members >= 2, "expected ≥ 2 members, got " .. tostring(#members))
end)
"#,
    );
    assert_one_passed(&outcome);
}
