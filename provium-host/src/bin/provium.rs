//! `provium` — the v1 test-runner binary.
//!
//! Slice-6 surface:
//!
//! ```text
//! provium [PATHS]...                      run *.test.lua under each path
//!   --config <path>                       provium.toml location (default: ./provium.toml)
//!   --filter <substring>                  only run files whose path contains this
//!   --include-slow                        run tests marked `slow` (default: skipped)
//!   --fail-fast                           stop after the first failed file
//!   --mem <bytes>                         override pool memory budget
//!   --cpus <n>                            override pool CPU budget
//!   --json                                line-delimited JSON instead of plain text
//!   -v, --verbose                         show passes too
//!   -q, --quiet                           failures only
//!   --no-progress                         disable the live progress bar
//!   --no-preflight                        skip /dev/kvm + iproute2 checks (tests/dev)
//! ```
//!
//! Exit code:
//!   - `0` — every file passed (or skipped).
//!   - `1` — at least one file did not finish cleanly.
//!   - `2` — internal error (config load failure, etc.).
//!
//! The exit code is intentionally clamped to `0/1/2` rather than
//! "number of failed files" so shell scripts don't have to worry
//! about overflow at the 256-file mark or accidentally interpret
//! `failed_count == 2` as the internal-error sentinel.

use std::path::PathBuf;
use std::io::IsTerminal;
use std::process::ExitCode;
use std::sync::atomic::{AtomicI64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use clap::{Parser, Subcommand};
use indicatif::{ProgressBar, ProgressStyle};

use provium_host::profile::Config;
use provium_host::scheduler::{
    dispatch_files_with_progress, run_preflight, DispatchOpts, EventSink, FileTimeout,
    NullSink, Pool, ResourceAmount, WriteSink,
};
use provium_host::vmm::local_agent::LocalAgentVmm;
use provium_host::vmm::Vmm;

#[derive(Clone, Debug, Parser)]
#[command(
    name = "provium",
    version,
    about = "Run *.test.lua against the configured VMM."
)]
struct Args {
    /// Paths to scan for `*.test.lua`. If omitted, the current
    /// directory is scanned.
    #[arg(value_name = "PATH")]
    paths: Vec<PathBuf>,

    /// Path to `provium.toml`.
    #[arg(long, default_value = "provium.toml")]
    config: PathBuf,

    /// Only run files whose path contains this substring.
    #[arg(long)]
    filter: Option<String>,

    /// Run tests marked `slow`. Default skips them.
    #[arg(long)]
    include_slow: bool,

    /// Run only tests whose `tags` metadata includes one of the
    /// given values. Repeatable: `--tag a --tag b` is "a OR b".
    #[arg(long, value_name = "TAG")]
    tag: Vec<String>,

    /// Skip tests whose `tags` metadata includes any of these
    /// values. Wins over `--tag` (intersect).
    #[arg(long = "no-tag", value_name = "TAG")]
    no_tag: Vec<String>,

    /// Run only tests whose meta[KEY] contains VALUE. Repeatable.
    /// Multiple flags with the same KEY are OR'd within that key;
    /// different KEYs are AND'd. Useful for filtering on
    /// arbitrary meta fields like `subsystems = {"peinit", "loregd"}`.
    /// Format: `--tag-meta subsystems=peinit`.
    #[arg(long = "tag-meta", value_name = "KEY=VALUE", value_parser = parse_kv)]
    tag_meta: Vec<(String, String)>,

    /// Skip tests whose meta[KEY] contains VALUE. Repeatable. Wins
    /// over `--tag-meta`. Same key/value semantics.
    #[arg(long = "no-tag-meta", value_name = "KEY=VALUE", value_parser = parse_kv)]
    no_tag_meta: Vec<(String, String)>,

    /// CPU oversubscription factor. `1.0` means "strict" (default
    /// per design: total declared vCPUs ≤ host cores). Pass `1.5`
    /// or `2.0` to allow oversubscription if the workload tolerates
    /// scheduling jitter.
    #[arg(long, default_value = "1.0")]
    cpu_overcommit: f64,

    /// Convenience: pipe events to `provium-coverage` after the run
    /// completes, using `provium-coverage` on PATH.
    #[arg(long)]
    coverage: bool,

    /// Stop after the first failed file.
    #[arg(long)]
    fail_fast: bool,

    /// Pool memory budget. Accepts plain bytes or `512M` / `2G`.
    #[arg(long, value_name = "BYTES")]
    mem: Option<String>,

    /// Pool vCPU budget.
    #[arg(long, value_name = "N")]
    cpus: Option<u32>,

    /// Per-file timeout. Accepts integer seconds or a duration
    /// string (`"10m"`, `"30s"`, `"500ms"`). `0` disables.
    /// R9 mat-M2: was `u64` only — DESIGN documents `--timeout 10m`
    /// but the bare `u64` parser rejected the unit suffix.
    #[arg(long, default_value = "300", value_parser = parse_timeout_arg)]
    timeout: u64,

    /// JSON-lines output (one object per file). Mutually
    /// exclusive with `--events-stdout` so the JSON lines and
    /// msgpack frames don't interleave on the same descriptor.
    #[arg(long, conflicts_with = "events_stdout")]
    json: bool,

    /// Persist the observability event stream to PATH as
    /// length-prefixed msgpack frames. Compatible with
    /// `provium-coverage --from PATH`.
    #[arg(long, value_name = "PATH")]
    save_events: Option<PathBuf>,

    /// Emit the raw msgpack event stream to stdout (length-prefixed
    /// frames). When set, the human-readable / JSON test renderer
    /// writes to stderr instead. Pipe to `provium-coverage` etc.
    #[arg(long, conflicts_with = "json")]
    events_stdout: bool,

    /// Live-multiplex the event stream over a Unix socket. The
    /// binary listens, accepts connections, fans out frames.
    #[arg(long, value_name = "PATH")]
    events_socket: Option<PathBuf>,

    /// Re-run only files that failed in the last `provium` run.
    /// Reads the per-file outcome from the rerun-state file at
    /// `$PROVIUM_RERUN_STATE` (default: `~/.cache/provium/rerun.json`).
    #[arg(long)]
    rerun_failed: bool,

    /// Watch test roots for changes and re-run on save. Polls
    /// every 500ms for stat-change.
    #[arg(long)]
    watch: bool,

    /// Run only tests whose path mtime is newer than the named
    /// reference file. Useful for `provium --since main.lua`-
    /// style narrow runs.
    #[arg(long, value_name = "PATH")]
    since: Option<PathBuf>,

    /// Show passing tests too. Default lists only files + failures.
    #[arg(short = 'v', long)]
    verbose: bool,

    /// Show only failures. Mutually exclusive with `--verbose`.
    #[arg(short = 'q', long, conflicts_with = "verbose")]
    quiet: bool,

    /// Disable the live progress bar even on an interactive
    /// terminal. Per-file results still stream as they complete.
    #[arg(long)]
    no_progress: bool,

    /// Skip pre-flight checks (`/dev/kvm`, iproute2, …). Used by
    /// integration tests + dev workflows where the harness knows
    /// the environment is fine.
    #[arg(long)]
    no_preflight: bool,

    /// Skip KSM tuning at startup. Default tunes
    /// `/sys/kernel/mm/ksm/*` knobs per the design; pass this to
    /// keep KSM at distro defaults (e.g. on shared dev hosts).
    #[arg(long)]
    no_ksm: bool,

    /// Skip dynamic-profile build hooks. By default a profile's
    /// `build` command runs once before its VM boots; pass this when
    /// you know the artifacts are already current and want to skip
    /// straight to booting. See `provium prepare` to build without
    /// booting.
    #[arg(long)]
    no_build: bool,

    /// Choose the VMM backend. `qemu` is the production default;
    /// `local` uses the in-process agent harness — useful when KVM
    /// isn't available (CI, dev-on-laptop) and the tests don't
    /// actually need a guest kernel.
    #[arg(long, default_value = "qemu")]
    vmm: VmmChoice,

    /// Subcommand. When omitted, the `run` (test-suite) mode runs.
    #[command(subcommand)]
    cmd: Option<Cmd>,
}

#[derive(Clone, Debug, Subcommand)]
enum Cmd {
    /// Boot a VM and drop into an interactive Lua REPL against it.
    Repl {
        /// Profile name from `provium.toml`. Optional when
        /// `--fixture` is given.
        #[arg(default_value = "")]
        profile: String,
        /// VM name. Defaults to "repl".
        #[arg(long, default_value = "repl")]
        name: String,
        /// Resume from a fixture instead of cold-booting a profile.
        #[arg(long, value_name = "PATH")]
        fixture: Option<String>,
    },

    /// Boot a profile's VM and attach your terminal to its serial
    /// console — like running `qemu-system-x86_64` by hand. No agent,
    /// no Lua: just the guest's console on your TTY.
    ///
    /// `Ctrl-A X` quits QEMU; `Ctrl-A C` toggles the QEMU monitor.
    Console {
        /// Profile name from `provium.toml`.
        profile: String,
        /// Override guest memory. Accepts plain bytes or `512M` / `2G`.
        /// Defaults to the per-VM default (512 MiB).
        #[arg(long, value_name = "BYTES")]
        mem: Option<String>,
        /// Override the vCPU count.
        #[arg(long, value_name = "N")]
        cpus: Option<u32>,
        /// Override the profile's kernel command line.
        #[arg(long, value_name = "TEXT")]
        cmdline: Option<String>,
        /// Inject the provium agent overlay (vsock control plane)
        /// alongside the interactive console. Off by default.
        #[arg(long)]
        agent: bool,
        /// QEMU binary to exec. Defaults to `qemu-system-x86_64` on PATH.
        #[arg(long, value_name = "PATH")]
        qemu: Option<PathBuf>,
        /// Print the assembled QEMU command line and exit without
        /// booting.
        #[arg(long)]
        print_command: bool,
        /// Extra arguments passed verbatim to QEMU, after a `--`
        /// separator (e.g. `provium console peios -- -drive file=d.img`).
        #[arg(last = true, value_name = "QEMU_ARG")]
        qemu_args: Vec<String>,
    },

    /// Run dynamic profiles' `build` commands without booting anything.
    /// With no PROFILE, builds every profile that declares a `build`.
    /// Useful to pre-warm artifacts, then run `provium --no-build`.
    Prepare {
        /// Profile to build. Omit to build all profiles that declare a
        /// `build` command.
        profile: Option<String>,
    },

    /// Fixture-cache management.
    Fixture {
        #[command(subcommand)]
        op: FixtureCmd,
    },

    /// List discovered tests / fixtures without running anything.
    List {
        /// Show fixtures instead of tests.
        #[arg(long)]
        fixtures: bool,
    },

    /// Drop Lua LSP definition files into a test directory so
    /// `test`, `provium`, `wait_until`, and friends stop showing
    /// up as undefined globals in your editor.
    ///
    /// Writes:
    ///   <DIR>/.provium-meta/types.lua  — ---@meta stubs
    ///   <DIR>/.luarc.json              — config pointing at the stubs
    ///
    /// If `.luarc.json` already exists, refuses to overwrite and
    /// prints the snippet to merge in by hand.
    LspSetup {
        /// Target directory (defaults to the current directory).
        #[arg(default_value = ".")]
        dir: std::path::PathBuf,
        /// Overwrite an existing `.luarc.json`.
        #[arg(long)]
        force: bool,
    },
}

#[derive(Clone, Debug, Subcommand)]
enum FixtureCmd {
    /// List cached fixtures + size + key.
    List,
    /// Force a build for the named fixture.
    Build {
        /// Fixture path (test-root-relative, no `.fixture.lua`).
        path: String,
    },
    /// Force a rebuild for the named fixture (evicts existing entry first).
    Rebuild {
        /// Fixture path.
        path: String,
    },
    /// Wipe the entire cache.
    Clean,
    /// List fixtures whose source has changed since last build.
    Stale,
}

#[derive(Clone, Copy, Debug, clap::ValueEnum)]
enum VmmChoice {
    Qemu,
    Local,
}

/// Hard fallback when /proc/meminfo isn't readable. Per
/// DESIGN.md § Scheduler / Pool the real default is "80% of host
/// RAM"; the constant only kicks in for unusual hosts.
const DEFAULT_POOL_MEMORY_BYTES: u64 = 8 * 1024 * 1024 * 1024;
/// Hard fallback when sysconf(SC_NPROCESSORS_ONLN) fails.
const DEFAULT_POOL_CPUS: u32 = 8;

/// Process-wide list of (msgpack tmp, marker) pairs to remove on
/// exit. Populated by `--coverage` when it stamps a temp file;
/// cleared by the post-run cleanup AND by the SIGINT/atexit hook
/// so a killed run doesn't leak the msgpack into $TMPDIR.
static COVERAGE_TMP_CLEANUP: std::sync::OnceLock<
    std::sync::Mutex<Vec<(PathBuf, PathBuf)>>,
> = std::sync::OnceLock::new();

/// Parser for `--tag-meta KEY=VALUE` clap args. Splits on the first
/// `=` only so values may contain further `=` characters
/// (e.g. `--tag-meta spec=PSD-123-4.2=baseline`).
fn parse_kv(raw: &str) -> Result<(String, String), String> {
    match raw.split_once('=') {
        Some((k, v)) if !k.is_empty() => Ok((k.to_owned(), v.to_owned())),
        _ => Err(format!(
            "expected KEY=VALUE, got {raw:?} (KEY must be non-empty)"
        )),
    }
}

fn register_coverage_temp_cleanup(tmp: &std::path::Path, marker: &std::path::Path) {
    let list = COVERAGE_TMP_CLEANUP
        .get_or_init(|| std::sync::Mutex::new(Vec::new()));
    list.lock().unwrap().push((tmp.to_path_buf(), marker.to_path_buf()));
    install_coverage_signal_handler();
}

fn cleanup_coverage_temps() {
    if let Some(list) = COVERAGE_TMP_CLEANUP.get() {
        if let Ok(mut g) = list.lock() {
            for (tmp, marker) in g.drain(..) {
                let _ = std::fs::remove_file(&tmp);
                let _ = std::fs::remove_file(&marker);
            }
        }
    }
}

extern "C" fn coverage_signal_handler(sig: libc::c_int) {
    // SAFETY: cleanup_coverage_temps only does fs::remove_file on
    // paths and a Mutex lock; not strictly async-signal-safe but
    // good enough in practice for a one-shot cleanup before exit.
    cleanup_coverage_temps();
    // Restore default disposition and re-raise so the parent shell
    // sees actual signal-death (not a clean exit code 130). Tools
    // doing `wait -n` / `set -e` / `trap` rely on the termination
    // mode, not just the exit number. process::exit would short-
    // circuit that contract.
    unsafe {
        libc::signal(sig, libc::SIG_DFL);
        libc::raise(sig);
    }
}

fn install_coverage_signal_handler() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        // SAFETY: signal(2) returns the previous handler; we
        // don't rely on it. SIGINT only — SIGKILL is unstoppable.
        unsafe {
            libc::signal(libc::SIGINT, coverage_signal_handler as *const () as libc::sighandler_t);
            libc::signal(libc::SIGTERM, coverage_signal_handler as *const () as libc::sighandler_t);
        }
        // Also fire on normal exit so a panic late in run() that
        // bypasses the explicit cleanup still removes the file.
        unsafe {
            extern "C" fn at_exit() {
                cleanup_coverage_temps();
            }
            libc::atexit(at_exit);
        }
    });
}

/// Process-wide cache for the `--events-socket` UnixSocketSink.
/// Keyed by canonical path so successive `build_event_sink` calls
/// (notably from the `--watch` loop) reuse the same listener and
/// the same connected-client list. Without this the socket would
/// be removed + rebound every iteration, dropping connected
/// dashboards on every re-run.
static EVENTS_SOCKET_SINK: std::sync::OnceLock<
    std::sync::Mutex<
        std::collections::HashMap<PathBuf, provium_host::scheduler::events::UnixSocketSink>,
    >,
> = std::sync::OnceLock::new();

fn events_socket_sink(
    path: &std::path::Path,
) -> std::io::Result<provium_host::scheduler::events::UnixSocketSink> {
    let canon = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let map = EVENTS_SOCKET_SINK
        .get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()));
    let mut g = map.lock().unwrap();
    if let Some(existing) = g.get(&canon) {
        return Ok(existing.clone());
    }
    let sink = provium_host::scheduler::events::UnixSocketSink::bind(path)?;
    g.insert(canon, sink.clone());
    Ok(sink)
}

/// 80% of host RAM in bytes per DESIGN.md § Scheduler / Pool.
/// Reads `/proc/meminfo` (`MemTotal`); falls back to
/// [`DEFAULT_POOL_MEMORY_BYTES`] if anything goes wrong.
fn detected_pool_memory_bytes() -> u64 {
    let meminfo = match std::fs::read_to_string("/proc/meminfo") {
        Ok(s) => s,
        Err(_) => return DEFAULT_POOL_MEMORY_BYTES,
    };
    for line in meminfo.lines() {
        if let Some(rest) = line.strip_prefix("MemTotal:") {
            // Format: `MemTotal:       65812296 kB`
            let kb: Option<u64> = rest
                .split_whitespace()
                .next()
                .and_then(|n| n.parse::<u64>().ok());
            if let Some(kb) = kb {
                if kb == 0 {
                    // Malformed line / cgroup with no memory
                    // limit — fall through to the default. A
                    // zero-byte pool would cause every per-file
                    // overhead acquisition to fail.
                    break;
                }
                let bytes = kb.saturating_mul(1024);
                // 80% — keep a margin so the host stays usable.
                return ((bytes as f64) * 0.8) as u64;
            }
        }
    }
    DEFAULT_POOL_MEMORY_BYTES
}

/// Online CPU count per DESIGN.md § Scheduler / Pool. Reads
/// `sysconf(SC_NPROCESSORS_ONLN)`; falls back to
/// [`DEFAULT_POOL_CPUS`] if the sysconf call returns ≤ 0.
fn detected_pool_cpus() -> u32 {
    // SAFETY: sysconf is async-signal-safe and takes a constant.
    let n = unsafe { libc::sysconf(libc::_SC_NPROCESSORS_ONLN) };
    if n > 0 {
        n as u32
    } else {
        DEFAULT_POOL_CPUS
    }
}

fn main() -> ExitCode {
    let args = Args::parse();
    match run(args) {
        Ok(0) => ExitCode::SUCCESS,
        Ok(_failed) => ExitCode::from(1),
        Err(e) => {
            eprintln!("provium: {e}");
            ExitCode::from(2)
        }
    }
}

fn run(args: Args) -> Result<u32, Box<dyn std::error::Error>> {
    // Verbose gates the per-VM lifecycle chatter ("agent overlay
    // injected", "agent up after Xs") emitted deep in the VMM
    // layer. Set it once, up front, so every later code path sees
    // the right level.
    provium_host::verbosity::set_verbose(args.verbose);

    // `lsp-setup` is a pure file-writer — no KVM, no networking,
    // no fixture cache. Short-circuit before preflight so users
    // can run it on machines that lack the test prerequisites.
    if let Some(Cmd::LspSetup { dir, force }) = &args.cmd {
        return run_lsp_setup(dir, *force).map(|_| 0);
    }

    // `prepare` only runs a profile's build command — no KVM, no
    // networking, no fixture cache. Short-circuit before preflight
    // (like `lsp-setup`) so images can be built on a machine without
    // the VM-test prerequisites (CAP_NET_ADMIN, /dev/kvm).
    if let Some(Cmd::Prepare { profile }) = &args.cmd {
        let mut config = Config::load(&args.config)?;
        config.expand_build_outputs();
        match profile {
            Some(name) => {
                let p = config.profile(name).ok_or_else(|| {
                    format!("provium prepare: profile `{name}` not found in provium.toml")
                })?;
                provium_host::build::run_build(name, p)?;
            }
            None => provium_host::build::run_all_builds(&config)?,
        }
        return Ok(0);
    }

    // -----------------------------------------------------------------
    // 1. Pre-flight (unless skipped).
    // -----------------------------------------------------------------
    if !args.no_preflight {
        if let Err(e) = run_preflight() {
            return Err(Box::new(e));
        }
    }

    // 1b. LRU eviction of the fixture cache. Per `DESIGN.md`
    //     § Fixtures / Eviction policy: GC runs at startup before
    //     scanning tests, sorts by access time, evicts oldest until
    //     total size is ≤ the cap.
    {
        let config_for_gc = provium_host::profile::Config::load(&args.config)
            .unwrap_or_default();
        let cache_dir = config_for_gc
            .provium
            .cache_dir
            .clone()
            .unwrap_or_else(provium_host::fixture::default_cache_dir);
        if cache_dir.exists() {
            let max_bytes = config_for_gc
                .provium
                .cache_max_size
                .as_deref()
                .and_then(|s| parse_size(s).ok())
                .unwrap_or(provium_host::fixture::DEFAULT_CACHE_MAX_BYTES);
            let _ = provium_host::fixture::evict_to(&cache_dir, max_bytes);
        }
    }

    if !args.no_ksm {
        let report = provium_host::perf::tune_ksm();
        if !args.json {
            // KSM tuning is best-effort + diagnostic. One line on
            // stderr keeps the test output clean.
            eprintln!("provium: {}", report.summary());
        }
    }

    // -----------------------------------------------------------------
    // 2. Config.
    // -----------------------------------------------------------------
    let mut config = Config::load(&args.config)?;
    // Expand `{out}` in every profile's build command + path fields to
    // that profile's resolved build-output dir, so a dynamic profile's
    // `--out` and the artifacts the VM later reads can't drift.
    config.expand_build_outputs();
    let config = Arc::new(config);

    // -----------------------------------------------------------------
    // Dispatch on subcommand if present.
    // -----------------------------------------------------------------
    match &args.cmd {
        Some(Cmd::Repl {
            profile,
            name,
            fixture,
        }) => {
            let vmm: Arc<dyn Vmm> = match args.vmm {
                VmmChoice::Qemu => Arc::new(provium_host::vmm::qemu::QemuVmm::new()),
                VmmChoice::Local => Arc::new(LocalAgentVmm::new()),
            };
            // For --fixture we still need a profile lookup name.
            // Take the first profile in the config if --fixture
            // was supplied without a profile arg.
            let profile_name = if !profile.is_empty() {
                profile.clone()
            } else if let Some(_fix) = fixture.as_ref() {
                config
                    .profiles
                    .keys()
                    .next()
                    .cloned()
                    .unwrap_or_else(|| "peios".into())
            } else {
                return Err("provium repl: profile is required (or pass --fixture)".into());
            };
            // Build the profile's artifacts before booting (unless the
            // profile is static or --no-build was passed).
            if !args.no_build {
                if let Some(p) = config.profile(&profile_name) {
                    provium_host::build::run_build(&profile_name, p)?;
                }
            }
            provium_host::repl::run(
                provium_host::repl::ReplOpts {
                    vm_name: name.clone(),
                    profile_name,
                    fixture: fixture.clone(),
                },
                config,
                vmm,
            )?;
            return Ok(0);
        }
        Some(Cmd::Console {
            profile,
            mem,
            cpus,
            cmdline,
            agent,
            qemu,
            print_command,
            qemu_args,
        }) => {
            let memory_bytes = match mem {
                Some(s) => Some(parse_size(s)?),
                None => None,
            };
            // Build the profile's artifacts before booting (unless the
            // profile is static or --no-build was passed).
            if !args.no_build {
                if let Some(p) = config.profile(profile) {
                    provium_host::build::run_build(profile, p)?;
                }
            }
            let code = provium_host::console::run(
                provium_host::console::ConsoleOpts {
                    profile_name: profile.clone(),
                    memory_bytes,
                    cpus: *cpus,
                    cmdline_override: cmdline.clone(),
                    inject_agent: *agent,
                    qemu_binary: qemu.clone(),
                    ksm_enabled: !args.no_ksm,
                    extra_args: qemu_args.clone(),
                    print_command: *print_command,
                },
                &config,
            )?;
            // Map QEMU's exit code onto provium's 0/1 success contract
            // (main clamps non-zero to exit 1 anyway).
            return Ok(if code == 0 { 0 } else { 1 });
        }
        Some(Cmd::Prepare { .. }) => {
            // Short-circuited before preflight (see top of `run`);
            // this arm exists only for match exhaustiveness.
            unreachable!("Cmd::Prepare is handled before preflight");
        }
        Some(Cmd::Fixture { op }) => {
            let vmm: Arc<dyn Vmm> = match args.vmm {
                VmmChoice::Qemu => Arc::new(provium_host::vmm::qemu::QemuVmm::new()),
                VmmChoice::Local => Arc::new(LocalAgentVmm::new()),
            };
            return run_fixture_cmd(op, &config, vmm).map(|_| 0);
        }
        Some(Cmd::List { fixtures }) => {
            return list_command(&config, *fixtures).map(|_| 0);
        }
        Some(Cmd::LspSetup { dir, force }) => {
            return run_lsp_setup(dir, *force).map(|_| 0);
        }
        None => {}
    }

    // -----------------------------------------------------------------
    // 2b. Dynamic-profile builds. Tests pick profiles at runtime from
    //     Lua, so provium can't statically know which a run will boot;
    //     build every profile that declares a `build` command, once,
    //     before any VM starts. `--no-build` skips the whole step.
    // -----------------------------------------------------------------
    if !args.no_build {
        provium_host::build::run_all_builds(&config)?;
    }

    // -----------------------------------------------------------------
    // 3. Discovery.
    // -----------------------------------------------------------------
    let scan_roots: Vec<PathBuf> = if args.paths.is_empty() {
        vec![std::env::current_dir()?]
    } else {
        args.paths.clone()
    };
    let mut discovered = discover_test_files(&scan_roots, args.filter.as_deref())?;

    // --rerun-failed: intersect with the previous-run failure set.
    // If the state file doesn't exist (or contained no entries),
    // exit cleanly — running the FULL suite under --rerun-failed
    // is almost never what the user wanted; better to say so.
    if args.rerun_failed {
        let failed = load_rerun_failed();
        if failed.is_empty() {
            let state_path = rerun_state_path();
            eprintln!(
                "provium --rerun-failed: no prior failure state at `{}` — \
                 nothing to re-run. Run `provium <paths>` once to populate.",
                state_path.display(),
            );
            return Ok(0);
        }
        let failed_set: std::collections::HashSet<PathBuf> =
            failed.into_iter().collect();
        discovered.retain(|p| failed_set.contains(p));
    }

    // --since: keep only files newer than the reference.
    if let Some(since_path) = &args.since {
        let since_mtime = std::fs::metadata(since_path)
            .ok()
            .and_then(|m| m.modified().ok());
        if let Some(threshold) = since_mtime {
            discovered.retain(|p| {
                std::fs::metadata(p)
                    .ok()
                    .and_then(|m| m.modified().ok())
                    .map(|t| t > threshold)
                    .unwrap_or(false)
            });
        }
    }

    // --watch: loop, re-running on change. The empty-set case is
    // valid here — the watcher rescans every tick, so dropping a
    // *.test.lua into the watched root after launch should
    // trigger discovery + re-run rather than the user finding
    // out the watcher already exited. Just print a notice.
    if args.watch {
        if discovered.is_empty() {
            eprintln!(
                "provium --watch: no *.test.lua files match the current \
                 filter yet — watching for new files"
            );
        }
        return watch_loop(scan_roots, args, config, discovered);
    }

    if discovered.is_empty() {
        if args.json {
            // Honour --json even on the empty-set path —
            // downstream consumers can choke on the plain-text
            // notice. One-line JSON object documents the empty
            // result; matches the per-file lines emitted in the
            // populated case (each is its own JSON object too).
            println!(
                "{}",
                serde_json::json!({
                    "type": "no_files",
                    "message": "no *.test.lua files found"
                }),
            );
        } else {
            println!("provium: no *.test.lua files found");
        }
        return Ok(0);
    }

    // -----------------------------------------------------------------
    // 4. Pool.
    // -----------------------------------------------------------------
    let pool_memory = match &args.mem {
        Some(s) => parse_size(s)?,
        None => detected_pool_memory_bytes(),
    };
    let base_cpus = args.cpus.unwrap_or_else(detected_pool_cpus);
    // Apply CPU overcommit factor per `DESIGN.md` § Scheduler / Pool.
    // Default is 1.0 (strict, no oversubscription); 1.5 / 2.0 allow
    // proportional oversubscription if the workload tolerates it.
    // Clamp to a sane range — anything beyond 8x is almost certainly
    // a typo (`--cpu-overcommit 100` would explode the pool).
    let raw_overcommit = args.cpu_overcommit;
    let overcommit = raw_overcommit.clamp(0.5, 8.0);
    if (overcommit - raw_overcommit).abs() > f64::EPSILON {
        eprintln!(
            "provium: --cpu-overcommit {raw_overcommit} clamped to {overcommit} (sane range 0.5–8.0)"
        );
    }
    let pool_cpus = ((base_cpus as f64) * overcommit)
        .round()
        .clamp(1.0, u32::MAX as f64) as u32;
    let pool = Pool::new(ResourceAmount {
        memory_bytes: pool_memory,
        cpus: pool_cpus,
    });

    // -----------------------------------------------------------------
    // 5. VMM.
    // -----------------------------------------------------------------
    let vmm: Arc<dyn Vmm> = match args.vmm {
        VmmChoice::Qemu => {
            let mut cfg = provium_host::vmm::qemu::QemuVmmConfig::defaults();
            cfg.ksm_enabled = !args.no_ksm;
            Arc::new(provium_host::vmm::qemu::QemuVmm::with_config(
                std::sync::Arc::new(provium_host::cid::CidAllocator::new()),
                cfg,
            ))
        }
        VmmChoice::Local => Arc::new(LocalAgentVmm::new()),
    };

    // -----------------------------------------------------------------
    // 6. Dispatch.
    // -----------------------------------------------------------------
    let timeout = if args.timeout == 0 {
        FileTimeout::Disabled
    } else {
        FileTimeout::Wall(Duration::from_secs(args.timeout))
    };
    // Progress UI, built here (before the event sink) so it can be
    // tee'd into the event stream below.
    //
    //   `stream`   — per-file results printed live as each file
    //                finishes. On for any interactive human run;
    //                off for non-TTY / `--json` / `--events-stdout`
    //                (the batch renderer in step 8 handles those,
    //                producing byte-identical-to-before output).
    //   `with_bar` — the animated bar, a subset of `stream`. Also
    //                suppressed by `--no-progress` (explicit
    //                opt-out) and `--verbose` (the bar would fight
    //                a redraw war with the per-VM diagnostic
    //                chatter verbose re-enables — streamed lines
    //                still appear).
    let total_files = discovered.len();
    let interactive =
        std::io::stderr().is_terminal() && !args.json && !args.events_stdout;
    let with_bar = interactive && !args.no_progress && !args.verbose;
    let reporter = Arc::new(ProgressReporter::new(
        total_files,
        args.verbose,
        args.quiet,
        interactive,
        with_bar,
    ));

    // The scheduler's event stream feeds the user-facing sinks
    // (`--save-events`, socket, coverage). When the bar is live,
    // tee it through the reporter too so the bar's text tracks VM
    // boots / test completions in real time.
    let user_sink = build_event_sink(&args)?;
    let event_sink: Arc<dyn EventSink> = if reporter.has_bar() {
        Arc::new(TeeSink {
            primary: user_sink,
            progress: Arc::clone(&reporter) as Arc<dyn EventSink>,
        })
    } else {
        user_sink
    };

    // file_discovered — one event per discovered file before
    // dispatch. R9 mat-M1: scan the file source for `vm_fixture(...)`
    // / `lab_fixture(...)` references so consumers see the
    // build-graph dependencies, not an empty array.
    for p in &discovered {
        let fixture_refs = match std::fs::read_to_string(p) {
            Ok(src) => provium_host::fixture::scan_fixture_deps(&src),
            Err(_) => Vec::new(),
        };
        event_sink.emit(provium_protocol::events::Event::FileDiscovered(
            provium_protocol::events::FileDiscovered {
                path: p.display().to_string(),
                fixture_refs,
                declared_claim: None,
            },
        ));
    }

    // Spawn PSI monitor when running on Linux with /proc/pressure
    // available. The flag is harmless when PSI is absent — the
    // monitor's parser returns None and the flag stays cleared.
    let pressure = if provium_host::scheduler::psi::read_some_avg10().is_some() {
        Some(provium_host::scheduler::spawn_psi_monitor(
            provium_host::scheduler::psi::DEFAULT_PSI_THRESHOLD_PCT,
            provium_host::scheduler::psi::DEFAULT_PSI_INTERVAL,
        ))
    } else {
        None
    };

    let dispatch_opts = DispatchOpts {
        per_file_overhead: ResourceAmount {
            memory_bytes: 50 * 1024 * 1024,
            cpus: 0,
        },
        timeout,
        events: Arc::clone(&event_sink),
        pressure,
        fail_fast: args.fail_fast,
    };

    // Periodic pool_state event emitter.
    let pool_for_emitter = Arc::clone(&pool);
    let sink_for_emitter = Arc::clone(&event_sink);
    let pool_emitter_stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let pool_emitter_stop_clone = Arc::clone(&pool_emitter_stop);
    std::thread::Builder::new()
        .name("provium-pool-state".into())
        .spawn(move || {
            while !pool_emitter_stop_clone.load(std::sync::atomic::Ordering::Relaxed) {
                let total = pool_for_emitter.total();
                let available = pool_for_emitter.available();
                let used = total.saturating_sub(available);
                sink_for_emitter.emit(
                    provium_protocol::events::Event::PoolState(
                        provium_protocol::events::PoolState {
                            used: provium_protocol::events::ResourceAmount {
                                memory_bytes: used.memory_bytes,
                                cpus: used.cpus,
                            },
                            available: provium_protocol::events::ResourceAmount {
                                memory_bytes: available.memory_bytes,
                                cpus: available.cpus,
                            },
                        },
                    ),
                );
                std::thread::sleep(std::time::Duration::from_secs(1));
            }
        })
        .ok();

    // Tag/slow filter envvar — must be set BEFORE dispatch so the
    // runner picks it up.
    let kv_pairs = |pairs: &[(String, String)]| -> serde_json::Value {
        serde_json::Value::Array(
            pairs
                .iter()
                .map(|(k, v)| {
                    serde_json::json!({"key": k, "value": v})
                })
                .collect(),
        )
    };
    let test_filter_json = serde_json::json!({
        "include_slow": args.include_slow,
        "tag": args.tag,
        "no_tag": args.no_tag,
        "tag_meta": kv_pairs(&args.tag_meta),
        "no_tag_meta": kv_pairs(&args.no_tag_meta),
    });
    std::env::set_var(
        "PROVIUM_TEST_FILTER",
        serde_json::to_string(&test_filter_json).unwrap_or_default(),
    );

    // --events-stdout reserves stdout for the msgpack frame
    // stream — the human-readable / JSON renderer redirects to
    // stderr so consumers piping `provium --events-stdout |
    // provium-coverage` don't see human text interleaved into
    // their msgpack parser.
    let route_to_stderr = args.events_stdout;

    // 7. Dispatch. The `reporter` (built earlier so it could be
    //    tee'd into the event stream) streams per-file results
    //    live via the completion callback.
    let started = std::time::Instant::now();
    let results = {
        let reporter = Arc::clone(&reporter);
        dispatch_files_with_progress(
            discovered,
            Arc::clone(&pool),
            config,
            vmm,
            dispatch_opts,
            move |df| reporter.on_file_done(df),
        )
    };
    let elapsed = started.elapsed();
    reporter.finish();
    pool_emitter_stop.store(true, std::sync::atomic::Ordering::Relaxed);

    // -----------------------------------------------------------------
    // 8. Render.
    // -----------------------------------------------------------------
    let mut failed = 0u32;
    if args.json {
        for r in &results {
            let line = render_json(r);
            if route_to_stderr {
                eprintln!("{line}");
            } else {
                println!("{line}");
            }
            if !r.passed() {
                failed += 1;
                if args.fail_fast {
                    break;
                }
            }
        }
    } else {
        if reporter.streamed() {
            // The progress callback already printed each file's
            // outcome live as it completed.
            failed = results.iter().filter(|r| !r.passed()).count() as u32;
        } else {
            for r in &results {
                print_file_outcome(r, args.verbose, args.quiet, route_to_stderr, None);
                if !r.passed() {
                    failed += 1;
                    if args.fail_fast {
                        break;
                    }
                }
            }
        }
        print_summary(&results, elapsed, route_to_stderr);
    }

    // Persist failed-file list so a subsequent --rerun-failed can
    // narrow down. Only written when at least one failure exists;
    // a clean run leaves the previous state alone so the user can
    // still --rerun-failed against an earlier issue.
    let failed_paths: Vec<PathBuf> =
        results.iter().filter(|r| !r.passed()).map(|r| r.path.clone()).collect();
    if !failed_paths.is_empty() {
        save_rerun_failed(&failed_paths);
    }

    // --coverage post-run: pipe the buffered event stream into
    // `provium-coverage` if it's on PATH. Failures here are
    // surfaced as warnings, not test failures — the test results
    // themselves are already the source of truth.
    if args.coverage {
        if let Ok(tmp) = std::env::var("PROVIUM_COVERAGE_TMP") {
            let status = std::process::Command::new("provium-coverage")
                .args(["--from", &tmp])
                .status();
            match status {
                Ok(s) if s.success() => {}
                Ok(s) => {
                    eprintln!("provium-coverage exited with {s}");
                    // Coverage was explicitly asked for; a non-zero
                    // post-run shouldn't disappear into a 0 exit.
                    return Ok(s.code().unwrap_or(1) as u32);
                }
                Err(e) => {
                    eprintln!(
                        "provium-coverage not on PATH: {e}\n\
                         (--coverage was requested; install `provium-coverage` \
                         or drop the flag)"
                    );
                    return Ok(1);
                }
            }
            // Only delete if WE stamped a marker file alongside —
            // protects against externally-set PROVIUM_COVERAGE_TMP
            // pointing at unrelated user files.
            if let Ok(marker) = std::env::var("PROVIUM_COVERAGE_MARKER") {
                let marker_path = std::path::PathBuf::from(&marker);
                if marker_path.exists() {
                    let _ = std::fs::remove_file(&tmp);
                    let _ = std::fs::remove_file(&marker_path);
                }
            }
            std::env::remove_var("PROVIUM_COVERAGE_TMP");
            std::env::remove_var("PROVIUM_COVERAGE_USER_FILE");
            std::env::remove_var("PROVIUM_COVERAGE_MARKER");
        }
    }

    Ok(failed)
}

/// `--watch` mode. Polls the file mtimes of every discovered
/// `*.test.lua` and re-runs on change. Ctrl-C exits.
fn watch_loop(
    scan_roots: Vec<PathBuf>,
    args: Args,
    _config: Arc<Config>,
    initial_paths: Vec<PathBuf>,
) -> Result<u32, Box<dyn std::error::Error>> {
    let mut last_mtimes = mtimes_of(&initial_paths);
    eprintln!("provium --watch: monitoring {} files", initial_paths.len());
    loop {
        std::thread::sleep(std::time::Duration::from_millis(500));
        let current = discover_test_files(&scan_roots, args.filter.as_deref())?;
        let now = mtimes_of(&current);
        if now != last_mtimes {
            eprintln!("provium --watch: change detected, re-running");
            let mut sub = args.clone();
            sub.watch = false;
            sub.cmd = None;
            // Drop --rerun-failed under watch — without this each
            // tick re-runs only the original failed set forever
            // (a clean run leaves the failed-state file
            // untouched), so edits to other files are silently
            // ignored. Mtime detection is the right filter for
            // watch mode.
            sub.rerun_failed = false;
            // Surface internal errors from the run (config load
            // failure, preflight failure, pool panic) so the
            // watch user knows why nothing seems to happen.
            // Test failures are already printed by the runner
            // and arrive as Ok(failed_count) — those don't go
            // through this branch. R9 reg-1: was `let _ =` which
            // silently dropped genuine errors.
            if let Err(e) = run(sub) {
                eprintln!("provium --watch: iteration failed: {e}");
            }
            // Re-snapshot AFTER the run so edits made *during*
            // the run are detected on the next tick (and don't
            // cause spurious second re-runs).
            let post = discover_test_files(&scan_roots, args.filter.as_deref())?;
            last_mtimes = mtimes_of(&post);
        }
    }
}

fn mtimes_of(paths: &[PathBuf]) -> Vec<(PathBuf, std::time::SystemTime)> {
    let mut out: Vec<_> = paths
        .iter()
        .filter_map(|p| {
            std::fs::metadata(p)
                .ok()
                .and_then(|m| m.modified().ok())
                .map(|t| (p.clone(), t))
        })
        .collect();
    out.sort();
    out
}

// ---------------------------------------------------------------------------
// Discovery
// ---------------------------------------------------------------------------

fn discover_test_files(
    roots: &[PathBuf],
    filter: Option<&str>,
) -> Result<Vec<PathBuf>, Box<dyn std::error::Error>> {
    let mut found = Vec::new();
    for root in roots {
        if root.is_file() && is_test_file(root) {
            if filter_matches(filter, root, root) {
                found.push(root.clone());
            }
            continue;
        }
        for entry in walkdir::WalkDir::new(root)
            .follow_links(false)
            .into_iter()
            .filter_map(|e| e.ok())
        {
            if entry.file_type().is_file() && is_test_file(entry.path()) {
                let p = entry.into_path();
                if filter_matches(filter, &p, root) {
                    found.push(p);
                }
            }
        }
    }
    found.sort();
    Ok(found)
}

fn is_test_file(p: &std::path::Path) -> bool {
    p.file_name()
        .and_then(|n| n.to_str())
        .map(|n| n.ends_with(".test.lua"))
        .unwrap_or(false)
}

/// Substring match against the test-root-relative path so the same
/// `--filter` value works whether roots were passed absolute or
/// relative — per `DESIGN.md` § CLI / `--filter`.
fn filter_matches(
    filter: Option<&str>,
    path: &std::path::Path,
    root: &std::path::Path,
) -> bool {
    match filter {
        Some(f) => {
            let rel = path.strip_prefix(root).unwrap_or(path);
            rel.to_string_lossy().contains(f)
        }
        None => true,
    }
}

// ---------------------------------------------------------------------------
// Output rendering
// ---------------------------------------------------------------------------

/// Drives the streamed per-file output (and optional live progress
/// bar) for a `provium tests/` run.
///
/// Built once per run and shared (behind an [`Arc`]) with every
/// dispatcher runner thread: [`Self::on_file_done`] is invoked from
/// the runner thread the moment its file finishes.
///
/// It is *also* an [`EventSink`]: when the bar is active it is
/// tee'd into the scheduler's event stream so the bar's text can
/// reflect live VM / test activity — see [`Self::emit`]. This is
/// what keeps the bar moving during the startup pause, when no file
/// has finished yet but fixtures are restoring and VMs are booting.
///
/// Two independent switches:
///
/// * `stream` — print each file's outcome live as it completes,
///   with an `[i/N]` prefix. Off for non-TTY / `--json` /
///   `--events-stdout` runs, where the batch renderer in `run`
///   produces the (byte-identical-to-before) output instead.
/// * `bar` — the animated [`ProgressBar`]. A strict subset of
///   `stream`: a bar makes no sense without live updates. Off
///   additionally under `--no-progress` and `--verbose` (the
///   latter because the bar would fight a redraw war with the
///   per-VM diagnostic chatter verbose mode re-enables).
struct ProgressReporter {
    /// `Some` only when the animated bar is active.
    bar: Option<ProgressBar>,
    /// Whether to stream per-file outcomes from the callback.
    stream: bool,
    /// Total file count — the bar length and the `/N` in streamed
    /// `[i/N]` prefixes.
    total: usize,
    verbose: bool,
    quiet: bool,
    /// VMs currently up: `vm_spawned` minus `vm_shutdown`, from the
    /// event stream. Signed because shutdown/spawn can race across
    /// threads; the display clamps at 0.
    vms: AtomicI64,
    /// Tests finished so far (passed + failed + skipped), counted
    /// per-test off the event stream so the figure ticks smoothly
    /// rather than jumping a whole file at a time.
    tests: AtomicUsize,
    /// Serialises the print-then-redraw critical section so two
    /// runner threads finishing at once can't interleave output.
    state: Mutex<ProgressState>,
}

/// Mutable tallies behind [`ProgressReporter`]'s mutex.
struct ProgressState {
    /// Files finished so far — drives the `[i/N]` streamed prefix
    /// and the bar position.
    done: usize,
}

impl ProgressReporter {
    fn new(total: usize, verbose: bool, quiet: bool, stream: bool, with_bar: bool) -> Self {
        let bar = (stream && with_bar).then(|| {
            let pb = ProgressBar::new(total as u64);
            pb.set_style(
                ProgressStyle::with_template(
                    "{spinner:.green} [{elapsed_precise}] [{bar:28.cyan/blue}] \
                     {pos}/{len} files · {msg}",
                )
                .unwrap_or_else(|_| ProgressStyle::default_bar())
                .progress_chars("##-"),
            );
            pb.set_message("booting");
            // Steady tick so the spinner + elapsed clock animate
            // even while every runner is mid-file (no completions).
            pb.enable_steady_tick(Duration::from_millis(120));
            pb
        });
        Self {
            bar,
            stream,
            total,
            verbose,
            quiet,
            vms: AtomicI64::new(0),
            tests: AtomicUsize::new(0),
            state: Mutex::new(ProgressState { done: 0 }),
        }
    }

    /// Recompute the bar's trailing text from the live counters.
    ///
    /// `vms` is `vm_spawned` minus `vm_shutdown` — VMs *currently*
    /// up, not a cumulative total (every restore emits a spawn,
    /// every lab teardown the matching shutdown). Before any test
    /// finishes the run is still booting VMs / restoring fixtures,
    /// so the count is the headline; once tests start landing the
    /// `booting` label gives way to the test tally and `vms` moves
    /// to a secondary position.
    fn refresh_message(&self) {
        let Some(bar) = &self.bar else { return };
        let vms = self.vms.load(Ordering::Relaxed).max(0);
        let tests = self.tests.load(Ordering::Relaxed);
        let msg = if tests == 0 {
            if vms == 0 {
                "booting".to_string()
            } else {
                format!("booting · {vms} vms")
            }
        } else {
            format!("{vms} vms · {tests} tests")
        };
        bar.set_message(msg);
    }

    /// Called from a runner thread when one file completes. Streams
    /// that file's outcome and, if the bar is live, advances it.
    fn on_file_done(&self, r: &provium_host::scheduler::dispatch::DispatchedFile) {
        if !self.stream {
            return; // streaming off — batch renderer handles output.
        }
        let idx = {
            let mut st = self.state.lock().unwrap();
            st.done += 1;
            st.done
        };
        // Progress output is ephemeral UI → stdout (`false`), never
        // the events-stdout stderr route, which forces `stream`
        // off anyway.
        let do_print = || {
            print_file_outcome(r, self.verbose, self.quiet, false, Some((idx, self.total)));
        };
        match &self.bar {
            // `suspend` clears the bar for the closure and redraws
            // after, so the line lands cleanly above the bar. The
            // trailing text is event-driven (see `emit`); here we
            // only advance the file position.
            Some(bar) => {
                bar.suspend(do_print);
                bar.set_position(idx as u64);
            }
            None => do_print(),
        }
    }

    /// Tear the bar down before the final summary prints.
    fn finish(&self) {
        if let Some(bar) = &self.bar {
            bar.finish_and_clear();
        }
    }

    /// `true` when per-file output was streamed live, so the caller
    /// should skip the batch render.
    fn streamed(&self) -> bool {
        self.stream
    }

    /// `true` when the animated bar is active — the gate for tee'ing
    /// this reporter into the event stream.
    fn has_bar(&self) -> bool {
        self.bar.is_some()
    }
}

impl provium_host::scheduler::EventSink for ProgressReporter {
    /// Fold scheduler / runner events into the bar's live counters.
    /// Only VM-lifecycle and test-completion events matter here;
    /// everything else is ignored. Called concurrently from runner
    /// threads — the counters are atomics, and `indicatif` does its
    /// own draw synchronisation.
    fn emit(&self, event: provium_protocol::events::Event) {
        use provium_protocol::events::Event;
        match event {
            Event::VmSpawned(_) => {
                self.vms.fetch_add(1, Ordering::Relaxed);
            }
            Event::VmShutdown(_) => {
                self.vms.fetch_sub(1, Ordering::Relaxed);
            }
            Event::TestPassed(_) | Event::TestFailed(_) | Event::TestSkipped(_) => {
                self.tests.fetch_add(1, Ordering::Relaxed);
            }
            _ => return,
        }
        self.refresh_message();
    }
}

/// Two-way [`EventSink`] fan-out. Used to tee the scheduler's event
/// stream into both the user-facing sink (`--save-events`, socket,
/// coverage) and the [`ProgressReporter`] so the progress bar can
/// reflect live activity without disturbing the existing consumers.
struct TeeSink {
    /// The user-configured sink built by `build_event_sink`.
    primary: Arc<dyn provium_host::scheduler::EventSink>,
    /// The progress reporter.
    progress: Arc<dyn provium_host::scheduler::EventSink>,
}

impl provium_host::scheduler::EventSink for TeeSink {
    fn emit(&self, event: provium_protocol::events::Event) {
        self.primary.emit(event.clone());
        self.progress.emit(event);
    }
}

fn print_file_outcome(
    r: &provium_host::scheduler::dispatch::DispatchedFile,
    verbose: bool,
    quiet: bool,
    to_stderr: bool,
    progress: Option<(usize, usize)>,
) {
    macro_rules! out {
        ($($a:tt)*) => {
            if to_stderr {
                eprintln!($($a)*);
            } else {
                println!($($a)*);
            }
        };
    }
    let path_disp = r.path.display();
    // `[done/total] ` prefix when streaming under the progress bar;
    // empty for the batch render so existing output is byte-stable.
    let prefix = match progress {
        Some((done, total)) => format!("[{done}/{total}] "),
        None => String::new(),
    };

    if let Some(err) = &r.outcome.chunk_error {
        out!("{prefix}FAIL {path_disp}");
        out!("  chunk_error: {err}");
        return;
    }

    let (p, f, s) = r.outcome.summary();
    let status_label = if r.timeout == provium_host::scheduler::FileTimeoutOutcome::TimedOut {
        "TIME"
    } else if f > 0 {
        "FAIL"
    } else {
        "PASS"
    };

    if !quiet || f > 0 || r.timeout == provium_host::scheduler::FileTimeoutOutcome::TimedOut {
        out!(
            "{prefix}{status_label} {path_disp} ({p} passed, {f} failed, {s} skipped)"
        );
    }

    for t in &r.outcome.tests {
        match t.status {
            provium_host::lua::TestStatus::Failed => {
                out!("    FAIL {}", t.name);
                if let Some(msg) = &t.message {
                    for line in msg.lines() {
                        out!("        {line}");
                    }
                }
            }
            provium_host::lua::TestStatus::Skipped if verbose => {
                let reason = t.message.as_deref().unwrap_or("");
                out!("    SKIP {} ({reason})", t.name);
            }
            provium_host::lua::TestStatus::Passed if verbose => {
                out!("    PASS {}", t.name);
            }
            _ => {}
        }
    }
}

fn print_summary(
    results: &[provium_host::scheduler::dispatch::DispatchedFile],
    elapsed: Duration,
    to_stderr: bool,
) {
    let mut files = 0;
    let mut p = 0;
    let mut f = 0;
    let mut s = 0;
    for r in results {
        files += 1;
        let (pp, ff, ss) = r.outcome.summary();
        p += pp;
        f += ff;
        s += ss;
    }
    let line = format!(
        "\n{files} file(s); {p} passed, {f} failed, {s} skipped; {:.2}s",
        elapsed.as_secs_f64()
    );
    if to_stderr {
        eprintln!("{line}");
    } else {
        println!("{line}");
    }
}

fn render_json(r: &provium_host::scheduler::dispatch::DispatchedFile) -> String {
    let tests: Vec<serde_json::Value> = r
        .outcome
        .tests
        .iter()
        .map(|t| {
            serde_json::json!({
                "name": t.name,
                "status": match t.status {
                    provium_host::lua::TestStatus::Passed => "passed",
                    provium_host::lua::TestStatus::Failed => "failed",
                    provium_host::lua::TestStatus::Skipped => "skipped",
                },
                "message": t.message,
                "log": t.log,
            })
        })
        .collect();

    let timeout_str = match r.timeout {
        provium_host::scheduler::FileTimeoutOutcome::InTime => "in_time",
        provium_host::scheduler::FileTimeoutOutcome::TimedOut => "timed_out",
    };
    let payload = serde_json::json!({
        "path": r.path.display().to_string(),
        "timeout": timeout_str,
        "passed": r.passed(),
        "chunk_error": r.outcome.chunk_error,
        "tests": tests,
    });
    serde_json::to_string(&payload).unwrap_or_else(|_| "{}".into())
}

// ---------------------------------------------------------------------------
// Misc helpers
// ---------------------------------------------------------------------------

/// Pick the first profile (by sorted name) and return its kernel +
/// initrd paths for cache-key folding. Re-export of the library
/// helper so the CLI doesn't reach into `provium_host::config`.
fn canonical_profile_paths(
    config: &Config,
) -> (Option<PathBuf>, Option<PathBuf>) {
    provium_host::fixture::canonical_profile_paths(config)
}

fn run_fixture_cmd(
    op: &FixtureCmd,
    config: &Config,
    vmm: Arc<dyn Vmm>,
) -> Result<(), Box<dyn std::error::Error>> {
    let cache_dir = config
        .provium
        .cache_dir
        .clone()
        .unwrap_or_else(provium_host::fixture::default_cache_dir);
    match op {
        FixtureCmd::List => {
            if !cache_dir.exists() {
                println!("(no cache directory at {})", cache_dir.display());
                return Ok(());
            }
            let mut entries = Vec::new();
            for d in std::fs::read_dir(&cache_dir)? {
                let d = d?;
                let p = d.path();
                let ext = p.extension().and_then(|s| s.to_str());
                let m = d.metadata()?;
                if m.is_dir()
                    && p.file_name()
                        .and_then(|s| s.to_str())
                        .map(|s| s.ends_with(".lab"))
                        .unwrap_or(false)
                {
                    // Lab fixture: total = sum of files inside.
                    let mut total = 0u64;
                    if let Ok(rd) = std::fs::read_dir(&p) {
                        for inner in rd.flatten() {
                            if let Ok(im) = inner.metadata() {
                                total = total.saturating_add(im.len());
                            }
                        }
                    }
                    entries.push((p, total, "lab"));
                } else if ext == Some("snap") {
                    entries.push((p, m.len(), "vm"));
                }
            }
            entries.sort_by(|a, b| a.0.cmp(&b.0));
            let mut total: u64 = 0;
            for (path, size, kind) in &entries {
                let key = path
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .unwrap_or("?");
                println!("  {:>12}  {kind:<3}  {key}", human_bytes(*size));
                total += *size;
            }
            println!("\n{} entries, {}", entries.len(), human_bytes(total));
        }
        FixtureCmd::Build { path } => {
            // Locate the .fixture.lua under the configured roots.
            let fixture_path = locate(&config.provium.roots, path)?;
            let source = std::fs::read(&fixture_path)?;
            // Match the runtime / Rebuild key shape — fold in
            // transitively-referenced fixture deps and the canonical
            // kernel/initrd identifier. Without this, `fixture build`
            // wrote to a different hash than the one the runner
            // looks up at test time.
            let dep_keys = provium_host::lua::lab_ud_resolve_dep_keys_pub(
                &config.provium.roots,
                &source,
            );
            let (kernel, initrd) = canonical_profile_paths(config);
            let externals = provium_host::lua::lab_ud_resolve_external_deps_pub(
                &config.provium.roots,
                &fixture_path,
                &source,
            );
            let kernels_v: Vec<&std::path::Path> =
                kernel.as_deref().into_iter().collect();
            let initrds_v: Vec<&std::path::Path> =
                initrd.as_deref().into_iter().collect();
            let external_refs: Vec<&std::path::Path> =
                externals.iter().map(|p| p.as_path()).collect();
            let key = provium_host::fixture::compute_key_with_deps_kernels_and_externals(
                &source,
                &dep_keys,
                &kernels_v,
                &initrds_v,
                &external_refs,
            );
            let entry = provium_host::fixture::CacheEntryPaths::for_key(&cache_dir, &key);
            if entry.snapshot.exists() {
                println!("  already built: {}", entry.snapshot.display());
            } else {
                std::fs::create_dir_all(&cache_dir)?;
                // Acquire the per-key build lock before spawning the
                // build VM. Without this a concurrent test runner
                // building the same fixture could race the install
                // step and leave torn writes.
                let _lock = provium_host::fixture::acquire_build_lock(&entry.lock)
                    .map_err(|e| Box::new(e) as Box<dyn std::error::Error>)?;
                // Re-check after acquiring — the runner may have
                // built it while we were queuing.
                if entry.snapshot.exists() {
                    println!("  already built: {}", entry.snapshot.display());
                    return Ok(());
                }
                let cfg_arc = std::sync::Arc::new(config.clone());
                let outcome = provium_host::lua::fixture_build::build_fixture(
                    &fixture_path,
                    cfg_arc,
                    Arc::clone(&vmm),
                )?;
                match outcome {
                    provium_host::lua::fixture_build::FixtureBuildOutcome::SingleVm {
                        snapshot_path,
                    } => {
                        // Match the runtime build pipeline: sparse +
                        // zstd before installing. CLI-built fixtures
                        // were 3–10x larger and didn't match the
                        // runner's compressed format.
                        let _ = provium_host::perf::make_sparse(&snapshot_path);
                        let zst_path = entry.snapshot.with_extension("snap.zst");
                        if provium_host::perf::compress_zst(&snapshot_path, &zst_path).is_ok() {
                            let _ = std::fs::rename(&zst_path, &entry.snapshot);
                            let _ = std::fs::remove_file(&snapshot_path);
                        } else if std::fs::rename(&snapshot_path, &entry.snapshot).is_err() {
                            std::fs::copy(&snapshot_path, &entry.snapshot)?;
                            let _ = std::fs::remove_file(&snapshot_path);
                        }
                        println!("  built: {}", entry.snapshot.display());
                    }
                    provium_host::lua::fixture_build::FixtureBuildOutcome::Lab {
                        snapshot_dir,
                    } => {
                        let lab_dir = cache_dir
                            .join(format!("{}.lab", entry.snapshot.file_stem().unwrap_or_default().to_string_lossy()));
                        let _ = std::fs::remove_dir_all(&lab_dir);
                        std::fs::rename(&snapshot_dir, &lab_dir)?;
                        println!("  built lab fixture: {}", lab_dir.display());
                    }
                }
            }
        }
        FixtureCmd::Rebuild { path } => {
            // Use the same dep+kernel folded key as the runner so
            // we evict the cache entry the runner would actually use.
            let fixture_path = locate(&config.provium.roots, path)?;
            let source = std::fs::read(&fixture_path)?;
            let dep_keys = provium_host::lua::lab_ud_resolve_dep_keys_pub(
                &config.provium.roots,
                &source,
            );
            let (kernel, initrd) = canonical_profile_paths(config);
            let externals = provium_host::lua::lab_ud_resolve_external_deps_pub(
                &config.provium.roots,
                &fixture_path,
                &source,
            );
            let kernels_v: Vec<&std::path::Path> =
                kernel.as_deref().into_iter().collect();
            let initrds_v: Vec<&std::path::Path> =
                initrd.as_deref().into_iter().collect();
            let external_refs: Vec<&std::path::Path> =
                externals.iter().map(|p| p.as_path()).collect();
            let key = provium_host::fixture::compute_key_with_deps_kernels_and_externals(
                &source,
                &dep_keys,
                &kernels_v,
                &initrds_v,
                &external_refs,
            );
            let entry = provium_host::fixture::CacheEntryPaths::for_key(&cache_dir, &key);
            let _ = std::fs::remove_file(&entry.snapshot);
            let _ = std::fs::remove_file(&entry.lock);
            // Lab-fixture sibling.
            let lab_dir = cache_dir.join(format!("{}.lab", key));
            let lab_lock = cache_dir.join(format!("{}.lab.lock", key));
            let _ = std::fs::remove_dir_all(&lab_dir);
            let _ = std::fs::remove_file(&lab_lock);
            println!("  evicted cache entry for `{path}`; rebuilding…");
            // Actually rebuild — DESIGN's "Force a rebuild" wording
            // implies the rebuild happens NOW, not at the next run.
            std::fs::create_dir_all(&cache_dir)?;
            let _lock = provium_host::fixture::acquire_build_lock(&entry.lock)
                .map_err(|e| Box::new(e) as Box<dyn std::error::Error>)?;
            let cfg_arc = std::sync::Arc::new(config.clone());
            let outcome = provium_host::lua::fixture_build::build_fixture(
                &fixture_path,
                cfg_arc,
                Arc::clone(&vmm),
            )?;
            match outcome {
                provium_host::lua::fixture_build::FixtureBuildOutcome::SingleVm {
                    snapshot_path,
                } => {
                    let _ = provium_host::perf::make_sparse(&snapshot_path);
                    let zst_path = entry.snapshot.with_extension("snap.zst");
                    if provium_host::perf::compress_zst(&snapshot_path, &zst_path).is_ok() {
                        let _ = std::fs::rename(&zst_path, &entry.snapshot);
                        let _ = std::fs::remove_file(&snapshot_path);
                    } else if std::fs::rename(&snapshot_path, &entry.snapshot).is_err() {
                        std::fs::copy(&snapshot_path, &entry.snapshot)?;
                        let _ = std::fs::remove_file(&snapshot_path);
                    }
                    println!("  rebuilt: {}", entry.snapshot.display());
                }
                provium_host::lua::fixture_build::FixtureBuildOutcome::Lab {
                    snapshot_dir,
                } => {
                    std::fs::rename(&snapshot_dir, &lab_dir)?;
                    println!("  rebuilt lab fixture: {}", lab_dir.display());
                }
            }
        }
        FixtureCmd::Clean => {
            if cache_dir.exists() {
                let mut count = 0u64;
                for d in std::fs::read_dir(&cache_dir)? {
                    let d = d?;
                    let p = d.path();
                    let ok = if d.metadata().map(|m| m.is_dir()).unwrap_or(false) {
                        std::fs::remove_dir_all(&p)
                    } else {
                        std::fs::remove_file(&p)
                    };
                    match ok {
                        Ok(()) => count += 1,
                        Err(e) => eprintln!("  skipping {}: {e}", p.display()),
                    }
                }
                println!("  removed {count} cache entries");
            } else {
                println!("  cache directory does not exist");
            }
        }
        FixtureCmd::Stale => {
            // A "stale" fixture is one whose .fixture.lua source
            // hashes to a key not present in the cache. Both single-VM
            // (.snap) and lab (.lab/) cache layouts are checked.
            let mut found = 0u64;
            let (kernel, initrd) = canonical_profile_paths(config);
            for root in &config.provium.roots {
                let root_path = std::path::Path::new(root);
                if !root_path.exists() {
                    continue;
                }
                for entry in walkdir::WalkDir::new(root_path)
                    .into_iter()
                    .filter_map(|e| e.ok())
                {
                    if entry.file_type().is_file()
                        && entry
                            .path()
                            .file_name()
                            .and_then(|s| s.to_str())
                            .map(|s| s.ends_with(".fixture.lua"))
                            .unwrap_or(false)
                    {
                        let source = std::fs::read(entry.path())?;
                        let dep_keys =
                            provium_host::lua::lab_ud_resolve_dep_keys_pub(
                                &config.provium.roots,
                                &source,
                            );
                        let externals =
                            provium_host::lua::lab_ud_resolve_external_deps_pub(
                                &config.provium.roots,
                                entry.path(),
                                &source,
                            );
                        let kernels_v: Vec<&std::path::Path> =
                            kernel.as_deref().into_iter().collect();
                        let initrds_v: Vec<&std::path::Path> =
                            initrd.as_deref().into_iter().collect();
                        let external_refs: Vec<&std::path::Path> =
                            externals.iter().map(|p| p.as_path()).collect();
                        let key =
                            provium_host::fixture::compute_key_with_deps_kernels_and_externals(
                                &source,
                                &dep_keys,
                                &kernels_v,
                                &initrds_v,
                                &external_refs,
                            );
                        let cache_entry =
                            provium_host::fixture::CacheEntryPaths::for_key(
                                &cache_dir, &key,
                            );
                        let lab_dir = cache_dir.join(format!("{key}.lab"));
                        if !cache_entry.snapshot.exists() && !lab_dir.exists() {
                            println!("  {}", entry.path().display());
                            found += 1;
                        }
                    }
                }
            }
            println!("\n{found} stale fixture(s)");
        }
    }
    Ok(())
}

fn run_lsp_setup(
    dir: &std::path::Path,
    force: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    if !dir.is_dir() {
        return Err(format!("`{}` is not a directory", dir.display()).into());
    }
    let meta_dir = dir.join(".provium-meta");
    let types_path = meta_dir.join("types.lua");
    let luarc_path = dir.join(".luarc.json");

    std::fs::create_dir_all(&meta_dir)?;
    std::fs::write(&types_path, provium_host::lsp_meta::TYPES_LUA)?;
    println!("  wrote {}", types_path.display());

    if luarc_path.exists() && !force {
        // Don't surprise the user — a pre-existing `.luarc.json`
        // probably has project-specific settings. Print the
        // recommended config so they can merge by hand.
        println!(
            "  {} already exists; not overwriting (pass --force to replace).",
            luarc_path.display(),
        );
        println!("  to enable provium types, merge this into the existing config:");
        println!();
        for line in provium_host::lsp_meta::LUARC_JSON.lines() {
            println!("    {line}");
        }
        return Ok(());
    }
    std::fs::write(&luarc_path, provium_host::lsp_meta::LUARC_JSON)?;
    println!("  wrote {}", luarc_path.display());
    println!();
    println!(
        "  Reload your editor's Lua server. `test`, `provium`, \
         `wait_until`, and `json` should now be recognised."
    );
    Ok(())
}

fn list_command(config: &Config, fixtures: bool) -> Result<(), Box<dyn std::error::Error>> {
    let suffix = if fixtures { ".fixture.lua" } else { ".test.lua" };
    let roots: Vec<PathBuf> = if config.provium.roots.is_empty() {
        vec![std::env::current_dir()?]
    } else {
        config.provium.roots.iter().map(PathBuf::from).collect()
    };
    let mut count = 0u64;
    for root in roots {
        for entry in walkdir::WalkDir::new(&root)
            .into_iter()
            .filter_map(|e| e.ok())
        {
            if entry.file_type().is_file()
                && entry
                    .path()
                    .file_name()
                    .and_then(|s| s.to_str())
                    .map(|s| s.ends_with(suffix))
                    .unwrap_or(false)
            {
                println!("  {}", entry.path().display());
                count += 1;
            }
        }
    }
    println!("\n{count} {} found", if fixtures { "fixture(s)" } else { "test(s)" });
    Ok(())
}

fn locate(roots: &[String], name: &str) -> Result<PathBuf, Box<dyn std::error::Error>> {
    for r in roots.iter().map(PathBuf::from) {
        let p = r.join(format!("{name}.fixture.lua"));
        if p.is_file() {
            return Ok(p);
        }
    }
    Err(format!("fixture `{name}` not found in any test root").into())
}

fn human_bytes(n: u64) -> String {
    const UNITS: &[&str] = &["B", "KiB", "MiB", "GiB", "TiB"];
    let mut v = n as f64;
    let mut i = 0;
    while v >= 1024.0 && i < UNITS.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    format!("{v:.2}{}", UNITS[i])
}

fn build_event_sink(args: &Args) -> Result<Arc<dyn EventSink>, Box<dyn std::error::Error>> {
    let mut sinks: Vec<Box<dyn EventSink>> = Vec::new();
    if let Some(path) = &args.save_events {
        // Under --watch, truncate at each iteration so the file
        // contains exactly the most recent run. Otherwise every
        // iteration's frames concatenate (DESIGN.md says
        // --save-events is "for later replay or analysis", which
        // implies one run per file).
        sinks.push(Box::new(WriteSink::file_with_mode(path, args.watch)?));
    }
    if args.events_stdout {
        sinks.push(Box::new(WriteSink::new(std::io::BufWriter::new(
            std::io::stdout(),
        ))));
    }
    // --coverage: needs an event file to feed `provium-coverage`.
    // If --save-events is also set, reuse that file; otherwise tee
    // events to a scratch file. The chosen path is recorded in
    // PROVIUM_COVERAGE_TMP for the post-run hook.
    if args.coverage {
        if let Some(path) = &args.save_events {
            std::env::set_var("PROVIUM_COVERAGE_TMP", path);
            // Mark as "user-owned" so the post-run hook doesn't
            // delete it.
            std::env::set_var("PROVIUM_COVERAGE_USER_FILE", "1");
        } else {
            let tmp = std::env::temp_dir().join(format!(
                "provium-coverage-{}.msgpack",
                std::process::id()
            ));
            std::env::set_var("PROVIUM_COVERAGE_TMP", &tmp);
            // Stamp a marker so the post-run cleanup knows
            // *we* created this file (vs an externally-set
            // PROVIUM_COVERAGE_TMP pointing at a user file).
            let marker = tmp.with_extension("msgpack.marker");
            let _ = std::fs::write(&marker, std::process::id().to_string());
            std::env::set_var("PROVIUM_COVERAGE_MARKER", &marker);
            // Register a process-wide cleanup guard so SIGINT or
            // a panic mid-run still removes the temp file (and
            // its marker) instead of leaking one msgpack per
            // killed CI run into $TMPDIR.
            register_coverage_temp_cleanup(&tmp, &marker);
            sinks.push(Box::new(WriteSink::file(&tmp)?));
        }
    }
    if let Some(sock_path) = &args.events_socket {
        // Reuse the same UnixSocketSink across watch iterations so
        // connected dashboard clients aren't dropped on every
        // re-run. The first call binds; subsequent calls return a
        // cheap clone that fans out to the same client list.
        let sink = events_socket_sink(sock_path)?;
        sinks.push(Box::new(sink));
    }
    Ok(if sinks.is_empty() {
        Arc::new(NullSink) as Arc<dyn EventSink>
    } else if sinks.len() == 1 {
        Arc::from(sinks.pop().unwrap())
    } else {
        Arc::new(provium_host::scheduler::events::MultiSink::new(sinks))
    })
}

const RERUN_STATE_REL: &str = ".cache/provium/rerun.json";

fn rerun_state_path() -> PathBuf {
    if let Ok(p) = std::env::var("PROVIUM_RERUN_STATE") {
        return PathBuf::from(p);
    }
    if let Ok(home) = std::env::var("HOME") {
        return PathBuf::from(home).join(RERUN_STATE_REL);
    }
    PathBuf::from("/tmp/provium-rerun.json")
}

fn load_rerun_failed() -> Vec<PathBuf> {
    let path = rerun_state_path();
    let bytes = match std::fs::read(&path) {
        Ok(b) => b,
        Err(_) => return Vec::new(),
    };
    serde_json::from_slice::<Vec<PathBuf>>(&bytes).unwrap_or_default()
}

fn save_rerun_failed(paths: &[PathBuf]) {
    let path = rerun_state_path();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    // Canonicalise so subsequent --rerun-failed works regardless
    // of the user's cwd at re-run time.
    let canon: Vec<PathBuf> = paths
        .iter()
        .map(|p| std::fs::canonicalize(p).unwrap_or_else(|_| p.clone()))
        .collect();
    let Ok(bytes) = serde_json::to_vec(&canon) else {
        return;
    };
    // Atomic install: write to a tmp file in the same directory
    // then rename. Without this a concurrent reader between the
    // truncate and the write sees a 0-byte file →
    // serde_json::from_slice returns empty → all files run
    // instead of the intended failed set.
    let tmp = path.with_extension(format!("tmp.{}", std::process::id()));
    if std::fs::write(&tmp, &bytes).is_err() {
        return;
    }
    let _ = std::fs::rename(&tmp, &path);
}

/// Parse `--timeout` value: integer seconds or a duration string
/// (`"10m"`, `"30s"`, `"500ms"`, `"2h"`). Returns whole seconds.
/// R9 mat-M2: clap was using a bare `u64` parser which rejected
/// the unit-suffix forms DESIGN documents.
fn parse_timeout_arg(s: &str) -> Result<u64, String> {
    let s = s.trim();
    // Bare integer = seconds.
    if let Ok(n) = s.parse::<u64>() {
        return Ok(n);
    }
    let (num, unit_secs) = if let Some(stripped) = s.strip_suffix("ms") {
        // Round up to whole seconds; never round a non-zero
        // duration down to 0.
        let ms: f64 = stripped.trim().parse()
            .map_err(|_| format!("--timeout: cannot parse `{s}`"))?;
        let secs = (ms / 1000.0).ceil() as u64;
        return Ok(secs.max(if ms > 0.0 { 1 } else { 0 }));
    } else if let Some(stripped) = s.strip_suffix('s') {
        (stripped.trim(), 1u64)
    } else if let Some(stripped) = s.strip_suffix('m') {
        (stripped.trim(), 60u64)
    } else if let Some(stripped) = s.strip_suffix('h') {
        (stripped.trim(), 3600u64)
    } else {
        return Err(format!(
            "--timeout: cannot parse `{s}` (try `300`, `30s`, `10m`, `2h`)"
        ));
    };
    let n: u64 = num
        .parse()
        .map_err(|_| format!("--timeout: cannot parse `{s}`"))?;
    n.checked_mul(unit_secs)
        .ok_or_else(|| format!("--timeout: `{s}` overflows u64"))
}

fn parse_size(s: &str) -> Result<u64, String> {
    let s = s.trim();
    let (num, multiplier) = match s.chars().last() {
        Some('K') | Some('k') => (&s[..s.len() - 1], 1024u64),
        Some('M') | Some('m') => (&s[..s.len() - 1], 1024 * 1024),
        Some('G') | Some('g') => (&s[..s.len() - 1], 1024 * 1024 * 1024),
        Some('T') | Some('t') => (&s[..s.len() - 1], 1024u64.pow(4)),
        _ => (s, 1),
    };
    let n: u64 = num
        .parse()
        .map_err(|_| format!("cannot parse size `{s}`"))?;
    // checked_mul so absurd values like "16777216T" don't wrap
    // to 0 silently — that wrap would then trip the
    // zero-pool-budget bug in detected_pool_memory_bytes.
    n.checked_mul(multiplier)
        .ok_or_else(|| format!("size `{s}` overflows u64"))
}
