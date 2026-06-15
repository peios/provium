//! Conformance: vm:stat / mkdir / listdir / rename / unlink per
//! `DESIGN.md` § File.

#![cfg(feature = "lua")]

mod common;

use common::{assert_one_passed, run_local_lua};

#[test]
fn stat_returns_size_and_mtime_fields() {
    let outcome = run_local_lua(
        r#"
test("stat shape", function(t)
    local tmp = os.tmpname()
    local f = io.open(tmp, "w"); f:write("body"); f:close()
    local vm = provium:vm("a", "peios"):boot()
    local st = vm:stat(tmp)
    t:assert(st.size ~= nil, "missing size")
    t:assert(st.mtime ~= nil, "missing mtime")
    t:assert(st.mtime_ns ~= nil, "missing mtime_ns")
    t:assert(st.entry_type ~= nil, "missing entry_type")
end)
"#,
    );
    assert_one_passed(&outcome);
}

#[test]
fn stat_reports_correct_size() {
    let outcome = run_local_lua(
        r#"
test("stat size", function(t)
    local tmp = os.tmpname()
    local f = io.open(tmp, "w"); f:write("12345"); f:close()
    local vm = provium:vm("a", "peios"):boot()
    local st = vm:stat(tmp)
    t:assert_eq(st.size, 5)
end)
"#,
    );
    assert_one_passed(&outcome);
}

#[test]
fn stat_mtime_is_seconds_not_nanoseconds() {
    // R7-era regression: mtime once carried ns, making it look
    // like the year 53000+. Ensure the SECONDS field reads as
    // a current-epoch number (well below 1e15).
    let outcome = run_local_lua(
        r#"
test("stat mtime seconds", function(t)
    local tmp = os.tmpname()
    io.open(tmp, "w"):close()
    local vm = provium:vm("a", "peios"):boot()
    local st = vm:stat(tmp)
    t:assert(st.mtime < 1e15,
        "mtime must be seconds, not ns: " .. tostring(st.mtime))
    t:assert(st.mtime > 1e9,
        "mtime must be post-2001 epoch: " .. tostring(st.mtime))
end)
"#,
    );
    assert_one_passed(&outcome);
}

#[test]
fn stat_entry_type_is_file_for_regular_file() {
    let outcome = run_local_lua(
        r#"
test("stat entry_type", function(t)
    local tmp = os.tmpname()
    io.open(tmp, "w"):close()
    local vm = provium:vm("a", "peios"):boot()
    local st = vm:stat(tmp)
    t:assert_eq(st.entry_type, "file")
end)
"#,
    );
    assert_one_passed(&outcome);
}

#[test]
fn stat_perm_present_and_in_octal_range() {
    // R9 vm-DOC: pin that the perm field is exposed and carries
    // POSIX mode bits (a small positive integer ≤ 0o7777).
    let outcome = run_local_lua(
        r#"
test("stat perm", function(t)
    local tmp = os.tmpname()
    io.open(tmp, "w"):close()
    local vm = provium:vm("a", "peios"):boot()
    local st = vm:stat(tmp)
    t:assert(st.perm ~= nil, "perm must be present")
    -- 0o7777 in decimal = 4095. Pin POSIX mode-bit range.
    t:assert(st.perm >= 0 and st.perm <= 4095,
        "perm must be in POSIX mode-bit range, got " .. tostring(st.perm))
end)
"#,
    );
    assert_one_passed(&outcome);
}

#[test]
fn stat_nonexistent_errors() {
    let outcome = run_local_lua(
        r#"
test("stat ENOENT", function(t)
    local vm = provium:vm("a", "peios"):boot()
    local ok, err = pcall(function() vm:stat("/no/such/file") end)
    t:assert(not ok)
end)
"#,
    );
    assert_one_passed(&outcome);
}

#[test]
fn mkdir_then_stat_reports_directory() {
    let outcome = run_local_lua(
        r#"
test("mkdir + stat", function(t)
    local dir = os.tmpname() .. ".d"
    local vm = provium:vm("a", "peios"):boot()
    vm:mkdir(dir)
    local st = vm:stat(dir)
    t:assert_eq(st.entry_type, "directory")
end)
"#,
    );
    assert_one_passed(&outcome);
}

#[test]
fn listdir_returns_table_of_entries() {
    let outcome = run_local_lua(
        r#"
test("listdir", function(t)
    local dir = os.tmpname() .. ".d"
    local vm = provium:vm("a", "peios"):boot()
    vm:mkdir(dir)
    local f = io.open(dir .. "/x", "w"); f:write(""); f:close()
    local entries = vm:listdir(dir)
    t:assert(type(entries) == "table")
    -- Find "x" in the table.
    local found = false
    for _, e in ipairs(entries) do
        local name = e.name or e
        if name == "x" then found = true; break end
    end
    t:assert(found, "listdir must include the created file")
end)
"#,
    );
    assert_one_passed(&outcome);
}

#[test]
fn rename_moves_file() {
    let outcome = run_local_lua(
        r#"
test("rename", function(t)
    local src = os.tmpname()
    local f = io.open(src, "w"); f:write("data"); f:close()
    local dst = src .. ".moved"
    local vm = provium:vm("a", "peios"):boot()
    vm:rename(src, dst)
    t:assert_eq(vm:read_file(dst), "data")
    -- src must be gone.
    local ok = pcall(function() vm:stat(src) end)
    t:assert(not ok, "src must be gone after rename")
end)
"#,
    );
    assert_one_passed(&outcome);
}

#[test]
fn unlink_removes_file() {
    let outcome = run_local_lua(
        r#"
test("unlink", function(t)
    local tmp = os.tmpname()
    io.open(tmp, "w"):close()
    local vm = provium:vm("a", "peios"):boot()
    vm:unlink(tmp)
    local ok = pcall(function() vm:stat(tmp) end)
    t:assert(not ok, "stat after unlink must error")
end)
"#,
    );
    assert_one_passed(&outcome);
}
