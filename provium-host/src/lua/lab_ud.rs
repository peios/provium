//! Lua bindings for [`Lab`].
//!
//! `provium` itself is a `LabUd` — every lab method is reachable
//! directly off the global. Sub-labs are also `LabUd`s.

use std::path::PathBuf;
use std::sync::Arc;

use mlua::{Lua, MetaMethod, UserData, UserDataMethods, Value};

use provium_protocol::events::{
    Event, FixtureBuildDone, FixtureBuildStarted, FixtureBuildWaiting, FixtureCacheHit,
};

use crate::fixture::{
    self, default_cache_dir, CacheEntryPaths,
};
use crate::lab::Lab;
use crate::profile::Config;
use crate::scheduler::events::EventSink;
use crate::vmm::{BootOpts, Vmm};

use super::bridge_ud::BridgeUd;
use super::vm_ud::VmUd;
use super::LuaContext;

/// Install `provium` as a global `LabUd` backed by `ctx.root_lab`.
/// The watchdog (in [`crate::scheduler::dispatch`]) holds a clone of
/// the same lab so it can tear every VM down on per-file timeout.
pub(crate) fn install_provium_global(lua: &Lua, ctx: LuaContext) -> mlua::Result<()> {
    let provium = LabUd {
        lab: ctx.root_lab,
        parent_chain: Vec::new(),
        config: Arc::clone(&ctx.config),
        vmm: Arc::clone(&ctx.vmm),
        events: Arc::clone(&ctx.events),
        pool: ctx.pool.clone(),
    };
    lua.globals().set("provium", provium)?;
    Ok(())
}

/// Build a LabUd that delegates lookups to a parent chain.
/// The runner uses this to push a per-test scope: VMs created inside
/// a `test()` body land in `local_lab` and are torn down at test end,
/// while lookups still find file-scope resources via `parents`.
pub(crate) fn scoped_lab_ud(
    local_lab: Lab,
    parents: Vec<Lab>,
    ctx: &LuaContext,
) -> LabUd {
    LabUd {
        lab: local_lab,
        parent_chain: parents,
        config: Arc::clone(&ctx.config),
        vmm: Arc::clone(&ctx.vmm),
        events: Arc::clone(&ctx.events),
        pool: ctx.pool.clone(),
    }
}

/// UserData wrapper for [`Lab`]. Cheap to clone — the inner [`Lab`]
/// holds an [`Arc<Mutex<…>>`] internally.
///
/// `parent_chain` is the lookup-fallthrough stack: when a name isn't
/// found in `lab`, the resolver walks `parent_chain` in order. Empty
/// for file-root labs, populated for per-test scopes (see the
/// runner's test-scope push/pop). Creates always go to `lab`.
#[derive(Clone)]
pub(crate) struct LabUd {
    pub(crate) lab: Lab,
    pub(crate) parent_chain: Vec<Lab>,
    pub(crate) config: Arc<Config>,
    pub(crate) vmm: Arc<dyn Vmm>,
    pub(crate) events: Arc<dyn EventSink>,
    pub(crate) pool: Option<Arc<crate::scheduler::Pool>>,
}

impl LabUd {
    /// Lab to act on for fixture caching — the bottom-most entry
    /// (file root) when scoped, else `lab` itself. Fixtures must
    /// land at file scope so the cache survives across tests; if
    /// they materialised in the test scope they'd be torn down
    /// before the next test could reuse them.
    pub(crate) fn fixture_lab(&self) -> &Lab {
        self.parent_chain.last().unwrap_or(&self.lab)
    }

    /// Walk local lab + parent chain looking for a VM by name.
    pub(crate) fn lookup_vm(&self, name: &str) -> Option<crate::vm::Vm> {
        if let Ok(v) = self.lab.get_vm(name) {
            return Some(v);
        }
        for parent in &self.parent_chain {
            if let Ok(v) = parent.get_vm(name) {
                return Some(v);
            }
        }
        None
    }

    /// Walk local lab + parent chain looking for a bridge by name.
    pub(crate) fn lookup_bridge(&self, name: &str) -> Option<crate::bridge::Bridge> {
        if let Ok(b) = self.lab.get_bridge(name) {
            return Some(b);
        }
        for parent in &self.parent_chain {
            if let Ok(b) = parent.get_bridge(name) {
                return Some(b);
            }
        }
        None
    }

    /// Walk local lab + parent chain looking for a sub-lab by name.
    pub(crate) fn lookup_sub_lab(&self, name: &str) -> Option<Lab> {
        if let Some(s) = self.lab.get_sub_lab(name) {
            return Some(s);
        }
        for parent in &self.parent_chain {
            if let Some(s) = parent.get_sub_lab(name) {
                return Some(s);
            }
        }
        None
    }
}

impl UserData for LabUd {
    fn add_methods<M: UserDataMethods<Self>>(methods: &mut M) {
        // ---------------------------------------------------------------
        // VM membership — the design's two-form `lab:vm(...)`:
        //   lab:vm("name")              → lookup
        //   lab:vm("name", "profile")   → create
        // ---------------------------------------------------------------
        methods.add_method(
            "vm",
            |lua, this, args: mlua::Variadic<Value>| {
                match args.len() {
                    1 => {
                        // Lookup form. Walks the parent chain so
                        // test-scope code can find file-scope VMs.
                        let name = arg_string(&args[0], "name")?;
                        let vm = this.lookup_vm(&name).ok_or_else(|| {
                            mlua::Error::external(
                                crate::lab::LabError::UnknownVm(name.clone()),
                            )
                        })?;
                        Ok(Value::UserData(
                            lua.create_userdata(VmUd::wrap(vm))?,
                        ))
                    }
                    2 | 3 => {
                        // Create form: (name, profile, opts?)
                        let name = arg_string(&args[0], "name")?;
                        let profile_name = arg_string(&args[1], "profile")?;
                        let boot_opts = match args.get(2) {
                            None | Some(Value::Nil) => BootOpts::default(),
                            Some(Value::Table(t)) => parse_boot_opts(t)?,
                            Some(other) => {
                                return Err(mlua::Error::external(format!(
                                    "lab:vm: opts must be a table, got {}",
                                    other.type_name()
                                )));
                            }
                        };
                        // Refuse to shadow a parent-scope name. Without
                        // this check, `provium:vm("foo", "p")` inside
                        // a test() body when a file-scope "foo" exists
                        // silently creates a second VM that lookup
                        // would never reach (the local one wins by
                        // construction). Surface the conflict so
                        // authors pick a different name or reuse the
                        // file-scope VM via the 1-arg lookup form.
                        for parent in &this.parent_chain {
                            if parent.get_vm(&name).is_ok() {
                                return Err(mlua::Error::external(format!(
                                    "provium:vm({name:?}, …): name already declared at \
                                     a parent scope (lab `{}`). Pick a different name, \
                                     or use `provium:vm({name:?})` to reuse the \
                                     parent-scope VM.",
                                    parent.name(),
                                )));
                            }
                        }
                        let memory_bytes = boot_opts.memory_bytes.unwrap_or(0);
                        let _cpus = boot_opts.cpus.unwrap_or(0);
                        let vm = this
                            .lab
                            .create_vm(name.clone(), profile_name.clone(), boot_opts)
                            .map_err(mlua::Error::external)?;
                        let ud = super::result_ud::register_resource(
                            lua,
                            VmUd::wrap_with_events_and_meta(
                                vm,
                                Arc::clone(&this.events),
                                this.lab.name().to_owned(),
                                profile_name,
                                memory_bytes,
                            ),
                            "vm",
                        )?;
                        Ok(Value::UserData(ud))
                    }
                    n => Err(mlua::Error::external(format!(
                        "lab:vm: expected 1, 2, or 3 args, got {n}"
                    ))),
                }
            },
        );

        // `lab.<name>` sugar — same as `lab:vm("name")` for declared
        // VMs, falls through to `lab:bridge("name")` for bridges,
        // returns nil otherwise so `if lab.dc1 then ...` is
        // idiomatic. Per `DESIGN.md` § Lab "lab.<name> shorthand
        // always works for lookup of named resources".
        //
        // Also serves `provium.pack` / `provium.unpack` per
        // `DESIGN.md` § API surface / Binary helpers — these route
        // through to Lua's standard `string.pack`/`string.unpack`.
        // `provium.<key> = value` for file-scope config. Routes to a
        // side table the runner reads at chunk-end. Without this,
        // `provium.reset_between_tests = true` would raise an
        // "attempt to index a userdata value" Lua error since LabUd
        // is a UserData with no settable fields by default.
        methods.add_meta_method(
            MetaMethod::NewIndex,
            |lua, _this, (key, value): (String, Value)| {
                const FILE_SCOPE_KEYS: &[&str] = &[
                    "reset_between_tests",
                    "timeout",
                ];
                if !FILE_SCOPE_KEYS.contains(&key.as_str()) {
                    return Err(mlua::Error::external(format!(
                        "cannot set `provium.{key}`: not a recognised \
                         file-scope config key. Known keys: {}",
                        FILE_SCOPE_KEYS.join(", "),
                    )));
                }
                let config: mlua::Table = match lua
                    .globals()
                    .get::<mlua::Table>("_provium_file_config")
                {
                    Ok(t) => t,
                    Err(_) => {
                        let t = lua.create_table()?;
                        lua.globals().set("_provium_file_config", t.clone())?;
                        t
                    }
                };
                config.set(key, value)?;
                Ok(())
            },
        );

        methods.add_meta_method(
            MetaMethod::Index,
            |lua, this, key: String| {
                // Reserved-key checks come FIRST so a sub-lab or
                // resource named "vm_fixture" / "lab_fixture" /
                // "pack" / "unpack" can't shadow the API. (R6 #2:
                // sub-lab lookup at the same priority as named
                // resources let those names hijack the dot-call
                // sugar.)
                if key == "pack" || key == "unpack" {
                    let string_lib: mlua::Table = lua.globals().get("string")?;
                    return string_lib.get::<Value>(key.as_str());
                }
                // File-scope config read-back so `provium.timeout`
                // returns whatever was assigned earlier in the chunk.
                if key == "reset_between_tests" || key == "timeout" {
                    if let Ok(config) = lua
                        .globals()
                        .get::<mlua::Table>("_provium_file_config")
                    {
                        return config.get::<Value>(key.as_str());
                    }
                    return Ok(Value::Nil);
                }
                if key == "vm_fixture" {
                    // Fixtures land at file root (bottom of the
                    // parent chain) so the materialised VM survives
                    // past the current test's scope and the cache
                    // pays off across tests.
                    let lab_clone = this.clone();
                    let f = lua.create_function(
                        move |_, name: String| {
                            let mut for_fixture = lab_clone.clone();
                            for_fixture.lab = lab_clone.fixture_lab().clone();
                            for_fixture.parent_chain = Vec::new();
                            let vm = build_or_resume_fixture(&for_fixture, &name)
                                .map_err(|e| mlua::Error::external(e.to_string()))?;
                            Ok(VmUd::wrap(vm))
                        },
                    )?;
                    return Ok(Value::Function(f));
                }
                if key == "lab_fixture" {
                    let lab_clone = this.clone();
                    let f = lua.create_function(
                        move |_, name: String| {
                            let mut for_fixture = lab_clone.clone();
                            for_fixture.lab = lab_clone.fixture_lab().clone();
                            for_fixture.parent_chain = Vec::new();
                            let lab = build_or_resume_lab_fixture(&for_fixture, &name)
                                .map_err(mlua::Error::external)?;
                            Ok(LabUd {
                                lab,
                                parent_chain: Vec::new(),
                                config: Arc::clone(&lab_clone.config),
                                vmm: Arc::clone(&lab_clone.vmm),
                                events: Arc::clone(&lab_clone.events),
                                pool: lab_clone.pool.clone(),
                            })
                        },
                    )?;
                    return Ok(Value::Function(f));
                }
                if let Some(vm) = this.lookup_vm(&key) {
                    return Ok(Value::UserData(
                        lua.create_userdata(VmUd::wrap(vm))?,
                    ));
                }
                if let Some(bridge) = this.lookup_bridge(&key) {
                    return Ok(Value::UserData(
                        lua.create_userdata(BridgeUd::wrap(bridge))?,
                    ));
                }
                if let Some(sub) = this.lookup_sub_lab(&key) {
                    return Ok(Value::UserData(lua.create_userdata(LabUd {
                        lab: sub,
                        parent_chain: Vec::new(),
                        config: Arc::clone(&this.config),
                        vmm: Arc::clone(&this.vmm),
                        events: Arc::clone(&this.events),
                        pool: this.pool.clone(),
                    })?));
                }
                Ok(Value::Nil)
            },
        );

        // ---------------------------------------------------------------
        // Bridges — overload like vm:
        //   lab:bridge("name")          → lookup
        //   lab:bridge("name", opts?)   → create
        // ---------------------------------------------------------------
        methods.add_method("bridge", |lua, this, args: mlua::Variadic<Value>| {
            match args.len() {
                1 => {
                    let name = arg_string(&args[0], "name")?;
                    let bridge = this
                        .lab
                        .get_bridge(&name)
                        .map_err(mlua::Error::external)?;
                    Ok(Value::UserData(
                        lua.create_userdata(BridgeUd::wrap(bridge))?,
                    ))
                }
                2 | 3 => {
                    let name = arg_string(&args[0], "name")?;
                    for parent in &this.parent_chain {
                        if parent.get_bridge(&name).is_ok() {
                            return Err(mlua::Error::external(format!(
                                "provium:bridge({name:?}, …): name already declared at \
                                 a parent scope (lab `{}`). Pick a different name, \
                                 or use `provium:bridge({name:?})` to reuse the \
                                 parent-scope bridge.",
                                parent.name(),
                            )));
                        }
                    }
                    let bridge = this
                        .lab
                        .create_bridge(name)
                        .map_err(mlua::Error::external)?;
                    let ud = super::result_ud::register_resource(
                        lua,
                        BridgeUd::wrap(bridge),
                        "bridge",
                    )?;
                    Ok(Value::UserData(ud))
                }
                n => Err(mlua::Error::external(format!(
                    "lab:bridge: expected 1, 2, or 3 args, got {n}"
                ))),
            }
        });

        methods.add_method("bridge_names", |_, this, ()| Ok(this.lab.bridge_names()));

        // ---------------------------------------------------------------
        // Sub-labs
        // ---------------------------------------------------------------
        methods.add_method(
            "lab",
            |_, this, name: Option<String>| {
                let chosen =
                    name.unwrap_or_else(|| auto_sub_lab_name(&this.lab));
                for parent in &this.parent_chain {
                    if parent.get_sub_lab(&chosen).is_some() {
                        return Err(mlua::Error::external(format!(
                            "provium:lab({chosen:?}): sub-lab name already \
                             declared at a parent scope (lab `{}`). Pick a \
                             different name.",
                            parent.name(),
                        )));
                    }
                }
                let sub = this
                    .lab
                    .sub_lab_checked(chosen)
                    .map_err(mlua::Error::external)?;
                Ok(LabUd {
                    lab: sub,
                    parent_chain: Vec::new(),
                    config: Arc::clone(&this.config),
                    vmm: Arc::clone(&this.vmm),
                    events: Arc::clone(&this.events),
                    pool: this.pool.clone(),
                })
            },
        );

        // ---------------------------------------------------------------
        // Fixtures
        // ---------------------------------------------------------------
        // `lab:depends_on_file(host_path)` — declare an external
        // host-side file as a fixture-cache dependency. The runtime
        // behaviour is a no-op; the call's literal first argument is
        // picked up by `fixture::scan_external_file_deps` and folded
        // into the cache key. Used for files the fixture reads on
        // the host side (config templates passed through Lua,
        // binaries the build process invokes locally, etc.) — files
        // pushed into the guest via `vm:push_file` already track
        // themselves.
        methods.add_method("depends_on_file", |_, _this, _host: String| Ok(()));

        methods.add_method("vm_fixture", |_, this, args: mlua::Variadic<Value>| {
            // `provium:vm_fixture("path/to/fixture")` —
            // resolves to <test_root>/path/to/fixture.fixture.lua.
            let fixture_name = match args.first() {
                Some(Value::String(s)) => s
                    .to_str()
                    .map_err(|e| e.to_string())
                    .map_err(mlua::Error::external)?
                    .to_string(),
                _ => {
                    return Err(mlua::Error::external(
                        "provium:vm_fixture(name): name must be a string",
                    ));
                }
            };
            let vm = build_or_resume_fixture(this, &fixture_name)
                .map_err(|e| mlua::Error::external(e.to_string()))?;
            Ok(VmUd::wrap(vm))
        });

        // ---------------------------------------------------------------
        // Batch lifecycle
        // ---------------------------------------------------------------
        methods.add_method("boot", |_, this, ()| {
            this.lab
                .boot_with_pool(this.pool.as_ref())
                .map_err(mlua::Error::external)?;
            Ok(this.clone())
        });
        methods.add_method("shutdown", |_, this, ()| {
            this.lab.shutdown().map_err(mlua::Error::external)?;
            Ok(())
        });
        methods.add_method("pause", |_, this, ()| {
            this.lab.pause().map_err(mlua::Error::external)?;
            Ok(())
        });
        methods.add_method("resume", |_, this, ()| {
            this.lab.resume().map_err(mlua::Error::external)?;
            Ok(())
        });

        // ---------------------------------------------------------------
        // Introspection
        // ---------------------------------------------------------------
        // Binary helpers. Re-exports of Lua 5.4 string.pack / unpack.
        methods.add_method(
            "pack",
            |lua, _this, args: mlua::Variadic<Value>| {
                let string_table: mlua::Table = lua.globals().get("string")?;
                let pack: mlua::Function = string_table.get("pack")?;
                pack.call::<Value>(args)
            },
        );
        methods.add_method(
            "unpack",
            |lua, _this, args: mlua::Variadic<Value>| {
                let string_table: mlua::Table = lua.globals().get("string")?;
                let unpack: mlua::Function = string_table.get("unpack")?;
                unpack.call::<mlua::MultiValue>(args)
            },
        );
        // `provium.lab_fixture` — like vm_fixture but the builder
        // returns a `provium:snapshot()` (whole-lab snapshot, no
        // dir arg) instead of a `vm:snapshot()`. The cached entry
        // is a `<key>.lab/` directory containing per-VM snapshots
        // + lab.json. Restored into a fresh sub-lab, returned.
        methods.add_method("lab_fixture", |_, this, name: String| {
            let mut for_fixture = this.clone();
            for_fixture.lab = this.fixture_lab().clone();
            for_fixture.parent_chain = Vec::new();
            let lab = build_or_resume_lab_fixture(&for_fixture, &name)
                .map_err(mlua::Error::external)?;
            Ok(LabUd {
                lab,
                parent_chain: Vec::new(),
                config: Arc::clone(&this.config),
                vmm: Arc::clone(&this.vmm),
                events: Arc::clone(&this.events),
                pool: this.pool.clone(),
            })
        });

        methods.add_method("vm_names", |_, this, ()| Ok(this.lab.vm_names()));
        methods.add_method("sub_lab_names", |_, this, ()| Ok(this.lab.sub_lab_names()));
        methods.add_method("name", |_, this, ()| Ok(this.lab.name().to_owned()));

        // include / remove / members.
        // lab:include(vm) — single resource form. Also accepts a
        // Lua array of resources, per `DESIGN.md` § Lab:
        // `lab:include(resource_or_list)`.
        methods.add_method("include", |_, this, value: Value| {
            fn shadow_check(
                parents: &[crate::lab::Lab],
                kind: &str,
                name: &str,
            ) -> mlua::Result<()> {
                for parent in parents {
                    let exists = match kind {
                        "vm" => parent.get_vm(name).is_ok(),
                        "bridge" => parent.get_bridge(name).is_ok(),
                        "sub-lab" => parent.get_sub_lab(name).is_some(),
                        _ => false,
                    };
                    if exists {
                        return Err(mlua::Error::external(format!(
                            "lab:include: {kind} name `{name}` already declared \
                             at a parent scope (lab `{}`). Pick a different name.",
                            parent.name(),
                        )));
                    }
                }
                Ok(())
            }
            fn include_one(
                lab: &crate::lab::Lab,
                parents: &[crate::lab::Lab],
                ud: &mlua::AnyUserData,
            ) -> mlua::Result<()> {
                // DESIGN.md § Lab specifies
                // `lab:include(resource_or_list)` — try every
                // first-class resource kind in turn.
                if let Ok(vm) = ud.borrow::<super::vm_ud::VmUd>() {
                    let inner = vm.clone().into_inner();
                    let n = inner.name().to_owned();
                    shadow_check(parents, "vm", &n)?;
                    return lab.include_vm(inner).map_err(mlua::Error::external);
                }
                if let Ok(b) = ud.borrow::<super::bridge_ud::BridgeUd>() {
                    let bridge = b.clone_bridge();
                    let n = bridge.name().to_owned();
                    shadow_check(parents, "bridge", &n)?;
                    return lab
                        .include_bridge(bridge)
                        .map_err(mlua::Error::external);
                }
                if let Ok(sub) = ud.borrow::<super::lab_ud::LabUd>() {
                    let sublab = sub.lab.clone();
                    let n = sublab.name().to_owned();
                    shadow_check(parents, "sub-lab", &n)?;
                    return lab
                        .include_sub_lab(sublab)
                        .map_err(mlua::Error::external);
                }
                Err(mlua::Error::external(
                    "lab:include: expected vm, bridge, or lab userdata",
                ))
            }
            match value {
                Value::UserData(u) => {
                    include_one(&this.lab, &this.parent_chain, &u)
                }
                Value::Table(t) => {
                    for pair in t.sequence_values::<Value>() {
                        let v = pair?;
                        if let Value::UserData(u) = v {
                            include_one(&this.lab, &this.parent_chain, &u)?;
                        } else {
                            return Err(mlua::Error::external(
                                "lab:include: array entries must be userdata",
                            ));
                        }
                    }
                    Ok(())
                }
                other => Err(mlua::Error::external(format!(
                    "lab:include expected userdata or array, got {}",
                    other.type_name()
                ))),
            }
        });
        // Accept either a string name or a VM userdata for ergonomic
        // `lab:remove(my_vm)` per `DESIGN.md` § Lab.
        methods.add_method("remove", |_, this, value: Value| {
            // Symmetric with lab:include — accepts a bare name or
            // a vm/bridge/sub-lab userdata. Removal is graph-state
            // only; the underlying resource (VM, bridge, sub-lab)
            // is NOT shut down or unrealised.
            match value {
                Value::String(s) => {
                    let name = s
                        .to_str()
                        .map_err(|e| mlua::Error::external(e.to_string()))?
                        .to_string();
                    // Bare-name lookup tries each kind in turn so
                    // existing tests passing a string still work.
                    if this.lab.remove_vm(&name).is_some() {
                        return Ok(());
                    }
                    if this.lab.remove_bridge(&name).is_some() {
                        return Ok(());
                    }
                    let _ = this.lab.remove_sub_lab(&name);
                    Ok(())
                }
                Value::UserData(ud) => {
                    if let Ok(vmud) = ud.borrow::<super::vm_ud::VmUd>() {
                        this.lab.remove_vm(&vmud.vm_name());
                        return Ok(());
                    }
                    if let Ok(b) = ud.borrow::<super::bridge_ud::BridgeUd>() {
                        this.lab.remove_bridge(b.clone_bridge().name());
                        return Ok(());
                    }
                    if let Ok(sub) = ud.borrow::<LabUd>() {
                        this.lab.remove_sub_lab(sub.lab.name());
                        return Ok(());
                    }
                    Err(mlua::Error::external(
                        "lab:remove: userdata is not a vm, bridge, or lab",
                    ))
                }
                other => Err(mlua::Error::external(format!(
                    "lab:remove expected name|vm|bridge|lab, got {}",
                    other.type_name()
                ))),
            }
        });
        methods.add_method("members", |lua, this, ()| {
            let table = lua.create_table()?;
            for (i, (kind, name)) in this.lab.members().iter().enumerate() {
                let entry = lua.create_table()?;
                entry.set("kind", *kind)?;
                entry.set("name", name.clone())?;
                table.set(i + 1, entry)?;
            }
            Ok(Value::Table(table))
        });

        // Lab snapshot / restore. With an explicit `dir` arg the
        // snapshot lands there and the dir path is returned (legacy
        // shape). Without args the snapshot writes to a fresh
        // tempdir and returns a [`LabSnapshotUd`] — the form a
        // lab fixture builder returns.
        methods.add_method("snapshot", |lua, this, dir: Option<String>| {
            match dir {
                Some(d) => {
                    this.lab
                        .lab_snapshot(std::path::Path::new(&d))
                        .map_err(mlua::Error::external)?;
                    Ok(Value::String(lua.create_string(&d)?))
                }
                None => {
                    let td = tempfile::tempdir()
                        .map_err(mlua::Error::external)?
                        .keep();
                    this.lab
                        .lab_snapshot(&td)
                        .map_err(mlua::Error::external)?;
                    let ud = super::lab_snapshot_ud::LabSnapshotUd { dir: td };
                    Ok(Value::UserData(lua.create_userdata(ud)?))
                }
            }
        });
        methods.add_method("restore", |_, this, arg: Value| {
            // Accept either a directory path (string) OR a
            // LabSnapshotUd returned by `lab:snapshot()`. The
            // userdata form is the idiomatic DESIGN pattern
            // (`local s = lab:snapshot(); lab:restore(s)`); the
            // string form stays for tests that load snapshots
            // off disk by path.
            let dir: std::path::PathBuf = match arg {
                Value::String(s) => std::path::PathBuf::from(
                    s.to_str().map_err(mlua::Error::external)?.to_string(),
                ),
                Value::UserData(ud) => {
                    let snap = ud
                        .borrow::<super::lab_snapshot_ud::LabSnapshotUd>()
                        .map_err(|_| mlua::Error::external(
                            "lab:restore: expected lab snapshot userdata or directory string",
                        ))?;
                    snap.dir.clone()
                }
                other => {
                    return Err(mlua::Error::external(format!(
                        "lab:restore: expected snapshot|string, got {}",
                        other.type_name()
                    )));
                }
            };
            let meta_path = dir.join("lab.json");
            let body = std::fs::read_to_string(&meta_path)
                .map_err(mlua::Error::external)?;
            let meta: crate::lab::LabSnapshotMeta =
                serde_json::from_str(&body).map_err(|e| mlua::Error::external(e.to_string()))?;
            this.lab
                .lab_restore(&meta)
                .map_err(mlua::Error::external)?;
            Ok(())
        });

        // claim({memory=..., cpus=...}) — file-level reservation
        // against the dispatcher's pool. Idempotent per-Lab: a
        // second call raises "claim already taken". When no pool
        // is wired (REPL / single-file ad-hoc runs), claims
        // succeed silently.
        methods.add_method("claim", |_, this, opts: mlua::Table| {
            let amount = parse_resource_amount(&opts)?;
            let Some(pool) = this.pool.as_ref() else {
                // No pool — still enforce one-shot per design.
                this.lab
                    .note_claim_taken()
                    .map_err(mlua::Error::external)?;
                return Ok(());
            };
            this.lab
                .claim(pool, amount)
                .map_err(mlua::Error::external)?;
            this.events.emit(Event::ClaimAcquired(
                provium_protocol::events::ClaimAcquired {
                    path: this.lab.name().to_owned(),
                    amount: provium_protocol::events::ResourceAmount {
                        memory_bytes: amount.memory_bytes,
                        cpus: amount.cpus,
                    },
                },
            ));
            Ok(())
        });

        // barrier(name, count, timeout?)
        methods.add_method(
            "barrier",
            |_, this, args: mlua::Variadic<Value>| {
                let name = match args.first() {
                    Some(Value::String(s)) => s.to_str().map_err(|e| e.to_string())
                        .map_err(mlua::Error::external)?
                        .to_string(),
                    _ => return Err(mlua::Error::external("barrier: name must be a string")),
                };
                let count = match args.get(1) {
                    Some(Value::Integer(n)) => *n as usize,
                    _ => return Err(mlua::Error::external("barrier: count must be int")),
                };
                let timeout = match args.get(2) {
                    Some(Value::Number(s)) => std::time::Duration::from_secs_f64(*s),
                    Some(Value::Integer(s)) => std::time::Duration::from_secs(*s as u64),
                    None | Some(Value::Nil) => std::time::Duration::from_secs(60),
                    _ => return Err(mlua::Error::external("barrier: timeout must be number")),
                };
                Ok(this.lab.barrier(&name, count, timeout))
            },
        );

        methods.add_meta_method(MetaMethod::ToString, |_, this, ()| {
            Ok(format!(
                "lab({}; {} vm(s))",
                this.lab.name(),
                this.lab.vm_names().len()
            ))
        });
    }
}

fn arg_string(v: &Value, what: &str) -> mlua::Result<String> {
    match v {
        Value::String(s) => Ok(s.to_str().map_err(|e| e.to_string()).map_err(mlua::Error::external)?.to_string()),
        other => Err(mlua::Error::external(format!(
            "lab:vm: {what} must be a string, got {}",
            other.type_name()
        ))),
    }
}

fn parse_boot_opts(t: &mlua::Table) -> mlua::Result<BootOpts> {
    let memory = t.get::<Option<Value>>("memory")?;
    let cpus = t.get::<Option<u32>>("cpus")?;

    let memory_bytes = match memory {
        None | Some(Value::Nil) => None,
        Some(Value::Integer(n)) => Some(u64::try_from(n).map_err(|_| {
            mlua::Error::external("opts.memory: negative or overflowing value")
        })?),
        Some(Value::String(s)) => {
            let owned = s
                .to_str()
                .map_err(|e| e.to_string())
                .map_err(mlua::Error::external)?
                .to_string();
            Some(parse_memory_string(&owned)?)
        }
        Some(other) => {
            return Err(mlua::Error::external(format!(
                "opts.memory must be int or string, got {}",
                other.type_name()
            )));
        }
    };

    Ok(BootOpts {
        memory_bytes,
        cpus,
        ..Default::default()
    })
}

/// Translate a Lua `{memory="2G", cpus=4}` table into the
/// scheduler's [`crate::scheduler::ResourceAmount`].
fn parse_resource_amount(t: &mlua::Table) -> mlua::Result<crate::scheduler::ResourceAmount> {
    let memory_bytes = match t.get::<Value>("memory")? {
        Value::Nil => 0,
        Value::String(s) => parse_memory_string(
            s.to_str()
                .map_err(|e| mlua::Error::external(e.to_string()))?
                .as_ref(),
        )?,
        Value::Integer(n) if n >= 0 => n as u64,
        Value::Number(n) if n >= 0.0 => n as u64,
        other => {
            return Err(mlua::Error::external(format!(
                "claim: memory expected string|int, got {}",
                other.type_name()
            )))
        }
    };
    let cpus = match t.get::<Value>("cpus")? {
        Value::Nil => 0,
        Value::Integer(n) if n >= 0 => n as u32,
        Value::Number(n) if n >= 0.0 => n as u32,
        other => {
            return Err(mlua::Error::external(format!(
                "claim: cpus expected int, got {}",
                other.type_name()
            )))
        }
    };
    Ok(crate::scheduler::ResourceAmount {
        memory_bytes,
        cpus,
    })
}

fn parse_memory_string(s: &str) -> mlua::Result<u64> {
    // Accept "512M", "2G", "1024k". Slice-2 simple form — more
    // formats can be added later.
    let s = s.trim();
    let (num_part, multiplier) = match s.chars().last() {
        Some('K') | Some('k') => (&s[..s.len() - 1], 1024u64),
        Some('M') | Some('m') => (&s[..s.len() - 1], 1024 * 1024),
        Some('G') | Some('g') => (&s[..s.len() - 1], 1024 * 1024 * 1024),
        _ => (s, 1),
    };
    let n: u64 = num_part.parse().map_err(|_| {
        mlua::Error::external(format!("opts.memory: cannot parse `{s}`"))
    })?;
    Ok(n * multiplier)
}

/// Resolve every dependency referenced from `source` into a list
/// of keys. Recursive — depth-first, with a `seen` guard to break
/// cycles (a self-referential fixture is a build error elsewhere
/// but we still avoid infinite recursion here).
/// Public-from-the-binary entry point. Discards the `seen` cycle
/// state — callers use this in one-shot CLI paths.
pub(crate) fn resolve_dep_keys_pub(
    roots: &[String],
    source: &[u8],
) -> Vec<crate::fixture::CacheKey> {
    resolve_dep_keys(roots, source, &mut Vec::new()).unwrap_or_default()
}

fn resolve_dep_keys(
    roots: &[String],
    source: &[u8],
    seen: &mut Vec<String>,
) -> Result<Vec<crate::fixture::CacheKey>, String> {
    let text = std::str::from_utf8(source).unwrap_or("");
    let mut keys = Vec::new();
    // Fixture references first — recurse fully.
    for dep in crate::fixture::scan_fixture_deps(text) {
        if seen.contains(&dep) {
            continue;
        }
        seen.push(dep.clone());
        let dep_path = match locate_fixture(roots, &dep) {
            Ok(p) => p,
            Err(_) => continue, // missing dep surfaces at build time
        };
        let dep_source = match crate::fixture::read_fixture_source(&dep_path) {
            Ok(s) => s,
            Err(_) => continue,
        };
        let nested = resolve_dep_keys(roots, &dep_source, seen)?;
        keys.push(crate::fixture::compute_key_with_deps(&dep_source, &nested));
    }
    // require()d helpers: locate `<root>/<name with .>...lua`. We
    // hash the helper's source bytes (recursing into its require()s)
    // so editing a helper invalidates the cache.
    for module in crate::fixture::scan_require_deps(text) {
        let marker = format!("require::{module}");
        if seen.contains(&marker) {
            continue;
        }
        seen.push(marker);
        let rel = module.replace('.', "/");
        let mut helper_path: Option<std::path::PathBuf> = None;
        for root in roots {
            for cand in &[
                std::path::Path::new(root).join(format!("{rel}.lua")),
                std::path::Path::new(root).join(&rel).join("init.lua"),
            ] {
                if cand.is_file() {
                    helper_path = Some(cand.clone());
                    break;
                }
            }
            if helper_path.is_some() {
                break;
            }
        }
        let Some(helper_path) = helper_path else { continue };
        let Ok(helper_source) = crate::fixture::read_fixture_source(&helper_path)
        else {
            continue;
        };
        let nested = resolve_dep_keys(roots, &helper_source, seen)?;
        keys.push(crate::fixture::compute_key_with_deps(
            &helper_source,
            &nested,
        ));
    }
    Ok(keys)
}

/// Walk the dependency graph starting at `fixture_path` and collect
/// every external host-file path declared via `vm:push_file(...)` or
/// `lab:depends_on_file(...)` — anywhere in the fixture or its
/// transitively-required helpers / referenced fixtures. Relative
/// paths are resolved against the directory of the file that
/// contains the call (so `vm:push_file("../bin", …)` is stable
/// regardless of where `provium` was invoked from).
///
/// Returns paths in deterministic order with duplicates removed.
/// Non-existent paths are kept in the list (canonicalisation falls
/// back to the raw path); the metadata fold quietly skips mtime/size
/// on stat failure, so a non-existent dep only folds its path
/// string into the key — creating the file later still invalidates.
pub(crate) fn resolve_external_deps_pub(
    roots: &[String],
    fixture_path: &std::path::Path,
    source: &[u8],
) -> Vec<std::path::PathBuf> {
    let mut out = Vec::new();
    let mut seen = Vec::new();
    let fixture_dir = fixture_path
        .parent()
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| std::path::PathBuf::from("."));
    collect_external_deps(roots, &fixture_dir, source, &mut out, &mut seen);
    // Stable dedupe: keep first occurrence (sorted insertion is
    // already deterministic — `scan_external_file_deps` returns
    // source order, and recursion is depth-first by source order).
    let mut deduped: Vec<std::path::PathBuf> = Vec::with_capacity(out.len());
    for p in out {
        if !deduped.iter().any(|q| q == &p) {
            deduped.push(p);
        }
    }
    deduped
}

fn collect_external_deps(
    roots: &[String],
    source_dir: &std::path::Path,
    source: &[u8],
    out: &mut Vec<std::path::PathBuf>,
    seen: &mut Vec<String>,
) {
    let text = std::str::from_utf8(source).unwrap_or("");
    for raw in crate::fixture::scan_external_file_deps(text) {
        let path = if std::path::Path::new(&raw).is_absolute() {
            std::path::PathBuf::from(&raw)
        } else {
            source_dir.join(&raw)
        };
        // Canonicalize when possible so `../foo` and a sibling's
        // `./foo` collapse to the same entry across recursion.
        let canon = std::fs::canonicalize(&path).unwrap_or(path);
        out.push(canon);
    }
    for dep in crate::fixture::scan_fixture_deps(text) {
        let marker = format!("ext::fixture::{dep}");
        if seen.contains(&marker) {
            continue;
        }
        seen.push(marker);
        let dep_path = match locate_fixture(roots, &dep) {
            Ok(p) => p,
            Err(_) => continue,
        };
        let dep_source = match crate::fixture::read_fixture_source(&dep_path) {
            Ok(s) => s,
            Err(_) => continue,
        };
        let dep_dir = dep_path
            .parent()
            .map(|p| p.to_path_buf())
            .unwrap_or_else(|| std::path::PathBuf::from("."));
        collect_external_deps(roots, &dep_dir, &dep_source, out, seen);
    }
    for module in crate::fixture::scan_require_deps(text) {
        let marker = format!("ext::require::{module}");
        if seen.contains(&marker) {
            continue;
        }
        seen.push(marker);
        let rel = module.replace('.', "/");
        let mut helper_path: Option<std::path::PathBuf> = None;
        for root in roots {
            for cand in &[
                std::path::Path::new(root).join(format!("{rel}.lua")),
                std::path::Path::new(root).join(&rel).join("init.lua"),
            ] {
                if cand.is_file() {
                    helper_path = Some(cand.clone());
                    break;
                }
            }
            if helper_path.is_some() {
                break;
            }
        }
        let Some(helper_path) = helper_path else { continue };
        let Ok(helper_source) = crate::fixture::read_fixture_source(&helper_path) else {
            continue;
        };
        let helper_dir = helper_path
            .parent()
            .map(|p| p.to_path_buf())
            .unwrap_or_else(|| std::path::PathBuf::from("."));
        collect_external_deps(roots, &helper_dir, &helper_source, out, seen);
    }
}

/// Canonical kernel/initrd paths for cache-key folding. Picks the
/// first profile by sorted name (deterministic across runs).
///
/// R9 sched-m4: this is the LEGACY single-profile helper. Use
/// [`canonical_profile_paths_all`] in new call sites — folding
/// only the first profile's paths silently misses kernel
/// invalidation on profiles that sort later. Kept temporarily
/// for callers we haven't migrated yet.
#[allow(dead_code)]
fn canonical_profile_paths(
    config: &Arc<Config>,
) -> (Option<std::path::PathBuf>, Option<std::path::PathBuf>) {
    let mut names: Vec<&String> = config.profiles.keys().collect();
    names.sort();
    let Some(first) = names.first() else {
        return (None, None);
    };
    let p = match config.profiles.get(*first) {
        Some(p) => p,
        None => return (None, None),
    };
    (Some(p.kernel.clone()), Some(p.initrd.clone()))
}

/// All profiles' kernel/initrd paths, sorted by profile name for
/// determinism. Multi-profile cache-key folding so a kernel
/// swap on any profile invalidates every fixture.
fn canonical_profile_paths_all(
    config: &Arc<Config>,
) -> (Vec<std::path::PathBuf>, Vec<std::path::PathBuf>) {
    let mut names: Vec<&String> = config.profiles.keys().collect();
    names.sort();
    let mut kernels = Vec::with_capacity(names.len());
    let mut initrds = Vec::with_capacity(names.len());
    for n in names {
        if let Some(p) = config.profiles.get(n) {
            kernels.push(p.kernel.clone());
            initrds.push(p.initrd.clone());
        }
    }
    (kernels, initrds)
}

#[cfg(test)]
mod external_dep_tests {
    use super::*;
    use std::fs;

    #[test]
    fn resolves_relative_against_fixture_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let fix_dir = tmp.path().join("tests/fixtures");
        fs::create_dir_all(&fix_dir).unwrap();
        let fix = fix_dir.join("base.fixture.lua");
        // External file lives at `tests/bin/foo`, referenced
        // relative to fixture dir as `../bin/foo`.
        let bin_dir = tmp.path().join("tests/bin");
        fs::create_dir_all(&bin_dir).unwrap();
        let bin = bin_dir.join("foo");
        fs::write(&bin, b"hello").unwrap();
        fs::write(
            &fix,
            r#"vm:push_file("../bin/foo", "/usr/bin/foo")"#,
        )
        .unwrap();
        let source = fs::read(&fix).unwrap();
        let roots = vec![tmp.path().join("tests").to_string_lossy().into_owned()];
        let out = resolve_external_deps_pub(&roots, &fix, &source);
        assert_eq!(out.len(), 1, "got {:?}", out);
        // canonicalize follows symlinks, so just check the file
        // it resolves to ends with "bin/foo".
        assert!(
            out[0].ends_with("bin/foo"),
            "expected path ending in bin/foo, got {:?}",
            out[0]
        );
    }

    #[test]
    fn dedupes_repeated_paths() {
        let tmp = tempfile::tempdir().unwrap();
        let fix = tmp.path().join("a.fixture.lua");
        let bin = tmp.path().join("foo");
        fs::write(&bin, b"x").unwrap();
        fs::write(
            &fix,
            r#"
                vm:push_file("foo", "/a")
                lab:depends_on_file("foo")
            "#,
        )
        .unwrap();
        let source = fs::read(&fix).unwrap();
        let out = resolve_external_deps_pub(&[], &fix, &source);
        assert_eq!(out.len(), 1);
    }

    #[test]
    fn opt_out_is_excluded() {
        let tmp = tempfile::tempdir().unwrap();
        let fix = tmp.path().join("a.fixture.lua");
        fs::write(
            &fix,
            r#"vm:push_file("foo", "/a", {auto_dep = false})"#,
        )
        .unwrap();
        let source = fs::read(&fix).unwrap();
        let out = resolve_external_deps_pub(&[], &fix, &source);
        assert!(out.is_empty(), "got {:?}", out);
    }
}

/// Resolve a lab fixture into a sub-Lab. Cache layout differs from
/// the single-VM case: snapshot lives under `<key>.lab/` directory
/// containing `lab.json` + per-VM `.snap` files.
fn build_or_resume_lab_fixture(
    this: &LabUd,
    fixture_name: &str,
) -> Result<crate::lab::Lab, String> {
    let fixture_path = locate_fixture(&this.config.provium.roots, fixture_name)
        .map_err(|e| e.to_string())?;
    let source = fixture::read_fixture_source(&fixture_path).map_err(|e| e.to_string())?;
    let dep_keys =
        resolve_dep_keys(&this.config.provium.roots, &source, &mut Vec::new())?;
    let (kernels, initrds) = canonical_profile_paths_all(&this.config);
    let kernel_refs: Vec<&std::path::Path> = kernels.iter().map(|p| p.as_path()).collect();
    let initrd_refs: Vec<&std::path::Path> = initrds.iter().map(|p| p.as_path()).collect();
    let externals =
        resolve_external_deps_pub(&this.config.provium.roots, &fixture_path, &source);
    let external_refs: Vec<&std::path::Path> = externals.iter().map(|p| p.as_path()).collect();
    let key = fixture::compute_key_with_deps_kernels_and_externals(
        &source,
        &dep_keys,
        &kernel_refs,
        &initrd_refs,
        &external_refs,
    );
    let cache_dir = this
        .config
        .provium
        .cache_dir
        .clone()
        .unwrap_or_else(default_cache_dir);
    std::fs::create_dir_all(&cache_dir).map_err(|e| e.to_string())?;
    let lab_dir = cache_dir.join(format!("{}.lab", key));
    let lock_path = cache_dir.join(format!("{}.lab.lock", key));

    // Cache hit?
    let meta_path = lab_dir.join("lab.json");
    let mut corrupt_to_evict = false;
    if meta_path.is_file() {
        match restore_lab_from_dir(this, fixture_name, &lab_dir) {
            Ok(lab) => {
                // Emit CacheHit only after successful restore so
                // consumers don't see a stray Hit on a corrupt
                // entry.
                this.events.emit(Event::FixtureCacheHit(FixtureCacheHit {
                    path: fixture_name.to_owned(),
                }));
                // Bump the cached entry's atime so LRU eviction
                // sorts this lab fixture as freshly used. Without
                // it, relatime would let every entry's atime
                // collapse together and eviction would degrade to
                // filesystem order.
                crate::fixture::bump_atime(&lab_dir);
                return Ok(lab);
            }
            Err(e) => {
                eprintln!(
                    "provium: lab fixture cache `{fixture_name}` corrupt ({e}); evicting + rebuilding"
                );
                // Defer the eviction until *after* we hold the
                // build lock — otherwise a peer that just installed
                // a fresh copy in the meantime would have its
                // install deleted by us. Re-check under lock.
                corrupt_to_evict = true;
            }
        }
    }

    let wait_started = std::time::Instant::now();
    // Emit FixtureBuildWaiting AS the wait begins (via the
    // notify-callback) rather than after it resolves — without
    // the callback, dashboards saw the event only when the wait
    // had already finished.
    let fixture_for_event = fixture_name.to_owned();
    let events_for_event = this.events.clone();
    let (_lock, was_contended) = crate::fixture::acquire_build_lock_observed_notify(
        &lock_path,
        move |holder| {
            events_for_event.emit(Event::FixtureBuildWaiting(FixtureBuildWaiting {
                path: fixture_for_event,
                held_by_file: holder.unwrap_or_default(),
            }));
        },
    )
    .map_err(|e| e.to_string())?;
    // Now that we hold the lock, re-evaluate whether the corrupt
    // entry is still corrupt — a peer may have replaced it with a
    // good copy while we were queuing.
    if corrupt_to_evict {
        match restore_lab_from_dir(this, fixture_name, &lab_dir) {
            Ok(lab) => {
                crate::fixture::bump_atime(&lab_dir);
                return Ok(lab);
            }
            Err(_) => {
                let _ = std::fs::remove_dir_all(&lab_dir);
            }
        }
    }

    // Critical section: if a peer just installed the fixture under
    // the same lock, skip the build. We must NOT emit
    // `FixtureBuildStarted` in that path — there'd be no matching
    // `FixtureBuildDone` for consumers to pair it with.
    if meta_path.is_file() {
        if was_contended {
            this.events.emit(Event::FixtureBuildDone(FixtureBuildDone {
                path: fixture_name.to_owned(),
                duration_ns: u64::try_from(wait_started.elapsed().as_nanos())
                    .unwrap_or(u64::MAX),
                snapshot_bytes: dir_size_bytes(&lab_dir),
            }));
        }
        let lab = restore_lab_from_dir(this, fixture_name, &lab_dir)?;
        crate::fixture::bump_atime(&lab_dir);
        return Ok(lab);
    }

    this.events.emit(Event::FixtureBuildStarted(FixtureBuildStarted {
        path: fixture_name.to_owned(),
    }));
    let build_started = std::time::Instant::now();

    let outcome = crate::lua::fixture_build::build_fixture(
        &fixture_path,
        Arc::clone(&this.config),
        Arc::clone(&this.vmm),
    )
    .map_err(|e| e.to_string())?;
    let src_dir = match &outcome {
        crate::lua::fixture_build::FixtureBuildOutcome::Lab { snapshot_dir } => {
            snapshot_dir.clone()
        }
        crate::lua::fixture_build::FixtureBuildOutcome::SingleVm { .. } => {
            return Err(format!(
                "lab_fixture `{fixture_name}` returned a single-VM snapshot; expected provium:snapshot()"
            ));
        }
    };
    // Atomic install: try `renameat2(RENAME_EXCHANGE)` so an
    // existing `<key>.lab/` is swapped with the freshly-built
    // one with no observable in-between state. If the kernel
    // doesn't support the syscall (very old kernels) or the
    // target doesn't exist, fall back to a plain rename, with
    // a remove-first only when strictly necessary.
    atomic_install_lab_dir(&src_dir, &lab_dir).map_err(|e| e.to_string())?;

    this.events.emit(Event::FixtureBuildDone(FixtureBuildDone {
        path: fixture_name.to_owned(),
        duration_ns: u64::try_from(build_started.elapsed().as_nanos())
            .unwrap_or(u64::MAX),
        snapshot_bytes: dir_size_bytes(&lab_dir),
    }));

    let lab = restore_lab_from_dir(this, fixture_name, &lab_dir)?;
    crate::fixture::bump_atime(&lab_dir);
    Ok(lab)
}

/// Sum the byte sizes of every regular file directly under `dir`.
/// Used as the `snapshot_bytes` value for lab-fixture build-done
/// events. Best-effort — unreadable entries contribute zero.
fn dir_size_bytes(dir: &std::path::Path) -> u64 {
    let mut total = 0u64;
    if let Ok(read) = std::fs::read_dir(dir) {
        for entry in read.flatten() {
            if let Ok(meta) = entry.metadata() {
                if meta.is_file() {
                    total = total.saturating_add(meta.len());
                }
            }
        }
    }
    total
}

/// Race-free directory swap for a fixture's `<key>.lab/` cache
/// entry. Tries `renameat2(RENAME_EXCHANGE)` so an in-flight
/// reader never sees a half-installed directory; falls back to
/// the rename-with-cleanup path on kernels / filesystems that
/// don't support exchange.
fn atomic_install_lab_dir(
    src_dir: &std::path::Path,
    lab_dir: &std::path::Path,
) -> std::io::Result<()> {
    if let Some(parent) = lab_dir.parent() {
        std::fs::create_dir_all(parent)?;
    }
    if !lab_dir.exists() {
        return std::fs::rename(src_dir, lab_dir);
    }
    // Both paths exist — try the atomic exchange first.
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;
    const RENAME_EXCHANGE: libc::c_uint = 1 << 1;
    let from = CString::new(src_dir.as_os_str().as_bytes())
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e))?;
    let to = CString::new(lab_dir.as_os_str().as_bytes())
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e))?;
    // SAFETY: both paths are valid C strings; AT_FDCWD is well-defined.
    let r = unsafe {
        libc::renameat2(
            libc::AT_FDCWD,
            from.as_ptr(),
            libc::AT_FDCWD,
            to.as_ptr(),
            RENAME_EXCHANGE,
        )
    };
    if r == 0 {
        // Old contents now live at `src_dir`; drop them.
        let _ = std::fs::remove_dir_all(src_dir);
        return Ok(());
    }
    // Kernel didn't honour RENAME_EXCHANGE — fall back. Move the
    // existing directory into a sibling path first so the window
    // where neither name resolves shrinks to a single rename.
    let stale = lab_dir.with_file_name(format!(
        "{}.stale.{}",
        lab_dir.file_name().and_then(|s| s.to_str()).unwrap_or("lab"),
        std::process::id(),
    ));
    std::fs::rename(lab_dir, &stale)?;
    let install = std::fs::rename(src_dir, lab_dir);
    let _ = std::fs::remove_dir_all(&stale);
    install
}

/// Read `lab.json` from `dir` and lab_restore into a fresh sub-Lab.
fn restore_lab_from_dir(
    this: &LabUd,
    fixture_name: &str,
    dir: &std::path::Path,
) -> Result<crate::lab::Lab, String> {
    let body = std::fs::read_to_string(dir.join("lab.json"))
        .map_err(|e| e.to_string())?;
    let meta: crate::lab::LabSnapshotMeta =
        serde_json::from_str(&body).map_err(|e| e.to_string())?;
    let sub = this
        .lab
        .sub_lab(format!("{}-{}", fixture_name, std::process::id()));
    sub.lab_restore(&meta).map_err(|e| e.to_string())?;
    Ok(sub)
}

/// Resolve, build (if needed), and restore a fixture into `this`.
fn build_or_resume_fixture(this: &LabUd, fixture_name: &str) -> Result<crate::vm::Vm, String> {
    let started = std::time::Instant::now();
    // 1. Locate the fixture file. Look in each test root.
    let fixture_path =
        locate_fixture(&this.config.provium.roots, fixture_name).map_err(|e| e.to_string())?;

    // 2. Read content + compute key. The key folds in the keys of
    //    every fixture this one transitively references via
    //    `vm_fixture(...)` / `lab_fixture(...)` so a parent rebuild
    //    invalidates derivatives — `DESIGN.md` § Fixtures /
    //    Dependency tracking.
    let source = fixture::read_fixture_source(&fixture_path).map_err(|e| e.to_string())?;
    let dep_keys =
        resolve_dep_keys(&this.config.provium.roots, &source, &mut Vec::new())?;
    // Fold in kernel/initrd identifiers for ALL profiles so a
    // kernel-image swap on any profile invalidates the cache, per
    // `DESIGN.md` § Fixtures. R9 sched-m4: previously folded only
    // the first profile by sorted name, which silently missed
    // kernel changes on later profiles.
    let (kernels, initrds) = canonical_profile_paths_all(&this.config);
    let kernel_refs: Vec<&std::path::Path> = kernels.iter().map(|p| p.as_path()).collect();
    let initrd_refs: Vec<&std::path::Path> = initrds.iter().map(|p| p.as_path()).collect();
    let externals =
        resolve_external_deps_pub(&this.config.provium.roots, &fixture_path, &source);
    let external_refs: Vec<&std::path::Path> = externals.iter().map(|p| p.as_path()).collect();
    let key = fixture::compute_key_with_deps_kernels_and_externals(
        &source,
        &dep_keys,
        &kernel_refs,
        &initrd_refs,
        &external_refs,
    );
    let cache_dir = this
        .config
        .provium
        .cache_dir
        .clone()
        .unwrap_or_else(default_cache_dir);
    std::fs::create_dir_all(&cache_dir).map_err(|e| e.to_string())?;
    let entry = CacheEntryPaths::for_key(&cache_dir, &key);

    // 3. Cache hit: restore directly. On any restore-side failure
    //    (decompression error, corrupt snapshot, version mismatch
    //    not caught by the key) we evict the entry, surface a
    //    warning, and fall through to rebuild — per
    //    `DESIGN.md` § Failure mode catalogue.
    if entry.snapshot.is_file() {
        // Decompress (if needed) outside the closure so the Err
        // arm can clean up the .restore.tmp on failure. Earlier
        // the closure-scoped `restore_path` leaked the temp on
        // any restore failure.
        let restore_path_result: Result<std::path::PathBuf, String> =
            if crate::perf::looks_zstd(&entry.snapshot) {
                let tmp = fixture::unique_restore_scratch();
                crate::perf::decompress_zst(&entry.snapshot, &tmp)
                    .map(|_| tmp)
                    .map_err(|e| e.to_string())
            } else {
                Ok(entry.snapshot.clone())
            };
        match restore_path_result {
            Ok(restore_path) => {
                let attempt = restore_into_lab(this, fixture_name, &restore_path);
                let cleanup_tmp = restore_path != entry.snapshot;
                match attempt {
                    Ok(vm) => {
                        if cleanup_tmp {
                            let _ = std::fs::remove_file(&restore_path);
                        }
                        // Emit CacheHit only after successful
                        // restore — emitting on file-existence
                        // produced confusing Hit→Build pairs for
                        // corrupt entries.
                        this.events.emit(Event::FixtureCacheHit(FixtureCacheHit {
                            path: fixture_name.to_owned(),
                        }));
                        crate::fixture::bump_atime(&entry.snapshot);
                        return Ok(vm);
                    }
                    Err(e) => {
                        if cleanup_tmp {
                            let _ = std::fs::remove_file(&restore_path);
                        }
                        eprintln!(
                            "provium: fixture cache entry `{fixture_name}` corrupt \
                             ({e}); evicting and rebuilding"
                        );
                        let _ = std::fs::remove_file(&entry.snapshot);
                        let _ = std::fs::remove_file(&entry.lock);
                        // Fall through to rebuild path below.
                    }
                }
            }
            Err(e) => {
                eprintln!(
                    "provium: fixture cache entry `{fixture_name}` decompression failed \
                     ({e}); evicting and rebuilding"
                );
                let _ = std::fs::remove_file(&entry.snapshot);
                let _ = std::fs::remove_file(&entry.lock);
                // Fall through.
            }
        }
    }

    // 4. Cache miss: build under lock, then restore. Emit
    //    FixtureBuildWaiting AS the wait begins so consumers
    //    see the wait in real time, not after it resolves.
    let wait_started = std::time::Instant::now();
    let fixture_for_event = fixture_name.to_owned();
    let events_for_event = this.events.clone();
    let (_lock, was_contended) = crate::fixture::acquire_build_lock_observed_notify(
        &entry.lock,
        move |holder| {
            events_for_event.emit(Event::FixtureBuildWaiting(FixtureBuildWaiting {
                path: fixture_for_event,
                held_by_file: holder.unwrap_or_default(),
            }));
        },
    )
    .map_err(|e| e.to_string())?;
    // Re-check after acquiring — another process may have built
    // the fixture while we were queuing. The two paths below each
    // emit AT MOST ONE FixtureBuildDone (paired with either the
    // earlier Waiting or a fresh Started) and then restore +
    // return. Falling through to a shared trailing Done emit was
    // the regression that produced double events.
    if !entry.snapshot.is_file() {
        // Cache miss inside the lock — we own the build.
        this.events.emit(Event::FixtureBuildStarted(FixtureBuildStarted {
            path: fixture_name.to_owned(),
        }));
        let build_started = std::time::Instant::now();
        let outcome = super::fixture_build::build_fixture(
            &fixture_path,
            Arc::clone(&this.config),
            Arc::clone(&this.vmm),
        )
        .map_err(|e| e.to_string())?;
        let snapshot_path = match outcome {
            super::fixture_build::FixtureBuildOutcome::SingleVm { snapshot_path } => snapshot_path,
            super::fixture_build::FixtureBuildOutcome::Lab { .. } => {
                return Err(format!(
                    "vm_fixture `{fixture_name}` returned a lab snapshot; use lab_fixture instead"
                ));
            }
        };
        // Sparse + zstd post-process: shrinks mostly-zero
        // snapshots dramatically. We do this in-place, then
        // zstd-compress the sparse output to <key>.snap.zst.
        let _ = crate::perf::make_sparse(&snapshot_path);
        let zst_path = entry.snapshot.with_extension("snap.zst");
        if crate::perf::compress_zst(&snapshot_path, &zst_path).is_ok() {
            let _ = std::fs::rename(&zst_path, &entry.snapshot);
            let _ = std::fs::remove_file(&snapshot_path);
        } else if std::fs::rename(&snapshot_path, &entry.snapshot).is_err() {
            std::fs::copy(&snapshot_path, &entry.snapshot)
                .map_err(|e| e.to_string())?;
            let _ = std::fs::remove_file(&snapshot_path);
        }
        this.events.emit(Event::FixtureBuildDone(FixtureBuildDone {
            path: fixture_name.to_owned(),
            duration_ns: u64::try_from(build_started.elapsed().as_nanos())
                .unwrap_or(u64::MAX),
            snapshot_bytes: std::fs::metadata(&entry.snapshot)
                .map(|m| m.len())
                .unwrap_or(0),
        }));
    } else if was_contended {
        // Cache hit after waiting on the lock. Pair the earlier
        // Waiting with a Done so consumers see a balanced sequence.
        this.events.emit(Event::FixtureBuildDone(FixtureBuildDone {
            path: fixture_name.to_owned(),
            duration_ns: u64::try_from(wait_started.elapsed().as_nanos())
                .unwrap_or(u64::MAX),
            snapshot_bytes: std::fs::metadata(&entry.snapshot)
                .map(|m| m.len())
                .unwrap_or(0),
        }));
    }
    // Restore is shared — same code path whether we just built or
    // a peer did.
    let restore_path = if crate::perf::looks_zstd(&entry.snapshot) {
        let tmp = fixture::unique_restore_scratch();
        crate::perf::decompress_zst(&entry.snapshot, &tmp)
            .map_err(|e| e.to_string())?;
        tmp
    } else {
        entry.snapshot.clone()
    };
    let vm = restore_into_lab(this, fixture_name, &restore_path)
        .map_err(|e| e.to_string())?;
    if restore_path != entry.snapshot {
        let _ = std::fs::remove_file(&restore_path);
    }
    crate::fixture::bump_atime(&entry.snapshot);
    let _ = started; // keep the variable live for any future timing use
    Ok(vm)
}


/// Look up `<root>/<name>.fixture.lua` across configured test roots.
fn locate_fixture(
    roots: &[String],
    name: &str,
) -> Result<PathBuf, fixture::FixtureError> {
    let candidates = if roots.is_empty() {
        // No roots configured — try the current directory.
        vec![std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))]
    } else {
        roots.iter().map(PathBuf::from).collect()
    };
    for root in candidates {
        let candidate = root.join(format!("{name}.fixture.lua"));
        if candidate.is_file() {
            return Ok(candidate);
        }
    }
    Err(fixture::FixtureError::NotFound(PathBuf::from(name)))
}

/// Build a unique VM name for a fixture-backed VM. Two restores
/// of the same fixture in one Lab need different names; we
/// disambiguate with a counter inside the VM-name string.
fn restore_into_lab(
    this: &LabUd,
    fixture_name: &str,
    snapshot_path: &std::path::Path,
) -> Result<crate::vm::Vm, crate::lab::LabError> {
    let vm_name = unique_fixture_vm_name(&this.lab, fixture_name);
    let profile_name = first_profile_name(&this.config);
    // `Lab::restore_vm` emits `vm_spawned` itself — it is the shared
    // choke point for every resume path — so this binding layer
    // doesn't.
    this.lab
        .restore_vm(vm_name, profile_name, BootOpts::default(), snapshot_path)
}

fn unique_fixture_vm_name(lab: &Lab, fixture_name: &str) -> String {
    let base = format!("fixture:{}", fixture_name.replace('/', "_"));
    let existing = lab.vm_names();
    if !existing.contains(&base) {
        return base;
    }
    for i in 1..u32::MAX {
        let candidate = format!("{base}#{i}");
        if !existing.contains(&candidate) {
            return candidate;
        }
    }
    base
}

fn first_profile_name(config: &Config) -> String {
    // Mirror `canonical_profile_paths` exactly — sort by name and
    // take the lex-first. Without sorting, multi-profile configs
    // would resolve the cache key against profile A and the
    // restore against profile B (different kernel/initrd) on
    // different runs of the same fixture, producing kernel
    // mismatches that QEMU rejects at restore time.
    let mut names: Vec<&String> = config.profiles.keys().collect();
    names.sort();
    names
        .first()
        .map(|s| (*s).clone())
        .unwrap_or_else(|| "peios".into())
}

fn auto_sub_lab_name(parent: &Lab) -> String {
    // Anonymous sub-labs get a generated name so introspection
    // still has a key. Slice 4 doesn't track these in the resource
    // graph; slice 7 will.
    let existing = parent.sub_lab_names().len();
    format!("__lab_{}", existing)
}
