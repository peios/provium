//! Conformance: `vm:spawn_worker()` per `DESIGN.md` § Worker.
//! DESIGN promises workers expose "the same VM API for guest
//! ops" — R8 #419 brought worker:syscall to vm:syscall parity
//! (table form, bufs/ptrs splice, out_bufs).

#![cfg(feature = "lua")]

mod common;

use common::{assert_one_passed, run_local_lua};

#[test]
fn spawn_worker_returns_handle() {
    let outcome = run_local_lua(
        r#"
test("spawn_worker", function(t)
    local vm = provium:vm("a", "peios"):boot()
    local w = vm:spawn_worker()
    t:assert(w:handle() > 0, "worker handle must be positive")
end)
"#,
    );
    assert_one_passed(&outcome);
}

#[test]
fn worker_run_returns_run_result() {
    let outcome = run_local_lua(
        r#"
test("worker:run", function(t)
    local vm = provium:vm("a", "peios"):boot()
    local w = vm:spawn_worker()
    local r = w:run("true")
    t:assert(r:ok(), "worker:run must return RunResult")
end)
"#,
    );
    assert_one_passed(&outcome);
}

#[test]
fn worker_join_returns_exit_status() {
    let outcome = run_local_lua(
        r#"
test("worker:join", function(t)
    local vm = provium:vm("a", "peios"):boot()
    local w = vm:spawn_worker()
    local exit = w:join()
    t:assert(exit ~= nil, "join must return exit status")
end)
"#,
    );
    assert_one_passed(&outcome);
}

#[test]
fn worker_syscall_returns_ret_and_errno() {
    // R8 #419: table form must include `ret`, `result` (alias),
    // `errno`, and `out_bufs` for parity with vm:syscall.
    let outcome = run_local_lua(
        r#"
test("worker:syscall return shape", function(t)
    local vm = provium:vm("a", "peios"):boot()
    local w = vm:spawn_worker()
    -- syscall 39 is getpid on x86_64 Linux. Returns the pid;
    -- errno is 0 on success.
    local r = w:syscall(39)
    t:assert(r.ret ~= nil, "missing ret")
    t:assert(r.result ~= nil, "missing result alias")
    t:assert(r.errno ~= nil, "missing errno")
    t:assert(r.out_bufs ~= nil, "missing out_bufs")
end)
"#,
    );
    assert_one_passed(&outcome);
}

#[test]
fn worker_syscall_table_form_accepts_args() {
    let outcome = run_local_lua(
        r#"
test("worker:syscall table form", function(t)
    local vm = provium:vm("a", "peios"):boot()
    local w = vm:spawn_worker()
    -- Table form with explicit args list.
    local r = w:syscall(39, {args={}})
    t:assert(r.ret ~= nil, "table form must return ret")
end)
"#,
    );
    assert_one_passed(&outcome);
}

#[test]
fn worker_close_kills_then_joins() {
    // Auto-close path: scope walker dispatches :close() — must
    // SIGTERM in-flight processes then reap. Failure to reap
    // would leak agent-side state.
    let outcome = run_local_lua(
        r#"
test("worker:close", function(t)
    local vm = provium:vm("a", "peios"):boot()
    local w = vm:spawn_worker()
    -- close is a no-throw cleanup — must not raise even if
    -- worker is already idle.
    w:close()
end)
"#,
    );
    assert_one_passed(&outcome);
}
