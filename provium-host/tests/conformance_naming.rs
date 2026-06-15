//! Conformance: reserved names + naming conventions per
//! `DESIGN.md` § Naming conventions and § Lab dot-access.

#![cfg(feature = "lua")]

mod common;

use common::{assert_one_failed_with, assert_one_passed, run_local_lua};

#[test]
fn reserved_vm_fixture_blocked_at_create_vm() {
    // R8 #412: dot-access surface defines `vm_fixture`,
    // `lab_fixture`, `pack`, `unpack` as reserved keys. Using
    // them as a VM/bridge/sub-lab name silently shadows the
    // helper method when looked up via lab.<name>. Refuse at
    // create time so test authors find out immediately.
    let outcome = run_local_lua(
        r#"
test("vm_fixture reserved", function(t)
    local ok, err = pcall(function()
        provium:vm("vm_fixture", "peios")
    end)
    t:assert(not ok, "vm_fixture should be reserved")
    t:assert(tostring(err):find("reserved"),
        "error must mention reserved: " .. tostring(err))
end)
"#,
    );
    assert_one_passed(&outcome);
}

#[test]
fn reserved_lab_fixture_blocked_at_bridge() {
    let outcome = run_local_lua(
        r#"
test("lab_fixture reserved on bridge", function(t)
    local ok, err = pcall(function()
        provium:bridge("lab_fixture", {})
    end)
    t:assert(not ok)
end)
"#,
    );
    assert_one_passed(&outcome);
}

#[test]
fn reserved_pack_blocked_at_sub_lab() {
    let outcome = run_local_lua(
        r#"
test("pack reserved on sub-lab", function(t)
    local ok, err = pcall(function()
        provium:lab("pack")
    end)
    t:assert(not ok)
end)
"#,
    );
    assert_one_passed(&outcome);
}

#[test]
fn reserved_unpack_blocked() {
    let outcome = run_local_lua(
        r#"
test("unpack reserved", function(t)
    local ok = pcall(function() provium:vm("unpack", "peios") end)
    t:assert(not ok, "unpack must be reserved")
end)
"#,
    );
    assert_one_passed(&outcome);
}

#[test]
fn ordinary_names_succeed() {
    let outcome = run_local_lua(
        r#"
test("ordinary names", function(t)
    -- Sanity: non-reserved names must work.
    provium:vm("web", "peios")
    provium:bridge("lan", {})
    provium:lab("dc1")
end)
"#,
    );
    assert_one_passed(&outcome);
}
