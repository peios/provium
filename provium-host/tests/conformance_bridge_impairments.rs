//! Conformance: bridge impairments per `DESIGN.md` § Bridge.

#![cfg(feature = "lua")]

mod common;

use common::{assert_one_passed, run_local_lua};

#[test]
fn add_latency_records_value() {
    let outcome = run_local_lua(
        r#"
test("add_latency", function(t)
    local lan = provium:bridge("lan", {})
    lan:add_latency(50)
    t:assert_eq(lan:latency_ms(), 50)
end)
"#,
    );
    assert_one_passed(&outcome);
}

#[test]
fn drop_rate_records_value() {
    let outcome = run_local_lua(
        r#"
test("drop_rate", function(t)
    local lan = provium:bridge("lan", {})
    lan:drop_rate(10)
    t:assert_eq(lan:drop_rate_pct(), 10)
end)
"#,
    );
    assert_one_passed(&outcome);
}

#[test]
fn drop_rate_clamps_above_100() {
    let outcome = run_local_lua(
        r#"
test("drop_rate clamp", function(t)
    local lan = provium:bridge("lan", {})
    lan:drop_rate(150)
    t:assert(lan:drop_rate_pct() <= 100, "drop rate must clamp to 100")
end)
"#,
    );
    assert_one_passed(&outcome);
}

#[test]
fn bandwidth_limit_records_value() {
    let outcome = run_local_lua(
        r#"
test("bandwidth_limit", function(t)
    local lan = provium:bridge("lan", {})
    lan:bandwidth_limit(1000000)
    t:assert_eq(lan:bandwidth_bps(), 1000000)
end)
"#,
    );
    assert_one_passed(&outcome);
}

#[test]
fn reset_clears_impairments_keeps_topology() {
    // DESIGN: bridge:reset() clears impairments + partitions,
    // preserves routes/uplink/isolation.
    let outcome = run_local_lua(
        r#"
test("reset scope", function(t)
    local lan = provium:bridge("lan", {})
    lan:add_latency(20)
    lan:bandwidth_limit(50000)
    lan:reset()
    t:assert_eq(lan:latency_ms(), 0)
    t:assert_eq(lan:bandwidth_bps(), 0)
end)
"#,
    );
    assert_one_passed(&outcome);
}
