//! Conformance: ClientError variants per
//! `provium_host::error::ClientError`. Each variant has a
//! distinct Display + variant kind so call sites can match
//! and tooling can show actionable messages.

use std::io;

use provium_host::ClientError;
use provium_protocol::wire::AgentError;
use provium_protocol::{FrameError, OsError};

#[test]
fn connect_error_includes_io_message() {
    let e = ClientError::Connect(io::Error::other("connection refused"));
    let s = e.to_string();
    assert!(s.contains("connect"), "Display must include 'connect': {s}");
    assert!(s.contains("connection refused"),
        "Display must include underlying io: {s}");
}

#[test]
fn frame_error_propagates_from_protocol() {
    // FrameError::Io variant.
    let inner = FrameError::Io(io::Error::other("broken pipe"));
    let e: ClientError = inner.into();
    let s = e.to_string();
    assert!(s.contains("frame"), "Display must include 'frame': {s}");
    assert!(matches!(e, ClientError::Frame(_)));
}

#[test]
fn version_mismatch_names_both_sides() {
    let e = ClientError::VersionMismatch { host: 5, agent: 3 };
    let s = e.to_string();
    assert!(s.contains("host: 5"),
        "Display must name host version: {s}");
    assert!(s.contains("agent: 3"),
        "Display must name agent version: {s}");
}

#[test]
fn unexpected_message_names_both_kinds() {
    let e = ClientError::UnexpectedMessage {
        expected: "worker_syscall_result",
        actual: "syscall_result",
    };
    let s = e.to_string();
    assert!(s.contains("worker_syscall_result"));
    assert!(s.contains("syscall_result"));
}

#[test]
fn agent_error_wraps_envelope_error() {
    let inner = AgentError {
        kind: provium_protocol::wire::AgentErrorKind::UnknownHandle,
        message: "no such handle".into(),
    };
    let e = ClientError::Agent(inner);
    let s = e.to_string();
    assert!(s.contains("agent error"),
        "Display must include 'agent error': {s}");
}

#[test]
fn stream_source_carries_os_error() {
    let inner = OsError { errno: 5, message: "EIO".into() };
    let e = ClientError::StreamSource(inner);
    let s = e.to_string();
    assert!(s.contains("stream source"),
        "Display must include 'stream source': {s}");
    assert!(s.contains("EIO"), "Display must include errno message: {s}");
}

#[test]
fn variants_are_send_and_sync() {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<ClientError>();
}
