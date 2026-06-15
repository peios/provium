//! Conformance: CLI behavior per `DESIGN.md` § Host: file runner.
//! Each test invokes the `provium` binary as a subprocess so the
//! flag handling, exit codes, and stderr/stdout shapes are
//! tested through the actual entry point.

use std::path::PathBuf;
use std::process::Command;

fn provium_bin() -> PathBuf {
    // The cargo test binary lives at target/debug/deps/<test>;
    // walk up to target/debug/provium.
    let exe = std::env::current_exe().unwrap();
    // exe = .../target/debug/deps/conformance_cli-XXXX
    let mut p = exe;
    p.pop(); // deps
    p.pop(); // debug
    p.push("provium");
    p
}

#[test]
fn no_files_plain_output_is_human_readable() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = dir.path().join("provium.toml");
    std::fs::write(&cfg, "").unwrap();
    let out = Command::new(provium_bin())
        .arg("--no-preflight")
        .arg("--config").arg(&cfg)
        .arg(dir.path())
        .output()
        .expect("spawn provium");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("no *.test.lua files found"),
        "expected human notice, got `{stdout}`");
    // Empty result is success.
    assert!(out.status.success());
}

#[test]
fn no_files_json_emits_json_object() {
    // R8 #394 fix: --json must not emit plain text on the empty
    // result path — downstream consumers parse line-delimited
    // JSON and would choke.
    let dir = tempfile::tempdir().unwrap();
    let cfg = dir.path().join("provium.toml");
    std::fs::write(&cfg, "").unwrap();
    let out = Command::new(provium_bin())
        .arg("--no-preflight")
        .arg("--config").arg(&cfg)
        .arg("--json")
        .arg(dir.path())
        .output()
        .expect("spawn provium");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.trim_start().starts_with('{'),
        "--json on empty must emit JSON object, got `{stdout}`");
    assert!(stdout.contains("no_files"),
        "type field must be 'no_files', got `{stdout}`");
}

#[test]
fn rerun_failed_without_state_file_exits_clean_with_notice() {
    // R8 #393 fix: --rerun-failed with no prior state used to
    // silently run the FULL suite. Now exits clean with a
    // pointer-message.
    let dir = tempfile::tempdir().unwrap();
    let cfg = dir.path().join("provium.toml");
    std::fs::write(&cfg, "").unwrap();
    let state = dir.path().join("nonexistent-rerun.json");
    let out = Command::new(provium_bin())
        .env("PROVIUM_RERUN_STATE", &state)
        .arg("--no-preflight")
        .arg("--config").arg(&cfg)
        .arg("--rerun-failed")
        .arg(dir.path())
        .output()
        .expect("spawn provium");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("no prior failure state")
        || stderr.contains("nothing to re-run"),
        "stderr must explain why nothing ran, got `{stderr}`");
    assert!(out.status.success(),
        "exit must be clean (no failures), got {:?}", out.status);
}

#[test]
fn coverage_without_provium_coverage_fails_with_pointer() {
    // R8 #395 fix: --coverage with the binary missing now
    // returns non-zero (was silent).
    let dir = tempfile::tempdir().unwrap();
    let cfg = dir.path().join("provium.toml");
    std::fs::write(&cfg, "").unwrap();
    let test_file = dir.path().join("dummy.test.lua");
    std::fs::write(&test_file, "test('x', function(t) end)").unwrap();
    // Force PATH to a directory that doesn't have provium-coverage.
    let out = Command::new(provium_bin())
        .env("PATH", "/no/such/path")
        .arg("--no-preflight")
        .arg("--config").arg(&cfg)
        .arg("--coverage")
        .arg(&test_file)
        .output()
        .expect("spawn provium");
    // Either non-zero exit or a stderr pointer at the missing
    // binary — both signal the user.
    let stderr = String::from_utf8_lossy(&out.stderr);
    let has_pointer = stderr.contains("provium-coverage")
        && (stderr.contains("not on PATH") || stderr.contains("install"));
    assert!(
        !out.status.success() || has_pointer,
        "missing provium-coverage must surface; got status={:?}, stderr=`{stderr}`",
        out.status,
    );
}

#[test]
fn help_includes_top_level_flags() {
    let out = Command::new(provium_bin())
        .arg("--help")
        .output()
        .expect("spawn provium --help");
    let stdout = String::from_utf8_lossy(&out.stdout);
    // Must mention every top-level flag tooling consumers might
    // grep for.
    for flag in &["--json", "--watch", "--coverage", "--filter"] {
        assert!(stdout.contains(flag),
            "--help must mention {flag}, got `{stdout}`");
    }
}

#[test]
fn version_emits_a_version_string() {
    let out = Command::new(provium_bin())
        .arg("--version")
        .output()
        .expect("spawn provium --version");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(!stdout.trim().is_empty(), "--version output empty");
}
