//! File-family op handlers: open, read, write, close, plus the
//! composite `read_file` / `write_file` and `stat`.

use std::fs::{self, File, Metadata, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::Path;
use std::sync::Arc;

use provium_protocol::wire::{
    AgentError, AgentErrorKind, AgentMessage, CloseArgs, DirEntry, EntryType, FileMetadata,
    ListdirArgs, MkdirArgs, OpResult, OpenFileArgs, OpenMode, ReadArgs, ReadFileArgs, ReadOk,
    RenameArgs, SeekArgs, SeekWhence, StatArgs, UnlinkArgs, WriteArgs, WriteFileArgs,
    WriteFileMode, WriteOk,
};

use crate::state::AgentState;

use super::os_error_from_io;

const DEFAULT_CREATE_PERM: u32 = 0o644;

/// `OpenFile` — open a path against the agent's open-file table.
pub fn open_file(args: OpenFileArgs, state: &Arc<AgentState>) -> AgentMessage {
    let mut opts = OpenOptions::new();
    apply_open_mode(&mut opts, &args.mode);
    if args.mode.create {
        opts.mode(args.create_perm.unwrap_or(DEFAULT_CREATE_PERM));
    }

    let result = match opts.open(Path::new(&args.path)) {
        Ok(file) => {
            let handle = state.insert_file(file);
            OpResult::Ok(handle)
        }
        Err(e) => OpResult::Err(os_error_from_io(e)),
    };
    AgentMessage::OpenFileResult(result)
}

fn apply_open_mode(opts: &mut OpenOptions, mode: &OpenMode) {
    opts.read(mode.read);
    opts.write(mode.write);
    opts.create(mode.create);
    opts.truncate(mode.truncate);
    opts.append(mode.append);
    opts.create_new(mode.create && mode.exclusive);
}

/// `Read` — pull up to `max_bytes` from an open file.
pub fn read(args: ReadArgs, state: &Arc<AgentState>) -> AgentMessage {
    // Cap at 4 MiB per call — the host can loop for larger reads, and
    // the cap keeps the response frame within the codec's default
    // size budget. Composite `read_file` is sized for full files.
    const PER_CALL_CAP: u64 = 4 * 1024 * 1024;
    let to_read = args.max_bytes.min(PER_CALL_CAP) as usize;

    let outcome = state.with_file_mut(args.handle, |file| {
        let mut buf = vec![0u8; to_read];
        match file.read(&mut buf) {
            Ok(n) => {
                buf.truncate(n);
                OpResult::Ok(ReadOk { data: buf })
            }
            Err(e) => OpResult::Err(os_error_from_io(e)),
        }
    });

    match outcome {
        Some(result) => AgentMessage::ReadResult(result),
        None => unknown_handle(format!("read on {}", args.handle)),
    }
}

/// `Write` — push bytes into an open file.
pub fn write(args: WriteArgs, state: &Arc<AgentState>) -> AgentMessage {
    let outcome = state.with_file_mut(args.handle, |file| match file.write(&args.data) {
        Ok(n) => OpResult::Ok(WriteOk { written: n as u64 }),
        Err(e) => OpResult::Err(os_error_from_io(e)),
    });

    match outcome {
        Some(result) => AgentMessage::WriteResult(result),
        None => unknown_handle(format!("write on {}", args.handle)),
    }
}

/// `Close` — drop the file from the open-file table.
pub fn close(args: CloseArgs, state: &Arc<AgentState>) -> AgentMessage {
    match state.take_file(args.handle) {
        Some(_) => AgentMessage::CloseResult(OpResult::Ok(())),
        None => unknown_handle(format!("close on {}", args.handle)),
    }
}

/// `ReadFile` — open + read-to-end + close in one shot.
pub fn read_file(args: ReadFileArgs) -> AgentMessage {
    let result = match File::open(Path::new(&args.path)) {
        Ok(mut file) => {
            let mut buf = Vec::new();
            match file.read_to_end(&mut buf) {
                Ok(_) => OpResult::Ok(ReadOk { data: buf }),
                Err(e) => OpResult::Err(os_error_from_io(e)),
            }
        }
        Err(e) => OpResult::Err(os_error_from_io(e)),
    };
    AgentMessage::ReadFileResult(result)
}

/// `WriteFile` — atomic create-or-replace (or append) with a single
/// frame of data.
pub fn write_file(args: WriteFileArgs) -> AgentMessage {
    let mut opts = OpenOptions::new();
    opts.write(true);
    match args.mode {
        WriteFileMode::Replace => {
            opts.create(true).truncate(true);
        }
        WriteFileMode::Append => {
            opts.create(true).append(true);
        }
        WriteFileMode::Exclusive => {
            opts.create_new(true);
        }
    }
    opts.mode(args.create_perm.unwrap_or(DEFAULT_CREATE_PERM));

    let result = match opts.open(Path::new(&args.path)) {
        Ok(mut file) => match file.write_all(&args.data) {
            Ok(()) => OpResult::Ok(()),
            Err(e) => OpResult::Err(os_error_from_io(e)),
        },
        Err(e) => OpResult::Err(os_error_from_io(e)),
    };
    AgentMessage::WriteFileResult(result)
}

/// `Stat` — filesystem entry metadata for a path.
pub fn stat(args: StatArgs) -> AgentMessage {
    let metadata_result = if args.follow_symlinks {
        std::fs::metadata(Path::new(&args.path))
    } else {
        std::fs::symlink_metadata(Path::new(&args.path))
    };
    let result = match metadata_result {
        Ok(m) => OpResult::Ok(metadata_to_wire(&m)),
        Err(e) => OpResult::Err(os_error_from_io(e)),
    };
    AgentMessage::StatResult(result)
}

fn metadata_to_wire(m: &Metadata) -> FileMetadata {
    FileMetadata {
        size: m.len(),
        mtime_ns: mtime_ns(m),
        entry_type: classify(m),
        perm: m.permissions().mode() & 0o7777,
    }
}

fn mtime_ns(m: &Metadata) -> i64 {
    // MetadataExt gives seconds + nanoseconds separately; combine.
    // Saturating arithmetic: i64 nanoseconds covers ±292 years.
    let sec = m.mtime();
    let nsec = m.mtime_nsec();
    sec.saturating_mul(1_000_000_000).saturating_add(nsec)
}

fn classify(m: &Metadata) -> EntryType {
    use std::os::unix::fs::FileTypeExt;
    let ft = m.file_type();
    if ft.is_file() {
        EntryType::File
    } else if ft.is_dir() {
        EntryType::Directory
    } else if ft.is_symlink() {
        EntryType::Symlink
    } else if ft.is_fifo() {
        EntryType::Fifo
    } else if ft.is_socket() {
        EntryType::Socket
    } else if ft.is_block_device() {
        EntryType::BlockDevice
    } else if ft.is_char_device() {
        EntryType::CharDevice
    } else {
        EntryType::Other
    }
}

fn unknown_handle(detail: String) -> AgentMessage {
    AgentMessage::AgentError(AgentError {
        kind: AgentErrorKind::UnknownHandle,
        message: detail,
    })
}

// ---------------------------------------------------------------------------
// listdir / mkdir / unlink / rename / seek
// ---------------------------------------------------------------------------

/// `Listdir` — enumerate a directory.
pub fn listdir(args: ListdirArgs) -> AgentMessage {
    let result = match fs::read_dir(Path::new(&args.path)) {
        Ok(iter) => {
            let mut out = Vec::new();
            for entry in iter.flatten() {
                let name = entry.file_name().to_string_lossy().into_owned();
                let entry_type = entry
                    .file_type()
                    .ok()
                    .map(classify_file_type)
                    .unwrap_or(EntryType::Other);
                out.push(DirEntry { name, entry_type });
            }
            OpResult::Ok(out)
        }
        Err(e) => OpResult::Err(os_error_from_io(e)),
    };
    AgentMessage::ListdirResult(result)
}

/// `Mkdir` — create a directory (optionally recursive).
pub fn mkdir(args: MkdirArgs) -> AgentMessage {
    let path = Path::new(&args.path);
    let mode = args.create_perm.unwrap_or(0o755);
    let mut builder = fs::DirBuilder::new();
    builder.recursive(args.parents);
    builder.mode(mode);
    let result = match builder.create(path) {
        Ok(()) => OpResult::Ok(()),
        Err(e) => OpResult::Err(os_error_from_io(e)),
    };
    AgentMessage::MkdirResult(result)
}

/// `Unlink` — remove a file or empty directory.
pub fn unlink(args: UnlinkArgs) -> AgentMessage {
    let path = Path::new(&args.path);
    // Try file first, fall back to dir for symmetry with most unlink callers.
    let result = match fs::remove_file(path) {
        Ok(()) => OpResult::Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::IsADirectory => match fs::remove_dir(path) {
            Ok(()) => OpResult::Ok(()),
            Err(e) => OpResult::Err(os_error_from_io(e)),
        },
        Err(e) => OpResult::Err(os_error_from_io(e)),
    };
    AgentMessage::UnlinkResult(result)
}

/// `Rename` — atomic rename (no cross-device support beyond what
/// `rename(2)` natively offers).
pub fn rename(args: RenameArgs) -> AgentMessage {
    let result = match fs::rename(Path::new(&args.from), Path::new(&args.to)) {
        Ok(()) => OpResult::Ok(()),
        Err(e) => OpResult::Err(os_error_from_io(e)),
    };
    AgentMessage::RenameResult(result)
}

/// `Seek` — seek an open file, returning the new absolute offset.
pub fn seek(args: SeekArgs, state: &Arc<AgentState>) -> AgentMessage {
    let outcome = state.with_file_mut(args.handle, |file| {
        let whence = match args.whence {
            SeekWhence::Set => SeekFrom::Start(args.offset.max(0) as u64),
            SeekWhence::Cur => SeekFrom::Current(args.offset),
            SeekWhence::End => SeekFrom::End(args.offset),
        };
        match file.seek(whence) {
            Ok(o) => OpResult::Ok(o),
            Err(e) => OpResult::Err(os_error_from_io(e)),
        }
    });
    match outcome {
        Some(r) => AgentMessage::SeekResult(r),
        None => unknown_handle(format!("seek on {}", args.handle)),
    }
}

fn classify_file_type(t: std::fs::FileType) -> EntryType {
    use std::os::unix::fs::FileTypeExt;
    if t.is_file() {
        EntryType::File
    } else if t.is_dir() {
        EntryType::Directory
    } else if t.is_symlink() {
        EntryType::Symlink
    } else if t.is_fifo() {
        EntryType::Fifo
    } else if t.is_socket() {
        EntryType::Socket
    } else if t.is_block_device() {
        EntryType::BlockDevice
    } else if t.is_char_device() {
        EntryType::CharDevice
    } else {
        EntryType::Other
    }
}
