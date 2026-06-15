//! Lua-facing wrapper for a whole-lab snapshot directory.
//!
//! Lab fixtures (`*.fixture.lua` files that build multi-VM topology
//! + bridges) return one of these.
//!
//! The fixture-cache stores the directory contents under
//! `<key>.lab/`; restore reads `lab.json` and feeds it to
//! [`crate::lab::Lab::lab_restore`].

use std::path::PathBuf;

use mlua::{MetaMethod, UserData, UserDataMethods};

/// Lua-facing handle wrapping the snapshot directory path.
#[derive(Clone, Debug)]
pub(crate) struct LabSnapshotUd {
    pub(crate) dir: PathBuf,
}

impl UserData for LabSnapshotUd {
    fn add_methods<M: UserDataMethods<Self>>(methods: &mut M) {
        methods.add_method("path", |_, this, ()| {
            Ok(this.dir.display().to_string())
        });
        methods.add_method("size", |_, this, ()| {
            let mut total: u64 = 0;
            if let Ok(rd) = std::fs::read_dir(&this.dir) {
                for entry in rd.flatten() {
                    if let Ok(m) = entry.metadata() {
                        total = total.saturating_add(m.len());
                    }
                }
            }
            Ok(total)
        });
        methods.add_method("delete", |_, this, ()| {
            match std::fs::remove_dir_all(&this.dir) {
                Ok(()) => Ok(()),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
                Err(e) => Err(mlua::Error::external(e)),
            }
        });
        methods.add_meta_method(MetaMethod::ToString, |_, this, ()| {
            Ok(format!("lab_snapshot({})", this.dir.display()))
        });
    }
}
