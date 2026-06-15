//! Drive one `.test.lua` file end-to-end.
//!
//! Sequence:
//!
//! 1. Build a fresh [`mlua::Lua`] state.
//! 2. Install bindings ([`super::install`]).
//! 3. Load and execute the file's chunk — top-level code runs once;
//!    `test(...)` calls register tests in declaration order.
//! 4. Iterate the registry; for each test, build a fresh `t`,
//!    `pcall` the test fn, classify the outcome.
//! 5. Return a [`FileOutcome`] aggregating per-test outcomes.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use mlua::{Function, Lua, Table, Value};

use provium_protocol::events::{
    Event, MetaMap, MetaValue, TestFailed, TestPassed, TestStarted, VmShutdown,
};

use crate::lab::Lab;
use crate::profile::Config;
use crate::scheduler::events::{EventSink, NullSink};
use crate::vmm::Vmm;

use super::test_framework::{TestStatus, SKIP_SENTINEL};
use super::{install, LuaContext};

/// Emit `vm_shutdown` for every still-running VM in `lab` before the
/// lab is torn down.
///
/// A resumed fixture VM (or any VM a test didn't explicitly
/// `:shutdown()`) otherwise dies silently when its lab is dropped —
/// leaving its earlier `vm_spawned` with no matching shutdown, so a
/// live-VM counter built off the event stream would never
/// decrement. Mirrors the file-end teardown in `scheduler/dispatch`.
/// VMs already shut down (state `Dead`/`Shutdown`) are skipped — the
/// explicit `:shutdown()` path already emitted for those.
fn emit_scope_vm_shutdowns(lab: &Lab, sink: &dyn EventSink, file: &str) {
    for (vm_name, vm) in lab.collect_all_vms() {
        if matches!(vm.state(), crate::VmState::Booted | crate::VmState::Paused) {
            sink.emit(Event::VmShutdown(VmShutdown {
                file: file.to_string(),
                vm_name,
                duration_ns: 0,
            }));
        }
    }
}

/// Aggregate outcome of one `.test.lua` file.
#[derive(Clone, Debug)]
pub struct FileOutcome {
    /// Path of the file that was run.
    pub path: PathBuf,
    /// Whether the file's *top-level* chunk loaded and executed
    /// without raising. A failure here counts as a file-level
    /// failure: no per-test outcomes are produced.
    pub chunk_error: Option<String>,
    /// Per-test outcomes, in declaration order. Empty on
    /// chunk-level failure.
    pub tests: Vec<TestOutcome>,
}

impl FileOutcome {
    /// `true` if the chunk ran cleanly *and* every registered test
    /// passed or was skipped.
    pub fn all_succeeded(&self) -> bool {
        self.chunk_error.is_none()
            && self
                .tests
                .iter()
                .all(|t| matches!(t.status, TestStatus::Passed | TestStatus::Skipped))
    }

    /// Count tests of each status. Convenience for reporting.
    pub fn summary(&self) -> (usize, usize, usize) {
        let mut p = 0;
        let mut f = 0;
        let mut s = 0;
        for t in &self.tests {
            match t.status {
                TestStatus::Passed => p += 1,
                TestStatus::Failed => f += 1,
                TestStatus::Skipped => s += 1,
            }
        }
        (p, f, s)
    }
}

/// Outcome of one `test()` call.
#[derive(Clone, Debug)]
pub struct TestOutcome {
    /// `test(name, …)` first argument.
    pub name: String,
    /// Pass/Fail/Skip.
    pub status: TestStatus,
    /// On `Failed` — the Lua error message; on `Skipped` — the
    /// reason if `t:skip("...")` was called with one.
    pub message: Option<String>,
    /// Anything pushed via `t:log(...)` during the test, in order.
    pub log: Vec<String>,
}

/// Execute one file end-to-end against `(config, vmm)`.
///
/// Returns a [`FileOutcome`] regardless of failures along the way —
/// chunk errors and per-test failures are both data, not Rust errors.
/// The only failure mode that bubbles a [`mlua::Error`] is a runner
/// bug (missing global, misuse of the registry), which is unreachable
/// in correct code.
pub fn run_file(
    path: impl AsRef<Path>,
    config: Arc<Config>,
    vmm: Arc<dyn Vmm>,
) -> mlua::Result<FileOutcome> {
    let root_lab = Lab::new(
        "provium",
        Arc::clone(&config),
        Arc::clone(&vmm),
    );
    run_file_with_lab_and_sink(path, config, vmm, root_lab, Arc::new(NullSink))
}

/// Variant of [`run_file`] that uses an externally-provided
/// [`Lab`]. Used by [`crate::scheduler::dispatch`] so the
/// per-file timeout watchdog has a handle on the same lab the
/// Lua side is mutating.
pub fn run_file_with_lab(
    path: impl AsRef<Path>,
    config: Arc<Config>,
    vmm: Arc<dyn Vmm>,
    root_lab: Lab,
) -> mlua::Result<FileOutcome> {
    run_file_with_lab_and_sink(path, config, vmm, root_lab, Arc::new(NullSink))
}

/// Most-explicit variant: takes both an externally-provided lab
/// (for the watchdog) and an event sink (for telemetry). The
/// dispatcher uses this; the slim helpers above default to
/// [`NullSink`] for ergonomic ad-hoc runs.
pub fn run_file_with_lab_and_sink(
    path: impl AsRef<Path>,
    config: Arc<Config>,
    vmm: Arc<dyn Vmm>,
    root_lab: Lab,
    sink: Arc<dyn EventSink>,
) -> mlua::Result<FileOutcome> {
    run_file_full(path, config, vmm, root_lab, sink, None)
}

/// Most-explicit variant: takes a pool reference so test code can
/// `lab:claim(...)` against it. The dispatcher calls this; ad-hoc
/// runs use `run_file_with_lab_and_sink` and pass `None`.
pub fn run_file_full(
    path: impl AsRef<Path>,
    config: Arc<Config>,
    vmm: Arc<dyn Vmm>,
    root_lab: Lab,
    sink: Arc<dyn EventSink>,
    pool: Option<Arc<crate::scheduler::Pool>>,
) -> mlua::Result<FileOutcome> {
    let path = path.as_ref().to_path_buf();
    let lua = Lua::new();
    install(
        &lua,
        LuaContext {
            config: Arc::clone(&config),
            vmm: Arc::clone(&vmm),
            root_lab: root_lab.clone(),
            events: Arc::clone(&sink),
            pool: pool.clone(),
        },
    )?;

    // -----------------------------------------------------------------
    // 1. Chunk
    // -----------------------------------------------------------------
    let chunk_source = match std::fs::read_to_string(&path) {
        Ok(s) => s,
        Err(e) => {
            return Ok(FileOutcome {
                path,
                chunk_error: Some(format!("read: {e}")),
                tests: Vec::new(),
            });
        }
    };

    let chunk_name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("test.lua")
        .to_owned();
    // Record the absolute test-file path so host-side bindings
    // (e.g. `vm:push_file`) can resolve relative paths against the
    // calling file's directory. `set_name(basename)` makes
    // `debug.getinfo` see this as a string chunk (no `@` prefix),
    // so the explicit global is needed for top-level chunks.
    // require()d helpers load via loadfile and DO get `@`-prefixed
    // sources, so push_file's debug-walk still picks them up.
    let abs_path = std::fs::canonicalize(&path).unwrap_or_else(|_| path.clone());
    let _ = lua
        .globals()
        .set("_PROVIUM_SOURCE_PATH", abs_path.to_string_lossy().into_owned());
    if let Err(e) = lua.load(&chunk_source).set_name(&chunk_name).exec() {
        return Ok(FileOutcome {
            path,
            chunk_error: Some(format!("{e}")),
            tests: Vec::new(),
        });
    }

    // -----------------------------------------------------------------
    // 1.5. provium.reset_between_tests = true → take a baseline
    //      snapshot now (after top-level setup, before tests run).
    //      Mutually exclusive with file-scope open streams.
    //
    // Config lives in the `_provium_file_config` table populated by
    // LabUd's NewIndex metamethod when the chunk does
    // `provium.reset_between_tests = true`. Reading via
    // `get::<Table>("provium")` doesn't work — provium is UserData,
    // not Table — so we go through the side table directly.
    // -----------------------------------------------------------------
    let file_config: Option<Table> =
        lua.globals().get::<Table>("_provium_file_config").ok();
    let reset_between = file_config
        .as_ref()
        .and_then(|t| t.get::<Value>("reset_between_tests").ok())
        .map(|v| matches!(v, Value::Boolean(true)))
        .unwrap_or(false);

    // File-level default test timeout — `provium.timeout = 30` (or
    // `"30s"`) is used for every test in the file that doesn't
    // already set its own `meta.timeout`. Per-test wins over file
    // default. Mirrors `DESIGN.md` § Time and timeouts file-scope.
    let file_default_timeout: Option<std::time::Duration> = file_config
        .as_ref()
        .and_then(|t| t.get::<Value>("timeout").ok())
        .and_then(|v| match v {
            Value::Integer(n) if n > 0 => Some(std::time::Duration::from_secs(n as u64)),
            Value::Number(n) if n > 0.0 => Some(std::time::Duration::from_secs_f64(n)),
            Value::String(s) => parse_duration_str(&s.to_str().ok()?)
                .map(std::time::Duration::from_secs_f64),
            _ => None,
        });

    let baseline_dir: Option<tempfile::TempDir> = if reset_between {
        // Pre-condition: no open streams at the top of the file.
        // Render in the format from `DESIGN.md` § Auto-reset
        // between tests / Mutually exclusive with file-scope streams.
        if root_lab.open_stream_total() > 0 {
            // Match the design's exact wording per
            // `DESIGN.md` § Mutually exclusive with file-scope streams.
            let file_name = path
                .file_name()
                .and_then(|s| s.to_str())
                .unwrap_or("file.test.lua");
            let mut msg = format!(
                "{file_name}: cannot use reset_between_tests with file-scope streams"
            );
            for vm in root_lab.vms() {
                for meta in vm.open_streams_meta() {
                    let site = meta
                        .creation_site
                        .as_ref()
                        .map(|(f, l)| format!(" at {f}:{l}"))
                        .unwrap_or_default();
                    msg.push_str(&format!(
                        "\n  - {} ({}){site} opens a stream at file scope",
                        meta.kind, meta.detail
                    ));
                }
            }
            msg.push_str(
                "\n  reset_between_tests would auto-snapshot after this stream is open\
                 \n  Either move the stream into a test() block, or set reset_between_tests = false.",
            );
            return Ok(FileOutcome {
                path,
                chunk_error: Some(msg),
                tests: Vec::new(),
            });
        }
        let td = tempfile::tempdir().map_err(|e| mlua::Error::external(e.to_string()))?;
        if let Err(e) = root_lab.lab_snapshot(td.path()) {
            return Ok(FileOutcome {
                path,
                chunk_error: Some(format!("reset_between_tests baseline snapshot failed: {e}")),
                tests: Vec::new(),
            });
        }
        Some(td)
    } else {
        None
    };

    // -----------------------------------------------------------------
    // 2. Iterate registered tests
    // -----------------------------------------------------------------
    let registry: Function = lua.globals().get("_provium_get_tests")?;
    let registered: Table = registry.call(())?;
    let make_t: Function = lua.globals().get("_provium_make_t")?;

    // File-scope `todo("reason")` — every registered test is
    // reported Skipped with that reason and the body is never
    // executed. Per DESIGN.md § Top-level / declarative skip.
    let file_skip_reason: Option<String> = lua
        .globals()
        .get::<Function>("_provium_file_skipped")
        .ok()
        .and_then(|f| f.call::<Option<String>>(()).ok().flatten());

    let path_str = path.to_string_lossy().into_owned();
    let filter = TestFilter::from_env();
    let mut outcomes = Vec::new();
    if let Some(reason) = file_skip_reason {
        for entry in registered.clone().sequence_values::<Table>() {
            let entry = entry?;
            let name: String = entry.get("name")?;
            let meta_value: Value = entry.get("meta")?;
            let meta_map = lua_value_to_meta_map(&meta_value);
            // Emit TestSkipped per-test so coverage / dashboards
            // see the same event shape they would for a t:skip()
            // body — without this the todo() path was silent on
            // the wire even though the file outcome listed
            // skipped tests.
            sink.emit(Event::TestSkipped(provium_protocol::events::TestSkipped {
                path: path_str.clone(),
                name: name.clone(),
                reason: reason.clone(),
                meta: meta_map,
            }));
            outcomes.push(TestOutcome {
                name,
                status: TestStatus::Skipped,
                message: Some(reason.clone()),
                log: Vec::new(),
            });
        }
        return Ok(FileOutcome {
            path,
            chunk_error: None,
            tests: outcomes,
        });
    }
    for entry in registered.clone().sequence_values::<Table>() {
        let entry = entry?;
        let name: String = entry.get("name")?;
        let meta_value: Value = entry.get("meta")?;
        let test_fn: Function = entry.get("fn")?;
        let meta_map = lua_value_to_meta_map(&meta_value);

        // Tag / slow filtering. Skipped tests still appear in
        // outcomes so consumers see the full set; they're tagged
        // `Skipped` with a stable reason.
        if let Some(reason) = filter.should_skip(&meta_map) {
            // Emit TestSkipped here too — without it the
            // tag/slow/meta.skip filter path was silent on the
            // wire even though the file outcome listed skipped
            // tests. The post-test classification path below
            // already emits TestSkipped for `t:skip()` bodies,
            // so this brings the filter path to parity.
            sink.emit(Event::TestSkipped(provium_protocol::events::TestSkipped {
                path: path_str.clone(),
                name: name.clone(),
                reason: reason.clone(),
                meta: meta_map.clone(),
            }));
            outcomes.push(TestOutcome {
                name: name.clone(),
                status: TestStatus::Skipped,
                message: Some(reason),
                log: Vec::new(),
            });
            continue;
        }

        // Per-test timeout from `meta.timeout`. Numbers are seconds;
        // strings parse via the standard "5s"/"500ms"/"5m"/"2h"
        // grammar.
        //
        // **Scope limitation (R9 sched-m5):** the watchdog tears
        // the file's *entire* lab down on fire, not just the
        // resources the timed-out test is using. This is the
        // only currently-available way to unblock an in-flight
        // Lua op — a finer-grained cancellation would require
        // cooperative timeouts threaded through every
        // AgentClient op. For files with `reset_between_tests =
        // true` the next test gets a clean restored snapshot
        // anyway, so the lab-wide kill is harmless. For files
        // sharing live state across tests, a per-test timeout
        // currently invalidates the rest of the file —
        // documented so test authors don't put a per-test
        // `timeout = N` on a flaky test in a state-sharing file
        // expecting siblings to be unaffected.
        let test_deadline = test_timeout_from_meta(&meta_map).or(file_default_timeout);
        let test_watchdog = test_deadline.map(|d| {
            let lab = root_lab.clone();
            let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
            let stop_for_thread = Arc::clone(&stop);
            let h = std::thread::spawn(move || {
                let deadline = Instant::now() + d;
                while Instant::now() < deadline {
                    if stop_for_thread.load(std::sync::atomic::Ordering::SeqCst) {
                        return false;
                    }
                    std::thread::sleep(std::time::Duration::from_millis(50));
                }
                if stop_for_thread.load(std::sync::atomic::Ordering::SeqCst) {
                    return false;
                }
                let _ = lab.shutdown();
                true
            });
            (h, stop)
        });

        // Build a fresh `t` per test.
        let t: Table = make_t.call((name.clone(), meta_value))?;

        sink.emit(Event::TestStarted(TestStarted {
            path: path_str.clone(),
            name: name.clone(),
            meta: meta_map.clone(),
        }));
        // Stash the current test's name so stream-creation sites
        // can attach it to their StreamMeta for snapshot diagnostics.
        let _ = lua.globals().set("_provium_current_test", name.as_str());
        let _ = lua.globals().set("_provium_in_test", true);

        // Push a per-test lab scope: `provium:vm("foo", "p")` from
        // inside a test() body now lands in this fresh sub-Lab. The
        // LabUd's parent_chain falls back to root_lab so 1-arg
        // lookups + dot-access still find file-scope resources. At
        // test end (or panic) we silently shutdown the scope lab and
        // restore the prior `provium` global. Without this, two
        // tests in the same file declaring the same VM name collide
        // on root_lab's BTreeMap.
        let original_provium: mlua::Value = lua.globals().get("provium")?;
        // Event sink wired through so a `vm_fixture(...)` resumed
        // inside this test emits `vm_spawned` (via `Lab::restore_vm`).
        let test_scope_lab = crate::lab::Lab::new_with_pool_and_events(
            format!("test:{name}"),
            Arc::clone(&config),
            Arc::clone(&vmm),
            pool.clone(),
            Arc::clone(&sink),
        );
        let scope_ctx = LuaContext {
            config: Arc::clone(&config),
            vmm: Arc::clone(&vmm),
            root_lab: root_lab.clone(),
            events: Arc::clone(&sink),
            pool: pool.clone(),
        };
        let scope_ud = super::lab_ud::scoped_lab_ud(
            test_scope_lab.clone(),
            vec![root_lab.clone()],
            &scope_ctx,
        );
        lua.globals().set("provium", scope_ud)?;

        let started = Instant::now();

        // Per `DESIGN.md` § Failure mode catalogue: a Rust panic
        // inside the test must mark this test failed AND the
        // remaining tests as `failed-due-to-poisoned-state` since
        // mlua state can't be safely reused after panic. The file's
        // own runner thread then unwinds normally.
        let t_for_panic = t.clone();
        let outcome_raw = match std::panic::catch_unwind(
            std::panic::AssertUnwindSafe(|| test_fn.call::<()>(t_for_panic)),
        ) {
            Ok(r) => r,
            Err(payload) => {
                let msg = panic_msg_to_string(&payload);
                outcomes.push(TestOutcome {
                    name: name.clone(),
                    status: TestStatus::Failed,
                    message: Some(format!("Rust panic: {msg}")),
                    log: collect_log(&t),
                });
                // Mark every subsequent registered test poisoned and
                // bail out of the loop.
                let remaining: Vec<Table> =
                    registered.clone().sequence_values::<Table>()
                        .skip(outcomes.len())
                        .filter_map(Result::ok)
                        .collect();
                for entry in remaining {
                    let n: String = entry.get("name").unwrap_or_default();
                    outcomes.push(TestOutcome {
                        name: n,
                        status: TestStatus::Failed,
                        message: Some(
                            "failed-due-to-poisoned-state (earlier test panicked)".into(),
                        ),
                        log: Vec::new(),
                    });
                }
                // Clear the in-test flag so file-scope teardown
                // observes the design's expected state.
                let _ = lua
                    .globals()
                    .set("_provium_current_test", mlua::Value::Nil);
                let _ = lua.globals().set("_provium_in_test", false);
                // Pop the test scope even on panic — otherwise the
                // file-end teardown runs against a poisoned `provium`
                // binding and any remaining test-scope VMs leak.
                emit_scope_vm_shutdowns(&test_scope_lab, sink.as_ref(), test_scope_lab.name());
                let _ = test_scope_lab.shutdown();
                let _ = lua.globals().set("provium", original_provium);
                break;
            }
        };
        // Stop the watchdog and observe whether it fired.
        let timed_out = if let Some((h, stop)) = test_watchdog {
            stop.store(true, std::sync::atomic::Ordering::SeqCst);
            h.join().ok().unwrap_or(false)
        } else {
            false
        };
        let outcome = match outcome_raw {
            Ok(()) if timed_out => TestOutcome {
                name: name.clone(),
                status: TestStatus::Failed,
                message: Some(format!(
                    "test timeout exceeded ({:?})",
                    test_deadline.unwrap()
                )),
                log: collect_log(&t),
            },
            Ok(()) => {
                // Three classifications when the test function
                // returns Ok:
                //   1. `t:skip()` was raised but caught by a
                //      user-side pcall — `_outcome == "skip"`.
                //      Without this branch the test would be
                //      reported Passed (regression risk).
                //   2. An assertion fired but was swallowed by a
                //      user-side pcall — `_failed` is sticky.
                //   3. Clean pass — neither flag set.
                let outcome_field: String = t
                    .get("_outcome")
                    .unwrap_or_else(|_| "pass".into());
                let failed: bool = t.get("_failed").unwrap_or(false);
                if outcome_field == "skip" {
                    let reason: Option<String> = t.get("_skip_reason").ok();
                    TestOutcome {
                        name: name.clone(),
                        status: TestStatus::Skipped,
                        message: reason,
                        log: collect_log(&t),
                    }
                } else if failed {
                    let msg: Option<String> = t.get("_failed_msg").ok();
                    TestOutcome {
                        name: name.clone(),
                        status: TestStatus::Failed,
                        message: Some(
                            msg.unwrap_or_else(|| "assertion failed (caught by pcall)".to_string()),
                        ),
                        log: collect_log(&t),
                    }
                } else {
                    TestOutcome {
                        name: name.clone(),
                        status: TestStatus::Passed,
                        message: None,
                        log: collect_log(&t),
                    }
                }
            }
            Err(err) if timed_out => TestOutcome {
                name: name.clone(),
                status: TestStatus::Failed,
                message: Some(format!(
                    "test timeout exceeded ({:?}); op error: {err}",
                    test_deadline.unwrap()
                )),
                log: collect_log(&t),
            },
            Err(err) => classify_failure(&t, err, &name),
        };
        let duration_ns = elapsed_ns(started);

        match outcome.status {
            TestStatus::Passed => {
                sink.emit(Event::TestPassed(TestPassed {
                    path: path_str.clone(),
                    name: name.clone(),
                    duration_ns,
                    meta: meta_map.clone(),
                }));
            }
            TestStatus::Failed => {
                // Capture last 4 KiB from each booted VM's console
                // log so consumers see what the guest was printing at
                // failure. Per `DESIGN.md` § Failure mode catalogue.
                let console_excerpt = collect_console_excerpts(&root_lab);
                sink.emit(Event::TestFailed(TestFailed {
                    path: path_str.clone(),
                    name: name.clone(),
                    duration_ns,
                    reason: outcome.message.clone().unwrap_or_default(),
                    console_excerpt,
                    meta: meta_map.clone(),
                }));
            }
            TestStatus::Skipped => {
                // Distinct event so coverage / dashboard consumers
                // can tell skips apart from passes. Reason is the
                // filter message, t:skip() argument, or todo()
                // argument — whichever produced the Skipped outcome.
                sink.emit(Event::TestSkipped(provium_protocol::events::TestSkipped {
                    path: path_str.clone(),
                    name: name.clone(),
                    reason: outcome.message.clone().unwrap_or_default(),
                    meta: meta_map.clone(),
                }));
            }
        }
        outcomes.push(outcome);

        // Clear the current-test marker and run the explicit
        // reverse-dep close walk. Per `DESIGN.md` § Auto-close
        // ordering: streams → procs → files → workers → VMs/bridges
        // (the last only at file end, so they're not in the
        // test-scope registry).
        let _ = lua.globals().set("_provium_current_test", mlua::Value::Nil);
        let _ = lua.globals().set("_provium_in_test", false);
        if let Ok(close_test) = lua
            .globals()
            .get::<Function>("_provium_close_test_scope")
        {
            let _ = close_test.call::<()>(());
        }
        // Pop the test scope: tear down any VMs that landed in the
        // per-test Lab (most are already shut by close_test_scope's
        // resource cascade above; this is the cleanup for any that
        // were created without going through register_resource), then
        // restore the prior `provium` binding so file-end teardown
        // and the next test see the file root.
        emit_scope_vm_shutdowns(&test_scope_lab, sink.as_ref(), test_scope_lab.name());
        let _ = test_scope_lab.shutdown();
        let _ = lua.globals().set("provium", original_provium);
        // GC sweep mops up anything not on the registry (e.g. helper
        // userdata not registered).
        let _ = lua.gc_collect();
        let _ = lua.gc_collect();

        // Auto-restore from the baseline snapshot if the file opted
        // into reset_between_tests. Skipped on the last iteration —
        // wasted work — but kept simple here.
        if let Some(baseline) = &baseline_dir {
            let meta_path = baseline.path().join("lab.json");
            // Surface restore failure as a stderr warning. Without
            // this the test harness silently runs subsequent tests
            // against whatever leftover state survived — which makes
            // failures non-deterministic and impossible to diagnose.
            // We don't fail the test that *just* ran (its own outcome
            // is already settled); we just complain so the file
            // author can act.
            match std::fs::read_to_string(&meta_path) {
                Ok(body) => {
                    match serde_json::from_str::<crate::lab::LabSnapshotMeta>(&body)
                    {
                        Ok(meta) => {
                            if let Err(e) = root_lab.lab_restore(&meta) {
                                eprintln!(
                                    "provium: reset_between_tests restore failed in `{}`: \
                                     {e}\n  Subsequent tests in this file may see leaked state.",
                                    path.display(),
                                );
                            }
                        }
                        Err(e) => {
                            eprintln!(
                                "provium: reset_between_tests baseline metadata corrupt \
                                 in `{}`: {e}\n  Subsequent tests in this file \
                                 may see leaked state.",
                                path.display(),
                            );
                        }
                    }
                }
                Err(e) => {
                    eprintln!(
                        "provium: reset_between_tests baseline metadata unreadable \
                         in `{}`: {e}\n  Subsequent tests in this file may \
                         see leaked state.",
                        path.display(),
                    );
                }
            }
        }
    }

    // File end — close file-scope resources in reverse-dep order
    // before the final GC sweep.
    if let Ok(close_file) = lua
        .globals()
        .get::<Function>("_provium_close_file_scope")
    {
        let _ = close_file.call::<()>(());
    }
    let _ = lua.gc_collect();
    let _ = lua.gc_collect();

    drop(baseline_dir);

    Ok(FileOutcome {
        path,
        chunk_error: None,
        tests: outcomes,
    })
}

fn classify_failure(t: &Table, err: mlua::Error, name: &str) -> TestOutcome {
    let outcome_field: String = t.get("_outcome").unwrap_or_else(|_| "fail".into());
    let message = format!("{err}");

    if outcome_field == "skip" || message.contains(SKIP_SENTINEL) {
        let reason: Option<String> = t.get("_skip_reason").ok();
        return TestOutcome {
            name: name.to_owned(),
            status: TestStatus::Skipped,
            message: reason,
            log: collect_log(t),
        };
    }

    TestOutcome {
        name: name.to_owned(),
        status: TestStatus::Failed,
        message: Some(message),
        log: collect_log(t),
    }
}

fn panic_msg_to_string(payload: &Box<dyn std::any::Any + Send>) -> String {
    if let Some(s) = payload.downcast_ref::<&'static str>() {
        (*s).to_owned()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        "<non-string panic payload>".to_owned()
    }
}

/// Snapshot the last 4 KiB of each VM's console log into a single
/// string. Empty when no VMs are booted.
fn collect_console_excerpts(lab: &Lab) -> String {
    const MAX_PER_VM: usize = 4 * 1024;
    let mut out = String::new();
    for vm in lab.vms() {
        let bytes = match vm.console_read() {
            Ok(b) => b,
            Err(_) => continue,
        };
        if bytes.is_empty() {
            continue;
        }
        let start = bytes.len().saturating_sub(MAX_PER_VM);
        out.push_str(&format!("---- vm {} (last {} bytes) ----\n", vm.name(), bytes.len() - start));
        out.push_str(&String::from_utf8_lossy(&bytes[start..]));
        out.push('\n');
    }
    out
}

fn collect_log(t: &Table) -> Vec<String> {
    match t.get::<Table>("_log") {
        Ok(log) => log
            .sequence_values::<String>()
            .filter_map(Result::ok)
            .collect(),
        Err(_) => Vec::new(),
    }
}

fn elapsed_ns(started: Instant) -> u64 {
    let elapsed = started.elapsed();
    u64::try_from(elapsed.as_nanos()).unwrap_or(u64::MAX)
}

/// Parse `meta.timeout` per `DESIGN.md` § Time and timeouts: numbers
/// are seconds, strings carry a unit suffix (`ms`/`s`/`m`/`h`).
fn test_timeout_from_meta(meta: &MetaMap) -> Option<std::time::Duration> {
    let v = meta.get("timeout")?;
    let secs = match v {
        MetaValue::Int(n) if *n > 0 => *n as f64,
        MetaValue::Float(n) if *n > 0.0 => *n,
        MetaValue::Str(s) => parse_duration_str(s)?,
        _ => return None,
    };
    Some(std::time::Duration::from_secs_f64(secs))
}

fn parse_duration_str(s: &str) -> Option<f64> {
    let s = s.trim();
    let (num, unit) = if let Some(stripped) = s.strip_suffix("ms") {
        (stripped, 0.001)
    } else if let Some(stripped) = s.strip_suffix('s') {
        (stripped, 1.0)
    } else if let Some(stripped) = s.strip_suffix('m') {
        (stripped, 60.0)
    } else if let Some(stripped) = s.strip_suffix('h') {
        (stripped, 3600.0)
    } else {
        (s, 1.0)
    };
    num.trim().parse::<f64>().ok().map(|n| n * unit)
}

/// `--include-slow`/`--tag`/`--no-tag`/`--tag-meta`/`--no-tag-meta`
/// filter assembled from the `PROVIUM_TEST_FILTER` env var the binary
/// sets at startup. The runner crate stays env-driven so tests /
/// external callers can inject overrides without growing every API a
/// `&TestFilter` arg.
#[derive(Default, Clone, Debug)]
struct TestFilter {
    include_slow: bool,
    tag: Vec<String>,
    no_tag: Vec<String>,
    /// `(key, value)` pairs from `--tag-meta KEY=VALUE`. Multi-flag
    /// same-key = OR within key; different keys = AND across keys.
    tag_meta: Vec<(String, String)>,
    /// `(key, value)` pairs from `--no-tag-meta KEY=VALUE`. Match
    /// = test is filtered out.
    no_tag_meta: Vec<(String, String)>,
}

impl TestFilter {
    fn from_env() -> Self {
        let raw = std::env::var("PROVIUM_TEST_FILTER").unwrap_or_default();
        if raw.is_empty() {
            return Self::default();
        }
        let v: serde_json::Value = serde_json::from_str(&raw).unwrap_or_default();
        let read_kv = |field: &str| -> Vec<(String, String)> {
            v.get(field)
                .and_then(|v| v.as_array())
                .map(|a| {
                    a.iter()
                        .filter_map(|entry| {
                            let k = entry.get("key").and_then(|v| v.as_str())?;
                            let v = entry.get("value").and_then(|v| v.as_str())?;
                            Some((k.to_owned(), v.to_owned()))
                        })
                        .collect()
                })
                .unwrap_or_default()
        };
        Self {
            include_slow: v
                .get("include_slow")
                .and_then(|v| v.as_bool())
                .unwrap_or(false),
            tag: v
                .get("tag")
                .and_then(|v| v.as_array())
                .map(|a| {
                    a.iter()
                        .filter_map(|v| v.as_str().map(String::from))
                        .collect()
                })
                .unwrap_or_default(),
            no_tag: v
                .get("no_tag")
                .and_then(|v| v.as_array())
                .map(|a| {
                    a.iter()
                        .filter_map(|v| v.as_str().map(String::from))
                        .collect()
                })
                .unwrap_or_default(),
            tag_meta: read_kv("tag_meta"),
            no_tag_meta: read_kv("no_tag_meta"),
        }
    }

    /// `Some(reason)` if the test should be skipped.
    fn should_skip(&self, meta: &MetaMap) -> Option<String> {
        // Declarative `meta = {skip = true}` or
        // `meta = {skip = "reason"}`. Symmetric with `slow` so a
        // test author can write `test("foo", {skip=true}, ...)`
        // without reaching for an inline `t:skip()` body.
        match meta.get("skip") {
            Some(MetaValue::Bool(true)) => return Some("skipped (meta.skip)".into()),
            // Lua-side `{skip = 1}` is a natural mistake — accept
            // any non-zero integer as truthy.
            Some(MetaValue::Int(n)) if *n != 0 => {
                return Some("skipped (meta.skip)".into());
            }
            Some(MetaValue::Str(s)) => return Some(format!("skipped: {s}")),
            _ => {}
        }
        if !self.include_slow {
            match meta.get("slow") {
                Some(MetaValue::Bool(true)) => {
                    return Some(
                        "filtered: slow tests skipped (pass --include-slow)".into(),
                    );
                }
                Some(MetaValue::Int(n)) if *n != 0 => {
                    return Some(
                        "filtered: slow tests skipped (pass --include-slow)".into(),
                    );
                }
                _ => {}
            }
        }
        let tags = collect_tags(meta);
        if !self.no_tag.is_empty() && tags.iter().any(|t| self.no_tag.contains(t)) {
            return Some(format!("filtered: matched --no-tag ({:?})", self.no_tag));
        }
        if !self.tag.is_empty() && !tags.iter().any(|t| self.tag.contains(t)) {
            return Some(format!("filtered: did not match --tag ({:?})", self.tag));
        }
        // --no-tag-meta: any matching pair filters out.
        for (k, v) in &self.no_tag_meta {
            if meta_field_contains(meta, k, v) {
                return Some(format!(
                    "filtered: matched --no-tag-meta ({k}={v})"
                ));
            }
        }
        // --tag-meta: group pairs by key. Within a key any value
        // matches (OR); across keys all keys must have at least one
        // matching value (AND). A key with no match filters the test.
        if !self.tag_meta.is_empty() {
            let mut keys: std::collections::BTreeSet<&String> =
                std::collections::BTreeSet::new();
            for (k, _) in &self.tag_meta {
                keys.insert(k);
            }
            for k in keys {
                let any_match = self
                    .tag_meta
                    .iter()
                    .filter(|(kk, _)| kk == k)
                    .any(|(_, v)| meta_field_contains(meta, k, v));
                if !any_match {
                    return Some(format!(
                        "filtered: did not match any --tag-meta {k}=…"
                    ));
                }
            }
        }
        None
    }
}

/// Does `meta[key]` contain `wanted` as a string? Treats:
/// - `MetaValue::Str` as a singleton list
/// - `MetaValue::Array` as a list (string entries only)
/// - everything else as no-match
fn meta_field_contains(meta: &MetaMap, key: &str, wanted: &str) -> bool {
    match meta.get(key) {
        Some(MetaValue::Str(s)) => s == wanted,
        Some(MetaValue::Array(items)) => items.iter().any(|v| match v {
            MetaValue::Str(s) => s == wanted,
            _ => false,
        }),
        _ => false,
    }
}

fn collect_tags(meta: &MetaMap) -> Vec<String> {
    // Accept either `tags = "smoke"` (single string, easy mistake)
    // or `tags = {"smoke", "fast"}` (array, the documented form).
    // Without the single-string arm a one-tag test was silently
    // filtered out by --tag with the misleading message
    // "did not match --tag" instead of seeing its lone tag.
    if let Some(MetaValue::Str(s)) = meta.get("tags") {
        return vec![s.clone()];
    }
    let Some(MetaValue::Array(items)) = meta.get("tags") else {
        return Vec::new();
    };
    items
        .iter()
        .filter_map(|v| match v {
            MetaValue::Str(s) => Some(s.clone()),
            _ => None,
        })
        .collect()
}

/// Best-effort conversion of a Lua-side `meta` table into a
/// [`MetaMap`] for event emission. Strings, integers, booleans,
/// and string-keyed nested tables come through; everything else
/// becomes [`MetaValue::Null`] for now.
fn lua_value_to_meta_map(value: &Value) -> MetaMap {
    let mut map = MetaMap::new();
    if let Value::Table(t) = value {
        for pair in t.clone().pairs::<String, Value>() {
            let Ok((k, v)) = pair else { continue };
            map.insert(k, lua_value_to_meta_value(&v));
        }
    }
    map
}

fn lua_value_to_meta_value(value: &Value) -> MetaValue {
    match value {
        Value::Nil => MetaValue::Null,
        Value::Boolean(b) => MetaValue::Bool(*b),
        Value::Integer(i) => MetaValue::Int(*i),
        Value::Number(n) => MetaValue::Float(*n),
        Value::String(s) => match s.to_str() {
            Ok(s) => MetaValue::Str(s.to_string()),
            Err(_) => MetaValue::Bytes(s.as_bytes().to_vec()),
        },
        Value::Table(t) => {
            // Try array form first; fall back to map.
            let len = t.raw_len();
            if len > 0 {
                let mut items = Vec::new();
                for i in 1..=len {
                    if let Ok(v) = t.raw_get::<Value>(i) {
                        items.push(lua_value_to_meta_value(&v));
                    }
                }
                MetaValue::Array(items)
            } else {
                let mut nested = std::collections::BTreeMap::new();
                for pair in t.clone().pairs::<String, Value>() {
                    let Ok((k, v)) = pair else { continue };
                    nested.insert(k, lua_value_to_meta_value(&v));
                }
                MetaValue::Map(nested)
            }
        }
        _ => MetaValue::Null,
    }
}

#[cfg(test)]
mod filter_tests {
    use super::*;
    use provium_protocol::events::MetaValue;

    fn meta_with(entries: &[(&str, MetaValue)]) -> MetaMap {
        let mut m = MetaMap::new();
        for (k, v) in entries {
            m.insert((*k).to_owned(), v.clone());
        }
        m
    }

    fn filter() -> TestFilter {
        TestFilter::default()
    }

    #[test]
    fn meta_field_contains_string_scalar() {
        let m = meta_with(&[("subsystems", MetaValue::Str("peinit".into()))]);
        assert!(meta_field_contains(&m, "subsystems", "peinit"));
        assert!(!meta_field_contains(&m, "subsystems", "loregd"));
    }

    #[test]
    fn meta_field_contains_array() {
        let m = meta_with(&[(
            "subsystems",
            MetaValue::Array(vec![
                MetaValue::Str("peinit".into()),
                MetaValue::Str("loregd".into()),
            ]),
        )]);
        assert!(meta_field_contains(&m, "subsystems", "peinit"));
        assert!(meta_field_contains(&m, "subsystems", "loregd"));
        assert!(!meta_field_contains(&m, "subsystems", "kacs"));
    }

    #[test]
    fn tag_meta_or_within_key() {
        let mut f = filter();
        f.tag_meta = vec![
            ("subsystems".into(), "peinit".into()),
            ("subsystems".into(), "loregd".into()),
        ];
        let m = meta_with(&[("subsystems", MetaValue::Str("loregd".into()))]);
        assert!(f.should_skip(&m).is_none(), "loregd matches OR group");
        let m = meta_with(&[("subsystems", MetaValue::Str("kacs".into()))]);
        assert!(f.should_skip(&m).is_some(), "kacs matches neither");
    }

    #[test]
    fn tag_meta_and_across_keys() {
        let mut f = filter();
        f.tag_meta = vec![
            ("subsystems".into(), "peinit".into()),
            ("area".into(), "boot".into()),
        ];
        let m = meta_with(&[
            ("subsystems", MetaValue::Str("peinit".into())),
            ("area", MetaValue::Str("boot".into())),
        ]);
        assert!(f.should_skip(&m).is_none(), "both keys match");
        let m = meta_with(&[("subsystems", MetaValue::Str("peinit".into()))]);
        assert!(f.should_skip(&m).is_some(), "area key missing");
    }

    #[test]
    fn no_tag_meta_filters_match() {
        let mut f = filter();
        f.no_tag_meta = vec![("flaky".into(), "true".into())];
        let m = meta_with(&[("flaky", MetaValue::Str("true".into()))]);
        let reason = f.should_skip(&m).expect("should be filtered");
        assert!(reason.contains("--no-tag-meta"), "reason: {reason}");
    }
}

