//! Lua bindings for [`Vm`].
//!
//! `VmUd` wraps a [`Vm`] (which itself owns its state-machine
//! internals via `Arc<Mutex<…>>`). Methods are mostly thin shims —
//! the Vm type does the heavy lifting and surfaces clean errors on
//! illegal-state transitions, which we convert to mlua errors here.

use mlua::{MetaMethod, UserData, UserDataMethods, Value};

use provium_protocol::wire::{ExecArgs, OpenMode, TailStart};

use std::sync::Arc;

use provium_protocol::events::{Event, VmShutdown};

use crate::scheduler::events::{EventSink, NullSink};
use crate::vm::Vm;

use super::result_ud::{wrap_run_result, TailUd};

/// UserData wrapper for [`Vm`].
#[derive(Clone)]
pub(crate) struct VmUd {
    vm: Vm,
    events: Arc<dyn EventSink>,
    boot_started: Arc<std::sync::Mutex<Option<std::time::Instant>>>,
    /// Lab name (`file` field on telemetry events) the VM was
    /// created in. Empty when the VM was wrapped without metadata.
    lab_name: String,
    /// Profile name; carried for the `vm_spawned` event.
    profile_name: String,
    /// Memory bytes; carried for the `vm_spawned` event.
    memory_bytes: u64,
}

impl VmUd {
    pub(crate) fn wrap(vm: Vm) -> Self {
        Self {
            vm,
            events: Arc::new(NullSink),
            boot_started: Arc::new(std::sync::Mutex::new(None)),
            lab_name: String::new(),
            profile_name: String::new(),
            memory_bytes: 0,
        }
    }

    #[allow(dead_code)]
    pub(crate) fn wrap_with_events(vm: Vm, events: Arc<dyn EventSink>) -> Self {
        Self {
            vm,
            events,
            boot_started: Arc::new(std::sync::Mutex::new(None)),
            lab_name: String::new(),
            profile_name: String::new(),
            memory_bytes: 0,
        }
    }

    /// Most-explicit form — used by `lab:vm` create so vm_spawned
    /// fires at boot with the real CID + the right lab/profile/mem
    /// payload.
    pub(crate) fn wrap_with_events_and_meta(
        vm: Vm,
        events: Arc<dyn EventSink>,
        lab_name: String,
        profile_name: String,
        memory_bytes: u64,
    ) -> Self {
        Self {
            vm,
            events,
            boot_started: Arc::new(std::sync::Mutex::new(None)),
            lab_name,
            profile_name,
            memory_bytes,
        }
    }

    /// Owned VM name. Used by the bridge bindings to convert a
    /// `vm` argument into the bridge's string-keyed graph state.
    pub(crate) fn vm_name(&self) -> String {
        self.vm.name().to_owned()
    }

    /// Move the inner `Vm` out — used by `lab:include(vm)` to
    /// re-attach the same handle under a different lab.
    pub(crate) fn into_inner(self) -> Vm {
        self.vm
    }

    /// Clone-out variant used when we need the VM handle without
    /// consuming the userdata (e.g. `bridge:nic(vm)` constructing
    /// a NIC that needs to live alongside the VM in Lua scope).
    pub(crate) fn vm_clone(&self) -> Vm {
        self.vm.clone()
    }

    /// Forward a bridge attachment onto the inner `Vm` so the
    /// boot path picks it up.
    pub(crate) fn add_bridge_attachment(&self, bridge: crate::bridge::Bridge) {
        self.vm.add_bridge_attachment(bridge);
    }
}

impl UserData for VmUd {
    fn add_methods<M: UserDataMethods<Self>>(methods: &mut M) {
        // ---------------------------------------------------------------
        // Lifecycle
        // ---------------------------------------------------------------
        methods.add_method("boot", |_, this, opts: Option<mlua::Table>| {
            *this.boot_started.lock().unwrap() = Some(std::time::Instant::now());
            if let Some(t) = opts {
                let overrides = parse_boot_overrides(&t)?;
                this.vm
                    .merge_boot_opts(overrides)
                    .map_err(mlua::Error::external)?;
            }
            this.vm.boot().map_err(mlua::Error::external)?;
            // Emit vm_spawned now (post-boot) so the payload carries
            // a real CID per `DESIGN.md` § Observability.
            this.events.emit(provium_protocol::events::Event::VmSpawned(
                provium_protocol::events::VmSpawned {
                    file: this.lab_name.clone(),
                    vm_name: this.vm.name().to_owned(),
                    profile: this.profile_name.clone(),
                    memory_bytes: this.memory_bytes,
                    cid: this.vm.cid().unwrap_or(0),
                },
            ));
            Ok(this.clone())
        });
        methods.add_method("pause", |_, this, ()| {
            this.vm.pause().map_err(mlua::Error::external)?;
            Ok(())
        });
        methods.add_method("resume", |_, this, ()| {
            this.vm.resume().map_err(mlua::Error::external)?;
            Ok(())
        });
        methods.add_method("shutdown", |_, this, ()| {
            let duration = this
                .boot_started
                .lock()
                .unwrap()
                .map(|t| u64::try_from(t.elapsed().as_nanos()).unwrap_or(u64::MAX))
                .unwrap_or(0);
            this.vm.shutdown().map_err(mlua::Error::external)?;
            this.events.emit(Event::VmShutdown(VmShutdown {
                file: this.lab_name.clone(),
                vm_name: this.vm.name().to_owned(),
                duration_ns: duration,
            }));
            Ok(())
        });
        // Auto-close hook used by the resource-graph walker. Idempotent.
        methods.add_method("close", |_, this, ()| {
            // Only call shutdown if not already in Shutdown state —
            // shutdown's state guard accepts Booted/Paused/Created/Dead.
            let state = this.vm.state();
            if !matches!(state, crate::vm::VmState::Shutdown) {
                let _ = this.vm.shutdown();
            }
            Ok(())
        });
        // vm:restore(snapshot_or_path) — DESIGN says the arg is a
        // SnapshotUd (returned by vm:snapshot()); we also accept
        // a bare path string for ergonomics.
        methods.add_method("restore", |_, this, arg: Value| {
            let path: std::path::PathBuf = match arg {
                Value::String(s) => std::path::PathBuf::from(
                    s.to_str().map_err(mlua::Error::external)?.to_string(),
                ),
                Value::UserData(ud) => {
                    let snap = ud
                        .borrow::<super::snapshot_ud::SnapshotUd>()
                        .map_err(|_| mlua::Error::external(
                            "vm:restore: expected snapshot userdata or path string",
                        ))?;
                    snap.path.clone()
                }
                other => {
                    return Err(mlua::Error::external(format!(
                        "vm:restore: expected snapshot|string, got {}",
                        other.type_name()
                    )));
                }
            };
            this.vm
                .restore(&path)
                .map_err(mlua::Error::external)?;
            Ok(())
        });

        methods.add_method("snapshot", |_, this, path: Option<String>| {
            let target = match path {
                Some(p) => std::path::PathBuf::from(p),
                None => generate_snapshot_tempfile_path(),
            };
            this.vm.snapshot(&target).map_err(mlua::Error::external)?;
            Ok(super::snapshot_ud::SnapshotUd { path: target })
        });
        methods.add_method("is_quiescent", |_, this, ()| Ok(this.vm.is_quiescent()));
        methods.add_method("open_file_count", |_, this, ()| {
            Ok(this.vm.open_file_count())
        });
        methods.add_method("open_stream_count", |_, this, ()| {
            Ok(this.vm.open_stream_count())
        });

        // ---------------------------------------------------------------
        // Layer-1 ops
        // ---------------------------------------------------------------
        methods.add_method("run", |lua, this, args: mlua::Variadic<Value>| {
            let exec = build_exec_args(&args).map_err(mlua::Error::external)?;
            let result = this.vm.run(exec).map_err(mlua::Error::external)?;
            wrap_run_result(lua, result)
        });

        methods.add_method("run_async", |lua, this, args: mlua::Variadic<Value>| {
            let exec = build_exec_args(&args).map_err(mlua::Error::external)?;
            // run_async returns a Process — the agent never auto-
            // kills it. The DESIGN.md analogue of `vm:run`'s
            // `timeout` is `proc:wait(timeout)` on the returned
            // handle. Silently dropping a `timeout` opt would be
            // a footgun, so reject it with a pointer.
            if exec.timeout_ms.is_some() {
                return Err(mlua::Error::external(
                    "vm:run_async: `timeout` is not honoured here; \
                     pass it to `proc:wait(timeout)` instead",
                ));
            }
            let async_args = provium_protocol::wire::RunAsyncArgs {
                cmd: exec.cmd,
                args: exec.args,
                env: exec.env,
                env_clear: exec.env_clear,
                cwd: exec.cwd,
            };
            let proc = this
                .vm
                .run_async(async_args)
                .map_err(mlua::Error::external)?;
            super::result_ud::register_resource(
                lua,
                super::result_ud::ProcessUd::wrap(proc),
                "proc",
            )
        });

        methods.add_method("read_file", |lua, this, path: String| {
            let bytes = this.vm.read_file(path).map_err(mlua::Error::external)?;
            lua.create_string(&bytes).map(Value::String)
        });

        methods.add_method(
            "write_file",
            |_, this, (path, data): (String, mlua::String)| {
                this.vm
                    .write_file(path, data.as_bytes().to_vec())
                    .map_err(mlua::Error::external)?;
                Ok(())
            },
        );

        // `vm:push_file(host_path, guest_path, opts?)` — read a host
        // file and write its bytes to the guest. Auto-folds the host
        // file into the fixture's cache key so editing the source
        // (e.g. rebuilding a binary referenced by a fixture)
        // invalidates the snapshot. Tracking is driven by static
        // source scanning in `fixture::scan_external_file_deps`;
        // `opts.auto_dep = false` (must be a string-literal `false`
        // at the call site) skips the fold. Variable host paths
        // aren't tracked — declare them with
        // `lab:depends_on_file("…")` instead.
        //
        // Relative `host_path` is resolved against the directory of
        // the Lua source file that issued the call (looked up via
        // `debug.getinfo`), matching the cache-key scanner. The
        // runtime cwd is the fallback when source info is
        // unavailable (REPL chunks, `loadstring`).
        methods.add_method(
            "push_file",
            |lua, this, (host, guest, _opts): (String, String, Option<mlua::Table>)| {
                let resolved = resolve_caller_relative(lua, &host)?;
                let data = std::fs::read(&resolved).map_err(|e| {
                    mlua::Error::external(format!(
                        "vm:push_file: read `{}` failed: {e}",
                        resolved.display()
                    ))
                })?;
                this.vm
                    .write_file(guest, data)
                    .map_err(mlua::Error::external)?;
                Ok(())
            },
        );

        methods.add_method("stat", |lua, this, path: String| {
            let meta = this.vm.stat(path).map_err(mlua::Error::external)?;
            let table = lua.create_table()?;
            table.set("size", meta.size)?;
            // `mtime` carries seconds (per DESIGN's surface-level
            // "modification time" expectation); `mtime_ns` keeps
            // the full nanosecond precision for tests that want
            // it. Earlier rounds set both to ns, which made
            // `stat.mtime` look like an absurdly large epoch.
            table.set("mtime", (meta.mtime_ns as f64) / 1_000_000_000.0)?;
            table.set("mtime_ns", meta.mtime_ns)?;
            table.set("perm", meta.perm)?;
            table.set("entry_type", entry_type_name(meta.entry_type))?;
            Ok(Value::Table(table))
        });

        methods.add_method("clock", |_, this, ()| {
            Ok(super::result_ud::ClockUd::new(this.vm.clone()))
        });

        methods.add_method("spawn_worker", |lua, this, opts: Option<mlua::Table>| {
            // DESIGN.md § VM lists `vm:spawn_worker(opts?)` with a
            // `{thread=true}` example. Thread-mode workers aren't a
            // v1 concept (we always spawn a separate sub-agent), so
            // accept the table for forward-compat: an explicit
            // `thread=true` raises a clear error pointing at the
            // future-work limitation rather than silently being
            // treated as a process worker.
            //
            // **v1 isolation note:** worker:open_file allocates the
            // file handle in the parent agent's table (so subsequent
            // file:read/write/close ops work), and worker:run_async
            // does the same for processes (with a per-worker
            // membership map for kill/join). The "same VM API"
            // promise holds, but the worker is NOT a hard isolation
            // boundary — handles are routable from outside the
            // worker. Per-worker dispatch wire ops are deferred to
            // the 10.6 slice; until then, treat workers as
            // bookkeeping namespaces, not security boundaries.
            if let Some(t) = opts.as_ref() {
                if t.get::<Option<bool>>("thread")?.unwrap_or(false) {
                    return Err(mlua::Error::external(
                        "vm:spawn_worker: `thread=true` is not yet supported \
                         (v1 always spawns a process sub-agent)",
                    ));
                }
            }
            let w = this.vm.spawn_worker().map_err(mlua::Error::external)?;
            super::result_ud::register_resource(
                lua,
                super::result_ud::WorkerUd::wrap(w),
                "worker",
            )
        });

        // Lifecycle extras (slice C).
        methods.add_method("reset", |_, this, ()| {
            this.vm.reset().map_err(mlua::Error::external)?;
            Ok(())
        });
        methods.add_method("power_button", |_, this, ()| {
            this.vm.power_button().map_err(mlua::Error::external)?;
            Ok(())
        });

        // File-system ops missing from earlier slices (Phase A/B).
        methods.add_method(
            "open_file",
            |lua, this, (path, mode): (String, mlua::Table)| {
                // `perm` is part of DESIGN's mode table — read it
                // out and forward as create_perm so a test that
                // writes `{create=true, perm=0o600}` actually
                // sees mode 0600 on the file (was silently
                // dropped before).
                let create_perm = mode.get::<Option<u32>>("perm")?;
                let parsed = open_mode_from_table(&mode)?;
                let h = this
                    .vm
                    .open_file_with_perm(path.clone(), parsed, create_perm)
                    .map_err(mlua::Error::external)?;
                super::result_ud::register_resource(
                    lua,
                    super::file_ud::FileUd::wrap_with_path(this.vm.clone(), h, path),
                    "file",
                )
            },
        );
        methods.add_method("listdir", |lua, this, path: String| {
            let entries = this.vm.listdir(path).map_err(mlua::Error::external)?;
            let table = lua.create_table()?;
            for (i, e) in entries.iter().enumerate() {
                let item = lua.create_table()?;
                item.set("name", e.name.clone())?;
                item.set("entry_type", entry_type_name(e.entry_type))?;
                table.set(i + 1, item)?;
            }
            Ok(Value::Table(table))
        });
        methods.add_method(
            "mkdir",
            |_, this, (path, opts): (String, Option<mlua::Table>)| {
                let mut parents = false;
                let mut perm: Option<u32> = None;
                if let Some(t) = opts {
                    parents = t.get::<Option<bool>>("parents")?.unwrap_or(false);
                    perm = t.get::<Option<u32>>("perm")?;
                }
                this.vm
                    .mkdir(path, parents, perm)
                    .map_err(mlua::Error::external)?;
                Ok(())
            },
        );
        methods.add_method("unlink", |_, this, path: String| {
            this.vm.unlink(path).map_err(mlua::Error::external)?;
            Ok(())
        });
        methods.add_method(
            "rename",
            |_, this, (from, to): (String, String)| {
                this.vm.rename(from, to).map_err(mlua::Error::external)?;
                Ok(())
            },
        );

        // Hypervisor-side resources.
        //
        // `vm:nic(name)` returns a NIC bound to the bridge of that
        // name from this VM's attachments. The NIC's `bridge` is
        // the same handle that `bridge:attach(vm)` recorded.
        methods.add_method("nic", |_, this, name: String| {
            // Two lookup forms:
            //
            //   1. Bridge name — `vm:nic("lan")` → the bridge
            //      attachment recorded by `bridge:attach(vm)`.
            //   2. Guest interface name — `vm:nic("eth0")`. Resolves
            //      to the Nth attached bridge sorted by bridge name
            //      (insertion-stable inside the VM's BTreeMap), so
            //      tests can reference NICs by the same names the
            //      guest's kernel will use.
            //
            // The guest-name mapping is deterministic per build but
            // may not match the kernel's actual device-probe order
            // on every distro; tests that need the precise guest
            // name should use `bridge:attach(vm)` and
            // `vm:nic(bridge_name)` to stay portable.
            if let Some(bridge) = this.vm.bridge_for(&name) {
                return Ok(super::nic_ud::NicUd::with_vm(bridge, this.vm.clone()));
            }
            if let Some(idx) = parse_guest_iface_index(&name) {
                let mut attachments = this.vm.bridge_attachments();
                attachments.sort();
                if let Some(bridge_name) = attachments.get(idx) {
                    if let Some(bridge) = this.vm.bridge_for(bridge_name) {
                        return Ok(super::nic_ud::NicUd::with_vm(
                            bridge,
                            this.vm.clone(),
                        ));
                    }
                }
            }
            Err(mlua::Error::external(format!(
                "vm:nic: no NIC `{name}` on vm `{}` (try a bridge name like `lan` \
                 or a guest iface like `eth0`)",
                this.vm.name()
            )))
        });

        // `vm:disk(id)` looks up a disk attached to this VM. Errors
        // if no disk with that id exists. To create + attach in one
        // call use `vm:attach_disk`.
        methods.add_method("disk", |_, this, id: String| {
            let entry = this.vm.disk_attachment(&id).ok_or_else(|| {
                mlua::Error::external(format!(
                    "vm:disk: no disk `{id}` attached to vm `{}`",
                    this.vm.name()
                ))
            })?;
            Ok(super::disk_ud::DiskUd::with_image_and_vm(
                entry.id,
                entry.size,
                entry.image,
                this.vm.clone(),
            ))
        });
        methods.add_method(
            "attach_disk",
            |_, this, opts: Option<mlua::Table>| {
                let id = opts
                    .as_ref()
                    .and_then(|t| t.get::<Option<String>>("id").ok().flatten())
                    .unwrap_or_else(|| format!("attached-{}", this.vm.name()));
                let size = opts
                    .as_ref()
                    .and_then(|t| t.get::<Option<u64>>("size").ok().flatten())
                    .unwrap_or(4 * 1024 * 1024 * 1024);
                let image: Option<std::path::PathBuf> = opts
                    .as_ref()
                    .and_then(|t| t.get::<Option<String>>("image").ok().flatten())
                    .map(std::path::PathBuf::from);
                this.vm.attach_disk_record(crate::vm::DiskAttachment {
                    id: id.clone(),
                    size,
                    image: image.clone(),
                });
                Ok(super::disk_ud::DiskUd::with_image_and_vm(
                    id,
                    size,
                    image,
                    this.vm.clone(),
                ))
            },
        );

        // Layer-0 ioctl.
        //
        // Two call shapes per `DESIGN.md`:
        //   vm:ioctl(fd, cmd)
        //   vm:ioctl(fd, cmd, data_bytes)
        //   vm:ioctl(fd, cmd, data_bytes, {bufs={…}, ptr_offsets={…}})
        // Returns {ret, result, out_data, out_bufs}.
        methods.add_method(
            "ioctl",
            |lua, this, args: mlua::Variadic<Value>| {
                let fd = args
                    .first()
                    .and_then(|v| match v {
                        Value::Integer(i) => Some(*i as u64),
                        _ => None,
                    })
                    .ok_or_else(|| mlua::Error::external("ioctl: missing fd"))?;
                let cmd = args
                    .get(1)
                    .and_then(|v| match v {
                        Value::Integer(i) => Some(*i as u64),
                        _ => None,
                    })
                    .ok_or_else(|| mlua::Error::external("ioctl: missing cmd"))?;
                let data = args
                    .get(2)
                    .and_then(|v| match v {
                        Value::String(s) => Some(s.as_bytes().to_vec()),
                        _ => None,
                    })
                    .unwrap_or_default();
                let mut bufs: Vec<Vec<u8>> = Vec::new();
                let mut ptr_offsets: Vec<u32> = Vec::new();
                if let Some(Value::Table(t)) = args.get(3) {
                    if let Ok(b) = t.get::<mlua::Table>("bufs") {
                        for pair in b.sequence_values::<mlua::String>() {
                            let s = pair?;
                            bufs.push(s.as_bytes().to_vec());
                        }
                    }
                    if let Ok(p) = t.get::<mlua::Table>("ptr_offsets") {
                        for pair in p.sequence_values::<u32>() {
                            ptr_offsets.push(pair?);
                        }
                    }
                }
                let h = provium_protocol::handle::FileHandle::new(fd);
                let r = this
                    .vm
                    .ioctl_with_ptrs(h, cmd, data, bufs, ptr_offsets)
                    .map_err(mlua::Error::external)?;
                let table = lua.create_table()?;
                table.set("ret", r.ret)?;
                table.set("result", r.ret)?;
                table.set("out_data", lua.create_string(&r.out_data)?)?;
                let outs = lua.create_table()?;
                for (i, b) in r.out_bufs.into_iter().enumerate() {
                    outs.set(i + 1, lua.create_string(&b)?)?;
                }
                table.set("out_bufs", outs)?;
                Ok(Value::Table(table))
            },
        );

        methods.add_method("console", |_, this, ()| {
            Ok(super::result_ud::ConsoleUd::new(this.vm.clone()))
        });

        // Layer-0 raw syscall.
        //
        // Two call shapes per `DESIGN.md` § VM:
        //   vm:syscall(nr, a, b, c, d, e, f)        -- integer-only
        //   vm:syscall(nr, {args=…, bufs=…, ptrs=…})-- with buffers
        //
        // The table form lets test code pass byte buffers and have
        // the agent splice their addresses into the indicated arg
        // slot (`ptrs[i]` is the arg index for `bufs[i]`). The
        // returned table includes `out_bufs` containing the
        // post-syscall buffer contents.
        methods.add_method("syscall", |lua, this, args: mlua::Variadic<Value>| {
            let nr_v = args.first().cloned().ok_or_else(|| {
                mlua::Error::external("vm:syscall(nr, ...): missing nr")
            })?;
            let nr = match nr_v {
                Value::Integer(n) => n,
                Value::Number(n) => n as i64,
                _ => return Err(mlua::Error::external(
                    "vm:syscall: nr must be integer",
                )),
            };
            // Detect table form vs integer form.
            let mut filled = [0i64; 6];
            let mut bufs: Vec<Vec<u8>> = Vec::new();
            let mut ptrs: Vec<u8> = Vec::new();
            let second = args.get(1).cloned();
            if let Some(Value::Table(t)) = second {
                if let Ok(arg_tbl) = t.get::<mlua::Table>("args") {
                    for (i, slot) in filled.iter_mut().enumerate() {
                        let v: Option<i64> = arg_tbl.get((i + 1) as i64).ok();
                        if let Some(v) = v {
                            *slot = v;
                        }
                    }
                }
                if let Ok(buf_tbl) = t.get::<mlua::Table>("bufs") {
                    for pair in buf_tbl.sequence_values::<mlua::String>() {
                        let s = pair?;
                        bufs.push(s.as_bytes().to_vec());
                    }
                }
                if let Ok(ptr_tbl) = t.get::<mlua::Table>("ptrs") {
                    for pair in ptr_tbl.sequence_values::<u8>() {
                        ptrs.push(pair?);
                    }
                }
            } else {
                // Plain-int form: collect remaining integer args.
                let mut all: Vec<i64> = Vec::new();
                for v in args.iter().skip(1) {
                    let n = match v {
                        Value::Integer(n) => *n,
                        Value::Number(n) => *n as i64,
                        _ => 0,
                    };
                    all.push(n);
                }
                for (i, slot) in filled.iter_mut().enumerate() {
                    if let Some(v) = all.get(i) {
                        *slot = *v;
                    }
                }
            }
            let r = this
                .vm
                .syscall_with_bufs(nr, filled, bufs, ptrs)
                .map_err(mlua::Error::external)?;
            let table = lua.create_table()?;
            table.set("ret", r.ret)?;
            table.set("result", r.ret)?;
            table.set("errno", r.errno)?;
            let outs = lua.create_table()?;
            for (i, b) in r.out_bufs.into_iter().enumerate() {
                outs.set(i + 1, lua.create_string(&b)?)?;
            }
            table.set("out_bufs", outs)?;
            Ok(table)
        });

        methods.add_method("tail_file", |lua, this, args: mlua::Variadic<Value>| {
            // Accept `vm:tail_file(path)` or
            // `vm:tail_file(path, opts)` where opts is
            // `{start = "beginning"|"end"|<offset_int>}`. Default
            // is "end" — only new content past current EOF is
            // streamed (DESIGN's tail-style intent). "beginning"
            // replays the whole file from byte 0; an integer
            // offset starts streaming from that exact byte.
            let path: String = match args.first() {
                Some(Value::String(s)) => s
                    .to_str()
                    .map_err(mlua::Error::external)?
                    .to_string(),
                _ => return Err(mlua::Error::external(
                    "vm:tail_file: first arg must be a path string",
                )),
            };
            let start = match args.get(1) {
                None | Some(Value::Nil) => TailStart::End,
                Some(Value::Table(t)) => {
                    match t.get::<Option<Value>>("start")? {
                        None | Some(Value::Nil) => TailStart::End,
                        Some(Value::String(s)) => {
                            let raw = s
                                .to_str()
                                .map_err(mlua::Error::external)?
                                .to_string();
                            match raw.as_str() {
                                "beginning" | "start" => TailStart::Beginning,
                                "end" => TailStart::End,
                                other => {
                                    return Err(mlua::Error::external(format!(
                                        "vm:tail_file: opts.start = `{other}` (expected \"beginning\"|\"end\"|number)"
                                    )));
                                }
                            }
                        }
                        Some(Value::Integer(n)) if n >= 0 => {
                            TailStart::Offset(n as u64)
                        }
                        Some(Value::Integer(n)) => {
                            // Negative offset = N bytes before EOF.
                            // Stat the file to get its size, then
                            // convert to an absolute offset. Race
                            // between stat and the tail-open is
                            // benign: if the file grows in between,
                            // we just emit a few extra bytes from
                            // the older tail; if it shrinks, we
                            // clamp to byte 0.
                            let abs = n.unsigned_abs();
                            let size = this
                                .vm
                                .stat(path.clone())
                                .map_err(mlua::Error::external)?
                                .size;
                            TailStart::Offset(size.saturating_sub(abs))
                        }
                        Some(Value::Number(f)) if f.is_finite() => {
                            // Float for symmetry with Lua's number
                            // type. Negative = from-end, like the
                            // Integer arm above. Truncates toward
                            // zero — half-byte offsets aren't a
                            // thing.
                            let trunc = f.trunc();
                            if trunc >= 0.0 {
                                TailStart::Offset(trunc as u64)
                            } else {
                                let abs = (-trunc) as u64;
                                let size = this
                                    .vm
                                    .stat(path.clone())
                                    .map_err(mlua::Error::external)?
                                    .size;
                                TailStart::Offset(size.saturating_sub(abs))
                            }
                        }
                        Some(other) => {
                            return Err(mlua::Error::external(format!(
                                "vm:tail_file: opts.start type {} not supported \
                                 (expected number or \"beginning\"/\"end\")",
                                other.type_name()
                            )));
                        }
                    }
                }
                Some(other) => {
                    return Err(mlua::Error::external(format!(
                        "vm:tail_file: second arg must be opts table, got {}",
                        other.type_name()
                    )));
                }
            };
            let session = this
                .vm
                .tail_file(path.clone(), start)
                .map_err(mlua::Error::external)?;
            let site = super::result_ud::capture_creation_site(lua);
            let test_name = super::result_ud::current_test_name(lua);
            this.vm.set_stream_meta(
                &session,
                crate::vm::StreamMeta {
                    kind: "tail_file".into(),
                    detail: format!("\"{path}\""),
                    creation_site: site.clone(),
                    test_name,
                },
            );
            super::result_ud::register_resource(
                lua,
                TailUd::new_with_site(session, site),
                "stream",
            )
        });

        // vm:fd_stream(fd) — open a streaming subscription to an
        // existing file handle. Accepts either a numeric handle id
        // (from `file:fd()`) or a `File` userdata directly.
        methods.add_method("fd_stream", |lua, this, fd: Value| {
            let raw_handle = match fd {
                Value::Integer(n) if n > 0 => n as u64,
                Value::Number(n) if n > 0.0 => n as u64,
                Value::UserData(ud) => {
                    if let Ok(file) = ud.borrow::<super::file_ud::FileUd>() {
                        file.raw_handle().ok_or_else(|| {
                            mlua::Error::external("vm:fd_stream: file is closed")
                        })?
                    } else {
                        return Err(mlua::Error::external(
                            "vm:fd_stream expected a File userdata or integer fd",
                        ));
                    }
                }
                other => {
                    return Err(mlua::Error::external(format!(
                        "vm:fd_stream expected fd or File, got {}",
                        other.type_name()
                    )));
                }
            };
            let handle = provium_protocol::handle::FileHandle::new(raw_handle);
            let session = this
                .vm
                .fd_stream(handle)
                .map_err(mlua::Error::external)?;
            let site = super::result_ud::capture_creation_site(lua);
            let test_name = super::result_ud::current_test_name(lua);
            this.vm.set_stream_meta(
                &session,
                crate::vm::StreamMeta {
                    kind: "fd_stream".into(),
                    detail: raw_handle.to_string(),
                    creation_site: site.clone(),
                    test_name,
                },
            );
            super::result_ud::register_resource(
                lua,
                TailUd::new_with_site(session, site),
                "stream",
            )
        });

        // ---------------------------------------------------------------
        // Accessors
        // ---------------------------------------------------------------
        methods.add_method("name", |_, this, ()| Ok(this.vm.name().to_owned()));
        methods.add_method("profile", |_, this, ()| {
            Ok(this.vm.profile_name().to_owned())
        });
        methods.add_method("state", |_, this, ()| Ok(this.vm.state().as_str()));
        methods.add_method("cid", |_, this, ()| Ok(this.vm.cid()));

        methods.add_meta_method(MetaMethod::ToString, |_, this, ()| {
            Ok(format!(
                "vm({} state={} cid={})",
                this.vm.name(),
                this.vm.state().as_str(),
                this.vm.cid().map(|c| c.to_string()).unwrap_or_else(|| "-".into()),
            ))
        });

        // batch(fn) — collect ops inside `fn(b)` and ship as one
        // wire round-trip. Returns a Lua array; entry shape depends
        // on the op:
        //   :run(...)        → RunResult userdata
        //   :read_file(p)    → string (file bytes)
        //   :write_file(...) → nil
        //   :stat(p)         → table {size, mtime_ns, …}
        //   :listdir(p)      → table of entries
        //   :mkdir/unlink/rename → nil
        // Any other op is recorded with a `kind` placeholder so the
        // index pairing stays correct even for ops that don't return
        // useful Lua values yet.
        methods.add_method("batch", |lua, this, fn_value: mlua::Function| {
            let collector = std::sync::Arc::new(std::sync::Mutex::new(BatchInner::default()));
            let batcher = BatchUd {
                collector: std::sync::Arc::clone(&collector),
            };
            fn_value.call::<()>(batcher)?;
            let inner = std::mem::take(&mut *collector.lock().unwrap());
            let responses = this
                .vm
                .batch_op(inner.items)
                .map_err(mlua::Error::external)?;
            let table = lua.create_table()?;
            for (i, resp) in responses.into_iter().enumerate() {
                let value = batch_response_to_lua(lua, resp)?;
                table.set(i + 1, value)?;
            }
            Ok(table)
        });
    }
}

#[derive(Default)]
struct BatchInner {
    items: Vec<provium_protocol::wire::HostMessage>,
}

/// Userdata passed to the function inside [`Vm::batch`].
#[derive(Clone)]
pub(crate) struct BatchUd {
    collector: std::sync::Arc<std::sync::Mutex<BatchInner>>,
}

impl BatchUd {
    fn push(&self, msg: provium_protocol::wire::HostMessage) {
        self.collector.lock().unwrap().items.push(msg);
    }
}

impl UserData for BatchUd {
    fn add_methods<M: UserDataMethods<Self>>(methods: &mut M) {
        methods.add_method("run", |_, this, args: mlua::Variadic<Value>| {
            let exec = build_exec_args(&args).map_err(mlua::Error::external)?;
            this.push(provium_protocol::wire::HostMessage::Exec(exec));
            Ok(())
        });
        methods.add_method("read_file", |_, this, path: String| {
            this.push(provium_protocol::wire::HostMessage::ReadFile(
                provium_protocol::wire::ReadFileArgs { path },
            ));
            Ok(())
        });
        methods.add_method(
            "write_file",
            |_, this, (path, data): (String, mlua::String)| {
                this.push(provium_protocol::wire::HostMessage::WriteFile(
                    provium_protocol::wire::WriteFileArgs {
                        path,
                        data: data.as_bytes().to_vec(),
                        mode: provium_protocol::wire::WriteFileMode::Replace,
                        create_perm: None,
                    },
                ));
                Ok(())
            },
        );
        methods.add_method("stat", |_, this, path: String| {
            this.push(provium_protocol::wire::HostMessage::Stat(
                provium_protocol::wire::StatArgs {
                    path,
                    follow_symlinks: true,
                },
            ));
            Ok(())
        });
        methods.add_method("listdir", |_, this, path: String| {
            this.push(provium_protocol::wire::HostMessage::Listdir(
                provium_protocol::wire::ListdirArgs { path },
            ));
            Ok(())
        });
        methods.add_method(
            "mkdir",
            |_, this, (path, opts): (String, Option<mlua::Value>)| {
                // Mirror direct vm:mkdir's opts parsing: accept
                // either `(path, parents_bool)` for back-compat or
                // `(path, {parents=, perm=})` per DESIGN. Without
                // perm forwarded the batch path silently dropped
                // any test-supplied mode.
                let (parents, create_perm) = match opts {
                    None => (false, None),
                    Some(mlua::Value::Boolean(b)) => (b, None),
                    Some(mlua::Value::Table(t)) => {
                        let p = t.get::<Option<bool>>("parents")?.unwrap_or(false);
                        let m = t.get::<Option<u32>>("perm")?;
                        (p, m)
                    }
                    Some(other) => {
                        return Err(mlua::Error::external(format!(
                            "batch:mkdir: opts must be bool|table|nil, got {}",
                            other.type_name()
                        )));
                    }
                };
                this.push(provium_protocol::wire::HostMessage::Mkdir(
                    provium_protocol::wire::MkdirArgs {
                        path,
                        parents,
                        create_perm,
                    },
                ));
                Ok(())
            },
        );
        methods.add_method("unlink", |_, this, path: String| {
            this.push(provium_protocol::wire::HostMessage::Unlink(
                provium_protocol::wire::UnlinkArgs { path },
            ));
            Ok(())
        });
        methods.add_method(
            "rename",
            |_, this, (from, to): (String, String)| {
                this.push(provium_protocol::wire::HostMessage::Rename(
                    provium_protocol::wire::RenameArgs { from, to },
                ));
                Ok(())
            },
        );
        methods.add_method("syscall", |_, this, (nr, args): (i64, mlua::Variadic<Value>)| {
            // Reject the table form `vm:syscall(nr, {args=, bufs=,
            // ptrs=})` with a clear pointer rather than silently
            // dropping bufs/ptrs. The non-batch path supports it
            // via build_syscall_args; the batch form would need
            // its own parsing pass plus per-item buffer return
            // shape that isn't implemented yet.
            if args.len() == 1 {
                if let Some(Value::Table(_)) = args.get(0) {
                    return Err(mlua::Error::external(
                        "batch:syscall does not support the {args=, bufs=, ptrs=} \
                         table form; use vm:syscall outside the batch, or pass bare \
                         integer args here",
                    ));
                }
            }
            let mut padded = [0i64; 6];
            for (slot, value) in padded.iter_mut().zip(args.iter()) {
                *slot = match value {
                    Value::Integer(n) => *n,
                    Value::Number(n) => *n as i64,
                    _ => 0,
                };
            }
            this.push(provium_protocol::wire::HostMessage::Syscall(
                provium_protocol::wire::SyscallArgs {
                    nr,
                    args: padded,
                    bufs: Vec::new(),
                    ptrs: Vec::new(),
                },
            ));
            Ok(())
        });
    }
}

/// Convert one batch-item response into a Lua value. The whole
/// batch is returned as `{ {ok=…} or {err="…"} }` per index so a
/// single failing item never short-circuits the rest of the batch.
fn batch_response_to_lua(
    lua: &mlua::Lua,
    resp: provium_protocol::wire::AgentMessage,
) -> mlua::Result<Value> {
    use provium_protocol::wire::{AgentMessage, OpResult};
    let entry = lua.create_table()?;
    let set_ok = |t: &mlua::Table, v: Value| -> mlua::Result<Value> {
        t.set("ok", v)?;
        Ok(Value::Table(t.clone()))
    };
    let set_err = |t: &mlua::Table, msg: String| -> mlua::Result<Value> {
        t.set("err", msg)?;
        Ok(Value::Table(t.clone()))
    };
    match resp {
        AgentMessage::ExecResult(provium_protocol::wire::ExecResult::Ok(ok)) => {
            let v = wrap_run_result(lua, crate::vm::RunResult::from_exec_ok(ok))?;
            set_ok(&entry, v)
        }
        AgentMessage::ExecResult(provium_protocol::wire::ExecResult::Err(e)) => {
            set_err(&entry, e.to_string())
        }
        AgentMessage::ReadFileResult(OpResult::Ok(ok)) => {
            let s = lua.create_string(&ok.data)?;
            set_ok(&entry, Value::String(s))
        }
        AgentMessage::ReadFileResult(OpResult::Err(e)) => set_err(&entry, e.to_string()),
        AgentMessage::WriteFileResult(OpResult::Ok(())) => set_ok(&entry, Value::Nil),
        AgentMessage::WriteFileResult(OpResult::Err(e)) => set_err(&entry, e.to_string()),
        AgentMessage::StatResult(OpResult::Ok(meta)) => {
            let t = lua.create_table()?;
            t.set("size", meta.size)?;
            // Match the direct vm:stat() shape: `mtime` in seconds
            // (DESIGN-canonical), `mtime_ns` for nanosecond
            // precision, `perm`, `entry_type`.
            t.set("mtime", (meta.mtime_ns as f64) / 1_000_000_000.0)?;
            t.set("mtime_ns", meta.mtime_ns)?;
            t.set("perm", meta.perm)?;
            t.set("entry_type", entry_type_name(meta.entry_type))?;
            set_ok(&entry, Value::Table(t))
        }
        AgentMessage::StatResult(OpResult::Err(e)) => set_err(&entry, e.to_string()),
        AgentMessage::ListdirResult(OpResult::Ok(entries)) => {
            let t = lua.create_table()?;
            for (i, e) in entries.into_iter().enumerate() {
                let item = lua.create_table()?;
                item.set("name", e.name)?;
                // Match direct vm:listdir shape: each entry is
                // `{name, entry_type}`. Without this, batch:listdir
                // returned a stunted entry that surprised tests
                // checking `e.entry_type == "file"`.
                item.set("entry_type", entry_type_name(e.entry_type))?;
                t.set(i + 1, item)?;
            }
            set_ok(&entry, Value::Table(t))
        }
        AgentMessage::ListdirResult(OpResult::Err(e)) => set_err(&entry, e.to_string()),
        AgentMessage::MkdirResult(OpResult::Ok(())) => set_ok(&entry, Value::Nil),
        AgentMessage::MkdirResult(OpResult::Err(e)) => set_err(&entry, e.to_string()),
        AgentMessage::UnlinkResult(OpResult::Ok(())) => set_ok(&entry, Value::Nil),
        AgentMessage::UnlinkResult(OpResult::Err(e)) => set_err(&entry, e.to_string()),
        AgentMessage::RenameResult(OpResult::Ok(())) => set_ok(&entry, Value::Nil),
        AgentMessage::RenameResult(OpResult::Err(e)) => set_err(&entry, e.to_string()),
        AgentMessage::SyscallResult(r) => {
            let t = lua.create_table()?;
            t.set("ret", r.ret)?;
            t.set("errno", r.errno)?;
            // Mirror direct vm:syscall: out_bufs is a list of
            // post-syscall buffer contents (Lua strings). Tests
            // passing buffers via the table form need them on
            // the way back too — without this batch:syscall
            // discards them silently.
            let out_bufs = lua.create_table()?;
            for (i, buf) in r.out_bufs.iter().enumerate() {
                out_bufs.set(i + 1, lua.create_string(buf)?)?;
            }
            t.set("out_bufs", out_bufs)?;
            set_ok(&entry, Value::Table(t))
        }
        AgentMessage::AgentError(e) => set_err(&entry, e.to_string()),
        other => set_err(&entry, format!("unexpected batch response: {}", other.kind())),
    }
}

/// Parse a Lua boot-opts table per `DESIGN.md` § VM into a
/// partial [`crate::vmm::BootOpts`] suitable for
/// [`crate::vm::Vm::merge_boot_opts`].
fn parse_boot_overrides(t: &mlua::Table) -> mlua::Result<crate::vmm::BootOpts> {
    let mut opts = crate::vmm::BootOpts::default();
    if let Ok(cmdline) = t.get::<String>("kernel_cmdline") {
        opts.cmdline_override = Some(cmdline);
    }
    if let Ok(rng) = t.get::<u64>("rng_seed") {
        opts.rng_seed = Some(rng);
    }
    // initial_time accepts either an integer (seconds) or a float
    // (sub-second precision). DESIGN's `clock:set` already takes
    // floats so boot_opts must too — silent integer-only would
    // be a footgun.
    match t.get::<Value>("initial_time") {
        Ok(Value::Nil) => {}
        Ok(Value::Integer(secs)) => {
            opts.initial_time_ns = Some((secs as i64).saturating_mul(1_000_000_000));
        }
        Ok(Value::Number(secs)) => {
            if !secs.is_finite() {
                return Err(mlua::Error::external(
                    "boot_opts.initial_time: NaN/inf not allowed",
                ));
            }
            opts.initial_time_ns = Some((secs * 1e9) as i64);
        }
        Ok(other) => {
            return Err(mlua::Error::external(format!(
                "boot_opts.initial_time: expected number (seconds since epoch), got {}",
                other.type_name(),
            )));
        }
        Err(_) => {}
    }
    if let Ok(files_tbl) = t.get::<mlua::Table>("files") {
        for pair in files_tbl.sequence_values::<mlua::Table>() {
            let entry = pair?;
            let path: String = entry.get("path")?;
            let data: mlua::String = entry.get("content")?;
            opts.files.push(crate::vmm::InjectedFile {
                guest_path: std::path::PathBuf::from(path),
                content: data.as_bytes().to_vec(),
            });
        }
    }
    Ok(opts)
}

/// Public re-export of [`build_exec_args`] so [`super::result_ud`]'s
/// worker bindings can re-use the same shell-vs-table form.
pub(crate) fn build_exec_args_public(args: &mlua::Variadic<Value>) -> Result<ExecArgs, String> {
    build_exec_args(args)
}

/// Public re-export of [`open_mode_from_table`] for the worker
/// bindings.
pub(crate) fn open_mode_from_table_public(t: &mlua::Table) -> mlua::Result<OpenMode> {
    open_mode_from_table(t)
}

/// Translate a Lua-side `vm:run(...)` argument list into [`ExecArgs`].
///
/// Accepts:
/// * `("cmd")` — runs through `/bin/sh -c "cmd"` so shell features
///   work without callers having to wrap them.
/// * `("cmd", {"arg1", ...})` — direct exec, no shell.
fn build_exec_args(args: &mlua::Variadic<Value>) -> Result<ExecArgs, String> {
    let cmd_value = args
        .first()
        .ok_or_else(|| "vm:run expects at least a command string".to_owned())?;
    let cmd = match cmd_value {
        Value::String(s) => s.to_str().map_err(|e| e.to_string())?.to_string(),
        _ => return Err("vm:run: first argument must be a string".into()),
    };

    // `None` here means the caller passed no second argument →
    // shell form (`sh -c "<cmd>"`). An empty *table* on the other
    // hand is direct exec with zero positional args; the two cases
    // are distinct.
    //
    // The second arg can ALSO be an opts table per `DESIGN.md` § VM:
    // `{env=..., env_clear=..., cwd=..., stdin=..., timeout_ms=..., args=...}`.
    // We detect by looking for any non-array key (env/cwd/stdin/etc.)
    // — a pure array is the legacy direct-exec args form.
    let mut direct_exec_args: Option<Vec<String>> = None;
    let mut env: std::collections::BTreeMap<String, String> =
        std::collections::BTreeMap::new();
    let mut env_clear = false;
    let mut cwd: Option<String> = None;
    let mut stdin_bytes: Vec<u8> = Vec::new();
    let mut timeout_ms: Option<u64> = None;
    if let Some(Value::Table(t)) = args.get(1) {
        let has_opts_keys = t.contains_key("env").unwrap_or(false)
            || t.contains_key("env_clear").unwrap_or(false)
            || t.contains_key("cwd").unwrap_or(false)
            || t.contains_key("stdin").unwrap_or(false)
            || t.contains_key("timeout_ms").unwrap_or(false)
            || t.contains_key("timeout").unwrap_or(false)
            || t.contains_key("args").unwrap_or(false);
        if has_opts_keys {
            // opts form
            if let Ok(args_t) = t.get::<mlua::Table>("args") {
                let mut out = Vec::new();
                for pair in args_t.clone().pairs::<i64, mlua::String>() {
                    let (_, v) = pair.map_err(|e| e.to_string())?;
                    out.push(v.to_str().map_err(|e| e.to_string())?.to_string());
                }
                direct_exec_args = Some(out);
            }
            if let Ok(env_t) = t.get::<mlua::Table>("env") {
                for pair in env_t.pairs::<String, String>() {
                    let (k, v) = pair.map_err(|e| e.to_string())?;
                    env.insert(k, v);
                }
            }
            if let Ok(b) = t.get::<bool>("env_clear") {
                env_clear = b;
            }
            if let Ok(c) = t.get::<String>("cwd") {
                cwd = Some(c);
            }
            if let Ok(s) = t.get::<mlua::String>("stdin") {
                stdin_bytes = s.as_bytes().to_vec();
            }
            if let Ok(n) = t.get::<u64>("timeout_ms") {
                timeout_ms = Some(n);
            } else if let Ok(s) = t.get::<String>("timeout") {
                if let Some(secs) = parse_timeout_str(&s) {
                    if !secs.is_finite() || secs < 0.0 {
                        return Err(format!(
                            "vm:run timeout: must be a finite, non-negative number (got {secs})"
                        ));
                    }
                    timeout_ms = Some((secs * 1000.0) as u64);
                }
            } else if let Ok(n) = t.get::<f64>("timeout") {
                if !n.is_finite() || n < 0.0 {
                    return Err(format!(
                        "vm:run timeout: must be a finite, non-negative number (got {n})"
                    ));
                }
                timeout_ms = Some((n * 1000.0) as u64);
            }
        } else {
            // Legacy direct-args list form.
            let mut out = Vec::new();
            for pair in t.clone().pairs::<i64, mlua::String>() {
                let (_, v) = pair.map_err(|e| e.to_string())?;
                out.push(v.to_str().map_err(|e| e.to_string())?.to_string());
            }
            direct_exec_args = Some(out);
        }
    } else if let Some(other) = args.get(1) {
        if !matches!(other, Value::Nil) {
            return Err(format!(
                "vm:run: second argument must be a table, got {}",
                other.type_name()
            ));
        }
    }

    let (cmd, args_vec) = match direct_exec_args {
        None => ("sh".into(), vec!["-c".into(), cmd]),
        Some(direct) => (cmd, direct),
    };

    Ok(ExecArgs {
        cmd,
        args: args_vec,
        env,
        env_clear,
        stdin: stdin_bytes,
        cwd,
        timeout_ms,
    })
}

fn parse_timeout_str(s: &str) -> Option<f64> {
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

/// Translate a Lua `{read=true, write=true, …}` table into
/// [`OpenMode`]. Used by `vm:open_file`.
///
/// Refuses an empty / all-false table — without an explicit
/// access mode the open would deferred-error at the first
/// read/write call, surfaced as a stale-handle EBADF that
/// the test author can't easily map back to `open_file({})`.
/// Rejecting at parse time turns the silent footgun into an
/// at-the-call-site error.
fn open_mode_from_table(t: &mlua::Table) -> mlua::Result<OpenMode> {
    let mode = OpenMode {
        read: t.get::<Option<bool>>("read")?.unwrap_or(false),
        write: t.get::<Option<bool>>("write")?.unwrap_or(false),
        create: t.get::<Option<bool>>("create")?.unwrap_or(false),
        truncate: t.get::<Option<bool>>("truncate")?.unwrap_or(false),
        append: t.get::<Option<bool>>("append")?.unwrap_or(false),
        exclusive: t.get::<Option<bool>>("exclusive")?.unwrap_or(false),
    };
    if !mode.read && !mode.write && !mode.append {
        return Err(mlua::Error::external(
            "vm:open_file: must specify at least one of `read`, `write`, `append` \
             (got `{}` — a no-mode open would defer errors to the first I/O).",
        ));
    }
    Ok(mode)
}

/// Resolve `raw` against the directory of the Lua source file that
/// invoked the current Rust callback. Absolute paths are returned
/// unchanged. Resolution order:
///
/// 1. Walk the Lua stack looking for the first `@`-prefixed source
///    (a file-backed chunk — typically a `require`d helper).
/// 2. Fall back to the `_PROVIUM_SOURCE_PATH` global, set by the
///    runner / fixture builder to the absolute path of the
///    top-level test or fixture file. This is needed because those
///    chunks are loaded via `lua.load(&source).set_name(basename)`,
///    so their source field reads as a string chunk (no `@`).
/// 3. Otherwise return the raw path; `std::fs` resolves it against
///    cwd.
fn resolve_caller_relative(
    lua: &mlua::Lua,
    raw: &str,
) -> mlua::Result<std::path::PathBuf> {
    let p = std::path::Path::new(raw);
    if p.is_absolute() {
        return Ok(p.to_path_buf());
    }
    if let Ok(debug) = lua.globals().get::<mlua::Table>("debug") {
        if let Ok(getinfo) = debug.get::<mlua::Function>("getinfo") {
            for level in 1..16 {
                let info: Option<mlua::Table> =
                    getinfo.call((level, "S")).ok();
                let Some(info) = info else { break };
                let source: Option<String> = info.get("source").ok();
                if let Some(s) = source {
                    if let Some(file) = s.strip_prefix('@') {
                        let file_path = std::path::Path::new(file);
                        if let Some(dir) = file_path.parent() {
                            return Ok(dir.join(raw));
                        }
                    }
                }
            }
        }
    }
    if let Ok(top) = lua.globals().get::<String>("_PROVIUM_SOURCE_PATH") {
        let file_path = std::path::Path::new(&top);
        if let Some(dir) = file_path.parent() {
            return Ok(dir.join(raw));
        }
    }
    Ok(p.to_path_buf())
}

fn entry_type_name(t: provium_protocol::wire::EntryType) -> &'static str {
    use provium_protocol::wire::EntryType::*;
    match t {
        File => "file",
        Directory => "directory",
        Symlink => "symlink",
        Fifo => "fifo",
        Socket => "socket",
        BlockDevice => "block_device",
        CharDevice => "char_device",
        Other => "other",
    }
}

#[allow(dead_code)]
fn _open_mode_default_check() {
    let _ = OpenMode::default();
}

/// Parse a guest interface name like `eth0`/`eth7`/`enp0s3` into
/// its index. Returns `None` if the name doesn't follow a
/// recognised pattern. Used by `vm:nic(...)` to map design-style
/// guest names onto the VM's bridge-attachment ordering.
fn parse_guest_iface_index(name: &str) -> Option<usize> {
    if let Some(rest) = name.strip_prefix("eth") {
        return rest.parse::<usize>().ok();
    }
    if let Some(rest) = name.strip_prefix("ens") {
        return rest.parse::<usize>().ok();
    }
    if let Some(rest) = name.strip_prefix("enp0s") {
        return rest.parse::<usize>().ok();
    }
    None
}

fn generate_snapshot_tempfile_path() -> std::path::PathBuf {
    let id = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    std::env::temp_dir().join(format!(
        "provium-snap-{}-{}.bin",
        std::process::id(),
        id
    ))
}
