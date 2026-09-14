//! Interop between [`provium_host::nbd`] and QEMU's *real* NBD client.
//!
//! The unit tests in the module prove the server is consistent with a
//! hand-rolled client — which is to say, with our own reading of the
//! protocol. That is exactly the thing worth distrusting: a capture of
//! a live session showed QEMU negotiating `NBD_OPT_EXTENDED_HEADERS`
//! and then using a transmission format the specification's own
//! summary does not describe (request magic `0x21e41c71`, 64-bit
//! lengths, structured `OFFSET_DATA` replies).
//!
//! The server refuses that option so the client drops back to the
//! classic simple-reply format. Whether QEMU actually tolerates that
//! refusal is an assumption, and these tests are what turn it into a
//! fact. `qemu-io` is QEMU's block layer driven from the command line,
//! so it exercises the same NBD client a booted guest would.
//!
//! Skipped, loudly, when `qemu-io` is not installed.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use provium_host::nbd::MediatedDisk;

const SECTOR: usize = 512;

fn qemu_io_present() -> bool {
    Command::new("qemu-io")
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok()
}

/// A blank image of `size` bytes, with the temp dir that owns it.
///
/// The directory doubles as the socket's home: a unix socket address is
/// capped at 108 bytes, and a deeply nested path fails at bind time
/// with a message about the socket rather than about the directory.
fn image(size: u64) -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("disk.img");
    let file = std::fs::File::create(&path).unwrap();
    file.set_len(size).unwrap();
    (dir, path)
}

fn serve(path: &Path, dir: &tempfile::TempDir) -> MediatedDisk {
    MediatedDisk::start(path, &dir.path().join("nbd.sock")).unwrap()
}

/// Spin until `predicate` holds, or give up.
///
/// Used to observe that a write has actually reached the server before
/// the client is killed. Polling the server's own state is what makes
/// the test deterministic — a sleep would be a guess about how fast
/// QEMU is today.
/// `log` is the client's own output, reported on failure: a test that
/// kills its subprocess must not also throw away the one artefact that
/// explains why it was still waiting.
fn wait_until(what: &str, log: &Path, predicate: impl Fn() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        if predicate() {
            return;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    let output = std::fs::read_to_string(log).unwrap_or_else(|e| format!("<unreadable: {e}>"));
    panic!("timed out waiting for {what}\nqemu-io said:\n{output}");
}

#[test]
fn qemu_negotiates_with_the_server_and_round_trips_data() {
    if !qemu_io_present() {
        eprintln!("skipping: qemu-io not installed");
        return;
    }
    let (dir, path) = image(64 * 1024);
    let disk = serve(&path, &dir);

    // `read -P` verifies the pattern and fails the command if the bytes
    // differ, so a successful exit is the assertion.
    let output = Command::new("qemu-io")
        .args(["-f", "raw", &disk.qemu_url()])
        .args(["-c", "write -P 0xab 0 512"])
        .args(["-c", "flush"])
        .args(["-c", "read -P 0xab 0 512"])
        .output()
        .expect("running qemu-io");

    assert!(
        output.status.success(),
        "qemu-io failed — the client did not accept the server.\n\
         stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    assert!(
        disk.flushes() >= 1,
        "the client's flush should have reached the server"
    );

    let on_disk = std::fs::read(&path).unwrap();
    assert_eq!(&on_disk[..SECTOR], &[0xab; SECTOR], "the flush committed");
}

#[test]
fn a_write_qemu_never_flushed_does_not_survive_a_power_cut() {
    // PEI-1104's falsification criterion, driven through QEMU's own
    // client: the flushed write survives and the unflushed one does
    // not. If this fails the mediated-disk design does not give honest
    // durability semantics and the fallback is dm-flakey.
    //
    // qemu-io flushes when it closes the block device cleanly, which
    // would commit the very write this test needs to lose — so the
    // client is killed outright instead. That is also a better model of
    // the thing being tested: a machine losing power does not get to
    // run its shutdown path.
    if !qemu_io_present() {
        eprintln!("skipping: qemu-io not installed");
        return;
    }
    let (dir, path) = image(64 * 1024);
    let disk = serve(&path, &dir);

    // `-t writeback` is load-bearing, and verified rather than assumed:
    // a capture of both modes shows qemu-io's default sends every write
    // with `NBD_CMD_FLAG_FUA` set (command flags `0x0001`) and
    // `-t writeback` sends `0x0000`. The server honours FUA by
    // committing immediately, so under the default there would never be
    // an unflushed write to lose and this test would pass for entirely
    // the wrong reason.
    //
    // The same trap waits for the guest: a `-drive` wants `cache=none`
    // — direct *and* writeback — and `cache=writethrough` would make
    // every durability test green while proving nothing.

    // Phase 1: a durable write, from a client that exits cleanly. Its
    // close-flush is welcome here — this is the write that must live.
    let durable = Command::new("qemu-io")
        .args(["-t", "writeback"])
        .args(["-f", "raw", &disk.qemu_url()])
        .args(["-c", "write -P 0x11 0 512"])
        .args(["-c", "flush"])
        .output()
        .expect("running qemu-io");
    assert!(
        durable.status.success(),
        "qemu-io failed: {}",
        String::from_utf8_lossy(&durable.stderr)
    );

    // Phase 2: a write that is never flushed, from a client that is
    // killed rather than closed.
    //
    // Exactly one command may be driven this way. qemu-io executes the
    // first line from a pipe that stays open and then stalls waiting for
    // input it never parses — a second line is simply never run, which
    // cost this test an afternoon. That stall is what makes it useful:
    // the process sits alive holding a dirty overlay, so the kill lands
    // at a known point instead of racing the client's exit.
    let log_path = dir.path().join("qemu-io.log");
    let log = std::fs::File::create(&log_path).unwrap();
    let mut child = Command::new("qemu-io")
        .args(["-t", "writeback"])
        .args(["-f", "raw", &disk.qemu_url()])
        .stdin(Stdio::piped())
        .stdout(Stdio::from(log.try_clone().unwrap()))
        .stderr(Stdio::from(log))
        .spawn()
        .expect("spawning qemu-io");

    {
        let stdin = child.stdin.as_mut().expect("qemu-io stdin");
        stdin.write_all(b"write -P 0x22 512 512\n").unwrap();
        stdin.flush().unwrap();
    }

    wait_until("the unflushed write to reach the server", &log_path, || {
        disk.unflushed_sectors() >= 1
    });

    // No shutdown path, no close-flush.
    child.kill().expect("killing qemu-io");
    let _ = child.wait();

    disk.power_cut();

    let on_disk = std::fs::read(&path).unwrap();
    assert_eq!(
        &on_disk[..SECTOR],
        &[0x11; SECTOR],
        "the write QEMU flushed must survive the power cut"
    );
    assert_eq!(
        &on_disk[SECTOR..2 * SECTOR],
        &[0u8; SECTOR],
        "the write QEMU never flushed must not survive the power cut"
    );
}

#[test]
fn qemu_system_opens_the_export_as_a_guest_disk() {
    // qemu-io exercises QEMU's block layer directly. A booted guest
    // reaches its disk through `-drive` and a virtio-blk device, which
    // is a different path into the same NBD client — and it is the path
    // a mediated disk will actually be used through, so it is worth
    // proving separately.
    //
    // `-S` leaves the CPU stopped: there is no guest image here and
    // none is needed, because QEMU opens its drives while starting up
    // and exits if one cannot be opened. `cache=none` is the mode a
    // durability test requires (direct, and crucially *not*
    // writethrough), so it is the mode worth proving against.
    let present = Command::new("qemu-system-x86_64")
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok();
    if !present {
        eprintln!("skipping: qemu-system-x86_64 not installed");
        return;
    }

    let (dir, path) = image(64 * 1024);
    let disk = serve(&path, &dir);

    let log_path = dir.path().join("qemu-system.log");
    let log = std::fs::File::create(&log_path).unwrap();
    let mut child = Command::new("qemu-system-x86_64")
        .args(["-nographic", "-S", "-no-user-config", "-nodefaults"])
        .args(["-machine", "q35"])
        .args([
            "-drive",
            &format!(
                "file={},if=none,id=d0,format=raw,cache=none",
                disk.qemu_url()
            ),
        ])
        .args(["-device", "virtio-blk-pci,drive=d0,id=vd0"])
        .stdin(Stdio::null())
        .stdout(Stdio::from(log.try_clone().unwrap()))
        .stderr(Stdio::from(log))
        .spawn()
        .expect("spawning qemu-system-x86_64");

    // A connection is the positive signal. If QEMU rejected the drive
    // it has already exited, and the wait reports its complaint.
    wait_until("QEMU to connect to the export", &log_path, || {
        disk.connections() >= 1
    });

    child.kill().ok();
    let _ = child.wait();
}
