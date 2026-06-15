//! Lua bindings for snapshot handles.
//!
//! `vm:snapshot()` (no args) writes the snapshot bytes to a temp
//! file and returns this userdata wrapping the path. Fixture
//! builders return one of these from their chunk; the framework
//! reads the path, moves the file into the cache, and tears the
//! build VM down.
//!
//! `vm:snapshot("/explicit/path")` writes to that path and still
//! returns a `SnapshotUd` so the call shape is uniform.

use std::path::PathBuf;

use mlua::{MetaMethod, UserData, UserDataMethods};

/// Lua-facing wrapper over a snapshot file on disk.
#[derive(Clone, Debug)]
pub(crate) struct SnapshotUd {
    pub(crate) path: PathBuf,
}

impl UserData for SnapshotUd {
    fn add_methods<M: UserDataMethods<Self>>(methods: &mut M) {
        methods.add_method("path", |_, this, ()| {
            Ok(this.path.display().to_string())
        });
        methods.add_method("size", |_, this, ()| {
            // After `snap:delete()` the file is gone. DESIGN.md
            // documents `snap:delete` as idempotent and lists
            // `snap:size() -> bytes` with no error path; return 0
            // in the gone-file case rather than surfacing ENOENT.
            match std::fs::metadata(&this.path) {
                Ok(m) => Ok(m.len()),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(0),
                Err(e) => Err(mlua::Error::external(e)),
            }
        });
        methods.add_method("delete", |_, this, ()| {
            // Best-effort delete. Idempotent — calling on a path
            // that's already gone (e.g. moved into the cache)
            // returns Ok.
            match std::fs::remove_file(&this.path) {
                Ok(()) => Ok(()),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
                Err(e) => Err(mlua::Error::external(e)),
            }
        });
        methods.add_meta_method(MetaMethod::ToString, |_, this, ()| {
            Ok(format!("snapshot({})", this.path.display()))
        });
    }
}
