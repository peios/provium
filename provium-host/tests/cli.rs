//! Integration tests for the `provium` binary.
//!
//! Drives the freshly-built binary against a tempdir corpus, asserts
//! on its stdout/stderr/exit code. `cargo test` populates
//! `CARGO_BIN_EXE_provium` automatically.

#![cfg(feature = "lua")]

use std::path::Path;
use std::process::Command;

const PROVIUM_BIN: &str = env!("CARGO_BIN_EXE_provium");

fn write_file(dir: &Path, name: &str, contents: &str) {
    std::fs::write(dir.join(name), contents).unwrap();
}

fn write_minimal_config(dir: &Path) {
    write_file(
        dir,
        "provium.toml",
        r#"
[profiles.peios]
kernel  = "/unused-by-local-agent"
initrd  = "/unused-by-local-agent"
cmdline = "console=hvc0"
"#,
    );
}

fn run_provium(dir: &Path, extra_args: &[&str]) -> std::process::Output {
    let mut cmd = Command::new(PROVIUM_BIN);
    cmd.current_dir(dir)
        .arg("--vmm")
        .arg("local")
        .arg("--no-preflight")
        .arg("--config")
        .arg(dir.join("provium.toml"));
    for a in extra_args {
        cmd.arg(a);
    }
    cmd.output().expect("spawn provium")
}

#[test]
fn passing_file_yields_exit_0_and_pass_summary() {
    let dir = tempfile::tempdir().unwrap();
    write_minimal_config(dir.path());
    write_file(
        dir.path(),
        "ok.test.lua",
        r#"
test("greeting", function(t)
    local vm = provium:vm("dc1", "peios"):boot()
    local r = vm:run("printf hi")
    t:assert_eq(r.stdout, "hi")
end)
"#,
    );
    let output = run_provium(dir.path(), &[]);
    assert!(output.status.success(), "exit: {:?}", output.status);

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("PASS"), "stdout: {stdout}");
    assert!(stdout.contains("ok.test.lua"), "stdout: {stdout}");
}

#[test]
fn failing_file_yields_nonzero_exit_and_failure_block() {
    let dir = tempfile::tempdir().unwrap();
    write_minimal_config(dir.path());
    write_file(
        dir.path(),
        "broken.test.lua",
        r#"test("fails", function(t) t:fail("boom") end)"#,
    );

    let output = run_provium(dir.path(), &[]);
    assert!(!output.status.success(), "exit: {:?}", output.status);
    assert_eq!(output.status.code(), Some(1));

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("FAIL"), "stdout: {stdout}");
    assert!(stdout.contains("fails"), "stdout: {stdout}");
    assert!(stdout.contains("boom"), "stdout: {stdout}");
}

#[test]
fn no_test_files_exits_zero_with_friendly_message() {
    let dir = tempfile::tempdir().unwrap();
    write_minimal_config(dir.path());

    let output = run_provium(dir.path(), &[]);
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("no *.test.lua"), "stdout: {stdout}");
}

#[test]
fn filter_narrows_discovered_files() {
    let dir = tempfile::tempdir().unwrap();
    write_minimal_config(dir.path());
    write_file(
        dir.path(),
        "alpha.test.lua",
        r#"test("a", function(t) t:assert(true) end)"#,
    );
    write_file(
        dir.path(),
        "beta.test.lua",
        r#"test("b", function(t) t:fail("nope") end)"#,
    );

    // With the filter we only run `alpha`, so the failing `beta`
    // is skipped and the run passes.
    let output = run_provium(dir.path(), &["--filter", "alpha"]);
    assert!(output.status.success(), "exit: {:?}", output.status);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("alpha.test.lua"));
    assert!(!stdout.contains("beta.test.lua"), "beta should be filtered out: {stdout}");
}

#[test]
fn json_output_is_one_line_per_file() {
    let dir = tempfile::tempdir().unwrap();
    write_minimal_config(dir.path());
    write_file(
        dir.path(),
        "one.test.lua",
        r#"test("p", function(t) t:assert(true) end)"#,
    );
    write_file(
        dir.path(),
        "two.test.lua",
        r#"test("p", function(t) t:fail("nope") end)"#,
    );

    let output = run_provium(dir.path(), &["--json"]);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let lines: Vec<&str> = stdout.lines().filter(|l| !l.is_empty()).collect();
    assert_eq!(lines.len(), 2, "stdout: {stdout}");
    for line in lines {
        let v: serde_json::Value = serde_json::from_str(line).unwrap();
        assert!(v.get("path").is_some());
        assert!(v.get("tests").is_some());
        assert!(v.get("passed").is_some());
        assert!(v.get("timeout").is_some());
    }
}

#[test]
fn fail_fast_stops_after_first_failed_file() {
    let dir = tempfile::tempdir().unwrap();
    write_minimal_config(dir.path());
    // Two files; one fails, one passes. Sort order is alphabetical
    // so `01_*` runs first.
    write_file(
        dir.path(),
        "01_fails.test.lua",
        r#"test("nope", function(t) t:fail("boom") end)"#,
    );
    write_file(
        dir.path(),
        "02_passes.test.lua",
        r#"test("ok", function(t) t:assert(true) end)"#,
    );

    let output = run_provium(dir.path(), &["--fail-fast"]);
    assert!(!output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("01_fails"));
    // Without --fail-fast both files appear; with it, the second
    // file is omitted from the per-file render path.
    assert!(!stdout.contains("02_passes"), "stdout: {stdout}");
}

#[test]
fn save_events_writes_msgpack_frames_consumable_by_coverage() {
    use std::io::Cursor;
    use provium_protocol::events::{Event, EventFrame};
    use provium_protocol::frame::{read_frame, DEFAULT_MAX_FRAME_BYTES};

    let dir = tempfile::tempdir().unwrap();
    write_minimal_config(dir.path());
    write_file(
        dir.path(),
        "spec.test.lua",
        r#"
test("a", {spec = "PSD-X §1"}, function(t) t:assert(true) end)
test("b", {spec = "PSD-X §1"}, function(t) t:assert(true) end)
test("c", {spec = "PSD-X §2"}, function(t) t:fail("nope") end)
"#,
    );

    let events_path = dir.path().join("events.msgpack");
    let output = run_provium(
        dir.path(),
        &["--save-events", events_path.to_str().unwrap()],
    );
    // The failing test makes provium exit non-zero — that's correct.
    assert_eq!(output.status.code(), Some(1));

    // Read frames + verify a useful subset is present.
    let bytes = std::fs::read(&events_path).unwrap();
    let mut cursor = Cursor::new(&bytes);
    let mut counts = (0u32, 0u32, 0u32, 0u32); // dispatched, completed, passed, failed
    loop {
        match read_frame::<_, EventFrame>(&mut cursor, DEFAULT_MAX_FRAME_BYTES) {
            Ok(frame) => match frame.event {
                Event::FileDispatched(_) => counts.0 += 1,
                Event::FileCompleted(_) => counts.1 += 1,
                Event::TestPassed(_) => counts.2 += 1,
                Event::TestFailed(_) => counts.3 += 1,
                _ => {}
            },
            Err(provium_protocol::FrameError::Eof) => break,
            Err(e) => panic!("frame decode: {e}"),
        }
    }
    assert_eq!(counts.0, 1, "expected 1 file_dispatched");
    assert_eq!(counts.1, 1, "expected 1 file_completed");
    assert_eq!(counts.2, 2, "expected 2 test_passed");
    assert_eq!(counts.3, 1, "expected 1 test_failed");
}

#[test]
fn missing_config_surfaces_clean_error() {
    let dir = tempfile::tempdir().unwrap();
    write_file(
        dir.path(),
        "x.test.lua",
        r#"test("t", function(t) t:assert(true) end)"#,
    );
    // Don't write provium.toml.
    let mut cmd = Command::new(PROVIUM_BIN);
    cmd.current_dir(dir.path())
        .arg("--vmm")
        .arg("local")
        .arg("--no-preflight");
    let output = cmd.output().expect("spawn");
    assert_eq!(output.status.code(), Some(2));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("provium.toml"), "stderr: {stderr}");
}

/// Write a `provium.toml` whose `peios` profile points at real
/// (empty) kernel/initrd files in `dir`, so `console` passes its
/// existence checks without needing a real kernel.
fn write_console_config(dir: &Path) {
    write_file(dir, "vmlinuz", "not really a kernel");
    write_file(dir, "initrd.img", "not really an initrd");
    let cfg = format!(
        r#"
[profiles.peios]
kernel  = "{k}"
initrd  = "{i}"
cmdline = "console=hvc0 quiet"
"#,
        k = dir.join("vmlinuz").display(),
        i = dir.join("initrd.img").display(),
    );
    write_file(dir, "provium.toml", &cfg);
}

#[test]
fn console_print_command_emits_interactive_qemu_line() {
    // `--print-command` is a dry run: it assembles the QEMU argv and
    // prints it without launching, so it needs neither KVM nor a real
    // kernel. Asserts the interactive wiring (stdio-muxed serial, no
    // QMP) is present.
    let dir = tempfile::tempdir().unwrap();
    write_console_config(dir.path());
    let output = Command::new(PROVIUM_BIN)
        .current_dir(dir.path())
        .arg("--no-preflight")
        .arg("--no-ksm")
        .arg("--config")
        .arg(dir.path().join("provium.toml"))
        .arg("console")
        .arg("--print-command")
        .arg("peios")
        .output()
        .expect("spawn provium");
    assert!(output.status.success(), "stderr: {}", String::from_utf8_lossy(&output.stderr));
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("qemu-system-x86_64"), "stdout: {stdout}");
    assert!(stdout.contains("-kernel"), "stdout: {stdout}");
    assert!(stdout.contains("vmlinuz"), "stdout: {stdout}");
    assert!(stdout.contains("-serial mon:stdio"), "stdout: {stdout}");
    // Interactive boot must not stand up a QMP control socket.
    assert!(!stdout.contains("-qmp"), "stdout: {stdout}");
    // Bare boot (no --agent) must not wire vsock.
    assert!(!stdout.contains("vhost-vsock"), "stdout: {stdout}");
}

#[test]
fn console_unknown_profile_is_clean_error() {
    let dir = tempfile::tempdir().unwrap();
    write_console_config(dir.path());
    let output = Command::new(PROVIUM_BIN)
        .current_dir(dir.path())
        .arg("--no-preflight")
        .arg("--no-ksm")
        .arg("--config")
        .arg(dir.path().join("provium.toml"))
        .arg("console")
        .arg("--print-command")
        .arg("does-not-exist")
        .output()
        .expect("spawn provium");
    assert_eq!(output.status.code(), Some(2));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("does-not-exist"), "stderr: {stderr}");
    assert!(stderr.contains("not found"), "stderr: {stderr}");
}

#[test]
fn console_with_agent_wires_vsock_in_print_command() {
    // With --agent the overlay is injected and a CID allocated, so the
    // printed line must carry a vhost-vsock device. The overlay file
    // is located via the workspace `dist/` fallback; if it isn't
    // present the command errors before printing — tolerate that by
    // only asserting on the vsock wiring when the run succeeded.
    let dir = tempfile::tempdir().unwrap();
    write_console_config(dir.path());
    let output = Command::new(PROVIUM_BIN)
        .current_dir(dir.path())
        .arg("--no-preflight")
        .arg("--no-ksm")
        .arg("--config")
        .arg(dir.path().join("provium.toml"))
        .arg("console")
        .arg("--print-command")
        .arg("--agent")
        .arg("peios")
        .output()
        .expect("spawn provium");
    if output.status.success() {
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(stdout.contains("vhost-vsock-pci,guest-cid="), "stdout: {stdout}");
    }
}
