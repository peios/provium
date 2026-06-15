//! Layer-1 file ops: open, read, write, close, plus the composite
//! `read_file` / `write_file` and the metadata op `stat`.
//!
//! These are portable in concept across agent ports — each port maps
//! the abstract [`OpenMode`] flags to its native open() flags, etc. The
//! v1 Peios port uses Linux-ABI flags directly.

use serde::{Deserialize, Serialize};

use crate::handle::FileHandle;

use super::OpResult;

// ---------------------------------------------------------------------------
// open_file
// ---------------------------------------------------------------------------

/// Arguments for the `OpenFile` op.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct OpenFileArgs {
    /// Filesystem path. UTF-8; non-UTF-8 paths are not supported in v1.
    pub path: String,

    /// Access mode flags.
    pub mode: OpenMode,

    /// Permission bits used when the file is created
    /// ([`OpenMode::create`] = `true`). Defaults to `0o644` if `None`.
    /// Ignored otherwise.
    #[serde(default)]
    pub create_perm: Option<u32>,
}

/// Access-mode flags for [`OpenFileArgs`]. Mirrors POSIX `open(2)`
/// without committing to its bitfield encoding — the agent translates.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct OpenMode {
    /// Open for reading.
    #[serde(default)]
    pub read: bool,
    /// Open for writing.
    #[serde(default)]
    pub write: bool,
    /// Create the file if it doesn't exist.
    #[serde(default)]
    pub create: bool,
    /// Truncate to zero length on open.
    #[serde(default)]
    pub truncate: bool,
    /// Position writes at end-of-file.
    #[serde(default)]
    pub append: bool,
    /// Fail if [`Self::create`] is set and the file already exists.
    #[serde(default)]
    pub exclusive: bool,
}

/// Successful response payload for [`OpenFileArgs`]: the handle of the
/// newly-opened file.
pub type OpenFileResult = OpResult<FileHandle>;

// ---------------------------------------------------------------------------
// read
// ---------------------------------------------------------------------------

/// Arguments for the `Read` op.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReadArgs {
    /// Handle returned by a previous [`OpenFileArgs`].
    pub handle: FileHandle,
    /// Maximum bytes to read. The agent may return fewer; an empty
    /// returned buffer indicates EOF.
    pub max_bytes: u64,
}

/// Successful response: the bytes read. An empty `Vec` means EOF.
pub type ReadResult = OpResult<ReadOk>;

/// Successful-payload of [`ReadResult`] — separate struct so we can grow
/// it with extra fields (e.g. `eof: bool`) without breaking the wire.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReadOk {
    /// Bytes read. Empty implies EOF.
    #[serde(with = "serde_bytes")]
    pub data: Vec<u8>,
}

// ---------------------------------------------------------------------------
// write
// ---------------------------------------------------------------------------

/// Arguments for the `Write` op.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WriteArgs {
    /// Handle returned by a previous [`OpenFileArgs`].
    pub handle: FileHandle,
    /// Bytes to write.
    #[serde(with = "serde_bytes")]
    pub data: Vec<u8>,
}

/// Successful response: bytes actually written. May be less than the
/// input length on the agent's side (short write); host-side helpers
/// loop until the full payload lands or an error is returned.
pub type WriteResult = OpResult<WriteOk>;

/// Successful-payload of [`WriteResult`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WriteOk {
    /// Bytes successfully written.
    pub written: u64,
}

// ---------------------------------------------------------------------------
// close
// ---------------------------------------------------------------------------

/// Arguments for the `Close` op.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CloseArgs {
    /// Handle to close.
    pub handle: FileHandle,
}

/// Successful response carries no payload.
pub type CloseResult = OpResult<()>;

// ---------------------------------------------------------------------------
// read_file (composite)
// ---------------------------------------------------------------------------

/// Arguments for the `ReadFile` composite op — open + read-to-end +
/// close in one round-trip. Subject to the codec's frame size cap; for
/// large files use [`OpenFileArgs`] + chunked [`ReadArgs`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReadFileArgs {
    /// Filesystem path.
    pub path: String,
}

/// Successful response: the entire file contents.
pub type ReadFileResult = OpResult<ReadOk>;

// ---------------------------------------------------------------------------
// write_file (composite)
// ---------------------------------------------------------------------------

/// Arguments for the `WriteFile` composite op — atomic create-or-replace
/// (or append) of a file's contents.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WriteFileArgs {
    /// Filesystem path.
    pub path: String,
    /// Bytes to write.
    #[serde(with = "serde_bytes")]
    pub data: Vec<u8>,
    /// How an existing file at [`Self::path`] is treated.
    #[serde(default)]
    pub mode: WriteFileMode,
    /// Permission bits used when the file is being created. Defaults
    /// to `0o644`. Ignored if the file already exists.
    #[serde(default)]
    pub create_perm: Option<u32>,
}

/// Disposition for an existing file at the target path.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WriteFileMode {
    /// Truncate-or-create. The default — replaces an existing file's
    /// contents, creates it if absent.
    #[default]
    Replace,
    /// Append to existing file, or create if absent.
    Append,
    /// Create exclusively; fails with `EEXIST` if the file exists.
    Exclusive,
}

/// Successful response carries no payload.
pub type WriteFileResult = OpResult<()>;

// ---------------------------------------------------------------------------
// listdir / mkdir / unlink / rename / seek
// ---------------------------------------------------------------------------

/// Arguments for `Listdir`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ListdirArgs {
    /// Directory path.
    pub path: String,
}

/// One entry in a [`ListdirResult::Ok`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DirEntry {
    /// File / directory name (no parent path).
    pub name: String,
    /// Filesystem entry kind.
    pub entry_type: EntryType,
}

/// `Listdir` payload.
pub type ListdirResult = OpResult<Vec<DirEntry>>;

/// Arguments for `Mkdir`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MkdirArgs {
    /// Path of the new directory.
    pub path: String,
    /// Create intermediate components.
    #[serde(default)]
    pub parents: bool,
    /// POSIX mode bits when the directory is created. Default `0o755`.
    #[serde(default)]
    pub create_perm: Option<u32>,
}

/// `Mkdir` payload.
pub type MkdirResult = OpResult<()>;

/// Arguments for `Unlink`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct UnlinkArgs {
    /// Path to remove.
    pub path: String,
}

/// `Unlink` payload.
pub type UnlinkResult = OpResult<()>;

/// Arguments for `Rename`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RenameArgs {
    /// Source path.
    pub from: String,
    /// Destination path.
    pub to: String,
}

/// `Rename` payload.
pub type RenameResult = OpResult<()>;

/// Arguments for `Seek`. Returns the new file offset on success.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SeekArgs {
    /// Open-file handle.
    pub handle: crate::handle::FileHandle,
    /// Offset relative to `whence`.
    pub offset: i64,
    /// `Set` (start), `Cur` (current), `End` (end-of-file).
    pub whence: SeekWhence,
}

/// Whence values for [`SeekArgs`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SeekWhence {
    /// From the beginning of the file.
    Set,
    /// From the current position.
    Cur,
    /// From end-of-file.
    End,
}

/// `Seek` payload — new absolute offset on success.
pub type SeekResult = OpResult<u64>;

// ---------------------------------------------------------------------------
// stat
// ---------------------------------------------------------------------------

/// Arguments for the `Stat` op.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StatArgs {
    /// Filesystem path.
    pub path: String,
    /// If `true`, follow symlinks (POSIX `stat`); if `false`, return
    /// the symlink's own metadata (POSIX `lstat`). Defaults to `true`.
    #[serde(default = "default_follow_symlinks")]
    pub follow_symlinks: bool,
}

const fn default_follow_symlinks() -> bool {
    true
}

/// Successful response payload: the file's metadata.
pub type StatResult = OpResult<FileMetadata>;

/// Filesystem-entry metadata. Mirrors `vm:stat(path) -> {...}` in the
/// Lua API.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileMetadata {
    /// File size in bytes.
    pub size: u64,
    /// Modification time in nanoseconds since the Unix epoch.
    pub mtime_ns: i64,
    /// Filesystem entry kind.
    pub entry_type: EntryType,
    /// POSIX permission bits.
    pub perm: u32,
}

/// Filesystem entry classification.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EntryType {
    /// Regular file.
    File,
    /// Directory.
    Directory,
    /// Symbolic link (only seen with `follow_symlinks = false`).
    Symlink,
    /// FIFO / named pipe.
    Fifo,
    /// Unix-domain socket.
    Socket,
    /// Block device.
    BlockDevice,
    /// Character device.
    CharDevice,
    /// Anything not covered above.
    Other,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::OsError;

    fn round_trip<T>(value: &T) -> T
    where
        T: serde::Serialize + serde::de::DeserializeOwned,
    {
        let bytes = rmp_serde::to_vec_named(value).unwrap();
        rmp_serde::from_slice(&bytes).unwrap()
    }

    #[test]
    fn open_file_round_trips() {
        let args = OpenFileArgs {
            path: "/etc/hostname".into(),
            mode: OpenMode {
                read: true,
                ..Default::default()
            },
            create_perm: None,
        };
        assert_eq!(args, round_trip(&args));

        let ok: OpenFileResult = OpResult::Ok(FileHandle::new(7));
        assert_eq!(ok, round_trip(&ok));

        let err: OpenFileResult = OpResult::Err(OsError::from_errno(2));
        assert_eq!(err, round_trip(&err));
    }

    #[test]
    fn open_mode_default_is_all_false() {
        let m = OpenMode::default();
        assert!(!m.read && !m.write && !m.create);
    }

    #[test]
    fn read_args_round_trip() {
        let args = ReadArgs {
            handle: FileHandle::new(3),
            max_bytes: 4096,
        };
        assert_eq!(args, round_trip(&args));
    }

    #[test]
    fn read_result_round_trips() {
        let r: ReadResult = OpResult::Ok(ReadOk {
            data: b"hello".to_vec(),
        });
        assert_eq!(r, round_trip(&r));
    }

    #[test]
    fn write_args_round_trip() {
        let args = WriteArgs {
            handle: FileHandle::new(3),
            data: vec![1, 2, 3],
        };
        assert_eq!(args, round_trip(&args));
    }

    #[test]
    fn close_args_round_trip() {
        let args = CloseArgs {
            handle: FileHandle::new(99),
        };
        assert_eq!(args, round_trip(&args));
    }

    #[test]
    fn read_file_round_trips() {
        let args = ReadFileArgs {
            path: "/etc/passwd".into(),
        };
        assert_eq!(args, round_trip(&args));
    }

    #[test]
    fn write_file_args_round_trip() {
        for mode in [
            WriteFileMode::Replace,
            WriteFileMode::Append,
            WriteFileMode::Exclusive,
        ] {
            let args = WriteFileArgs {
                path: "/tmp/x".into(),
                data: b"contents".to_vec(),
                mode,
                create_perm: Some(0o600),
            };
            assert_eq!(args, round_trip(&args));
        }
    }

    #[test]
    fn write_file_mode_default_is_replace() {
        assert_eq!(WriteFileMode::default(), WriteFileMode::Replace);
    }

    #[test]
    fn stat_round_trips() {
        let args = StatArgs {
            path: "/tmp/x".into(),
            follow_symlinks: true,
        };
        assert_eq!(args, round_trip(&args));

        let r: StatResult = OpResult::Ok(FileMetadata {
            size: 1024,
            mtime_ns: 1_700_000_000_000_000_000,
            entry_type: EntryType::File,
            perm: 0o644,
        });
        assert_eq!(r, round_trip(&r));
    }

    #[test]
    fn stat_follows_symlinks_by_default_when_field_missing() {
        // Wire from a peer that omits the new field should default to true.
        // We synthesise that case by serializing a minimal map.
        let bytes = rmp_serde::to_vec_named(&serde_json::json!({
            "path": "/tmp/x",
        })).unwrap();
        let decoded: StatArgs = rmp_serde::from_slice(&bytes).unwrap();
        assert!(decoded.follow_symlinks);
    }
}
