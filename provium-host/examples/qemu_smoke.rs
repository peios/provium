//! End-to-end smoke test for [`provium_host::vmm::qemu::QemuVmm`]
//! against a real KVM guest.
//!
//! Boots one VM from a kernel + initrd pair, drives a couple of ops
//! through the in-VM `provium-agent`, and shuts down cleanly.
//!
//! Requires `/dev/kvm` access. Not part of the test suite — run
//! manually via `scripts/smoke-real-qemu.sh`.
//!
//! ## Usage
//!
//! ```text
//! cargo run --release --example qemu_smoke -- \
//!     --kernel <path/to/bzImage> \
//!     --initrd <path/to/initrd.cpio.gz>
//! ```

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::process;
use std::sync::Arc;

use provium_host::lab::Lab;
use provium_host::profile::{Config, Profile, ProviumSection};
use provium_host::protocol::wire::{ExecArgs, ExitStatus};
use provium_host::vmm::qemu::QemuVmm;
use provium_host::vmm::BootOpts;

fn main() {
    let args = match parse_args() {
        Ok(a) => a,
        Err(msg) => {
            eprintln!("{msg}");
            process::exit(2);
        }
    };

    if let Err(e) = run(&args) {
        eprintln!("smoke test FAILED: {e}");
        process::exit(1);
    }

    println!("\nsmoke test PASSED");
}

#[derive(Debug)]
struct CliArgs {
    kernel: PathBuf,
    initrd: PathBuf,
    cmdline: String,
}

fn parse_args() -> Result<CliArgs, String> {
    let mut kernel: Option<PathBuf> = None;
    let mut initrd: Option<PathBuf> = None;
    let mut cmdline = "console=ttyS0 quiet panic=1".to_owned();

    let mut iter = std::env::args().skip(1);
    while let Some(a) = iter.next() {
        match a.as_str() {
            "--kernel" => {
                kernel = Some(iter.next().ok_or("--kernel needs a value")?.into());
            }
            "--initrd" => {
                initrd = Some(iter.next().ok_or("--initrd needs a value")?.into());
            }
            "--cmdline" => {
                cmdline = iter.next().ok_or("--cmdline needs a value")?;
            }
            "-h" | "--help" => {
                println!("usage: qemu_smoke --kernel PATH --initrd PATH [--cmdline STR]");
                process::exit(0);
            }
            other => return Err(format!("unknown argument: {other}")),
        }
    }

    Ok(CliArgs {
        kernel: kernel.ok_or("missing --kernel")?,
        initrd: initrd.ok_or("missing --initrd")?,
        cmdline,
    })
}

fn run(args: &CliArgs) -> Result<(), Box<dyn std::error::Error>> {
    let config = build_config(args);

    println!("==> booting VM via QemuVmm");
    let vmm = Arc::new(QemuVmm::new());
    let lab = Lab::new("smoke", Arc::clone(&config), vmm);
    let vm = lab.create_vm("smoke", "smoke", BootOpts::default())?;
    vm.boot()?;
    println!("    vm.cid = {:?}", vm.cid());

    // -----------------------------------------------------------------
    // Smoke 1: pure metadata op, no exec required in guest.
    // -----------------------------------------------------------------
    println!("\n==> vm.stat(\"/\")");
    let meta = vm.stat("/")?;
    println!(
        "    size={} entry_type={:?} perm={:o}",
        meta.size, meta.entry_type, meta.perm
    );
    if !matches!(
        meta.entry_type,
        provium_host::protocol::wire::EntryType::Directory
    ) {
        return Err(format!("expected / to be a directory, got {:?}", meta.entry_type).into());
    }

    // -----------------------------------------------------------------
    // Smoke 2: exec the agent binary itself with --help (it's the
    // only ELF in the minimal initrd). This exercises the full
    // exec dispatch path: spawn → wait → capture stdout/stderr.
    // The agent's --help branch exits cleanly with code 0 before
    // doing any vsock binding, so re-execing /init from inside the
    // VM is safe.
    // -----------------------------------------------------------------
    println!("\n==> vm.run(\"/init\", [\"--help\"])");
    let result = vm.run(ExecArgs {
        cmd: "/init".into(),
        args: vec!["--help".into()],
        env: Default::default(),
        env_clear: false,
        stdin: vec![],
        cwd: None,
        timeout_ms: Some(5_000),
    })?;
    println!("    status={:?}", result.status);
    println!("    stdout={:?}", result.stdout_str());
    if result.status != ExitStatus::Exited(0) {
        return Err(format!(
            "/init --help exited unexpectedly: {:?} stderr={:?}",
            result.status,
            result.stderr_str()
        )
        .into());
    }

    // -----------------------------------------------------------------
    // Smoke 3: write_file + read_file round-trip on /tmp.
    // -----------------------------------------------------------------
    println!("\n==> write_file + read_file on /tmp/smoke");
    vm.write_file("/tmp/smoke", b"hello from host\n".to_vec())?;
    let back = vm.read_file("/tmp/smoke")?;
    if back != b"hello from host\n" {
        return Err(format!("round-trip mismatch: {back:?}").into());
    }
    println!("    {:?}", String::from_utf8_lossy(&back));

    println!("\n==> shutdown");
    vm.shutdown()?;

    Ok(())
}

// Avoid an unused-import dead code warning when the compiler can't
// see Lab being used through the local helper above.
#[allow(dead_code)]
fn _lab_typecheck(l: &Lab) -> &str {
    l.name()
}

fn build_config(args: &CliArgs) -> Arc<Config> {
    let mut profiles = BTreeMap::new();
    profiles.insert(
        "smoke".into(),
        Profile {
            kernel: args.kernel.clone(),
            initrd: args.initrd.clone(),
            cmdline: args.cmdline.clone(),
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
