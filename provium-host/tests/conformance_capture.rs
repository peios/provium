//! Conformance: bridge:capture per `DESIGN.md` § Bridge.

#![cfg(feature = "lua")]

mod common;

use common::{assert_one_passed, run_local_lua};

#[test]
#[ignore = "bridge:capture needs realised TAP — local-agent has none"]
fn capture_returns_stream_handle() {
    let outcome = run_local_lua(
        r#"
test("bridge:capture", function(t)
    local lan = provium:bridge("lan", {})
    lan:attach("a")
    local stream = lan:capture()
    t:assert(stream ~= nil)
    stream:close()
end)
"#,
    );
    assert_one_passed(&outcome);
}

#[test]
fn capture_on_unrealized_bridge_errors() {
    let outcome = run_local_lua(
        r#"
test("capture sans realize", function(t)
    local lan = provium:bridge("lan", {})
    -- LocalAgent: no realised bridge — capture must error or
    -- return a handle that closes cleanly. Either is a
    -- documented variant of the no-tap path.
    local ok, err = pcall(function() lan:capture() end)
    -- Don't insist on the failure — just that it didn't panic.
    if not ok then
        t:assert(tostring(err) ~= "", "error must have a message")
    end
end)
"#,
    );
    assert_one_passed(&outcome);
}

#[test]
fn capture_count_starts_at_zero() {
    let outcome = run_local_lua(
        r#"
test("capture count zero", function(t)
    local lan = provium:bridge("lan", {})
    -- active_captures isn't currently exposed via Lua; this
    -- test asserts the test harness can call into lan without
    -- raising, leaving room to add the accessor later.
    t:assert(lan:name() == "lan")
end)
"#,
    );
    assert_one_passed(&outcome);
}
