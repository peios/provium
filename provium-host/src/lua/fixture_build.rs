//! Run a `*.fixture.lua` file in a fresh Lua state to produce a
//! snapshot bytes file.
//!
//! Per `DESIGN.md` § Fixtures: the file's chunk runs once,
//! returns a [`super::snapshot_ud::SnapshotUd`], and the framework
//! moves the underlying file into the cache. Fixture build VMs
//! live in a transient [`Lab`] that's torn down after the build.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use mlua::{Lua, Value};

use crate::fixture::FixtureError;
use crate::lab::Lab;
use crate::profile::Config;
use crate::vmm::Vmm;

use super::snapshot_ud::SnapshotUd;
use super::test_framework;

/// Outcome of [`build_fixture`] — the path the fixture's snapshot
/// was written to (still inside the build's tempdir; the caller
/// is expected to move it to the cache atomically).
/// Outcome of [`build_fixture`].
#[derive(Debug)]
pub enum FixtureBuildOutcome {
    /// Single-VM snapshot — file path that can be moved into cache.
    SingleVm {
        /// Snapshot file.
        snapshot_path: PathBuf,
    },
    /// Whole-lab snapshot — directory containing per-VM snapshots
    /// + lab.json metadata.
    Lab {
        /// Snapshot directory.
        snapshot_dir: PathBuf,
    },
}

impl FixtureBuildOutcome {
    /// Convenience accessor for callers that only handle the
    /// single-VM form.
    pub fn snapshot_path(&self) -> Option<&PathBuf> {
        match self {
            Self::SingleVm { snapshot_path } => Some(snapshot_path),
            _ => None,
        }
    }
}

/// Execute the fixture file at `fixture_path`. The chunk must
/// return a [`SnapshotUd`]; anything else is a build failure.
///
/// On success the caller has the responsibility to:
///
/// 1. Move (`rename`) `outcome.snapshot_path` to its final
///    cache location.
/// 2. Tear down any VMs created in the build's transient lab —
///    we already do that here on the way out.
///
/// Public surface — used by the `provium fixture build PATH` CLI
/// subcommand to drive a fixture file directly outside the test dispatch path.
pub fn build_fixture(
    fixture_path: &Path,
    config: Arc<Config>,
    vmm: Arc<dyn Vmm>,
) -> Result<FixtureBuildOutcome, FixtureError> {
    let source = std::fs::read_to_string(fixture_path)?;

    let lua = Lua::new();
    // Fixture builds are intentionally NOT pool-accounted for v1.
    // Same-fixture concurrent builds are serialised by the
    // file-lock at the call site; different fixtures build in
    // parallel and each spawns full-sized VMs. On a small pool
    // this can spike actual memory usage above the configured
    // pool budget. Documented in DESIGN.md § Failure mode
    // catalogue (entry: "fixture build memory spike").
    //
    // The proper fix is threading the pool through here and
    // reserving from it before booting; deferred because the
    // ordering against the running test suite needs a design
    // decision (block vs degrade vs error).
    let build_lab = Lab::new("provium", Arc::clone(&config), Arc::clone(&vmm));

    // Bindings: provium global + the test framework helpers (so
    // shared library code that pcalls etc. still works), but the
    // sub-state has its own registry — fixture chunks aren't
    // expected to call `test()`.
    crate::lua::lab_ud::install_provium_global(
        &lua,
        super::LuaContext {
            config: Arc::clone(&config),
            vmm: Arc::clone(&vmm),
            root_lab: build_lab.clone(),
            events: Arc::new(crate::scheduler::events::NullSink),
            pool: None,
        },
    )
    .map_err(|e| FixtureError::BuildFailed {
        path: fixture_path.into(),
        detail: format!("install bindings: {e}"),
    })?;
    test_framework::install(&lua).ok();

    let chunk_name = fixture_path
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("fixture.lua");
    // Record the absolute fixture-file path so host-side bindings
    // (e.g. `vm:push_file`) can resolve relative paths against the
    // fixture's directory. See the parallel comment in runner.rs.
    let abs_path =
        std::fs::canonicalize(fixture_path).unwrap_or_else(|_| fixture_path.to_path_buf());
    let _ = lua
        .globals()
        .set("_PROVIUM_SOURCE_PATH", abs_path.to_string_lossy().into_owned());

    // Both the Lua eval and the outcome conversion can panic
    // (mlua catches Rust panics inside callbacks but not panics
    // raised by host-side helpers between them). Wrap the body in
    // `catch_unwind` so a panic still tears down `build_lab` and
    // surfaces as a structured `BuildFailed`, instead of leaking
    // VMs and propagating the unwind through cargo's harness.
    let lab_for_run = build_lab.clone();
    let path_for_panic = fixture_path.to_path_buf();
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let returned: Value = lua
            .load(&source)
            .set_name(chunk_name)
            .eval()
            .map_err(|e| format!("chunk: {e}"))?;
        outcome_from_value(&returned)
    }));

    let _ = lab_for_run.shutdown();

    match result {
        Ok(Ok(outcome)) => Ok(outcome),
        Ok(Err(detail)) => Err(FixtureError::BuildFailed {
            path: fixture_path.into(),
            detail,
        }),
        Err(panic) => {
            let detail = panic_payload(&panic);
            Err(FixtureError::BuildFailed {
                path: path_for_panic,
                detail: format!("panic during build: {detail}"),
            })
        }
    }
}

/// Best-effort recovery of a panic message from the boxed payload
/// returned by [`std::panic::catch_unwind`].
fn panic_payload(p: &Box<dyn std::any::Any + Send>) -> String {
    if let Some(s) = p.downcast_ref::<String>() {
        return s.clone();
    }
    if let Some(s) = p.downcast_ref::<&'static str>() {
        return (*s).to_string();
    }
    "unknown panic payload".into()
}

fn outcome_from_value(value: &Value) -> Result<FixtureBuildOutcome, String> {
    let ud = match value {
        Value::UserData(u) => u,
        other => {
            return Err(format!(
                "fixture must return vm:snapshot() or provium:snapshot(); got {}",
                other.type_name()
            ));
        }
    };
    if let Ok(snap) = ud.borrow::<SnapshotUd>() {
        if !snap.path.is_file() {
            return Err(format!(
                "snapshot file not found at {}",
                snap.path.display()
            ));
        }
        return Ok(FixtureBuildOutcome::SingleVm {
            snapshot_path: snap.path.clone(),
        });
    }
    if let Ok(snap) =
        ud.borrow::<crate::lua::lab_snapshot_ud::LabSnapshotUd>()
    {
        if !snap.dir.is_dir() {
            return Err(format!(
                "lab snapshot dir not found at {}",
                snap.dir.display()
            ));
        }
        return Ok(FixtureBuildOutcome::Lab {
            snapshot_dir: snap.dir.clone(),
        });
    }
    Err("fixture's return value is not a snapshot or lab snapshot userdata".into())
}
