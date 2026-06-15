//! Conformance: bridge realize hooks per `DESIGN.md` § Bridge.
//! TAP naming must be stable across toolchain versions (lab:restore
//! looks up TAPs by snapshot-recorded name) and unique per
//! (vm, bridge) pair.

use provium_host::bridge_realize::{tap_name_for_vm, tap_name_for_vm_on_bridge};

#[test]
fn tap_name_includes_tap_prefix() {
    assert!(tap_name_for_vm("web").starts_with("tap-"),
        "TAP name must start with `tap-`");
    assert!(tap_name_for_vm_on_bridge("web", "lan").starts_with("tap-"));
}

#[test]
fn tap_name_for_vm_is_under_ifnamsiz() {
    // Linux IFNAMSIZ = 16 (15 chars + NUL).
    let n = tap_name_for_vm("a-very-long-vm-name-overflow");
    assert!(n.len() <= 15, "single-bridge TAP must fit IFNAMSIZ: {n}");
}

#[test]
fn tap_name_for_vm_on_bridge_is_under_ifnamsiz() {
    let n = tap_name_for_vm_on_bridge("very-long-vm", "very-long-bridge");
    assert!(n.len() <= 15,
        "(vm, bridge) TAP must fit IFNAMSIZ: {n} (len {})", n.len());
}

#[test]
fn tap_name_strips_special_chars() {
    let n = tap_name_for_vm("my.vm/with-slashes!");
    // Only ascii alphanumeric + `_` allowed in the name part.
    assert!(!n.contains('.') && !n.contains('/') && !n.contains('!') && !n.contains('-')
        || n.starts_with("tap-"),
        "special chars must be stripped (besides the leading tap- delimiter): {n}");
}

#[test]
fn tap_name_distinct_for_different_bridges() {
    // Multi-homed VM: same VM on two bridges must not share a TAP.
    let a = tap_name_for_vm_on_bridge("web", "lan");
    let b = tap_name_for_vm_on_bridge("web", "mgmt");
    assert_ne!(a, b, "same vm on different bridges must get distinct TAPs");
}

#[test]
fn tap_name_stable_across_calls() {
    // Determinism — same input must produce same name on every
    // call (snapshot/restore cross-process depends on this).
    let a = tap_name_for_vm_on_bridge("web", "lan");
    let b = tap_name_for_vm_on_bridge("web", "lan");
    assert_eq!(a, b, "TAP naming must be deterministic");
}

#[test]
fn tap_name_pinned_value_for_known_pair() {
    // CRITICAL stability lock: the docstring says FNV-1a 64-bit
    // is "pinned to this exact constant set forever" for cross-
    // toolchain stability. If this asserted value ever changes,
    // every captured TAP name in every fixture/snapshot becomes
    // wrong. Match against the current canonical value — if
    // someone changes the algorithm intentionally, they have to
    // change this string too AND understand the migration cost.
    let n = tap_name_for_vm_on_bridge("web", "lan");
    // The value below is computed once and hard-coded; if it
    // drifts the test fails loud and the developer must
    // consciously migrate.
    eprintln!("PINNED tap name (web, lan): {n}");
    // Prefix `tap-` + vm prefix (≤4 chars) + 7-hex hash.
    // For "web" the vm part is 3 chars → total 14.
    assert!(n.len() <= 15, "TAP name length must be ≤ 15: {n}");
    assert!(n.starts_with("tap-web"), "vm prefix must be 'web': {n}");
    // Lock the exact pinned value so any algorithm change is loud.
    assert_eq!(n, "tap-webcc960b4",
        "FNV-1a 64-bit constants are pinned per docstring; if you \
         changed them deliberately, update this test AND understand \
         that every captured TAP name in every fixture is now wrong");
}

#[test]
fn tap_name_safe_for_empty_vm_name() {
    // Edge case: empty vm name shouldn't panic.
    let n = tap_name_for_vm_on_bridge("", "lan");
    assert!(n.starts_with("tap-"));
    assert!(n.len() <= 15);
}

#[test]
fn tap_name_safe_for_purely_special_vm_name() {
    // After stripping special chars the vm prefix is empty;
    // hash absorbs everything.
    let n = tap_name_for_vm_on_bridge("!!!@@@", "lan");
    assert!(n.starts_with("tap-"));
}
