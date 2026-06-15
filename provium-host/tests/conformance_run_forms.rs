//! Conformance: vm:run argument forms per `DESIGN.md` § VM.

#![cfg(feature = "lua")]

mod common;

use common::{assert_one_passed, run_local_lua};

#[test]
fn run_one_arg_is_shell_form() {
    // vm:run("echo hi") should run via /bin/sh -c so the
    // shell pipeline / redirection / globbing work.
    let outcome = run_local_lua(
        r#"
test("shell form", function(t)
    local vm = provium:vm("a", "peios"):boot()
    -- Pipe + redirect — only works through a shell.
    local r = vm:run("printf hello | wc -c")
    t:assert(r:ok(), r.stderr)
    -- wc -c output includes a trailing newline; just check the
    -- digit is present.
    t:assert(r.stdout:find("5"), "expected '5', got " .. r.stdout)
end)
"#,
    );
    assert_one_passed(&outcome);
}

#[test]
fn run_two_arg_table_is_direct_exec() {
    // vm:run("printf", {"hello"}) should call printf directly,
    // no shell expansion.
    let outcome = run_local_lua(
        r#"
test("direct exec", function(t)
    local vm = provium:vm("a", "peios"):boot()
    local r = vm:run("printf", {"hello"})
    t:assert(r:ok(), r.stderr)
    t:assert_eq(r.stdout, "hello")
end)
"#,
    );
    assert_one_passed(&outcome);
}

#[test]
fn run_with_env_table_passes_env() {
    let outcome = run_local_lua(
        r#"
test("env table", function(t)
    local vm = provium:vm("a", "peios"):boot()
    -- printenv is unreliable across coreutils; use sh + echo.
    local r = vm:run("sh", {args={"-c", "printf %s \"$MARKER\""}, env={MARKER="hit"}})
    t:assert(r:ok(), r.stderr)
    t:assert_eq(r.stdout, "hit")
end)
"#,
    );
    assert_one_passed(&outcome);
}

#[test]
fn run_with_stdin_pipes_data_in() {
    let outcome = run_local_lua(
        r#"
test("stdin", function(t)
    local vm = provium:vm("a", "peios"):boot()
    local r = vm:run("cat", {stdin="incoming"})
    t:assert(r:ok(), r.stderr)
    t:assert_eq(r.stdout, "incoming")
end)
"#,
    );
    assert_one_passed(&outcome);
}

#[test]
fn run_with_cwd_changes_working_dir() {
    let outcome = run_local_lua(
        r#"
test("cwd", function(t)
    local dir = os.tmpname() .. ".d"
    -- Materialise the dir using the host (LocalAgent shares fs).
    os.execute("mkdir -p " .. dir)
    local vm = provium:vm("a", "peios"):boot()
    local r = vm:run("pwd", {cwd=dir})
    t:assert(r:ok(), r.stderr)
    -- Strip trailing newline.
    local out = r.stdout:gsub("%s+$", "")
    t:assert(out == dir or out:find(dir, 1, true),
        "pwd should match cwd: got " .. out .. " expected " .. dir)
end)
"#,
    );
    assert_one_passed(&outcome);
}

#[test]
fn run_with_timeout_kills_long_proc() {
    let outcome = run_local_lua(
        r#"
test("run timeout", function(t)
    local vm = provium:vm("a", "peios"):boot()
    local r = vm:run("sh", {args={"-c", "sleep 60"}, timeout_ms=100})
    -- Should report timed_out / non-ok within ~100ms, not hang
    -- for the full 60s.
    t:assert(r.timed_out or not r:ok(),
        "expected timed_out or non-ok status: " .. tostring(r.status))
end)
"#,
    );
    assert_one_passed(&outcome);
}

#[test]
fn run_first_arg_must_be_string() {
    let outcome = run_local_lua(
        r#"
test("non-string cmd", function(t)
    local vm = provium:vm("a", "peios"):boot()
    local ok = pcall(function() vm:run(42) end)
    t:assert(not ok, "non-string cmd must error")
end)
"#,
    );
    assert_one_passed(&outcome);
}

#[test]
fn run_no_args_errors_with_message() {
    let outcome = run_local_lua(
        r#"
test("no args", function(t)
    local vm = provium:vm("a", "peios"):boot()
    local ok, err = pcall(function() vm:run() end)
    t:assert(not ok)
    t:assert(tostring(err):find("command") or tostring(err):find("string"),
        "error must explain missing arg: " .. tostring(err))
end)
"#,
    );
    assert_one_passed(&outcome);
}
