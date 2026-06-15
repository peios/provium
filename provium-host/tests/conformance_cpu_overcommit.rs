//! Conformance: --cpu-overcommit clamping per `DESIGN.md` §
//! Scheduler / Pool. Default 1.0 (strict); accepts ≥0.5
//! oversubscription up to 8.0; out-of-range clamps with notice.

use std::path::PathBuf;
use std::process::Command;

fn provium_bin() -> PathBuf {
    let exe = std::env::current_exe().unwrap();
    let mut p = exe;
    p.pop();
    p.pop();
    p.push("provium");
    p
}

fn cfg_dir() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("provium.toml"), "").unwrap();
    std::fs::write(
        dir.path().join("dummy.test.lua"),
        "test('x', function(t) end)",
    ).unwrap();
    dir
}

#[test]
fn overcommit_below_half_clamps_with_notice() {
    let dir = cfg_dir();
    let cfg = dir.path().join("provium.toml");
    let out = Command::new(provium_bin())
        .arg("--no-preflight")
        .arg("--config").arg(&cfg)
        .arg("--cpu-overcommit").arg("0.1")
        .arg(dir.path())
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("clamped"),
        "below-range overcommit must print clamp notice: `{stderr}`");
}

#[test]
fn overcommit_above_eight_clamps_with_notice() {
    let dir = cfg_dir();
    let cfg = dir.path().join("provium.toml");
    let out = Command::new(provium_bin())
        .arg("--no-preflight")
        .arg("--config").arg(&cfg)
        .arg("--cpu-overcommit").arg("100")
        .arg(dir.path())
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("clamped"),
        "above-range overcommit must clamp with notice: `{stderr}`");
}

#[test]
fn overcommit_in_range_no_clamp_notice() {
    let dir = cfg_dir();
    let cfg = dir.path().join("provium.toml");
    let out = Command::new(provium_bin())
        .arg("--no-preflight")
        .arg("--config").arg(&cfg)
        .arg("--cpu-overcommit").arg("1.5")
        .arg(dir.path())
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(!stderr.contains("clamped"),
        "in-range overcommit must NOT print clamp notice: `{stderr}`");
}

#[test]
fn overcommit_default_does_not_emit_clamp_notice() {
    let dir = cfg_dir();
    let cfg = dir.path().join("provium.toml");
    let out = Command::new(provium_bin())
        .arg("--no-preflight")
        .arg("--config").arg(&cfg)
        .arg(dir.path())
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(!stderr.contains("clamped"),
        "default overcommit must NOT clamp: `{stderr}`");
}
