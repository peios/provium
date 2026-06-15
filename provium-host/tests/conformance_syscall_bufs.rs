//! Conformance: vm:syscall with bufs/ptrs splice per
//! `DESIGN.md` § VM. Table-form syscall takes byte buffers,
//! splices their addresses into the indicated args slot, and
//! returns post-syscall buffer contents in `out_bufs`.

#![cfg(feature = "lua")]

mod common;

use common::{assert_one_passed, run_local_lua};

#[test]
fn plain_int_form_returns_ret_and_errno() {
    let outcome = run_local_lua(
        r#"
test("int form", function(t)
    local vm = provium:vm("a", "peios"):boot()
    -- syscall 39 = getpid on x86_64.
    local r = vm:syscall(39)
    t:assert(r.ret ~= nil, "missing ret")
    t:assert(r.errno ~= nil, "missing errno")
end)
"#,
    );
    assert_one_passed(&outcome);
}

#[test]
fn table_form_with_args_works() {
    let outcome = run_local_lua(
        r#"
test("table form args", function(t)
    local vm = provium:vm("a", "peios"):boot()
    local r = vm:syscall(39, {args = {}})
    t:assert(r.ret ~= nil)
end)
"#,
    );
    assert_one_passed(&outcome);
}

#[test]
fn table_form_returns_out_bufs_array() {
    let outcome = run_local_lua(
        r#"
test("out_bufs array", function(t)
    local vm = provium:vm("a", "peios"):boot()
    local r = vm:syscall(39, {args = {}, bufs = {}, ptrs = {}})
    t:assert(type(r.out_bufs) == "table",
        "out_bufs must be table, got " .. type(r.out_bufs))
end)
"#,
    );
    assert_one_passed(&outcome);
}

#[test]
fn result_field_aliases_ret() {
    // R8 #419: the `result` field is a documented alias for ret.
    let outcome = run_local_lua(
        r#"
test("result alias", function(t)
    local vm = provium:vm("a", "peios"):boot()
    local r = vm:syscall(39)
    t:assert(r.result == r.ret, "result must alias ret")
end)
"#,
    );
    assert_one_passed(&outcome);
}

#[test]
fn nr_must_be_integer() {
    let outcome = run_local_lua(
        r#"
test("nr type check", function(t)
    local vm = provium:vm("a", "peios"):boot()
    local ok, err = pcall(function() vm:syscall("not-int") end)
    t:assert(not ok)
    t:assert(tostring(err):find("nr") or tostring(err):find("int"),
        "error must mention nr/integer: " .. tostring(err))
end)
"#,
    );
    assert_one_passed(&outcome);
}

#[test]
fn missing_nr_errors() {
    let outcome = run_local_lua(
        r#"
test("missing nr", function(t)
    local vm = provium:vm("a", "peios"):boot()
    local ok, err = pcall(function() vm:syscall() end)
    t:assert(not ok)
end)
"#,
    );
    assert_one_passed(&outcome);
}

#[test]
fn syscall_with_invalid_nr_returns_negative_with_errno() {
    let outcome = run_local_lua(
        r#"
test("bad nr → ENOSYS", function(t)
    local vm = provium:vm("a", "peios"):boot()
    -- syscall 9999 is not implemented → ENOSYS.
    local r = vm:syscall(9999)
    t:assert(r.ret == -1, "invalid syscall must return -1, got " .. tostring(r.ret))
    t:assert(r.errno ~= 0, "errno must be set for failed syscall")
end)
"#,
    );
    assert_one_passed(&outcome);
}

#[test]
fn excess_int_args_truncated_to_six() {
    // Syscalls take at most 6 args (Linux SVR4 calling conv).
    // Anything past index 6 must be silently dropped.
    let outcome = run_local_lua(
        r#"
test("truncate args", function(t)
    local vm = provium:vm("a", "peios"):boot()
    -- Pass 8 args; only first 6 are forwarded.
    local r = vm:syscall(39, 1, 2, 3, 4, 5, 6, 7, 8)
    t:assert(r.ret ~= nil, "extra args must not error")
end)
"#,
    );
    assert_one_passed(&outcome);
}
