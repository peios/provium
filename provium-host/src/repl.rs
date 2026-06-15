//! Interactive Lua REPL bound to a single VM.
//!
//! Slice-13 surface: `provium repl <profile>` boots a VM with the
//! requested profile and drops into a `provium> ` prompt. Each line
//! is evaluated as Lua, with the same `provium` / `vm:run`-shaped
//! bindings as a `.test.lua` chunk would see.
//!
//! Lines are tried as expressions first (`return <line>`); on parse
//! failure they're re-tried as statements. This lets the user type
//! `1+2` and see `3`, while still allowing `local x = 5` etc.
//!
//! `^D` exits cleanly. `^C` discards the current line. `exit()` /
//! `quit()` are exposed as Lua functions.

use std::io;
use std::sync::Arc;

use mlua::{Function, Lua, ObjectLike, Value};
use rustyline::completion::{Completer, Pair};
use rustyline::error::ReadlineError;
use rustyline::highlight::Highlighter;
use rustyline::hint::Hinter;
use rustyline::validate::Validator;
use rustyline::{Context, Editor, Helper, Result as RlResult};

use crate::lab::Lab;
use crate::lua::{self, LuaContext};
use crate::profile::Config;
use crate::vm::Vm;
use crate::vmm::{BootOpts, Vmm};

/// Spec for one REPL run.
#[derive(Debug)]
pub struct ReplOpts {
    /// VM name as it appears in the REPL banner + `lab:vm("name")`.
    pub vm_name: String,
    /// Profile from `provium.toml` to boot the VM against.
    pub profile_name: String,
    /// Optional fixture name (test-root-relative, no
    /// `.fixture.lua`). When set, the REPL resumes the cached
    /// fixture instead of cold-booting the profile. Per
    /// `DESIGN.md` § REPL.
    pub fixture: Option<String>,
}

impl Default for ReplOpts {
    fn default() -> Self {
        Self {
            vm_name: "repl".into(),
            profile_name: "peios".into(),
            fixture: None,
        }
    }
}

/// Run the REPL loop. Boots a VM, installs bindings, reads lines
/// from stdin via rustyline, evaluates them. Returns when the
/// user disconnects (^D) or types `exit()`.
pub fn run(
    opts: ReplOpts,
    config: Arc<Config>,
    vmm: Arc<dyn Vmm>,
) -> Result<(), Box<dyn std::error::Error>> {
    println!(
        "provium repl — booting VM `{}` (profile `{}`)",
        opts.vm_name, opts.profile_name
    );

    let root_lab = Lab::new("provium", Arc::clone(&config), Arc::clone(&vmm));
    let vm = root_lab.create_vm(
        opts.vm_name.clone(),
        opts.profile_name.clone(),
        BootOpts::default(),
    )?;

    // --fixture path: resolve cached snapshot under cache_dir using
    // the same hashing that lab_ud.build_or_resume_fixture uses, then
    // call vm.restore. If the fixture isn't built yet, fall back to
    // cold boot with a notice.
    if let Some(fixture_name) = &opts.fixture {
        match resume_fixture_into(&vm, &config, fixture_name) {
            Ok(()) => println!(
                "  resumed fixture `{}`: cid={:?} state={}\n",
                fixture_name,
                vm.cid().unwrap_or(0),
                vm.state().as_str()
            ),
            Err(e) => {
                eprintln!(
                    "  fixture resume failed ({e}); cold-booting profile instead"
                );
                vm.boot()?;
                println!(
                    "  ready: cid={:?} state={}\n",
                    vm.cid().unwrap_or(0),
                    vm.state().as_str()
                );
            }
        }
    } else {
        vm.boot()?;
        println!(
            "  ready: cid={:?} state={}\n",
            vm.cid().unwrap_or(0),
            vm.state().as_str()
        );
    }
    let _ = vm; // VM is reachable from Lua via `provium.<vm_name>`.

    let lua = Lua::new();
    lua::install(
        &lua,
        LuaContext {
            config,
            vmm: Arc::clone(&vmm),
            root_lab: root_lab.clone(),
            events: Arc::new(crate::scheduler::events::NullSink),
            pool: None,
        },
    )?;

    // Convenience: bind a top-level `vm` variable so the user
    // doesn't have to type `provium.<name>` every line.
    if let Ok(Value::UserData(ud)) = lua.globals().get::<Value>("provium") {
        if let Ok(vm_ud) = ud.call_method::<Value>("vm", opts.vm_name.clone()) {
            let _ = lua.globals().set("vm", vm_ud);
        }
    }

    // `exit` / `quit` Lua helpers.
    let exit_fn: Function = lua.create_function(|_, ()| {
        Err::<(), _>(mlua::Error::external("__provium_repl_exit"))
    })?;
    lua.globals().set("exit", exit_fn.clone())?;
    lua.globals().set("quit", exit_fn)?;

    // Editor with method-name completion. Per `DESIGN.md` § REPL
    // line editing — basic identifier completion seeded from a
    // static method map per type prefix, augmented at runtime by
    // probing the live Lua globals for top-level identifiers.
    let mut helper = ReplHelper::new();
    helper.augment_from_lua(&lua);
    let mut editor: Editor<ReplHelper, _> = Editor::new()?;
    editor.set_helper(Some(helper));
    let _ = editor.load_history(&history_path());

    let outcome = repl_loop(&lua, &mut editor);

    let _ = editor.save_history(&history_path());

    if let Err(e) = root_lab.shutdown() {
        eprintln!("repl: shutdown: {e}");
    }
    outcome
}

fn repl_loop<H: Helper>(
    lua: &Lua,
    editor: &mut Editor<H, rustyline::history::FileHistory>,
) -> Result<(), Box<dyn std::error::Error>> {
    loop {
        match editor.readline("provium> ") {
            Ok(line) => {
                let trimmed = line.trim();
                if trimmed.is_empty() {
                    continue;
                }
                let _ = editor.add_history_entry(&line);
                match eval_one(lua, trimmed) {
                    Ok(values) => print_values(lua, &values),
                    Err(e) => {
                        if e.to_string().contains("__provium_repl_exit") {
                            println!("bye");
                            return Ok(());
                        }
                        eprintln!("error: {e}");
                    }
                }
            }
            Err(ReadlineError::Interrupted) => {
                // ^C: discard line + continue.
                continue;
            }
            Err(ReadlineError::Eof) => {
                println!("bye");
                return Ok(());
            }
            Err(e) => return Err(Box::new(e)),
        }
    }
}

/// Try to evaluate `line` as an expression first; fall back to
/// statement on parse failure. Returns the produced values (one
/// per `return` value, possibly empty).
fn eval_one(lua: &Lua, line: &str) -> mlua::Result<Vec<Value>> {
    // Expression form: `return <line>` so the chunk produces values.
    let as_expr = format!("return {line}");
    if let Ok(chunk) = lua.load(&as_expr).set_name("=stdin").into_function() {
        return chunk.call::<mlua::MultiValue>(()).map(|m| m.into_iter().collect());
    }
    // Statement form: just run the line, no return.
    lua.load(line).set_name("=stdin").exec().map(|()| Vec::new())
}

fn print_values(lua: &Lua, values: &[Value]) {
    for v in values {
        match v {
            Value::Nil => {}
            Value::String(s) => match s.to_str() {
                Ok(s) => println!("{}", &*s),
                Err(_) => println!("<non-utf8 string>"),
            },
            Value::Boolean(b) => println!("{b}"),
            Value::Integer(i) => println!("{i}"),
            Value::Number(n) => println!("{n}"),
            Value::Table(_) | Value::UserData(_) | Value::Function(_) => {
                // Use Lua's standard tostring — honours __tostring
                // metamethods on userdata.
                match lua_tostring(lua, v) {
                    Ok(s) => println!("{s}"),
                    Err(_) => println!("<{}>", v.type_name()),
                }
            }
            other => println!("{}", other.type_name()),
        }
    }
}

/// Call Lua's standard `tostring(v)` — honours `__tostring` on
/// tables / userdata so printed values match what test code sees.
fn lua_tostring(lua: &Lua, v: &Value) -> mlua::Result<String> {
    let tostring: mlua::Function = lua.globals().get("tostring")?;
    tostring.call::<String>(v.clone())
}

fn history_path() -> std::path::PathBuf {
    if let Some(home) = std::env::var_os("HOME") {
        return std::path::PathBuf::from(home)
            .join(".provium_history");
    }
    std::path::PathBuf::from("/tmp/.provium_history")
}

#[allow(dead_code)]
fn _io_use(_: io::Error) {}

#[allow(dead_code)]
fn _vm_use(_: &Vm) {}

/// Resolve a fixture name to its cached snapshot path and call
/// `vm.restore()`. Returns an error if the fixture either doesn't
/// exist on disk or hasn't been built.
fn resume_fixture_into(
    vm: &Vm,
    config: &Arc<Config>,
    fixture_name: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    use crate::fixture::{
        canonical_profile_paths, compute_key_with_deps_and_kernel, default_cache_dir,
        read_fixture_source, CacheEntryPaths,
    };
    let cache_dir = config
        .provium
        .cache_dir
        .clone()
        .unwrap_or_else(default_cache_dir);
    // Locate the .fixture.lua under any of the configured roots.
    let mut found: Option<std::path::PathBuf> = None;
    for root in &config.provium.roots {
        let candidate = std::path::Path::new(root)
            .join(format!("{fixture_name}.fixture.lua"));
        if candidate.is_file() {
            found = Some(candidate);
            break;
        }
    }
    let path = found.ok_or_else(|| {
        format!("fixture `{fixture_name}` not found under configured roots")
    })?;
    let source = read_fixture_source(&path)?;
    // Match the runner / `provium fixture build` exactly: fold in
    // transitively-referenced fixture deps + the canonical
    // kernel/initrd identifier. Without this, a fixture rebuilt
    // by the runner with a kernel-bumped key looks stale to the
    // REPL even though it just got fresh data.
    let dep_keys = crate::lua::lab_ud_resolve_dep_keys_pub(
        &config.provium.roots,
        &source,
    );
    let (kernel, initrd) = canonical_profile_paths(config);
    let key = compute_key_with_deps_and_kernel(
        &source,
        &dep_keys,
        kernel.as_deref(),
        initrd.as_deref(),
    );
    let entry = CacheEntryPaths::for_key(&cache_dir, &key);
    if !entry.snapshot.is_file() {
        return Err(format!(
            "fixture `{fixture_name}` is not built (run `provium fixture build {fixture_name}`)"
        )
        .into());
    }
    // Restore handles zstd transparently when the file is compressed.
    let restore_path = if crate::perf::looks_zstd(&entry.snapshot) {
        let tmp = crate::fixture::unique_restore_scratch();
        crate::perf::decompress_zst(&entry.snapshot, &tmp)?;
        tmp
    } else {
        entry.snapshot.clone()
    };
    vm.restore(&restore_path)?;
    if restore_path != entry.snapshot {
        let _ = std::fs::remove_file(&restore_path);
    }
    // Match the runner's cache-hit semantics: bump atime so LRU
    // eviction sees this fixture as recently used. Without this,
    // a fixture used exclusively via `provium repl --fixture X`
    // ages out as if never accessed.
    crate::fixture::bump_atime(&entry.snapshot);
    Ok(())
}

/// rustyline `Helper` implementing identifier completion for
/// `vm:`/`lab:`/`provium:`/`bridge:`/`proc:`/`stream:` method
/// prefixes. Matches the design's "basic identifier completion".
struct ReplHelper {
    methods: std::collections::HashMap<String, Vec<String>>,
    /// Top-level Lua globals (for completion of bare identifiers
    /// like `provi<tab>`). Populated by `augment_from_lua` at
    /// REPL startup.
    globals: Vec<String>,
}

impl ReplHelper {
    /// Snapshot the top-level Lua globals so the completer can
    /// suggest bare identifiers (`provium<tab>`, `vm<tab>`, etc.).
    fn augment_from_lua(&mut self, lua: &Lua) {
        if let Ok(globals) = lua.globals().pairs::<String, mlua::Value>().collect::<mlua::Result<Vec<_>>>() {
            self.globals = globals.into_iter().map(|(k, _)| k).collect();
            self.globals.sort();
            self.globals.dedup();
        }
    }

    fn new() -> Self {
        let mut raw: std::collections::HashMap<&'static str, Vec<&'static str>> =
            std::collections::HashMap::new();
        raw.insert(
            "vm",
            vec![
                "boot",
                "shutdown",
                "pause",
                "resume",
                "snapshot",
                "restore",
                "reset",
                "power_button",
                "run",
                "run_async",
                "read_file",
                "write_file",
                "stat",
                "listdir",
                "mkdir",
                "unlink",
                "rename",
                "open_file",
                "tail_file",
                "fd_stream",
                "syscall",
                "ioctl",
                "console",
                "clock",
                "nic",
                "disk",
                "attach_disk",
                "spawn_worker",
                "name",
                "state",
                "cid",
                "batch",
            ],
        );
        raw.insert(
            "lab",
            vec![
                "vm",
                "bridge",
                "lab",
                "include",
                "remove",
                "members",
                "boot",
                "shutdown",
                "pause",
                "resume",
                "snapshot",
                "restore",
                "claim",
                "barrier",
            ],
        );
        raw.insert(
            "provium",
            vec![
                "vm",
                "bridge",
                "lab",
                "boot",
                "shutdown",
                "snapshot",
                "claim",
                "barrier",
                "vm_fixture",
                "lab_fixture",
                "pack",
                "unpack",
            ],
        );
        raw.insert(
            "bridge",
            vec![
                "attach",
                "detach",
                "members",
                "partition",
                "unpartition",
                "partition_all",
                "restore_all",
                "add_latency",
                "drop_rate",
                "bandwidth_limit",
                "reset",
                "name",
                "nic",
                "capture",
                "enable_uplink",
                "disable_uplink",
                "route",
                "isolate",
                "unisolate",
            ],
        );
        raw.insert(
            "proc",
            vec![
                "wait",
                "kill",
                "signal",
                "pid",
                "status",
                "stdout_stream",
                "stderr_stream",
                "stdin_write",
                "close_stdin",
                "close",
            ],
        );
        raw.insert(
            "stream",
            vec!["next", "read_until", "expect", "drain", "close", "eof", "creation_site"],
        );
        raw.insert("file", vec!["read", "read_all", "write", "seek", "tell", "close", "fd", "tail_stream"]);
        raw.insert("clock", vec!["get", "set", "sleep", "advance"]);
        raw.insert(
            "disk",
            vec!["read_sectors", "write_sectors", "size", "fault_inject", "clear_faults", "detach"],
        );
        raw.insert("nic", vec!["counters", "capture", "disconnect", "reconnect"]);
        raw.insert("console", vec!["read", "write", "expect", "close"]);
        raw.insert("worker", vec!["run", "run_async", "open_file", "syscall", "kill", "join"]);
        raw.insert("snap", vec!["delete", "size", "path"]);
        let methods = raw
            .into_iter()
            .map(|(k, v)| (k.to_string(), v.into_iter().map(String::from).collect()))
            .collect();
        Self {
            methods,
            globals: Vec::new(),
        }
    }
}

impl Completer for ReplHelper {
    type Candidate = Pair;

    fn complete(
        &self,
        line: &str,
        pos: usize,
        _ctx: &Context<'_>,
    ) -> rustyline::Result<(usize, Vec<Pair>)> {
        let head = &line[..pos];
        // First try `ident:partial` method completion.
        if let Some(colon_idx) = head.rfind(':') {
            let before = &head[..colon_idx];
            let ident_start = before
                .rfind(|c: char| !c.is_alphanumeric() && c != '_')
                .map(|i| i + 1)
                .unwrap_or(0);
            let ident = &before[ident_start..];
            let partial = &head[colon_idx + 1..];
            if let Some(methods) = self.methods.get(ident) {
                let candidates = methods
                    .iter()
                    .filter(|m| m.starts_with(partial))
                    .map(|m| Pair {
                        display: m.clone(),
                        replacement: m.clone(),
                    })
                    .collect();
                return Ok((colon_idx + 1, candidates));
            }
        }
        // Fall through: bare-identifier completion against
        // the snapshot of Lua globals taken at REPL startup.
        let ident_start = head
            .rfind(|c: char| !c.is_alphanumeric() && c != '_')
            .map(|i| i + 1)
            .unwrap_or(0);
        let partial = &head[ident_start..];
        if partial.is_empty() {
            return Ok((pos, Vec::new()));
        }
        let candidates: Vec<Pair> = self
            .globals
            .iter()
            .filter(|g| g.starts_with(partial))
            .map(|g| Pair {
                display: g.clone(),
                replacement: g.clone(),
            })
            .collect();
        Ok((ident_start, candidates))
    }
}

impl Hinter for ReplHelper {
    type Hint = String;
}
impl Highlighter for ReplHelper {}
impl Validator for ReplHelper {}
impl Helper for ReplHelper {}

// rustyline's RlResult re-export, kept in scope for future
// per-line completion work.
#[allow(dead_code)]
fn _rl_use<T>(_: RlResult<T>) {}
