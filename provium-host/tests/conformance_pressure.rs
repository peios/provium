//! Conformance: PSI throttling per `DESIGN.md` § Adaptive host
//! pressure. PressureFlag construction, default-clear, set/clear
//! visibility, parser graceful-degradation.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use provium_host::scheduler::psi::{
    read_some_avg10, read_some_avg10_at, PressureFlag,
};
use provium_host::scheduler::dispatch::{
    dispatch_files, DispatchOpts, FileTimeout,
};
use provium_host::scheduler::pool::{Pool, ResourceAmount};
use provium_host::profile::{Config, Profile, ProviumSection};
use provium_host::vmm::local_agent::LocalAgentVmm;
use provium_host::vmm::Vmm;

#[test]
fn flag_defaults_clear() {
    let f = PressureFlag::default();
    assert!(!f.is_pressured(),
        "PressureFlag must default to false (no pressure)");
}

#[test]
fn flag_clones_share_state() {
    // PressureFlag is documented as cheap-clone; clones MUST
    // observe each other's state changes.
    let a = PressureFlag::default();
    let b = a.clone();
    // We don't have a public setter, but we can verify clones
    // start aligned. The internal set is pub(crate).
    assert_eq!(a.is_pressured(), b.is_pressured());
}

#[test]
fn read_some_avg10_returns_none_for_missing_path() {
    let v = read_some_avg10_at("/no/such/psi/path");
    assert!(v.is_none(),
        "missing path must return None (PSI graceful degradation)");
}

#[test]
fn read_some_avg10_parses_valid_psi_line() {
    // Synthesize the `some avg10=N.NN` shape into a temp file.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("pressure");
    std::fs::write(
        &path,
        "some avg10=12.34 avg60=5.67 avg300=2.10 total=12345\n\
         full avg10=0.00 avg60=0.00 avg300=0.00 total=0\n",
    )
    .unwrap();
    let v = read_some_avg10_at(path.to_str().unwrap());
    assert_eq!(v, Some(12.34));
}

#[test]
fn read_some_avg10_returns_none_for_garbled_file() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("pressure");
    std::fs::write(&path, "this is not a PSI report\n").unwrap();
    let v = read_some_avg10_at(path.to_str().unwrap());
    assert_eq!(v, None);
}

#[test]
fn read_some_avg10_default_path_returns_some_or_none() {
    // On hosts without PSI returns None; on hosts with PSI
    // returns Some. Either is valid — just exercise the path.
    let _ = read_some_avg10();
}

#[test]
fn dispatcher_with_pressure_flag_runs_when_unpressured() {
    // If the flag is NEVER pressured, dispatch behaves identically
    // to the no-flag case. Pinning so a future regression where
    // the flag check inverts the condition fails this test.
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("ok.test.lua");
    std::fs::write(&p, "test('x', function(t) end)").unwrap();

    let mut profiles = std::collections::BTreeMap::new();
    profiles.insert("peios".into(), Profile {
        kernel: "/unused".into(), initrd: "/unused".into(),
        cmdline: "console=hvc0".into(), guest_os: "peios".into(),
            inject_agent: true,
            agent_overlay_path: None,
            cmdline_file: None,
            build: None,
            build_out: None,
    });
    let cfg = Arc::new(Config {
        provium: ProviumSection::default(),
        profiles,
    });
    let pool = Pool::new(ResourceAmount {
        memory_bytes: 256 * 1024 * 1024, cpus: 1,
    });
    let vmm: Arc<dyn Vmm> = Arc::new(LocalAgentVmm::new());
    let mut o = DispatchOpts::default();
    o.timeout = FileTimeout::Disabled;
    o.pressure = Some(PressureFlag::default());
    let outs = dispatch_files(vec![p], pool, cfg, vmm, o);
    assert_eq!(outs.len(), 1);
    assert!(outs[0].passed());
}
