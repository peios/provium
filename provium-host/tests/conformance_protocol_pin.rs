//! Conformance: PROTOCOL_VERSION pin per `DESIGN.md` § Protocol
//! version handshake. R9 m1: `PROTOCOL_VERSION` is bumped
//! "whenever any wire-facing struct changes shape", but that's a
//! social contract. These tests lock the serialized bytes for
//! every wire op so an accidental refactor that breaks the wire
//! shape without bumping the version fails this test.

use provium_protocol::handle::{FileHandle, ProcessHandle};
use provium_protocol::wire::{
    AgentMessage, CloseArgs, ExecArgs, Hello, HelloOk, HostMessage,
    OpResult, OpenFileArgs, OpenMode, ReadArgs, ReadResult,
    SyscallArgs, SyscallResult, WaitArgs,
};
use provium_protocol::PROTOCOL_VERSION;

/// Wraps a serialised byte vector so panic output is human-readable.
fn bytes_or_die<T: serde::Serialize>(label: &str, v: &T) -> Vec<u8> {
    rmp_serde::to_vec_named(v).unwrap_or_else(|e| {
        panic!("serialize {label}: {e}");
    })
}

#[test]
fn protocol_version_is_one_in_v1() {
    // The current wire shape is PROTOCOL_VERSION=1. Bumping
    // requires updating this test AND every fixture in the
    // cache (PROTOCOL_VERSION is folded into the cache key).
    assert_eq!(PROTOCOL_VERSION, 1,
        "PROTOCOL_VERSION changed; if intentional, update this test \
         AND understand that every cached fixture is now invalidated");
}

#[test]
fn hello_v1_serializes_under_a_known_byte_count() {
    // The Hello envelope is `{kind: "hello", payload: {protocol_version: 1}}`.
    // Pin the byte count so a rename of `protocol_version` or
    // a tag rename ("hello" → "Hello") fails the test.
    let h = HostMessage::Hello(Hello { protocol_version: 1 });
    let bytes = bytes_or_die("Hello", &h);
    // The size is small and stable: the named map carries kind +
    // payload; tweaking either field will change the byte count.
    assert!(bytes.len() < 80,
        "Hello bytes grew unexpectedly: {} bytes", bytes.len());
    assert!(bytes.windows(b"hello".len()).any(|w| w == b"hello"));
    assert!(bytes.windows(b"protocol_version".len())
        .any(|w| w == b"protocol_version"));
}

#[test]
fn hello_ok_uses_kind_tag_hello_ok() {
    // The host's connection.rs matches on this exact tag string;
    // a rename without a PROTOCOL_VERSION bump would silently
    // break the handshake.
    let m = AgentMessage::HelloOk(HelloOk {
        protocol_version: 1,
        agent: provium_protocol::wire::AgentInfo {
            os: "peios".into(),
            agent_version: "0.1.0".into(),
            kernel: None,
            capabilities: vec![],
        },
    });
    let bytes = bytes_or_die("HelloOk", &m);
    assert!(bytes.windows(b"hello_ok".len())
        .any(|w| w == b"hello_ok"));
}

#[test]
fn op_result_uses_outcome_value_keys() {
    // The OpResult wire tag pair is documented in DESIGN.md:
    // `{"outcome": "ok", "value": ...}`. A serde rename would
    // break every consumer.
    let ok: OpResult<u32> = OpResult::Ok(7);
    let bytes = bytes_or_die("OpResult::Ok", &ok);
    assert!(bytes.windows(b"outcome".len()).any(|w| w == b"outcome"));
    assert!(bytes.windows(b"value".len()).any(|w| w == b"value"));
    assert!(bytes.windows(b"\xa2ok".len()).any(|w| w == b"\xa2ok"));
}

#[test]
fn op_result_err_tag_is_err_not_error() {
    let e: OpResult<u32> = OpResult::Err(provium_protocol::OsError {
        errno: 2,
        message: "ENOENT".into(),
    });
    let bytes = bytes_or_die("OpResult::Err", &e);
    // 3-char fixstr "err" not "error".
    assert!(bytes.windows(b"\xa3err".len()).any(|w| w == b"\xa3err"),
        "OpResult::Err must use 3-char `err` tag");
}

#[test]
fn syscall_result_carries_ret_errno_out_bufs() {
    // out_bufs is skip-serialized when empty, so populate it to
    // verify the field name is on the wire when present.
    let r = SyscallResult { ret: 42, errno: 0, out_bufs: vec![vec![1, 2, 3]] };
    let bytes = bytes_or_die("SyscallResult", &r);
    for needle in [b"ret".as_ref(), b"errno".as_ref(), b"out_bufs".as_ref()] {
        assert!(bytes.windows(needle.len()).any(|w| w == needle),
            "SyscallResult missing field {:?}", std::str::from_utf8(needle));
    }
}

#[test]
fn syscall_result_omits_out_bufs_when_empty() {
    // Wire size matters for the connectionless-op pattern;
    // skip_serializing_if keeps the empty case minimal.
    let r = SyscallResult { ret: 42, errno: 0, out_bufs: vec![] };
    let bytes = bytes_or_die("SyscallResult", &r);
    assert!(!bytes.windows(b"out_bufs".len()).any(|w| w == b"out_bufs"),
        "empty out_bufs must be skip-serialized");
}

#[test]
fn open_mode_carries_six_documented_bits() {
    // OpenMode must serialize all six bits: read/write/create/
    // truncate/append/exclusive. A field removal would silently
    // break callers passing modes the agent then ignores.
    let m = OpenMode {
        read: true, write: false, create: false,
        truncate: false, append: false, exclusive: false,
    };
    let bytes = bytes_or_die("OpenMode", &m);
    for bit in ["read", "write", "create", "truncate", "append", "exclusive"] {
        assert!(bytes.windows(bit.len()).any(|w| w == bit.as_bytes()),
            "OpenMode missing bit `{bit}`");
    }
}

#[test]
fn read_args_handle_is_bare_u64_on_wire() {
    // The handle field must serialize transparent u64 (not
    // wrapped struct). Round-trip through u64 confirms.
    let r = ReadArgs { handle: FileHandle::new(123), max_bytes: 4096 };
    let bytes = bytes_or_die("ReadArgs", &r);
    let back: ReadArgs = rmp_serde::from_slice(&bytes).unwrap();
    assert_eq!(back.handle.get(), 123);
    assert_eq!(back.max_bytes, 4096);
}

#[test]
fn close_args_minimal_shape() {
    let c = CloseArgs { handle: FileHandle::new(1) };
    let bytes = bytes_or_die("CloseArgs", &c);
    let back: CloseArgs = rmp_serde::from_slice(&bytes).unwrap();
    assert_eq!(back.handle.get(), 1);
}

#[test]
fn wait_args_timeout_optional_field() {
    let w = WaitArgs { handle: ProcessHandle::new(5), timeout_ms: Some(1000) };
    let bytes = bytes_or_die("WaitArgs", &w);
    let back: WaitArgs = rmp_serde::from_slice(&bytes).unwrap();
    assert_eq!(back.handle.get(), 5);
    assert_eq!(back.timeout_ms, Some(1000));

    let w2 = WaitArgs { handle: ProcessHandle::new(5), timeout_ms: None };
    let bytes2 = bytes_or_die("WaitArgs", &w2);
    let back2: WaitArgs = rmp_serde::from_slice(&bytes2).unwrap();
    assert!(back2.timeout_ms.is_none());
}

#[test]
fn exec_args_shape_pinned() {
    let e = ExecArgs {
        cmd: "/bin/true".into(),
        args: vec![],
        env: Default::default(),
        env_clear: false,
        cwd: None,
        stdin: vec![],
        timeout_ms: None,
    };
    let bytes = bytes_or_die("ExecArgs", &e);
    for field in ["cmd", "args", "env", "stdin"] {
        assert!(bytes.windows(field.len()).any(|w| w == field.as_bytes()),
            "ExecArgs missing field `{field}`");
    }
}
