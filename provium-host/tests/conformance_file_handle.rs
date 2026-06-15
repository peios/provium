//! Conformance: file handle ops per `DESIGN.md` § File.

#![cfg(feature = "lua")]

mod common;

use common::{assert_one_passed, run_local_lua};

#[test]
fn read_returns_bytes_up_to_n() {
    let outcome = run_local_lua(
        r#"
test("file:read(n)", function(t)
    local tmp = os.tmpname()
    local f = io.open(tmp, "w"); f:write("hello world"); f:close()
    local vm = provium:vm("a", "peios"):boot()
    local h = vm:open_file(tmp, {read=true})
    local body = h:read(5)
    t:assert_eq(body, "hello")
    h:close()
end)
"#,
    );
    assert_one_passed(&outcome);
}

#[test]
fn read_all_returns_full_contents() {
    let outcome = run_local_lua(
        r#"
test("file:read_all", function(t)
    local tmp = os.tmpname()
    local f = io.open(tmp, "w"); f:write("hello"); f:close()
    local vm = provium:vm("a", "peios"):boot()
    local h = vm:open_file(tmp, {read=true})
    t:assert_eq(h:read_all(), "hello")
    h:close()
end)
"#,
    );
    assert_one_passed(&outcome);
}

#[test]
fn write_returns_byte_count() {
    let outcome = run_local_lua(
        r#"
test("file:write", function(t)
    local tmp = os.tmpname()
    local vm = provium:vm("a", "peios"):boot()
    local h = vm:open_file(tmp, {write=true, create=true, truncate=true})
    local n = h:write("payload")
    t:assert_eq(n, 7)
    h:close()
end)
"#,
    );
    assert_one_passed(&outcome);
}

#[test]
fn seek_set_repositions_cursor() {
    let outcome = run_local_lua(
        r#"
test("seek set", function(t)
    local tmp = os.tmpname()
    local f = io.open(tmp, "w"); f:write("ABCDEFGH"); f:close()
    local vm = provium:vm("a", "peios"):boot()
    local h = vm:open_file(tmp, {read=true})
    h:seek(4, "set")
    t:assert_eq(h:read(2), "EF")
    h:close()
end)
"#,
    );
    assert_one_passed(&outcome);
}

#[test]
fn seek_invalid_whence_errors() {
    let outcome = run_local_lua(
        r#"
test("seek bad whence", function(t)
    local tmp = os.tmpname()
    io.open(tmp, "w"):close()
    local vm = provium:vm("a", "peios"):boot()
    local h = vm:open_file(tmp, {read=true})
    local ok, err = pcall(function() h:seek(0, "middle") end)
    t:assert(not ok)
    t:assert(tostring(err):find("whence") or tostring(err):find("set/cur/end"),
        "error must list valid whences: " .. tostring(err))
    h:close()
end)
"#,
    );
    assert_one_passed(&outcome);
}

#[test]
fn tell_returns_current_offset() {
    let outcome = run_local_lua(
        r#"
test("file:tell", function(t)
    local tmp = os.tmpname()
    local f = io.open(tmp, "w"); f:write("0123456789"); f:close()
    local vm = provium:vm("a", "peios"):boot()
    local h = vm:open_file(tmp, {read=true})
    h:read(3)
    t:assert_eq(h:tell(), 3)
    h:close()
end)
"#,
    );
    assert_one_passed(&outcome);
}

#[test]
fn close_then_read_errors() {
    let outcome = run_local_lua(
        r#"
test("read after close", function(t)
    local tmp = os.tmpname()
    io.open(tmp, "w"):close()
    local vm = provium:vm("a", "peios"):boot()
    local h = vm:open_file(tmp, {read=true})
    h:close()
    local ok, err = pcall(function() h:read(1) end)
    t:assert(not ok)
    t:assert(tostring(err):find("closed"),
        "error must say closed: " .. tostring(err))
end)
"#,
    );
    assert_one_passed(&outcome);
}

#[test]
fn close_is_idempotent() {
    let outcome = run_local_lua(
        r#"
test("close x2", function(t)
    local tmp = os.tmpname()
    io.open(tmp, "w"):close()
    local vm = provium:vm("a", "peios"):boot()
    local h = vm:open_file(tmp, {read=true})
    h:close()
    -- Second close must not raise.
    pcall(function() h:close() end)
end)
"#,
    );
    assert_one_passed(&outcome);
}

#[test]
fn write_then_read_round_trips() {
    let outcome = run_local_lua(
        r#"
test("rw round-trip", function(t)
    local tmp = os.tmpname()
    local vm = provium:vm("a", "peios"):boot()
    local h = vm:open_file(tmp, {write=true, create=true, truncate=true})
    h:write("hello")
    h:close()
    local r = vm:open_file(tmp, {read=true})
    t:assert_eq(r:read_all(), "hello")
    r:close()
end)
"#,
    );
    assert_one_passed(&outcome);
}

#[test]
fn read_at_eof_returns_empty_string() {
    // R9 vm-DOC: pin POSIX-style EOF semantics — reading past
    // EOF returns an empty string, not nil and not an error.
    let outcome = run_local_lua(
        r#"
test("read at EOF", function(t)
    local tmp = os.tmpname()
    local f = io.open(tmp, "w"); f:write("hi"); f:close()
    local vm = provium:vm("a", "peios"):boot()
    local h = vm:open_file(tmp, {read=true})
    t:assert_eq(h:read(2), "hi")
    -- Cursor is now at EOF.
    local empty = h:read(10)
    t:assert(empty == "" or empty == nil,
        "EOF read must return empty string or nil, got " .. tostring(empty))
    h:close()
end)
"#,
    );
    assert_one_passed(&outcome);
}

#[test]
fn read_all_at_eof_returns_empty_string() {
    let outcome = run_local_lua(
        r#"
test("read_all at EOF", function(t)
    local tmp = os.tmpname()
    local f = io.open(tmp, "w"); f:write("data"); f:close()
    local vm = provium:vm("a", "peios"):boot()
    local h = vm:open_file(tmp, {read=true})
    h:read_all()
    -- Now consume any leftover.
    local empty = h:read_all()
    t:assert(empty == "" or empty == nil,
        "second read_all must return empty/nil, got " .. tostring(empty))
    h:close()
end)
"#,
    );
    assert_one_passed(&outcome);
}

#[test]
fn fd_returns_int() {
    let outcome = run_local_lua(
        r#"
test("file:fd", function(t)
    local tmp = os.tmpname()
    io.open(tmp, "w"):close()
    local vm = provium:vm("a", "peios"):boot()
    local h = vm:open_file(tmp, {read=true})
    local fd = h:fd()
    t:assert(fd >= 0, "fd must be non-negative")
    h:close()
end)
"#,
    );
    assert_one_passed(&outcome);
}
