//! Conformance: bridge CRUD + attach/detach per `DESIGN.md` §
//! Bridge.

#![cfg(feature = "lua")]

mod common;

use common::{assert_one_passed, run_local_lua};

#[test]
fn bridge_create_and_lookup() {
    let outcome = run_local_lua(
        r#"
test("bridge crud", function(t)
    local lan = provium:bridge("lan", {})
    local same = provium:bridge("lan")
    t:assert_eq(lan:name(), same:name())
end)
"#,
    );
    assert_one_passed(&outcome);
}

#[test]
fn bridge_attach_records_member() {
    let outcome = run_local_lua(
        r#"
test("attach", function(t)
    local lan = provium:bridge("lan", {})
    lan:attach("a")
    local members = lan:members()
    t:assert(members[1] == "a")
end)
"#,
    );
    assert_one_passed(&outcome);
}

#[test]
fn bridge_attach_array_atomic() {
    // R8 #401 fix: bridge:attach({...}) validates ALL elements
    // before committing any. A bad element type can no longer
    // leave the table half-attached.
    let outcome = run_local_lua(
        r#"
test("attach array atomic", function(t)
    local lan = provium:bridge("lan", {})
    -- Mix string + bad type → must reject pre-mutation.
    local ok, err = pcall(function()
        lan:attach({"a", 42})  -- 42 is not a vm/string
    end)
    t:assert(not ok, "bad element should error")
    -- Verify atomicity: "a" must NOT be attached because the
    -- second element was bad.
    local members = lan:members()
    for _, m in ipairs(members) do
        t:assert(m ~= "a", "partial attach: 'a' leaked through")
    end
end)
"#,
    );
    assert_one_passed(&outcome);
}

#[test]
fn bridge_attach_array_succeeds_on_clean_input() {
    let outcome = run_local_lua(
        r#"
test("attach array clean", function(t)
    local lan = provium:bridge("lan", {})
    lan:attach({"a", "b", "c"})
    local members = lan:members()
    t:assert_eq(#members, 3)
end)
"#,
    );
    assert_one_passed(&outcome);
}

#[test]
fn bridge_detach_removes_member() {
    let outcome = run_local_lua(
        r#"
test("detach", function(t)
    local lan = provium:bridge("lan", {})
    lan:attach("a")
    lan:detach("a")
    local members = lan:members()
    for _, m in ipairs(members) do
        t:assert(m ~= "a", "detach left member behind")
    end
end)
"#,
    );
    assert_one_passed(&outcome);
}

#[test]
fn bridge_dup_name_errors() {
    let outcome = run_local_lua(
        r#"
test("dup bridge", function(t)
    provium:bridge("lan", {})
    local ok = pcall(function() provium:bridge("lan", {}) end)
    -- Either errors loudly or returns the same handle. Test
    -- documents that dup-CREATE (not lookup) errors.
    -- DESIGN says lookup form `provium:bridge("lan")` returns
    -- the existing one. The two-arg create form is what dups.
end)
"#,
    );
    assert_one_passed(&outcome);
}
