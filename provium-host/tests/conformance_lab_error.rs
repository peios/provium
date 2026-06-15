//! Conformance: LabError Display per `provium_host::lab::LabError`.

use std::collections::BTreeMap;

use provium_host::lab::LabError;

#[test]
fn duplicate_vm_name_includes_name() {
    let e = LabError::DuplicateVmName("dupe".into());
    let s = e.to_string();
    assert!(s.contains("dupe"));
    assert!(s.contains("already used"));
}

#[test]
fn unknown_vm_includes_name() {
    let e = LabError::UnknownVm("ghost".into());
    let s = e.to_string();
    assert!(s.contains("ghost"));
    assert!(s.contains("no vm named"));
}

#[test]
fn unknown_profile_includes_name() {
    let e = LabError::UnknownProfile("missing".into());
    let s = e.to_string();
    assert!(s.contains("missing"));
    assert!(s.contains("provium.toml"),
        "error must point at provium.toml: {s}");
}

#[test]
fn duplicate_bridge_name_includes_name() {
    let e = LabError::DuplicateBridgeName("lan".into());
    let s = e.to_string();
    assert!(s.contains("lan"));
}

#[test]
fn unknown_bridge_includes_name() {
    let e = LabError::UnknownBridge("dmz".into());
    let s = e.to_string();
    assert!(s.contains("dmz"));
}

#[test]
fn restore_failed_carries_vm_and_reason() {
    let e = LabError::RestoreFailed("dc1/web".into(), "decompress error".into());
    let s = e.to_string();
    assert!(s.contains("dc1/web"));
    assert!(s.contains("decompress error"));
}

#[test]
fn claim_already_held_says_one_shot() {
    let e = LabError::ClaimAlreadyHeld;
    let s = e.to_string();
    assert!(s.contains("one-shot"),
        "ClaimAlreadyHeld must mention one-shot: {s}");
}

#[test]
fn claim_exceeds_budget_says_pool_budget() {
    let e = LabError::ClaimExceedsBudget;
    let s = e.to_string();
    assert!(s.contains("pool"));
    assert!(s.contains("budget"));
}

#[test]
fn reserved_name_lists_the_reserved_keys() {
    let e = LabError::ReservedName("pack".into());
    let s = e.to_string();
    assert!(s.contains("pack"));
    assert!(s.contains("vm_fixture"));
    assert!(s.contains("lab_fixture"));
    assert!(s.contains("unpack"));
    assert!(s.contains("reserved"));
}

#[test]
fn batch_failed_includes_action_and_failures() {
    let mut failures = BTreeMap::new();
    failures.insert("vm1".to_string(), "out of memory".to_string());
    failures.insert("vm2".to_string(), "kernel missing".to_string());
    let e = LabError::BatchFailed {
        action: "boot",
        failures,
    };
    let s = e.to_string();
    assert!(s.contains("boot"), "must name action: {s}");
    assert!(s.contains("vm1"), "must name failed members: {s}");
    assert!(s.contains("vm2"));
}

#[test]
fn already_booted_message_present() {
    // R8 #414 made boot idempotent so this isn't emitted any
    // more, but the variant exists for source compat. Pin its
    // message so downstream matchers don't break.
    let e = LabError::AlreadyBooted;
    let s = e.to_string();
    assert!(s.contains("already booted"));
}
