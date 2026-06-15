//! Conformance: NIC handle per `DESIGN.md` § NIC. NIC is a
//! per-(bridge, vm) handle returned by `bridge:nic(vm)`.

#![cfg(feature = "lua")]

mod common;

use common::{assert_one_passed, run_local_lua};

#[test]
fn nic_carries_vm_and_bridge_name() {
    let outcome = run_local_lua(
        r#"
test("nic identity", function(t)
    local lan = provium:bridge("lan", {})
    lan:attach("a")
    local nic = lan:nic("a")
    t:assert_eq(nic:vm_name(), "a")
    t:assert_eq(nic:bridge(), "lan")
end)
"#,
    );
    assert_one_passed(&outcome);
}

#[test]
fn nic_counters_returns_table_with_required_keys() {
    // DESIGN: counters returns rx_bytes, tx_bytes, rx_packets,
    // tx_packets, errors. Local-agent backend reports zeros
    // (no real TAP) but the keys must be present.
    let outcome = run_local_lua(
        r#"
test("nic counters keys", function(t)
    local lan = provium:bridge("lan", {})
    lan:attach("a")
    local nic = lan:nic("a")
    local c = nic:counters()
    t:assert(c.rx_bytes ~= nil)
    t:assert(c.tx_bytes ~= nil)
    t:assert(c.rx_packets ~= nil)
    t:assert(c.tx_packets ~= nil)
    t:assert(c.errors ~= nil)
end)
"#,
    );
    assert_one_passed(&outcome);
}

#[test]
#[ignore = "nic:capture needs a realised TAP — local-agent has none"]
fn nic_capture_requires_realized_tap() {
    let outcome = run_local_lua(
        r#"
test("nic:capture sans tap", function(t)
    local lan = provium:bridge("lan", {})
    lan:attach("a")
    local nic = lan:nic("a")
    local ok, err = pcall(function() nic:capture() end)
    t:assert(not ok)
    t:assert(tostring(err):find("TAP") or tostring(err):find("booted"),
        "error must mention TAP/booted: " .. tostring(err))
end)
"#,
    );
    assert_one_passed(&outcome);
}

#[test]
fn nic_disconnect_does_not_panic_on_unbound_nic() {
    let outcome = run_local_lua(
        r#"
test("disconnect bare nic", function(t)
    local lan = provium:bridge("lan", {})
    lan:attach("a")
    local nic = lan:nic("a")
    -- No VM userdata bound — disconnect is graph-state.
    nic:disconnect()
end)
"#,
    );
    assert_one_passed(&outcome);
}
