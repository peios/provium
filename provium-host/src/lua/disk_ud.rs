//! Disk userdata. Hypervisor-side disk ops (read_sectors,
//! write_sectors, fault_inject) are slice-11.6 territory — they
//! need QMP block-backend integration. This stub records the API
//! shape so test code can call the methods without erroring; calls
//! return either default values or `Unimplemented` errors that the
//! 11.6 implementation will replace.

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
