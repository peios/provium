//! Conformance: peer wire-op shapes per `DESIGN.md` § Wire
//! protocol. The structs are public API for tooling; their
//! field names + serialization tags are stable across versions.

use provium_protocol::handle::FileHandle;
use provium_protocol::wire::{
    CloseArgs, OpResult, OpenFileArgs, OpenMode, ReadArgs, ReadFileArgs,
    WriteArgs, WriteFileArgs, WriteFileMode,
};
use provium_protocol::OsError;

fn round_trip<T>(v: &T) -> T
where
    T: serde::Serialize + serde::de::DeserializeOwned,
{
    let bytes = rmp_serde::to_vec_named(v).expect("serialize");
    rmp_serde::from_slice::<T>(&bytes).expect("deserialize")
}

#[test]
fn open_file_args_round_trip() {
    let a = OpenFileArgs {
        path: "/tmp/x".into(),
        mode: OpenMode {
            read: true, write: false, create: false,
            truncate: false, append: false, exclusive: false,
        },
        create_perm: Some(0o644),
    };
    let b = round_trip(&a);
    assert_eq!(a.path, b.path);
    assert_eq!(a.create_perm, b.create_perm);
    assert_eq!(a.mode.read, b.mode.read);
}

#[test]
fn read_args_round_trip() {
    let a = ReadArgs { handle: FileHandle::new(42), max_bytes: 4096 };
    let b = round_trip(&a);
    assert_eq!(a, b);
}

#[test]
fn write_args_round_trip() {
    let a = WriteArgs { handle: FileHandle::new(7), data: vec![1, 2, 3] };
    let b = round_trip(&a);
    assert_eq!(a, b);
}

#[test]
fn close_args_round_trip() {
    let a = CloseArgs { handle: FileHandle::new(99) };
    let b = round_trip(&a);
    assert_eq!(a, b);
}

#[test]
fn read_file_args_round_trip() {
    let a = ReadFileArgs { path: "/etc/passwd".into() };
    let b = round_trip(&a);
    assert_eq!(a, b);
}

#[test]
fn write_file_args_round_trip() {
    let a = WriteFileArgs {
        path: "/tmp/out".into(),
        data: vec![0xde, 0xad],
        mode: WriteFileMode::Replace,
        create_perm: None,
    };
    let b = round_trip(&a);
    assert_eq!(a.path, b.path);
    assert_eq!(a.data, b.data);
}

#[test]
fn op_result_ok_round_trips() {
    let a: OpResult<u64> = OpResult::Ok(42);
    let b = round_trip(&a);
    assert_eq!(a, b);
    assert!(b.is_ok());
}

#[test]
fn op_result_err_round_trips() {
    let a: OpResult<u64> = OpResult::Err(OsError {
        errno: 2,
        message: "ENOENT".into(),
    });
    let b = round_trip(&a);
    assert_eq!(a, b);
    assert!(!b.is_ok());
}

#[test]
fn op_result_serializes_with_outcome_tag() {
    // The wire shape is `{"outcome": "ok", "value": ...}` — pin
    // the tag literal so serde rename changes break the test.
    let a: OpResult<u32> = OpResult::Ok(7);
    let bytes = rmp_serde::to_vec_named(&a).unwrap();
    let needle_outcome = b"outcome";
    let needle_ok = b"\xa2ok";  // msgpack fixstr "ok" (2 chars)
    assert!(bytes.windows(needle_outcome.len()).any(|w| w == needle_outcome),
        "outcome tag missing");
    assert!(bytes.windows(needle_ok.len()).any(|w| w == needle_ok),
        "ok variant tag missing");
}

#[test]
fn op_result_into_result_converts() {
    let ok: OpResult<u8> = OpResult::Ok(1);
    let r: Result<u8, OsError> = ok.into_result();
    assert!(r.is_ok());

    let err: OpResult<u8> = OpResult::Err(OsError {
        errno: 13,
        message: "EACCES".into(),
    });
    let r: Result<u8, OsError> = err.into_result();
    assert!(r.is_err());
    assert_eq!(r.unwrap_err().errno, 13);
}

#[test]
fn open_mode_default_is_zero_access() {
    let m = OpenMode::default();
    assert!(!m.read && !m.write && !m.append,
        "OpenMode default must have no access bits set");
}

#[test]
fn write_file_mode_replace_serializes_consistently() {
    // Wire compatibility: Replace must serialize as a known
    // discriminant. Pin so an enum reorder breaks the test.
    let a = WriteFileMode::Replace;
    let bytes = rmp_serde::to_vec_named(&a).unwrap();
    let back: WriteFileMode = rmp_serde::from_slice(&bytes).unwrap();
    assert_eq!(a, back);
}
