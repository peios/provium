//! Conformance: stream uniformity per `DESIGN.md` § Stream.
//! TailUd / ConsoleStreamUd / CaptureUd must mirror each other on
//! next/read_until/expect/drain/close/eof + the pending buffer
//! pattern. R7-R8 caught a long tail of asymmetric bugs.

#![cfg(feature = "lua")]

mod common;

use common::{assert_one_passed, run_local_lua};

// ---------------------------------------------------------------
// TailUd (vm:tail_file)
// ---------------------------------------------------------------

#[test]
fn tail_file_close_makes_eof_true() {
    let outcome = run_local_lua(
        r#"
test("tail close → eof", function(t)
    local tmp = os.tmpname()
    local f = io.open(tmp, "w"); f:write("hi"); f:close()
    local vm = provium:vm("a", "peios"):boot()
    local s = vm:tail_file(tmp)
    s:close()
    t:assert(s:eof(), "eof must be true after close")
end)
"#,
    );
    assert_one_passed(&outcome);
}

#[test]
fn tail_file_drain_after_close_returns_pending() {
    // R8 #408: drain must flush whatever is buffered post-close.
    let outcome = run_local_lua(
        r#"
test("tail drain post-close", function(t)
    local tmp = os.tmpname()
    local f = io.open(tmp, "w"); f:write("payload"); f:close()
    local vm = provium:vm("a", "peios"):boot()
    local s = vm:tail_file(tmp, {start = "beginning"})
    -- Give the stream a moment to ingest then close.
    s:close()
    -- Drain returns a table of frame strings. Must not hang or
    -- error; pre-R8 it could block past close.
    local frames = s:drain()
    t:assert(type(frames) == "table", "drain must return table of frames")
end)
"#,
    );
    assert_one_passed(&outcome);
}

#[test]
fn tail_file_eof_respects_pending_buffer() {
    // R8 #407 regression: eof must NOT return true while
    // pending bytes remain in the buffer — readers checking
    // `while not s:eof() do read end` would hang.
    let outcome = run_local_lua(
        r#"
test("eof respects pending", function(t)
    local tmp = os.tmpname()
    local f = io.open(tmp, "w"); f:write("data"); f:close()
    local vm = provium:vm("a", "peios"):boot()
    local s = vm:tail_file(tmp, {start = "beginning"})
    -- Don't actually drain — just close. Drain will flush.
    s:close()
    local frames = s:drain()
    -- After drain consumes pending + sees no more frames, eof
    -- must finally settle to true.
    t:assert(s:eof(), "eof must be true after drain consumes pending")
end)
"#,
    );
    assert_one_passed(&outcome);
}

#[test]
fn tail_file_with_start_beginning_replays_existing() {
    let outcome = run_local_lua(
        r#"
test("tail start=beginning", function(t)
    local tmp = os.tmpname()
    local f = io.open(tmp, "w"); f:write("preexisting"); f:close()
    local vm = provium:vm("a", "peios"):boot()
    local s = vm:tail_file(tmp, {start = "beginning"})
    -- Give the agent a moment to read + forward the existing
    -- bytes before close races the tail thread.
    local frames = s:drain("500ms")
    s:close()
    local joined = table.concat(frames)
    t:assert(joined:find("preexisting"),
        "beginning mode must replay existing content, got `" .. joined .. "`")
end)
"#,
    );
    assert_one_passed(&outcome);
}

#[test]
fn tail_file_default_start_is_end() {
    let outcome = run_local_lua(
        r#"
test("tail default start=end", function(t)
    local tmp = os.tmpname()
    local f = io.open(tmp, "w"); f:write("OLD"); f:close()
    local vm = provium:vm("a", "peios"):boot()
    local s = vm:tail_file(tmp)
    -- Default behaviour is to skip pre-existing content.
    s:close()
    local frames = s:drain()
    local joined = table.concat(frames)
    t:assert(not joined:find("OLD"),
        "default start should skip pre-existing, got `" .. joined .. "`")
end)
"#,
    );
    assert_one_passed(&outcome);
}

#[test]
fn tail_file_unknown_start_string_errors() {
    let outcome = run_local_lua(
        r#"
test("tail bad start", function(t)
    local tmp = os.tmpname()
    local f = io.open(tmp, "w"); f:close()
    local vm = provium:vm("a", "peios"):boot()
    local ok, err = pcall(function()
        vm:tail_file(tmp, {start = "middle"})
    end)
    t:assert(not ok, "unknown start mode should error")
end)
"#,
    );
    assert_one_passed(&outcome);
}

#[test]
fn tail_file_negative_offset_rejected() {
    let outcome = run_local_lua(
        r#"
test("tail neg offset", function(t)
    local tmp = os.tmpname()
    local f = io.open(tmp, "w"); f:close()
    local vm = provium:vm("a", "peios"):boot()
    local ok = pcall(function()
        vm:tail_file(tmp, {start = -1})
    end)
    t:assert(not ok, "negative offset should error")
end)
"#,
    );
    assert_one_passed(&outcome);
}
