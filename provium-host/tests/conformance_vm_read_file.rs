//! Conformance: `vm:read_file` per `DESIGN.md` § File. R8 #402
//! added a 96 MiB host-side guard so a giant file no longer
//! crashes the VM by exceeding the 128 MiB wire frame.

#![cfg(feature = "lua")]

mod common;

use common::{assert_one_passed, run_local_lua};

#[test]
fn small_file_reads_back() {
    let outcome = run_local_lua(
        r#"
test("read_file small", function(t)
    local tmp = os.tmpname()
    local f = io.open(tmp, "w"); f:write("hello"); f:close()
    local vm = provium:vm("a", "peios"):boot()
    local body = vm:read_file(tmp)
    t:assert_eq(body, "hello")
end)
"#,
    );
    assert_one_passed(&outcome);
}

#[test]
fn nonexistent_file_errors_clean() {
    let outcome = run_local_lua(
        r#"
test("read_file ENOENT", function(t)
    local vm = provium:vm("a", "peios"):boot()
    local ok, err = pcall(function()
        vm:read_file("/no/such/file/here")
    end)
    t:assert(not ok)
end)
"#,
    );
    assert_one_passed(&outcome);
}

#[test]
fn huge_file_refused_with_helpful_pointer() {
    // R8 #402: anything past ~96 MiB exceeds the wire frame
    // and would crash the agent on serialize. Pre-stat and
    // refuse with a pointer at the streaming alternatives.
    let outcome = run_local_lua(
        r#"
test("read_file too big", function(t)
    local tmp = os.tmpname()
    -- Make a 100 MiB sparse file via seek+write — fast, no
    -- 100 MiB of disk needed for the test.
    local f = io.open(tmp, "w")
    f:seek("set", 100 * 1024 * 1024)
    f:write("x")
    f:close()
    local vm = provium:vm("a", "peios"):boot()
    local ok, err = pcall(function() vm:read_file(tmp) end)
    t:assert(not ok, "read_file should refuse > 96 MiB")
    -- Error must mention the alternatives so the test author
    -- knows what to do.
    t:assert(tostring(err):find("tail_file") or tostring(err):find("open_file"),
        "error must point at alternatives: " .. tostring(err))
end)
"#,
    );
    assert_one_passed(&outcome);
}
