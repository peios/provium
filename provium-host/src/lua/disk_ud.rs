//! Disk userdata. Sector reads and writes go straight to the backing
//! image on the host, and fault injection acts on that path; `detach`
//! is the one op that reaches QEMU, as a best-effort `device_del`.
//! Nothing here adds a device to a running guest: `vm:attach_disk`
//! records a host-side attachment only, and a disk the guest should
//! see is declared at boot (the profile's `disks` or `vm:boot({disks})`).

use std::sync::{Arc, Mutex};

use mlua::{UserData, UserDataMethods, Value};

const SECTOR_BYTES: u64 = 512;

/// Latency injected per host-side I/O when `slow` is in the fault
/// set. Tunable in source — tests that need a finer dial can set
/// the actual value via a future `disk:fault_inject_slow_ms(n)` op.
const SLOW_FAULT_MS: u64 = 50;

/// Recognised `disk:fault_inject(mode)` values. The userdata
/// rejects anything outside this set so test typos surface
/// immediately rather than silently being inert.
const VALID_FAULT_MODES: &[&str] = &["eio_read", "eio_write", "slow"];

#[derive(Clone)]
pub(crate) struct DiskUd {
    inner: Arc<Mutex<DiskInner>>,
}

struct DiskInner {
    id: String,
    /// Path of the backing file. `None` for tests that exercise
    /// the API surface only.
    image: Option<std::path::PathBuf>,
    /// Modeled disk size for tests; real impl reads from QMP.
    size: u64,
    /// Active fault injections — a set of mode names.
    faults: std::collections::BTreeSet<String>,
    detached: bool,
    /// Parent VM, when known. Used so `:detach()` can issue a
    /// real QMP `device_del` (best-effort).
    vm: Option<crate::vm::Vm>,
}

impl DiskUd {
    #[allow(dead_code)]
    pub(crate) fn new(id: impl Into<String>, size: u64) -> Self {
        Self::with_image(id, size, None)
    }

    /// Build a DiskUd backed by an image file. Sector ops route
    /// through it directly.
    pub(crate) fn with_image(
        id: impl Into<String>,
        size: u64,
        image: Option<std::path::PathBuf>,
    ) -> Self {
        Self {
            inner: Arc::new(Mutex::new(DiskInner {
                id: id.into(),
                image,
                size,
                faults: std::collections::BTreeSet::new(),
                detached: false,
                vm: None,
            })),
        }
    }

    /// Variant carrying the parent VM so `:detach()` can issue a
    /// real QMP `device_del`.
    pub(crate) fn with_image_and_vm(
        id: impl Into<String>,
        size: u64,
        image: Option<std::path::PathBuf>,
        vm: crate::vm::Vm,
    ) -> Self {
        Self {
            inner: Arc::new(Mutex::new(DiskInner {
                id: id.into(),
                image,
                size,
                faults: std::collections::BTreeSet::new(),
                detached: false,
                vm: Some(vm),
            })),
        }
    }
}

/// Read-modify-write the guest-visible fault policy of a mediated disk.
///
/// Each Lua verb sets one field and leaves the others, so rules
/// accumulate the way `bridge:add_latency` + `bridge:drop_rate` do
/// rather than each call silently clearing the last.
fn amend_policy(
    disk: &DiskUd,
    op: &str,
    change: impl FnOnce(&mut crate::nbd::FaultPolicy),
) -> mlua::Result<()> {
    let (id, vm) = {
        let g = disk.inner.lock().unwrap();
        (g.id.clone(), g.vm.clone())
    };
    let Some(vm) = vm else {
        return Err(mlua::Error::external(format!(
            "disk:{op}: disk `{id}` has no VM — a fault the guest can see \
             needs a disk booted with `mediated = true`"
        )));
    };
    let mut policy = vm.disk_policy(&id).map_err(|e| unmediated(op, &id, e))?;
    change(&mut policy);
    vm.set_disk_policy(&id, policy)
        .map_err(|e| unmediated(op, &id, e))
}

/// The error a guest-visible fault op fails with when there is no
/// mediated server behind the disk.
///
/// Both halves earn their place. The op named is the one the *test*
/// called: every verb reads the policy before amending it, so the raw
/// error says `disk_policy` no matter which verb the author wrote, and
/// a message naming a method nobody called is a bad way to learn what
/// went wrong. And the underlying error is kept, because "the VM is not
/// booted" and "the disk is not mediated" are different mistakes and
/// only one of them is fixed by editing the boot.
fn unmediated(op: &str, id: &str, cause: impl std::fmt::Display) -> mlua::Error {
    mlua::Error::external(format!(
        "disk:{op}: no mediated server for disk `{id}` — a fault the guest \
         can see needs `mediated = true` on the disk when the VM boots \
         ({cause})"
    ))
}

impl UserData for DiskUd {
    fn add_methods<M: UserDataMethods<Self>>(methods: &mut M) {
        methods.add_method("size", |_, this, ()| {
            let (image, size) = {
                let g = this.inner.lock().unwrap();
                (g.image.clone(), g.size)
            };
            // Prefer the live image file size when one is attached
            // — keeps `disk:size()` honest when the test resized
            // the underlying file (truncate, fallocate, etc.).
            if let Some(image) = image {
                if let Ok(meta) = std::fs::metadata(&image) {
                    return Ok(meta.len());
                }
            }
            Ok(size)
        });
        methods.add_method("read_sectors", |lua, this, (off, n): (u64, u64)| {
            let g = this.inner.lock().unwrap();
            if g.detached {
                return Err(mlua::Error::external(
                    "disk:read_sectors: disk is detached",
                ));
            }
            let image = g.image.clone().ok_or_else(|| {
                mlua::Error::external(
                    "disk:read_sectors: no backing image — disk:with_image required",
                )
            })?;
            // Honour active fault injections per
            // `DESIGN.md` § Disk: eio_read short-circuits to EIO,
            // slow inserts artificial latency.
            if g.faults.contains("eio_read") {
                return Err(mlua::Error::external(
                    "disk:read_sectors: EIO (fault_inject)",
                ));
            }
            let slow = g.faults.contains("slow");
            drop(g);
            if slow {
                std::thread::sleep(std::time::Duration::from_millis(SLOW_FAULT_MS));
                // Re-check after the slow-fault sleep — a
                // concurrent fault_inject("eio_read") that landed
                // while we were asleep should still take effect.
                if this.inner.lock().unwrap().faults.contains("eio_read") {
                    return Err(mlua::Error::external(
                        "disk:read_sectors: EIO (fault_inject, set during slow read)",
                    ));
                }
            }
            let mut f =
                std::fs::File::open(&image).map_err(mlua::Error::external)?;
            use std::io::{Read, Seek, SeekFrom};
            f.seek(SeekFrom::Start(off.saturating_mul(SECTOR_BYTES)))
                .map_err(mlua::Error::external)?;
            let want = (n.saturating_mul(SECTOR_BYTES)) as usize;
            let mut buf = vec![0u8; want];
            let read = f
                .read(&mut buf)
                .map_err(mlua::Error::external)?;
            buf.truncate(read);
            // Final post-I/O check — a fault_inject that landed
            // during the actual read should still convert the
            // result to EIO. Without this the test author can't
            // observe a mid-read injection at all.
            if this.inner.lock().unwrap().faults.contains("eio_read") {
                return Err(mlua::Error::external(
                    "disk:read_sectors: EIO (fault_inject, set during I/O)",
                ));
            }
            lua.create_string(&buf).map(Value::String)
        });
        methods.add_method(
            "write_sectors",
            |_, this, (off, data): (u64, mlua::String)| {
                let g = this.inner.lock().unwrap();
                if g.detached {
                    return Err(mlua::Error::external(
                        "disk:write_sectors: disk is detached",
                    ));
                }
                let image = g.image.clone().ok_or_else(|| {
                    mlua::Error::external(
                        "disk:write_sectors: no backing image — disk:with_image required",
                    )
                })?;
                if g.faults.contains("eio_write") {
                    return Err(mlua::Error::external(
                        "disk:write_sectors: EIO (fault_inject)",
                    ));
                }
                let slow = g.faults.contains("slow");
                drop(g);
                if slow {
                    std::thread::sleep(std::time::Duration::from_millis(SLOW_FAULT_MS));
                    // R9 vm-MINOR: mirror read_sectors —
                    // re-check after the slow sleep so a
                    // concurrent fault_inject("eio_write") that
                    // arrived during the sleep still takes
                    // effect.
                    if this.inner.lock().unwrap().faults.contains("eio_write") {
                        return Err(mlua::Error::external(
                            "disk:write_sectors: EIO (fault_inject, set during slow write)",
                        ));
                    }
                }
                use std::io::{Seek, SeekFrom, Write};
                let mut f = std::fs::OpenOptions::new()
                    .write(true)
                    .open(&image)
                    .map_err(mlua::Error::external)?;
                f.seek(SeekFrom::Start(off.saturating_mul(SECTOR_BYTES)))
                    .map_err(mlua::Error::external)?;
                f.write_all(&data.as_bytes())
                    .map_err(mlua::Error::external)?;
                // Final post-I/O recheck — symmetric with
                // read_sectors. Without this a concurrent
                // fault_inject mid-write is silently swallowed.
                if this.inner.lock().unwrap().faults.contains("eio_write") {
                    return Err(mlua::Error::external(
                        "disk:write_sectors: EIO (fault_inject, set during I/O)",
                    ));
                }
                Ok(())
            },
        );
        methods.add_method("fault_inject", |_, this, mode: String| {
            if !VALID_FAULT_MODES.contains(&mode.as_str()) {
                return Err(mlua::Error::external(format!(
                    "disk:fault_inject: unknown mode `{mode}` (valid: {})",
                    VALID_FAULT_MODES.join(", "),
                )));
            }
            this.inner.lock().unwrap().faults.insert(mode);
            Ok(())
        });
        methods.add_method("clear_faults", |_, this, ()| {
            this.inner.lock().unwrap().faults.clear();
            Ok(())
        });
        methods.add_method("active_faults", |lua, this, ()| {
            let g = this.inner.lock().unwrap();
            let table = lua.create_table()?;
            for (i, mode) in g.faults.iter().enumerate() {
                table.set(i + 1, mode.as_str())?;
            }
            Ok(Value::Table(table))
        });
        methods.add_method("power_cut", |_, this, ()| {
            // The fault modes above are the *test's* view of the image.
            // This one is the guest's: it throws away whatever the
            // guest wrote but never flushed, so what is left is exactly
            // what it made durable.
            //
            // Only a disk booted with `mediated = true` has a server to
            // ask, and a disk without one says so rather than quietly
            // succeeding — a power cut that silently did nothing would
            // let a test conclude data survived a crash that never
            // happened.
            let (id, vm) = {
                let g = this.inner.lock().unwrap();
                (g.id.clone(), g.vm.clone())
            };
            let Some(vm) = vm else {
                return Err(mlua::Error::external(format!(
                    "disk:power_cut: disk `{id}` has no VM to cut power to"
                )));
            };
            vm.power_cut_disk(&id).map_err(mlua::Error::external)?;
            Ok(())
        });

        // --- faults the guest can see -------------------------------
        //
        // `fault_inject` above changes what `read_sectors` and
        // `write_sectors` do — the *test's* view of the image. These
        // are served into the guest's own I/O, so they need a disk
        // booted `mediated = true` and say so on any other rather than
        // quietly doing the weaker host-side thing.
        //
        // Named for what the guest sees rather than reusing
        // `fault_inject`'s `eio_read` / `eio_write` mode strings: one
        // name meaning two different layers on one object is the
        // easiest possible thing for a test author to misread
        // (PEI-1104).
        methods.add_method("fail_reads", |_, this, ()| {
            amend_policy(this, "fail_reads", |p| p.fail_reads = true)
        });
        methods.add_method("fail_writes", |_, this, ()| {
            amend_policy(this, "fail_writes", |p| p.fail_writes = true)
        });
        methods.add_method("fail_after", |_, this, n: u64| {
            if n == 0 {
                return Err(mlua::Error::external(
                    "disk:fail_after: n counts commands and must be at least \
                     1; 0 would mean failing one that never happened",
                ));
            }
            amend_policy(this, "fail_after", |p| p.fail_after = Some(n))
        });
        methods.add_method("fail_range", |_, this, (sector, count): (u64, u64)| {
            if count == 0 {
                return Err(mlua::Error::external(
                    "disk:fail_range: count must be at least 1 — an empty \
                     range overlaps nothing, so the fault would never fire",
                ));
            }
            amend_policy(this, "fail_range", |p| {
                p.fail_range = Some((sector, count))
            })
        });
        methods.add_method("delay", |_, this, ms: u64| {
            amend_policy(this, "delay", |p| {
                p.delay = Some(std::time::Duration::from_millis(ms))
            })
        });
        methods.add_method("clear_policy", |_, this, ()| {
            amend_policy(this, "clear_policy", |p| {
                *p = crate::nbd::FaultPolicy::default()
            })
        });
        methods.add_method("fault_policy", |lua, this, ()| {
            let (id, vm) = {
                let g = this.inner.lock().unwrap();
                (g.id.clone(), g.vm.clone())
            };
            let Some(vm) = vm else {
                return Err(mlua::Error::external(format!(
                    "disk:fault_policy: disk `{id}` has no VM — a fault the \
                     guest can see needs a disk booted with `mediated = true`"
                )));
            };
            let policy = vm
                .disk_policy(&id)
                .map_err(|e| unmediated("fault_policy", &id, e))?;
            let table = lua.create_table()?;
            table.set("fail_reads", policy.fail_reads)?;
            table.set("fail_writes", policy.fail_writes)?;
            // Absent rather than false/0 when unset, so `if
            // p.fail_after then` is the natural test in Lua.
            if let Some(n) = policy.fail_after {
                table.set("fail_after", n)?;
            }
            if let Some((sector, count)) = policy.fail_range {
                table.set("fail_range_sector", sector)?;
                table.set("fail_range_count", count)?;
            }
            if let Some(delay) = policy.delay {
                table.set("delay_ms", delay.as_millis() as u64)?;
            }
            Ok(Value::Table(table))
        });

        methods.add_method("detach", |_, this, ()| {
            let (id, vm) = {
                let mut g = this.inner.lock().unwrap();
                g.detached = true;
                (g.id.clone(), g.vm.clone())
            };
            // Best-effort QMP device_del. If the disk was never
            // QMP-added (host-bookkeeping-only attach), this errors
            // with "Device not found"; treat as soft failure.
            if let Some(vm) = vm {
                let _ = vm.detach_disk(&id);
            }
            Ok(())
        });
        methods.add_method("is_detached", |_, this, ()| {
            Ok(this.inner.lock().unwrap().detached)
        });
        methods.add_method("id", |_, this, ()| {
            Ok(this.inner.lock().unwrap().id.clone())
        });
    }
}
