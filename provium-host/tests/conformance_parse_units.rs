//! Conformance: --mem size parsing per `DESIGN.md` § Pool. The
//! parser accepts integer bytes or `K`/`M`/`G`/`T` suffixes;
//! invalid values fail at CLI parse time.
//!
//! Tests invoke the binary as a subprocess so the same parse
//! path consumed by users is exercised.

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
    // Provide a test file so the binary reaches pool setup
    // (where --mem is parsed). Without this, no-files exits
    // 0 before --mem is touched.
    std::fs::write(
        dir.path().join("dummy.test.lua"),
        "test('x', function(t) end)",
    ).unwrap();
    dir
}

#[test]
fn invalid_mem_value_errors_cleanly() {
    let dir = cfg_dir();
    let cfg = dir.path().join("provium.toml");
    let out = Command::new(provium_bin())
        .arg("--no-preflight")
        .arg("--config").arg(&cfg)
        .arg("--mem").arg("not-a-size")
        .arg(dir.path())
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success(), "bad --mem must fail");
    assert!(stderr.contains("size") || stderr.contains("parse"),
        "stderr must explain bad size: `{stderr}`");
}

#[test]
fn mem_with_g_suffix_accepted() {
    let dir = cfg_dir();
    let cfg = dir.path().join("provium.toml");
    let out = Command::new(provium_bin())
        .arg("--no-preflight")
        .arg("--config").arg(&cfg)
        .arg("--mem").arg("4G")
        .arg(dir.path())
        .output()
        .unwrap();
    // No test files → exit clean. Failure here means parse
    // rejected `4G`.
    assert!(out.status.success(), "`--mem 4G` must parse: stderr=`{}`",
        String::from_utf8_lossy(&out.stderr));
}

#[test]
fn mem_with_m_suffix_accepted() {
    let dir = cfg_dir();
    let cfg = dir.path().join("provium.toml");
    let out = Command::new(provium_bin())
        .arg("--no-preflight")
        .arg("--config").arg(&cfg)
        .arg("--mem").arg("512M")
        .arg(dir.path())
        .output()
        .unwrap();
    assert!(out.status.success(), "`--mem 512M` must parse");
}

#[test]
fn mem_lowercase_suffix_accepted() {
    let dir = cfg_dir();
    let cfg = dir.path().join("provium.toml");
    let out = Command::new(provium_bin())
        .arg("--no-preflight")
        .arg("--config").arg(&cfg)
        .arg("--mem").arg("4g")
        .arg(dir.path())
        .output()
        .unwrap();
    assert!(out.status.success(), "lowercase `g` must parse like `G`");
}

#[test]
fn mem_plain_integer_accepted_as_bytes() {
    let dir = cfg_dir();
    let cfg = dir.path().join("provium.toml");
    let out = Command::new(provium_bin())
        .arg("--no-preflight")
        .arg("--config").arg(&cfg)
        .arg("--mem").arg("536870912")  // 512 MiB
        .arg(dir.path())
        .output()
        .unwrap();
    assert!(out.status.success(), "plain integer must parse as bytes");
}

#[test]
fn mem_overflow_fails_cleanly() {
    let dir = cfg_dir();
    let cfg = dir.path().join("provium.toml");
    let out = Command::new(provium_bin())
        .arg("--no-preflight")
        .arg("--config").arg(&cfg)
        .arg("--mem").arg("16777216T")  // 16 EiB × 1024 = overflow
        .arg(dir.path())
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success(), "overflow must fail");
    assert!(stderr.contains("overflow") || stderr.contains("u64")
        || stderr.contains("size"),
        "stderr must explain overflow: `{stderr}`");
}
