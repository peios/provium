//! Conformance: vm:batch per `DESIGN.md` § VM. Collect ops in
//! `fn(b)`, ship one round-trip, get array of results.

#![cfg(feature = "lua")]

mod common;

use common::{assert_one_passed, run_local_lua};

#[test]
fn batch_returns_array_of_results() {
    let outcome = run_local_lua(
        r#"
test("batch shape", function(t)
    local tmp = os.tmpname()
    local f = io.open(tmp, "w"); f:write("data"); f:close()
    local vm = provium:vm("a", "peios"):boot()
    local results = vm:batch(function(b)
        b:read_file(tmp)
        b:read_file(tmp)
    end)
    t:assert(type(results) == "table")
    t:assert_eq(#results, 2, "batch must return one entry per op")
end)
"#,
    );
    assert_one_passed(&outcome);
}

#[test]
fn batch_read_file_returns_string() {
    let outcome = run_local_lua(
        r#"
test("batch read_file", function(t)
    local tmp = os.tmpname()
    local f = io.open(tmp, "w"); f:write("payload"); f:close()
    local vm = provium:vm("a", "peios"):boot()
    local results = vm:batch(function(b)
        b:read_file(tmp)
    end)
    t:assert_eq(results[1].ok, "payload")
end)
"#,
    );
    assert_one_passed(&outcome);
}

#[test]
fn batch_stat_returns_table() {
    let outcome = run_local_lua(
        r#"
test("batch stat", function(t)
    local tmp = os.tmpname()
    io.open(tmp, "w"):close()
    local vm = provium:vm("a", "peios"):boot()
    local results = vm:batch(function(b)
        b:stat(tmp)
    end)
    t:assert(type(results[1]) == "table")
    t:assert(results[1].ok ~= nil, "stat must succeed: " .. tostring(results[1].err))
    t:assert(results[1].ok.size ~= nil)
end)
"#,
    );
    assert_one_passed(&outcome);
}

#[test]
fn batch_mixed_ops_preserve_order() {
    let outcome = run_local_lua(
        r#"
test("batch order", function(t)
    local a = os.tmpname()
    local b = os.tmpname()
    local fa = io.open(a, "w"); fa:write("AAA"); fa:close()
    local fb = io.open(b, "w"); fb:write("BBB"); fb:close()
    local vm = provium:vm("a", "peios"):boot()
    local results = vm:batch(function(builder)
        builder:read_file(a)
        builder:read_file(b)
    end)
    t:assert_eq(results[1].ok, "AAA")
    t:assert_eq(results[2].ok, "BBB")
end)
"#,
    );
    assert_one_passed(&outcome);
}

#[test]
fn empty_batch_returns_empty_array() {
    let outcome = run_local_lua(
        r#"
test("empty batch", function(t)
    local vm = provium:vm("a", "peios"):boot()
    local results = vm:batch(function(b) end)
    t:assert(type(results) == "table")
    t:assert_eq(#results, 0)
end)
"#,
    );
    assert_one_passed(&outcome);
}
