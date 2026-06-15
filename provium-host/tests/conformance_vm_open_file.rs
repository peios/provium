//! Conformance: `vm:open_file` per `DESIGN.md` § File. R8 #405
//! tightened the parser to refuse modes that have no access bit.

#![cfg(feature = "lua")]

mod common;

use common::{assert_one_failed_with, assert_one_passed, run_local_lua};

#[test]
fn empty_mode_table_rejected() {
    // R8 #405: an empty `{}` table previously opened read-only
    // and any subsequent write deferred-errored at the first
    // I/O. The error was un-attributable to the open call.
    let outcome = run_local_lua(
        r#"
test("empty mode rejected", function(t)
    local vm = provium:vm("a", "peios"):boot()
    local ok, err = pcall(function()
        vm:open_file("/tmp/x", {})
    end)
    t:assert(not ok, "{} should be rejected at parse time")
    t:assert(tostring(err):find("read") or tostring(err):find("write")
        or tostring(err):find("append"),
        "error must list missing access bits: " .. tostring(err))
end)
"#,
    );
    assert_one_passed(&outcome);
}

#[test]
fn create_only_without_access_rejected() {
    // create=true alone — no read/write/append — is the same
    // footgun: "I want this to exist but never read or write
    // it". Surface as an error.
    let outcome = run_local_lua(
        r#"
test("create-only rejected", function(t)
    local vm = provium:vm("a", "peios"):boot()
    local ok, err = pcall(function()
        vm:open_file("/tmp/x", {create=true})
    end)
    t:assert(not ok, "create-only should be rejected")
end)
"#,
    );
    assert_one_passed(&outcome);
}

#[test]
fn read_alone_accepted() {
    let outcome = run_local_lua(
        r#"
test("read alone OK", function(t)
    local tmp = os.tmpname()
    local f = io.open(tmp, "w"); f:write("hi"); f:close()
    local vm = provium:vm("a", "peios"):boot()
    -- Should not raise the parse-time error.
    local h = vm:open_file(tmp, {read=true})
    h:close()
end)
"#,
    );
    assert_one_passed(&outcome);
}

#[test]
fn append_implies_write_for_parse() {
    // append=true alone is enough to satisfy the access-bit
    // check (append is a write mode).
    let outcome = run_local_lua(
        r#"
test("append alone OK", function(t)
    local tmp = os.tmpname()
    local vm = provium:vm("a", "peios"):boot()
    local h = vm:open_file(tmp, {append=true, create=true})
    h:close()
end)
"#,
    );
    assert_one_passed(&outcome);
}
