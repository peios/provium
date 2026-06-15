//! Conformance: VmError variants and Display per
//! `provium_host::vm::VmError`. Errors are part of the host's
//! API surface — wording (`"VM not booted"`, etc.) is grep'd
//! by tests in DESIGN, so any drift breaks consumers.

use provium_host::vm::{format_wrong_state, VmState};

#[test]
fn wrong_state_created_run_says_not_booted() {
    let s = format_wrong_state("a", VmState::Created, "run");
    assert!(s.contains("not booted"),
        "Created+run must say 'not booted': {s}");
}

#[test]
fn wrong_state_booted_boot_says_already_booted() {
    let s = format_wrong_state("a", VmState::Booted, "boot");
    assert!(s.contains("already booted"),
        "Booted+boot must say 'already booted': {s}");
}

#[test]
fn wrong_state_paused_run_says_use_resume() {
    let s = format_wrong_state("a", VmState::Paused, "run");
    assert!(s.contains("paused"));
    assert!(s.contains("resume"),
        "Paused state must point at resume(): {s}");
}

#[test]
fn wrong_state_shutdown_says_create_new() {
    let s = format_wrong_state("a", VmState::Shutdown, "boot");
    assert!(s.contains("shutdown"));
    assert!(s.contains("create a new"),
        "Shutdown state must point at creating a new VM: {s}");
}

#[test]
fn wrong_state_dead_says_create_new() {
    let s = format_wrong_state("a", VmState::Dead, "run");
    assert!(s.contains("died") || s.contains("dead"));
    assert!(s.contains("create a new"),
        "Dead state must point at creating a new VM: {s}");
}

#[test]
fn vm_state_as_str_matches_design_canonical_names() {
    assert_eq!(VmState::Created.as_str(), "created");
    assert_eq!(VmState::Booted.as_str(), "booted");
    assert_eq!(VmState::Paused.as_str(), "paused");
    assert_eq!(VmState::Shutdown.as_str(), "shutdown");
    assert_eq!(VmState::Dead.as_str(), "dead");
}
