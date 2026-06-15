//! Lua-side `File` userdata. Wraps a [`provium_protocol::handle::FileHandle`]
//! and the parent [`crate::vm::Vm`], exposing the design's
//! `file:read/write/seek/tell/close/fd/tail_stream`.

use std::sync::{Arc, Mutex};

use mlua::{MetaMethod, UserData, UserDataMethods, Value};

use provium_protocol::handle::FileHandle;
use provium_protocol::wire::{SeekWhence, TailStart};

use crate::vm::Vm;

/// Lua-facing wrapper. The handle is `None` after `:close()` so
/// subsequent ops return a friendly Lua error.
#[derive(Clone)]
pub(crate) struct FileUd {
    inner: Arc<Mutex<FileInner>>,
}

struct FileInner {
    handle: Option<FileHandle>,
    vm: Vm,
    cursor: u64,
    /// Source path the file was opened with — recorded so
    /// [`tail_stream`] can call back through `vm:tail_file(path)`
    /// without a path-of-fd lookup.
    path: Option<String>,
}

impl FileUd {
    #[allow(dead_code)]
    pub(crate) fn wrap(vm: Vm, handle: FileHandle) -> Self {
        Self {
            inner: Arc::new(Mutex::new(FileInner {
                handle: Some(handle),
                vm,
                cursor: 0,
                path: None,
            })),
        }
    }

    /// Build a [`FileUd`] retaining the source path. Preferred over
    /// [`Self::wrap`] for files opened via `vm:open_file`, where the
    /// caller's path is the cheapest way to support
    /// [`Self::tail_stream`].
    pub(crate) fn wrap_with_path(vm: Vm, handle: FileHandle, path: String) -> Self {
        Self {
            inner: Arc::new(Mutex::new(FileInner {
                handle: Some(handle),
                vm,
                cursor: 0,
                path: Some(path),
            })),
        }
    }

    /// Return the raw u64 handle id, or `None` if the file is closed.
    /// Used by `vm:fd_stream(file)` to dispatch via the file table.
    pub(crate) fn raw_handle(&self) -> Option<u64> {
        self.inner.lock().unwrap().handle.map(|h| h.get())
    }
}

impl UserData for FileUd {
    fn add_methods<M: UserDataMethods<Self>>(methods: &mut M) {
        // file:read(n) — read up to n bytes.
        methods.add_method("read", |lua, this, n: u64| {
            let mut g = this.inner.lock().unwrap();
            let h = g
                .handle
                .ok_or_else(|| mlua::Error::external("file is closed"))?;
            let bytes = g.vm.read(h, n).map_err(mlua::Error::external)?;
            g.cursor = g.cursor.saturating_add(bytes.len() as u64);
            lua.create_string(&bytes).map(Value::String)
        });

        // file:read_all() — drain to EOF (chunked under the hood).
        methods.add_method("read_all", |lua, this, ()| {
            let mut g = this.inner.lock().unwrap();
            let h = g
                .handle
                .ok_or_else(|| mlua::Error::external("file is closed"))?;
            let mut acc = Vec::new();
            const CHUNK: u64 = 64 * 1024;
            loop {
                let bytes = g.vm.read(h, CHUNK).map_err(mlua::Error::external)?;
                if bytes.is_empty() {
                    break;
                }
                g.cursor = g.cursor.saturating_add(bytes.len() as u64);
                acc.extend_from_slice(&bytes);
            }
            lua.create_string(&acc).map(Value::String)
        });

        // file:write(data) — returns bytes written.
        methods.add_method("write", |_, this, data: mlua::String| {
            let mut g = this.inner.lock().unwrap();
            let h = g
                .handle
                .ok_or_else(|| mlua::Error::external("file is closed"))?;
            let bytes = data.as_bytes().to_vec();
            let written = g
                .vm
                .write(h, bytes)
                .map_err(mlua::Error::external)?;
            g.cursor = g.cursor.saturating_add(written);
            Ok(written)
        });

        // file:seek(offset, whence?) — whence: "set"/"cur"/"end".
        methods.add_method(
            "seek",
            |_, this, (offset, whence): (i64, Option<String>)| {
                let mut g = this.inner.lock().unwrap();
                let h = g
                    .handle
                    .ok_or_else(|| mlua::Error::external("file is closed"))?;
                let w = match whence.as_deref().unwrap_or("set") {
                    "set" => SeekWhence::Set,
                    "cur" => SeekWhence::Cur,
                    "end" => SeekWhence::End,
                    other => {
                        return Err(mlua::Error::external(format!(
                            "seek: whence must be set/cur/end, got `{other}`"
                        )));
                    }
                };
                let new = g.vm.seek(h, offset, w).map_err(mlua::Error::external)?;
                g.cursor = new;
                Ok(new)
            },
        );

        methods.add_method("tell", |_, this, ()| {
            // Authoritative position via a no-op `Seek(Cur, 0)` on
            // the agent. The host-side `cursor` cache can drift
            // when a read/write returns mid-flight (the cursor
            // increment is skipped on the error path); reading
            // back from the agent keeps `tell()` honest at the
            // cost of one wire round-trip.
            let mut g = this.inner.lock().unwrap();
            let h = g
                .handle
                .ok_or_else(|| mlua::Error::external("file is closed"))?;
            let pos = g
                .vm
                .seek(h, 0, SeekWhence::Cur)
                .map_err(mlua::Error::external)?;
            g.cursor = pos;
            Ok(pos)
        });

        methods.add_method("close", |_, this, ()| {
            let mut g = this.inner.lock().unwrap();
            if let Some(h) = g.handle.take() {
                g.vm.close(h).map_err(mlua::Error::external)?;
            }
            Ok(())
        });

        // file:fd() — raw fd (host-side handle id).
        methods.add_method("fd", |_, this, ()| {
            let g = this.inner.lock().unwrap();
            Ok(g.handle.map(|h| h.get()).unwrap_or(0))
        });

        // file:tail_stream() — open a tail stream rooted at the
        // file's current position. Defers to vm:tail_file with
        // TailStart::Offset for the file's current cursor.
        methods.add_method("tail_stream", |lua, this, ()| {
            let g = this.inner.lock().unwrap();
            let path = g
                .path
                .clone()
                .ok_or_else(|| mlua::Error::external(
                    "file:tail_stream needs the source path; \
                     opened via wrap() not wrap_with_path()",
                ))?;
            let cursor = g.cursor;
            let vm = g.vm.clone();
            drop(g);
            let session = vm
                .tail_file(path.clone(), TailStart::Offset(cursor))
                .map_err(mlua::Error::external)?;
            let site = super::result_ud::capture_creation_site(lua);
            let test_name = super::result_ud::current_test_name(lua);
            vm.set_stream_meta(
                &session,
                crate::vm::StreamMeta {
                    kind: "file_tail_stream".into(),
                    detail: format!("\"{path}\""),
                    creation_site: site.clone(),
                    test_name,
                },
            );
            super::result_ud::register_resource(
                lua,
                super::result_ud::TailUd::new_with_site(session, site),
                "stream",
            )
        });

        methods.add_meta_method(MetaMethod::ToString, |_, this, ()| {
            let g = this.inner.lock().unwrap();
            Ok(format!(
                "file({})",
                g.handle.map(|h| h.to_string()).unwrap_or_else(|| "closed".into())
            ))
        });
    }
}

#[allow(dead_code)]
fn _tail_start_use() {
    let _ = TailStart::End;
}
