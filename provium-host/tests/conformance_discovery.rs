//! Conformance: file discovery + --filter per `DESIGN.md` §
//! Test organisation / File suffixes drive discovery.

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

fn write_file(dir: &std::path::Path, name: &str, body: &str) {
    let p = dir.join(name);
    if let Some(parent) = p.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    std::fs::write(p, body).unwrap();
}

fn cfg_in(dir: &std::path::Path) {
    std::fs::write(dir.join("provium.toml"), "").unwrap();
}

#[test]
fn test_lua_suffix_picked_up() {
    let dir = tempfile::tempdir().unwrap();
    cfg_in(dir.path());
    write_file(dir.path(), "good.test.lua", "test('a', function(t) end)");
    let out = Command::new(provium_bin())
        .arg("--no-preflight")
        .arg("--config").arg(dir.path().join("provium.toml"))
        .arg(dir.path())
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("good.test.lua") || stdout.contains("PASS")
        || stdout.contains("ok"),
        "*.test.lua must be discovered + run: stdout=`{stdout}`");
}

#[test]
fn non_test_lua_files_ignored() {
    let dir = tempfile::tempdir().unwrap();
    cfg_in(dir.path());
    write_file(dir.path(), "helper.lua", "-- not a test");
    write_file(dir.path(), "config.lua", "-- nor this");
    let out = Command::new(provium_bin())
        .arg("--no-preflight")
        .arg("--config").arg(dir.path().join("provium.toml"))
        .arg(dir.path())
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("no *.test.lua files found"),
        "non-test lua files must be ignored: `{stdout}`");
}

#[test]
fn recursive_scan_finds_nested_files() {
    let dir = tempfile::tempdir().unwrap();
    cfg_in(dir.path());
    write_file(dir.path(), "nested/sub/deep.test.lua",
        "test('nested', function(t) end)");
    let out = Command::new(provium_bin())
        .arg("--no-preflight")
        .arg("--config").arg(dir.path().join("provium.toml"))
        .arg(dir.path())
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("deep.test.lua") || stdout.contains("nested"),
        "recursive scan must find nested files: `{stdout}`");
}

#[test]
fn filter_substring_match() {
    let dir = tempfile::tempdir().unwrap();
    cfg_in(dir.path());
    write_file(dir.path(), "alpha.test.lua",
        "test('a', function(t) end)");
    write_file(dir.path(), "beta.test.lua",
        "test('b', function(t) end)");
    let out = Command::new(provium_bin())
        .arg("--no-preflight")
        .arg("--config").arg(dir.path().join("provium.toml"))
        .arg("--filter").arg("alpha")
        .arg(dir.path())
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    // alpha must be in output, beta must not
    assert!(stdout.contains("alpha"),
        "filter must match alpha: `{stdout}`");
    assert!(!stdout.contains("beta.test.lua"),
        "filter must exclude beta: `{stdout}`");
}

#[test]
fn fixture_lua_not_run_directly() {
    // *.fixture.lua files are referenced by tests, not executed
    // standalone. Discovery skips them.
    let dir = tempfile::tempdir().unwrap();
    cfg_in(dir.path());
    write_file(dir.path(), "x.fixture.lua",
        "return provium:vm('a', 'peios'):snapshot()");
    let out = Command::new(provium_bin())
        .arg("--no-preflight")
        .arg("--config").arg(dir.path().join("provium.toml"))
        .arg(dir.path())
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("no *.test.lua files found"),
        "*.fixture.lua must not be picked up by discovery: `{stdout}`");
}
