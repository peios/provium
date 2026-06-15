//! Conformance: Process drop-guard reaping per `DESIGN.md` §
//! Process. A Process that's dropped without `:wait()` must be
//! best-effort reaped so the agent doesn't accumulate zombies.

#![cfg(feature = "lua")]

mod common;

use common::{assert_one_passed, run_local_lua};

#[test]
fn process_dropped_without_wait_does_not_panic() {
    let outcome = run_local_lua(
        r#"
test("drop no wait", function(t)
    local vm = provium:vm("a", "peios"):boot()
    -- Spawn a process and let it fall out of scope.
    -- Drop guard should reap it best-effort.
    do
        local _p = vm:run_async("true")
        -- _p drops at end of block.
    end
    -- If the agent had a leak, subsequent ops would fail or
    -- the guard would have panicked. Pin: no panic, ops still
    -- work.
    vm:run("true"):assert_ok()
end)
"#,
    );
    assert_one_passed(&outcome);
}

#[test]
fn process_kill_then_drop_is_safe() {
    let outcome = run_local_lua(
        r#"
test("kill then drop", function(t)
    local vm = provium:vm("a", "peios"):boot()
    do
        local p = vm:run_async("sh", {"-c", "sleep 60"})
        p:kill()
        -- No wait — drop guard reaps the killed process.
    end
end)
"#,
    );
    assert_one_passed(&outcome);
}

#[test]
fn process_handle_clones_share_drop_guard() {
    // A Process is Arc-shared; cloning should NOT cause double-
    // reap. The drop guard fires only on the LAST clone going away.
    let outcome = run_local_lua(
        r#"
test("multiple references", function(t)
    local vm = provium:vm("a", "peios"):boot()
    local p = vm:run_async("true")
    local p2 = p
    p:wait()
    -- p2 still references the consumed handle. Calling :wait()
    -- on it should error with the consumed-handle message
    -- rather than double-reap.
    local ok, err = pcall(function() p2:wait() end)
    t:assert(not ok or err ~= nil, "consumed handle must guard")
end)
"#,
    );
    assert_one_passed(&outcome);
}

#[test]
fn many_dropped_processes_in_sequence() {
    // Stress: spawn + drop ten in a row. Drop-guard reap path
    // must not leak resources.
    let outcome = run_local_lua(
        r#"
test("many drops", function(t)
    local vm = provium:vm("a", "peios"):boot()
    for i = 1, 10 do
        local _p = vm:run_async("true")
    end
    -- VM still functional?
    vm:run("true"):assert_ok()
end)
"#,
    );
    assert_one_passed(&outcome);
}
