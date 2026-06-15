//! Multi-file dispatcher.
//!
//! Spins up one runner thread per `*.test.lua`, gates each on a
//! [`super::pool::Pool`] reservation, and aggregates per-file
//! outcomes. Adds two design-mandated guard rails on top of
//! [`crate::lua::run_file`]:
//!
//! 1. **Per-file timeout watchdog** — if the file's tests aren't
//!    finished in `opts.timeout`, the watchdog tears every VM the
//!    file owns down via [`crate::lab::Lab::shutdown`]. The
//!    blocked Lua op (almost always inside `vm:run`) sees a
//!    connection error, raises, and the runner thread exits.
//!
//! 2. **Panic isolation** — Rust panics inside the runner thread
//!    are caught with [`std::panic::catch_unwind`]. A panicked file
//!    is reported with `chunk_error: Some("panicked: ...")`; the
//!    process keeps running.
//!
//! ## What slice 5 doesn't do (yet)
//!
//! * No `claim()` wiring on the Lua side — files always reserve
//!   `opts.per_file_overhead` and that's it. The big resource cost
//!   is per-VM memory at boot, which goes through the pool indirectly
//!   in slice 5.5+ when [`crate::vmm::Vmm`]s grow a `&Pool` arg.
//! * No SCHED_BATCH on QEMU children — slice 5.5.
//! * No PSI-driven adaptive throttling — slice 5.5.

use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use provium_protocol::events::{
    Event, FileBlocked, FileCompleted, FileDispatched, FileStatus,
    ResourceAmount as EventResourceAmount,
};
// `ClaimReleased` is referenced by the file-end emit below.
#[allow(unused_imports)]
use provium_protocol::events::ClaimReleased;

use crate::lab::Lab;
use crate::lua::{run_file_full, FileOutcome};
use crate::profile::Config;
use crate::scheduler::events::{EventSink, NullSink};
use crate::scheduler::pool::{Pool, ResourceAmount};
use crate::scheduler::psi::PressureFlag;
use crate::vmm::Vmm;

/// Dispatcher tunables.
#[derive(Clone)]
pub struct DispatchOpts {
    /// Resources reserved for the runner thread itself (mlua state,
    /// host-side Lua memory). Released when the file completes.
    /// Default 50 MiB / 0 cpus per the design.
    pub per_file_overhead: ResourceAmount,
    /// Per-file timeout — fires the watchdog. Default 5 minutes per
    /// the design.
    pub timeout: FileTimeout,
    /// Where per-test + per-file events go. Defaults to a no-op
    /// sink — set to a [`crate::scheduler::WriteSink`] to persist
    /// the stream, or [`crate::scheduler::MultiSink`] to fan out to
    /// stdout + a file simultaneously.
    pub events: Arc<dyn EventSink>,
    /// PSI throttle flag. When `Some(flag)` and `flag.is_pressured()`
    /// returns `true`, the dispatcher pauses pulling new files until
    /// the flag clears. `None` disables PSI throttling.
    pub pressure: Option<PressureFlag>,
    /// Stop dispatching new files after the first failed file. Per
    /// `DESIGN.md` § CLI / `--fail-fast`. Threads already running
    /// run to completion; subsequent files are skipped.
    pub fail_fast: bool,
}

impl std::fmt::Debug for DispatchOpts {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DispatchOpts")
            .field("per_file_overhead", &self.per_file_overhead)
            .field("timeout", &self.timeout)
            .finish()
    }
}

impl Default for DispatchOpts {
    fn default() -> Self {
        Self {
            per_file_overhead: ResourceAmount {
                memory_bytes: 50 * 1024 * 1024,
                cpus: 0,
            },
            timeout: FileTimeout::Wall(Duration::from_secs(5 * 60)),
            events: Arc::new(NullSink),
            pressure: None,
            fail_fast: false,
        }
    }
}

/// Per-file timeout policy.
#[derive(Clone, Copy, Debug)]
pub enum FileTimeout {
    /// Wall-clock timeout per file.
    Wall(Duration),
    /// No timeout. Used in tests where the file is bounded by
    /// other means.
    Disabled,
}

/// Outcome flag added to a file when the watchdog fired.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FileTimeoutOutcome {
    /// File finished within the timeout. Standard case.
    InTime,
    /// Watchdog fired — the [`FileOutcome`]'s tests probably show
    /// failures from the forced shutdown.
    TimedOut,
}

/// Per-file aggregate returned by [`dispatch_files`]. Wraps the
/// inner [`FileOutcome`] with dispatcher-level metadata.
#[derive(Debug)]
pub struct DispatchedFile {
    /// File path.
    pub path: PathBuf,
    /// Whether the watchdog fired.
    pub timeout: FileTimeoutOutcome,
    /// Inner file outcome — populated whether or not the file
    /// timed out. On panic, `chunk_error` is `Some("panicked: …")`.
    pub outcome: FileOutcome,
}

impl DispatchedFile {
    /// `true` if the file ran cleanly to completion with no failed
    /// tests. Convenience for an `--exit-code` style summary.
    pub fn passed(&self) -> bool {
        self.timeout == FileTimeoutOutcome::InTime && self.outcome.all_succeeded()
    }
}

/// Dispatch every file in `paths` against `(config, vmm)`.
///
/// Threads run in parallel up to `pool` allows. Each thread:
///
/// 1. Acquires `opts.per_file_overhead` from the pool (blocks until
///    available).
/// 2. Builds a fresh per-file [`Lab`].
/// 3. Spawns the watchdog thread (if `opts.timeout` is `Wall`).
/// 4. Runs the file via [`run_file_with_lab`] inside `catch_unwind`.
/// 5. Cancels the watchdog, shuts the lab down, releases the
///    reservation.
///
/// Returns one [`DispatchedFile`] per input path, in input order.
pub fn dispatch_files(
    paths: Vec<PathBuf>,
    pool: Arc<Pool>,
    config: Arc<Config>,
    vmm: Arc<dyn Vmm>,
    opts: DispatchOpts,
) -> Vec<DispatchedFile> {
    dispatch_files_with_progress(paths, pool, config, vmm, opts, |_| {})
}

/// As [`dispatch_files`] but invokes `on_complete` with each file's
/// [`DispatchedFile`] the moment that file's runner thread finishes
/// — before the join. This is what drives the CLI's live progress
/// bar / streamed per-file output: results would otherwise only be
/// observable in a batch once every thread has joined.
///
/// `on_complete` runs on the runner thread, so it may be called
/// concurrently from several threads; the callback is responsible
/// for its own synchronisation. The returned `Vec` is still in
/// input order regardless of completion order.
pub fn dispatch_files_with_progress<F>(
    paths: Vec<PathBuf>,
    pool: Arc<Pool>,
    config: Arc<Config>,
    vmm: Arc<dyn Vmm>,
    opts: DispatchOpts,
    on_complete: F,
) -> Vec<DispatchedFile>
where
    F: Fn(&DispatchedFile) + Send + Sync + 'static,
{
    // Spawn a thread per file. They block on pool.acquire so the
    // effective concurrency is bounded by the pool's CPU/memory
    // budget plus per_file_overhead.
    //
    // `--fail-fast`: a shared flag is flipped by the first failing
    // runner. New files entering run_one_file see the flag set and
    // short-circuit with a `Skipped` outcome. Already-running files
    // are not interrupted (their teardown still runs cleanly).
    let stop_flag = Arc::new(AtomicBool::new(false));
    let on_complete = Arc::new(on_complete);
    let mut handles = Vec::with_capacity(paths.len());
    for path in paths {
        let pool = Arc::clone(&pool);
        let config = Arc::clone(&config);
        let vmm = Arc::clone(&vmm);
        let opts = opts.clone();
        let stop = Arc::clone(&stop_flag);
        let on_complete = Arc::clone(&on_complete);
        handles.push(thread::spawn(move || {
            let result = if opts.fail_fast && stop.load(Ordering::SeqCst) {
                DispatchedFile {
                    path: path.clone(),
                    timeout: FileTimeoutOutcome::InTime,
                    outcome: FileOutcome {
                        path,
                        chunk_error: Some("skipped: --fail-fast".into()),
                        tests: Vec::new(),
                    },
                }
            } else {
                let r = run_one_file(path, pool, config, vmm, opts.clone());
                if opts.fail_fast && !r.passed() {
                    stop.store(true, Ordering::SeqCst);
                }
                r
            };
            // Report completion before returning so the progress
            // consumer sees it in real time, not at join.
            on_complete(&result);
            result
        }));
    }

    handles.into_iter().map(|h| h.join().unwrap_or_else(panic_to_dispatched)).collect()
}

fn run_one_file(
    path: PathBuf,
    pool: Arc<Pool>,
    config: Arc<Config>,
    vmm: Arc<dyn Vmm>,
    opts: DispatchOpts,
) -> DispatchedFile {
    // 0. PSI throttle. Wait for pressure to drop before even
    //    attempting acquisition. Polls the flag rather than blocking
    //    on a condvar so the dispatch latency stays bounded.
    if let Some(pressure) = &opts.pressure {
        let mut announced = false;
        while pressure.is_pressured() {
            if !announced {
                opts.events.emit(Event::FileBlocked(FileBlocked {
                    path: path.to_string_lossy().into_owned(),
                    waiting_for: EventResourceAmount {
                        memory_bytes: opts.per_file_overhead.memory_bytes,
                        cpus: opts.per_file_overhead.cpus,
                    },
                    reason: "psi_pressure".into(),
                }));
                announced = true;
            }
            thread::sleep(Duration::from_millis(200));
        }
    }

    // 1. Pool reservation. Held until the dispatched-file goes out
    //    of scope at function end (RAII). If `try_acquire` would
    //    block, emit a file_blocked event first.
    let reservation = match pool.try_acquire(opts.per_file_overhead) {
        Some(r) => r,
        None => {
            opts.events.emit(Event::FileBlocked(FileBlocked {
                path: path.to_string_lossy().into_owned(),
                waiting_for: EventResourceAmount {
                    memory_bytes: opts.per_file_overhead.memory_bytes,
                    cpus: opts.per_file_overhead.cpus,
                },
                reason: "pool_full".into(),
            }));
            match pool.acquire(opts.per_file_overhead) {
                Some(r) => r,
                None => {
                    return DispatchedFile {
                        path: path.clone(),
                        timeout: FileTimeoutOutcome::InTime,
                        outcome: FileOutcome {
                            path,
                            chunk_error: Some(format!(
                                "per-file overhead {:?} exceeds total pool budget {:?}",
                                opts.per_file_overhead,
                                pool.total(),
                            )),
                            tests: Vec::new(),
                        },
                    };
                }
            }
        }
    };
    let _hold = reservation; // explicit name for clarity

    // 2. Per-file lab — the watchdog needs a clone to tear down.
    //    Wired with the run's event sink so fixture resumes emit
    //    `vm_spawned` (the sink is inherited by sub-labs + the
    //    per-test scope labs).
    let root_lab = Lab::new_with_pool_and_events(
        "provium",
        Arc::clone(&config),
        Arc::clone(&vmm),
        Some(Arc::clone(&pool)),
        Arc::clone(&opts.events),
    );

    // 3. Watchdog (if enabled).
    let completed = Arc::new(AtomicBool::new(false));
    let watchdog = match opts.timeout {
        FileTimeout::Disabled => None,
        FileTimeout::Wall(timeout) => {
            let completed = Arc::clone(&completed);
            let lab = root_lab.clone();
            Some(thread::spawn(move || {
                let deadline = Instant::now() + timeout;
                while Instant::now() < deadline {
                    if completed.load(Ordering::SeqCst) {
                        return false;
                    }
                    thread::sleep(Duration::from_millis(50));
                }
                if completed.load(Ordering::SeqCst) {
                    return false;
                }
                // Force every VM down. Errors are non-fatal — a VM
                // that's already shut down returns Ok; one in
                // Created state transitions cleanly.
                let _ = lab.shutdown();
                true
            }))
        }
    };

    // 3.5. Emit FileDispatched.
    let started = Instant::now();
    opts.events.emit(Event::FileDispatched(FileDispatched {
        path: path.to_string_lossy().into_owned(),
        reservation: EventResourceAmount {
            memory_bytes: opts.per_file_overhead.memory_bytes,
            cpus: opts.per_file_overhead.cpus,
        },
    }));

    // 4. Run the file, catching panics.
    let path_clone = path.clone();
    let lab_clone = root_lab.clone();
    let sink_for_runner = Arc::clone(&opts.events);
    let pool_for_runner = Arc::clone(&pool);
    let result = catch_unwind(AssertUnwindSafe(|| {
        run_file_full(
            &path_clone,
            config,
            vmm,
            lab_clone,
            sink_for_runner,
            Some(pool_for_runner),
        )
    }));

    // 5. Tell the watchdog we're done; collect its decision.
    completed.store(true, Ordering::SeqCst);
    let timed_out = watchdog
        .and_then(|h| h.join().ok())
        .map(|fired| {
            if fired {
                FileTimeoutOutcome::TimedOut
            } else {
                FileTimeoutOutcome::InTime
            }
        })
        .unwrap_or(FileTimeoutOutcome::InTime);

    // 6. Release any file-scope `lab:claim()` and emit
    //    `claim_released`. Done before lab shutdown so the event
    //    fires regardless of shutdown errors.
    if let Some(amount) = root_lab.release_claim() {
        opts.events.emit(Event::ClaimReleased(
            provium_protocol::events::ClaimReleased {
                path: path.display().to_string(),
                amount: EventResourceAmount {
                    memory_bytes: amount.memory_bytes,
                    cpus: amount.cpus,
                },
            },
        ));
    }

    // 7. Lab shutdown — covers the case where the runner exited
    //    normally without a vm:shutdown() in the test code.
    //    Emit `vm_shutdown` for every VM that's still alive so
    //    consumers see the same event sequence regardless of
    //    whether the test's Lua chose to shut down explicitly.
    {
        let alive: Vec<(String, crate::Vm)> = root_lab.collect_all_vms();
        for (vm_name, vm) in &alive {
            // Only the still-running ones; an explicit
            // vm:shutdown() in test code already emitted.
            use crate::VmState;
            if matches!(vm.state(), VmState::Booted | VmState::Paused) {
                opts.events.emit(Event::VmShutdown(
                    provium_protocol::events::VmShutdown {
                        file: path.display().to_string(),
                        vm_name: vm_name.clone(),
                        duration_ns: 0,
                    },
                ));
            }
        }
    }
    let _ = root_lab.shutdown();

    // 7. Build the dispatched outcome.
    let dispatched = match result {
        Ok(Ok(outcome)) => DispatchedFile {
            path: path.clone(),
            timeout: timed_out,
            outcome,
        },
        Ok(Err(mlua_err)) => DispatchedFile {
            path: path.clone(),
            timeout: timed_out,
            outcome: FileOutcome {
                path: path.clone(),
                chunk_error: Some(format!("runner: {mlua_err}")),
                tests: Vec::new(),
            },
        },
        Err(panic_payload) => {
            let msg = panic_message(&panic_payload);
            DispatchedFile {
                path: path.clone(),
                timeout: timed_out,
                outcome: FileOutcome {
                    path: path.clone(),
                    chunk_error: Some(format!("panicked: {msg}")),
                    tests: Vec::new(),
                },
            }
        }
    };

    // 8. Emit FileCompleted with the resolved status.
    let status = if dispatched.timeout == FileTimeoutOutcome::TimedOut {
        FileStatus::TimedOut
    } else if dispatched.outcome.chunk_error.is_some() {
        FileStatus::Crashed
    } else if dispatched.outcome.all_succeeded() {
        FileStatus::Passed
    } else {
        FileStatus::Failed
    };
    opts.events.emit(Event::FileCompleted(FileCompleted {
        path: path.to_string_lossy().into_owned(),
        status,
        duration_ns: u64::try_from(started.elapsed().as_nanos()).unwrap_or(u64::MAX),
    }));

    dispatched
}

fn panic_message(payload: &Box<dyn std::any::Any + Send>) -> String {
    if let Some(s) = payload.downcast_ref::<&'static str>() {
        (*s).to_owned()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        "<non-string panic payload>".to_owned()
    }
}

/// Synthesise a DispatchedFile if the dispatcher's *own* worker
/// thread panicked (not the file's mlua runner — that's caught
/// inside). Should never happen in correct code.
fn panic_to_dispatched(_e: Box<dyn std::any::Any + Send>) -> DispatchedFile {
    DispatchedFile {
        path: PathBuf::from("<unknown>"),
        timeout: FileTimeoutOutcome::InTime,
        outcome: FileOutcome {
            path: PathBuf::from("<unknown>"),
            chunk_error: Some("dispatcher worker thread panicked".into()),
            tests: Vec::new(),
        },
    }
}
