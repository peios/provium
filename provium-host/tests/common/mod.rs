//! Shared scaffolding for the conformance test suite.
//!
//! Every conformance file (`tests/conformance_*.rs`) routes Lua
//! through [`run_local_lua`] — local-agent backend, fixed
//! `peios` profile, no real QEMU. QEMU-only behaviors live in
//! their own dedicated tests that gate on `cfg(unix)` and KVM
//! presence.
//!
//! The helper here is intentionally tiny: a conformance test
//! should be a one-liner table-of-Lua. Anything more involved
//! belongs in the slice-specific test file (`bridges.rs`, etc.)
//! that already exists.

#![cfg(feature = "lua")]
#![allow(dead_code)] // each test binary uses a subset of these

use std::collections::BTreeMap;
use std::sync::Arc;

use provium_host::lua::{run_file, FileOutcome, TestStatus};
use provium_host::profile::{Config, Profile, ProviumSection};
use provium_host::vmm::local_agent::LocalAgentVmm;
use provium_host::vmm::Vmm;

/// Default `peios` profile pointing at non-existent kernel/initrd.
/// LocalAgentVmm ignores both, so the dummy paths are fine.
pub fn default_config() -> Arc<Config> {
    let mut profiles = BTreeMap::new();
    profiles.insert(
        "peios".into(),
        Profile {
            kernel: "/unused".into(),
            initrd: "/unused".into(),
            cmdline: "console=hvc0".into(),
            guest_os: "peios".into(),
            inject_agent: true,
            agent_overlay_path: None,
            cmdline_file: None,
            build: None,
            build_out: None,
        },
    );
    Arc::new(Config {
        provium: ProviumSection::default(),
        profiles,
    })
}

/// Run a Lua source string under the local-agent VMM. Writes
/// the source to a temp `*.test.lua` so the runner's
/// path-based discovery + filename-conventions all work.
pub fn run_local_lua(source: &str) -> FileOutcome {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("conformance.test.lua");
    std::fs::write(&path, source).unwrap();
    let vmm: Arc<dyn Vmm> = Arc::new(LocalAgentVmm::new());
    run_file(&path, default_config(), vmm).unwrap()
}

/// Assert every test in the outcome passed. Pretty-prints
/// any failure's message for diagnostic.
pub fn assert_all_passed(outcome: &FileOutcome) {
    for t in &outcome.tests {
        assert_eq!(
            t.status,
            TestStatus::Passed,
            "test `{}` failed: {:?}",
            t.name,
            t.message,
        );
    }
}

/// Assert exactly one test ran and it passed.
pub fn assert_one_passed(outcome: &FileOutcome) {
    assert_eq!(outcome.tests.len(), 1, "expected one test, got {:?}",
        outcome.tests.iter().map(|t| &t.name).collect::<Vec<_>>());
    assert_eq!(
        outcome.tests[0].status,
        TestStatus::Passed,
        "test failed: {:?}",
        outcome.tests[0].message,
    );
}

/// Assert exactly one test ran and it FAILED, with `needle`
/// appearing in the failure message. Used for pin-pointing
/// the exact wording of error paths.
pub fn assert_one_failed_with(outcome: &FileOutcome, needle: &str) {
    assert_eq!(outcome.tests.len(), 1, "expected one test, got {:?}",
        outcome.tests.iter().map(|t| &t.name).collect::<Vec<_>>());
    assert_eq!(
        outcome.tests[0].status,
        TestStatus::Failed,
        "expected fail, got {:?}",
        outcome.tests[0].status,
    );
    let msg = outcome.tests[0]
        .message
        .as_deref()
        .unwrap_or("<no message>");
    assert!(
        msg.contains(needle),
        "failure message `{msg}` does not contain `{needle}`",
    );
}
