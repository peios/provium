//! mlua bindings + the Lua-facing test framework.
//!
//! Slice-2 surface — minimal but end-to-end:
//!
//! * [`install`] — wire up the `provium` global, the [`Vm`] /
//!   [`crate::RunResult`] userdata, and the `test()` /
//!   `t.assert_*` test framework against a fresh [`mlua::Lua`].
//! * [`run_file`] — load a `.test.lua` file, execute its top-level
//!   chunk (which registers tests via `test(...)`), then run each
//!   registered test in turn and collect outcomes.
//!
//! Everything is single-threaded against one [`mlua::Lua`] state per
//! file. Multi-file scheduling is the slice-2.5+ scheduler's job.

mod bridge_ud;
mod capture_ud;
mod console_stream_ud;
mod disk_ud;
mod file_ud;
mod json_ud;
pub mod fixture_build;
pub(crate) mod lab_snapshot_ud;
mod lab_ud;
mod nic_ud;
mod result_ud;
mod runner;
mod snapshot_ud;
mod test_framework;
mod vm_ud;

use std::sync::Arc;

use mlua::Lua;

use crate::lab::Lab;
use crate::profile::Config;
use crate::scheduler::events::{EventSink, NullSink};
use crate::vmm::Vmm;

pub use runner::{
    run_file, run_file_full, run_file_with_lab, run_file_with_lab_and_sink, FileOutcome,
    TestOutcome,
};

/// Public re-export of the lab-fixture dep-key resolver so the
/// binary's `fixture rebuild` / `fixture stale` paths can compute
/// the same cache key the runner uses.
pub fn lab_ud_resolve_dep_keys_pub(
    roots: &[String],
    source: &[u8],
) -> Vec<crate::fixture::CacheKey> {
    lab_ud::resolve_dep_keys_pub(roots, source)
}

/// Public re-export of the external-file-dep walker so the CLI's
/// `fixture build` / `rebuild` / `stale` paths fold the same
/// `vm:push_file` and `lab:depends_on_file` host-side dependencies
/// into the cache key the runner uses.
pub fn lab_ud_resolve_external_deps_pub(
    roots: &[String],
    fixture_path: &std::path::Path,
    source: &[u8],
) -> Vec<std::path::PathBuf> {
    lab_ud::resolve_external_deps_pub(roots, fixture_path, source)
}
pub use test_framework::TestStatus;

/// Per-file Lua context — the configuration and VMM the bindings
/// route into, plus the root [`Lab`] reachable from the `provium`
/// Lua global.
///
/// One instance per file runner. Cloning is cheap because the inner
/// fields are [`Arc`]-shared; the Lua state itself is not [`Clone`].
#[derive(Clone)]
pub struct LuaContext {
    /// Resolved `provium.toml`. The bindings use it to look up
    /// profile names passed to `provium:vm(...)`.
    pub config: Arc<Config>,
    /// VMM that spawns VMs.
    pub vmm: Arc<dyn Vmm>,
    /// Root lab — the `provium` global. The dispatcher's per-file
    /// timeout watchdog holds a clone of this so it can shut down
    /// every VM the test created when the timeout fires.
    pub root_lab: Lab,
    /// Observability sink. Lab/VM/fixture bindings emit
    /// `vm_spawned` / `vm_shutdown` / `fixture_*` events through
    /// this sink. Default: [`NullSink`].
    pub events: Arc<dyn EventSink>,
    /// Resource pool the runner draws from when test code calls
    /// `lab:claim(...)`. `None` means "claims are no-ops" — used by
    /// REPL / single-file tests that don't share a global pool.
    pub pool: Option<Arc<crate::scheduler::Pool>>,
}

impl LuaContext {
    /// Convenience constructor for tests / ad-hoc callers that
    /// don't care about events.
    pub fn new_no_events(config: Arc<Config>, vmm: Arc<dyn Vmm>, root_lab: Lab) -> Self {
        Self {
            config,
            vmm,
            root_lab,
            events: Arc::new(NullSink),
            pool: None,
        }
    }
}

impl std::fmt::Debug for LuaContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LuaContext")
            .field("profiles", &self.config.profiles.keys().collect::<Vec<_>>())
            .finish()
    }
}

/// Install every binding the test framework needs onto `lua`.
///
/// Idempotent within one [`Lua`] state — calling twice replaces
/// previously-installed globals.
pub fn install(lua: &Lua, ctx: LuaContext) -> mlua::Result<()> {
    // Prepend each configured test root to `package.path` so
    // `require("helpers.foo")` resolves to `<root>/helpers/foo.lua`
    // — per `DESIGN.md` § Test organisation / Helpers are plain Lua.
    {
        let package: mlua::Table = lua.globals().get("package")?;
        let existing: String = package.get("path").unwrap_or_default();
        let mut prepend = String::new();
        for root in &ctx.config.provium.roots {
            prepend.push_str(&format!("{root}/?.lua;{root}/?/init.lua;"));
        }
        if !prepend.is_empty() {
            let new_path = format!("{prepend}{existing}");
            package.set("path", new_path)?;
        }
    }
    lab_ud::install_provium_global(lua, ctx)?;
    json_ud::install(lua)?;
    test_framework::install(lua)?;
    Ok(())
}
