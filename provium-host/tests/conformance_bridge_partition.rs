//! Conformance: `bridge:partition` family per `DESIGN.md` §
//! Bridge.

#![cfg(feature = "lua")]

mod common;

use common::{assert_one_failed_with, assert_one_passed, run_local_lua};

#[test]
fn symmetric_partition_works_on_bare_strings() {
    // Graph-state partition before attach is allowed — DESIGN
    // permits bare-string detach() as well.
    let outcome = run_local_lua(
        r#"
test("symmetric partition bare strings", function(t)
    local lan = provium:bridge("lan", {})
    lan:partition("a", "b")
    t:assert(lan:is_partitioned("a", "b"))
    t:assert(lan:is_partitioned("b", "a"), "partition is symmetric")
end)
"#,
    );
    assert_one_passed(&outcome);
}

#[test]
fn directional_partition_refuses_unattached_endpoints() {
    // R8 #399 fix: directional partition realises per-TAP rules
    // that need the TAP to exist. Calling on unattached
    // endpoints would silently no-op the rule, so we refuse.
    let outcome = run_local_lua(
        r#"
test("directional refuses unattached", function(t)
    local lan = provium:bridge("lan", {})
    local ok, err = pcall(function()
        lan:partition({from="ghost", to="phantom"})
    end)
    t:assert(not ok, "directional partition on unattached must error")
    t:assert(tostring(err):find("not attached") or tostring(err):find("attach"),
        "error must mention attachment: " .. tostring(err))
end)
"#,
    );
    assert_one_passed(&outcome);
}

#[test]
fn directional_partition_works_when_endpoints_attached() {
    let outcome = run_local_lua(
        r#"
test("directional with attached", function(t)
    local lan = provium:bridge("lan", {})
    lan:attach({"a", "b"})
    -- Should not raise.
    lan:partition({from="a", to="b"})
end)
"#,
    );
    assert_one_passed(&outcome);
}

#[test]
fn partition_all_blocks_every_pair() {
    let outcome = run_local_lua(
        r#"
test("partition all", function(t)
    local lan = provium:bridge("lan", {})
    lan:partition_all()
    t:assert(lan:is_partitioned("anything", "anywhere"))
    t:assert(lan:is_partitioned("x", "y"))
end)
"#,
    );
    assert_one_passed(&outcome);
}

#[test]
fn restore_all_only_undoes_partition_all() {
    // DESIGN: "restore_all only undoes partition_all; explicit
    // pair partitions and isolation survive (use bridge:reset
    // for impairments+partitions, or bridge:unisolate(vm) for
    // isolation)".
    let outcome = run_local_lua(
        r#"
test("restore_all preserves explicit partitions", function(t)
    local lan = provium:bridge("lan", {})
    lan:partition("a", "b")
    lan:partition_all()
    lan:restore_all()
    -- Explicit pair partition must survive restore_all.
    t:assert(lan:is_partitioned("a", "b"),
        "restore_all wiped explicit partition")
    -- The blanket fully_partitioned should be gone.
    t:assert(not lan:is_partitioned("c", "d"),
        "restore_all left fully_partitioned set")
end)
"#,
    );
    assert_one_passed(&outcome);
}

#[test]
fn reset_clears_everything() {
    let outcome = run_local_lua(
        r#"
test("bridge:reset", function(t)
    local lan = provium:bridge("lan", {})
    lan:partition("a", "b")
    lan:add_latency(50)
    lan:reset()
    t:assert(not lan:is_partitioned("a", "b"))
    t:assert(lan:latency_ms() == 0, "latency not cleared by reset")
end)
"#,
    );
    assert_one_passed(&outcome);
}

#[test]
fn unpartition_reverses_partition() {
    let outcome = run_local_lua(
        r#"
test("unpartition", function(t)
    local lan = provium:bridge("lan", {})
    lan:partition("a", "b")
    lan:unpartition("a", "b")
    t:assert(not lan:is_partitioned("a", "b"))
end)
"#,
    );
    assert_one_passed(&outcome);
}

#[test]
fn isolate_blocks_pairs_with_other_members() {
    let outcome = run_local_lua(
        r#"
test("isolate", function(t)
    local lan = provium:bridge("lan", {})
    lan:attach({"a", "b", "c"})
    lan:isolate("a")
    -- Isolation is its own dimension — surfaced via
    -- is_isolated, distinct from is_partitioned which only
    -- reports symmetric pair / fully_partitioned state.
    t:assert(lan:is_isolated("a"))
    t:assert(not lan:is_isolated("b"))
end)
"#,
    );
    assert_one_passed(&outcome);
}
