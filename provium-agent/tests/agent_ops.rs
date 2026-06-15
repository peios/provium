//! Integration tests for [`provium_agent::connection::handle_connection`].
//!
//! Each test creates a pair of [`UnixStream`]s (mocking the vsock
//! socket pair), spawns the agent's connection handler on one half,
//! and drives the other half manually. This lets us exercise the full
//! handshake + dispatch + result path without needing a vsock device
//! or a running QEMU.
//!
//! The protocol is connectionless for short ops — every op gets its
//! own handshake. [`run_op`] hides that behind a single function call;
//! [`TestAgent`] retains the state-bearing [`AgentState`] across the
//! sequence of connections that some tests need (e.g. open → read).

use std::io::Write as _;
use std::os::unix::net::UnixStream;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use provium_protocol::frame::{read_frame, write_frame, DEFAULT_MAX_FRAME_BYTES};
use provium_protocol::wire::{
    AgentMessage, CloseArgs, ExecArgs, ExitStatus, Hello, HostMessage, OpResult, OpenFileArgs,
    OpenMode, ReadArgs, ReadFileArgs, StatArgs, StreamEnd, TailFileArgs, TailStart, WriteArgs,
    WriteFileArgs, WriteFileMode,
};
use provium_protocol::PROTOCOL_VERSION;

use provium_agent::connection::{handle_connection, ConnectionOutcome};
use provium_agent::AgentState;

/// A test agent: an [`AgentState`] reused across multiple
/// connection-per-op invocations.
struct TestAgent {
    state: Arc<AgentState>,
}

impl TestAgent {
    fn new() -> Self {
        Self {
            state: Arc::new(AgentState::new()),
        }
    }

    /// Run one short op against a fresh connection. Returns the agent's
    /// final reply (after Hello+HelloOk).
    fn run_op(&self, op: HostMessage) -> AgentMessage {
        let (mut host, agent) = UnixStream::pair().expect("UnixStream::pair");
        let state = Arc::clone(&self.state);

        let mut agent_reader = agent.try_clone().expect("clone");
        let mut agent_writer = agent;

        let handler = thread::spawn(move || {
            handle_connection(&mut agent_reader, &mut agent_writer, state)
                .expect("handler clean exit")
        });

        complete_handshake(&mut host);
        send(&mut host, op);
        let resp = recv(&mut host);

        // Cleanly close so the handler thread can finish.
        drop(host);
        let outcome = handler.join().expect("handler thread");
        assert_eq!(outcome, ConnectionOutcome::Handled);
        resp
    }

    /// Open a streaming op — returns the host-side stream and the
    /// handler join handle. The caller drives the stream and drops
    /// `host` to terminate.
    fn run_stream_op(
        &self,
        op: HostMessage,
    ) -> (UnixStream, thread::JoinHandle<ConnectionOutcome>) {
        let (mut host, agent) = UnixStream::pair().expect("UnixStream::pair");
        let state = Arc::clone(&self.state);

        let mut agent_reader = agent.try_clone().expect("clone");
        let mut agent_writer = agent;

        let handler = thread::spawn(move || {
            handle_connection(&mut agent_reader, &mut agent_writer, state)
                .expect("handler clean exit")
        });

        complete_handshake(&mut host);
        send(&mut host, op);
        (host, handler)
    }
}

fn complete_handshake(host: &mut UnixStream) {
    write_frame(
        host,
        &HostMessage::Hello(Hello {
            protocol_version: PROTOCOL_VERSION,
        }),
        DEFAULT_MAX_FRAME_BYTES,
    )
    .expect("send Hello");

    let resp: AgentMessage = read_frame(host, DEFAULT_MAX_FRAME_BYTES).expect("read HelloOk");
    match resp {
        AgentMessage::HelloOk(ok) => {
            assert_eq!(ok.protocol_version, PROTOCOL_VERSION);
            assert!(!ok.agent.os.is_empty());
        }
        other => panic!("expected HelloOk, got {}", other.kind()),
    }
}

fn send(host: &mut UnixStream, msg: HostMessage) {
    write_frame(host, &msg, DEFAULT_MAX_FRAME_BYTES).expect("send");
}

fn recv(host: &mut UnixStream) -> AgentMessage {
    read_frame(host, DEFAULT_MAX_FRAME_BYTES).expect("recv")
}

// ---------------------------------------------------------------------------
// Handshake
// ---------------------------------------------------------------------------

#[test]
fn handshake_succeeds_on_matching_version() {
    let agent = TestAgent::new();
    let resp = agent.run_op(HostMessage::Stat(StatArgs {
        path: "/".into(),
        follow_symlinks: true,
    }));
    assert!(matches!(resp, AgentMessage::StatResult(_)));
}

#[test]
fn handshake_rejects_mismatched_version() {
    let (mut host, agent) = UnixStream::pair().unwrap();
    let state = Arc::new(AgentState::new());
    let mut reader = agent.try_clone().unwrap();
    let mut writer = agent;
    let handle = thread::spawn(move || handle_connection(&mut reader, &mut writer, state).unwrap());

    write_frame(
        &mut host,
        &HostMessage::Hello(Hello {
            protocol_version: PROTOCOL_VERSION.wrapping_add(99),
        }),
        DEFAULT_MAX_FRAME_BYTES,
    )
    .expect("send Hello");

    let resp = recv(&mut host);
    match resp {
        AgentMessage::HelloErr(err) => {
            assert_eq!(err.host_version, PROTOCOL_VERSION.wrapping_add(99));
            assert_eq!(err.agent_version, PROTOCOL_VERSION);
        }
        other => panic!("expected HelloErr, got {}", other.kind()),
    }

    drop(host);
    let outcome = handle.join().expect("handler thread");
    match outcome {
        ConnectionOutcome::VersionRejected { host_version } => {
            assert_eq!(host_version, PROTOCOL_VERSION.wrapping_add(99));
        }
        other => panic!("expected VersionRejected, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// exec
// ---------------------------------------------------------------------------

fn exec_args(cmd: &str, args: Vec<&str>) -> ExecArgs {
    ExecArgs {
        cmd: cmd.into(),
        args: args.into_iter().map(String::from).collect(),
        env: Default::default(),
        env_clear: false,
        stdin: vec![],
        cwd: None,
        timeout_ms: None,
    }
}

#[test]
fn exec_captures_stdout_and_exit_code_zero() {
    let agent = TestAgent::new();
    let resp = agent.run_op(HostMessage::Exec(exec_args(
        "sh",
        vec!["-c", "printf 'hello\\nworld\\n'"],
    )));
    match resp {
        AgentMessage::ExecResult(provium_protocol::wire::ExecResult::Ok(ok)) => {
            assert_eq!(ok.status, ExitStatus::Exited(0));
            assert_eq!(&ok.stdout, b"hello\nworld\n");
            assert!(ok.stderr.is_empty());
        }
        other => panic!("unexpected: {other:?}"),
    }
}

#[test]
fn exec_passes_stdin_to_child() {
    let agent = TestAgent::new();
    let resp = agent.run_op(HostMessage::Exec(ExecArgs {
        cmd: "cat".into(),
        args: vec![],
        env: Default::default(),
        env_clear: false,
        stdin: b"piped input\n".to_vec(),
        cwd: None,
        timeout_ms: Some(5_000),
    }));
    match resp {
        AgentMessage::ExecResult(provium_protocol::wire::ExecResult::Ok(ok)) => {
            assert_eq!(ok.status, ExitStatus::Exited(0));
            assert_eq!(&ok.stdout, b"piped input\n");
        }
        other => panic!("unexpected: {other:?}"),
    }
}

#[test]
fn exec_propagates_environment_overrides() {
    let agent = TestAgent::new();
    let mut env = std::collections::BTreeMap::new();
    env.insert("PROVIUM_TEST_VAR".into(), "abc123".into());
    let resp = agent.run_op(HostMessage::Exec(ExecArgs {
        cmd: "sh".into(),
        args: vec!["-c".into(), "echo $PROVIUM_TEST_VAR".into()],
        env,
        env_clear: false,
        stdin: vec![],
        cwd: None,
        timeout_ms: None,
    }));
    match resp {
        AgentMessage::ExecResult(provium_protocol::wire::ExecResult::Ok(ok)) => {
            assert_eq!(&ok.stdout, b"abc123\n");
        }
        other => panic!("unexpected: {other:?}"),
    }
}

#[test]
fn exec_returns_oserror_when_binary_does_not_exist() {
    let agent = TestAgent::new();
    let resp = agent.run_op(HostMessage::Exec(exec_args("/nonexistent/binary", vec![])));
    match resp {
        AgentMessage::ExecResult(provium_protocol::wire::ExecResult::Err(e)) => {
            assert_eq!(e.errno, 2); // ENOENT
        }
        other => panic!("unexpected: {other:?}"),
    }
}

#[test]
fn exec_kills_runaway_child_at_timeout() {
    let agent = TestAgent::new();
    let started = Instant::now();
    let resp = agent.run_op(HostMessage::Exec(ExecArgs {
        cmd: "sh".into(),
        args: vec!["-c".into(), "sleep 30".into()],
        env: Default::default(),
        env_clear: false,
        stdin: vec![],
        cwd: None,
        timeout_ms: Some(150),
    }));
    let elapsed = started.elapsed();
    assert!(
        elapsed < Duration::from_secs(3),
        "should have killed quickly, took {elapsed:?}"
    );
    match resp {
        AgentMessage::ExecResult(provium_protocol::wire::ExecResult::Ok(ok)) => {
            assert_eq!(ok.status, ExitStatus::TimedOut);
        }
        other => panic!("unexpected: {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// open / read / write / close — exercises state across connections
// ---------------------------------------------------------------------------

#[test]
fn open_persists_handle_into_subsequent_read() {
    let tmp = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(tmp.path(), b"hello agent").unwrap();

    let agent = TestAgent::new();
    let resp = agent.run_op(HostMessage::OpenFile(OpenFileArgs {
        path: tmp.path().to_string_lossy().into(),
        mode: OpenMode {
            read: true,
            ..Default::default()
        },
        create_perm: None,
    }));
    let handle = match resp {
        AgentMessage::OpenFileResult(OpResult::Ok(h)) => h,
        other => panic!("expected OpenFileResult::Ok, got: {other:?}"),
    };
    assert_eq!(agent.state.open_file_count(), 1);

    // Fresh connection — handle survives because it's in AgentState.
    let resp = agent.run_op(HostMessage::Read(ReadArgs {
        handle,
        max_bytes: 1024,
    }));
    match resp {
        AgentMessage::ReadResult(OpResult::Ok(ok)) => {
            assert_eq!(ok.data, b"hello agent");
        }
        other => panic!("expected ReadResult::Ok, got: {other:?}"),
    }

    let resp = agent.run_op(HostMessage::Close(CloseArgs { handle }));
    assert!(matches!(
        resp,
        AgentMessage::CloseResult(OpResult::Ok(()))
    ));
    assert_eq!(agent.state.open_file_count(), 0);
}

#[test]
fn write_then_close_persists_to_disk() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("out.txt");

    let agent = TestAgent::new();
    let handle = match agent.run_op(HostMessage::OpenFile(OpenFileArgs {
        path: path.to_string_lossy().into(),
        mode: OpenMode {
            write: true,
            create: true,
            truncate: true,
            ..Default::default()
        },
        create_perm: None,
    })) {
        AgentMessage::OpenFileResult(OpResult::Ok(h)) => h,
        other => panic!("{other:?}"),
    };

    match agent.run_op(HostMessage::Write(WriteArgs {
        handle,
        data: b"persisted bytes".to_vec(),
    })) {
        AgentMessage::WriteResult(OpResult::Ok(ok)) => assert_eq!(ok.written, 15),
        other => panic!("{other:?}"),
    }

    match agent.run_op(HostMessage::Close(CloseArgs { handle })) {
        AgentMessage::CloseResult(OpResult::Ok(())) => {}
        other => panic!("{other:?}"),
    }

    assert_eq!(std::fs::read(&path).unwrap(), b"persisted bytes");
}

#[test]
fn read_with_unknown_handle_yields_agent_error() {
    let agent = TestAgent::new();
    let resp = agent.run_op(HostMessage::Read(ReadArgs {
        handle: provium_protocol::handle::FileHandle::new(99_999),
        max_bytes: 128,
    }));
    match resp {
        AgentMessage::AgentError(e) => {
            assert_eq!(
                e.kind,
                provium_protocol::wire::AgentErrorKind::UnknownHandle
            );
            assert!(e.message.contains("99999"));
        }
        other => panic!("expected AgentError, got: {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// composite read_file / write_file
// ---------------------------------------------------------------------------

#[test]
fn read_file_returns_full_contents_in_one_round_trip() {
    let tmp = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(tmp.path(), b"composite read").unwrap();

    let agent = TestAgent::new();
    let resp = agent.run_op(HostMessage::ReadFile(ReadFileArgs {
        path: tmp.path().to_string_lossy().into(),
    }));
    match resp {
        AgentMessage::ReadFileResult(OpResult::Ok(ok)) => {
            assert_eq!(ok.data, b"composite read");
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn write_file_creates_or_replaces_at_path() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("composite.txt");

    let agent = TestAgent::new();
    match agent.run_op(HostMessage::WriteFile(WriteFileArgs {
        path: path.to_string_lossy().into(),
        data: b"first version".to_vec(),
        mode: WriteFileMode::Replace,
        create_perm: Some(0o600),
    })) {
        AgentMessage::WriteFileResult(OpResult::Ok(())) => {}
        other => panic!("{other:?}"),
    }

    assert_eq!(std::fs::read(&path).unwrap(), b"first version");
}

#[test]
fn write_file_exclusive_mode_rejects_existing_file() {
    let tmp = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(tmp.path(), b"original").unwrap();

    let agent = TestAgent::new();
    match agent.run_op(HostMessage::WriteFile(WriteFileArgs {
        path: tmp.path().to_string_lossy().into(),
        data: b"overwrite attempt".to_vec(),
        mode: WriteFileMode::Exclusive,
        create_perm: None,
    })) {
        AgentMessage::WriteFileResult(OpResult::Err(e)) => assert_eq!(e.errno, 17), // EEXIST
        other => panic!("{other:?}"),
    }
    assert_eq!(std::fs::read(tmp.path()).unwrap(), b"original");
}

// ---------------------------------------------------------------------------
// stat
// ---------------------------------------------------------------------------

#[test]
fn stat_classifies_regular_file() {
    let tmp = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(tmp.path(), b"abc").unwrap();

    let agent = TestAgent::new();
    let resp = agent.run_op(HostMessage::Stat(StatArgs {
        path: tmp.path().to_string_lossy().into(),
        follow_symlinks: true,
    }));
    match resp {
        AgentMessage::StatResult(OpResult::Ok(m)) => {
            assert_eq!(m.size, 3);
            assert_eq!(
                m.entry_type,
                provium_protocol::wire::EntryType::File
            );
            assert!(m.perm <= 0o7777);
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn stat_returns_oserror_for_missing_path() {
    let agent = TestAgent::new();
    let resp = agent.run_op(HostMessage::Stat(StatArgs {
        path: "/nonexistent/should/never/exist".into(),
        follow_symlinks: true,
    }));
    match resp {
        AgentMessage::StatResult(OpResult::Err(e)) => assert_eq!(e.errno, 2),
        other => panic!("{other:?}"),
    }
}

// ---------------------------------------------------------------------------
// tail_file (streaming)
// ---------------------------------------------------------------------------

#[test]
fn tail_file_streams_appended_bytes() {
    let tmp = tempfile::NamedTempFile::new().unwrap();
    let path = tmp.path().to_path_buf();
    std::fs::write(&path, b"initial\n").unwrap();

    let agent = TestAgent::new();
    let (mut host, _h) = agent.run_stream_op(HostMessage::TailFile(TailFileArgs {
        path: path.to_string_lossy().into(),
        start: TailStart::Beginning,
    }));

    // Open-ack.
    match recv(&mut host) {
        AgentMessage::TailFileResult(OpResult::Ok(())) => {}
        other => panic!("expected TailFileResult::Ok, got: {other:?}"),
    }

    // First frame: existing content.
    match recv(&mut host) {
        AgentMessage::StreamFrame(f) => assert_eq!(f.data, b"initial\n"),
        other => panic!("expected StreamFrame, got: {other:?}"),
    }

    // Append; agent picks it up on next poll.
    {
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        f.write_all(b"more bytes\n").unwrap();
    }

    match recv(&mut host) {
        AgentMessage::StreamFrame(f) => assert_eq!(f.data, b"more bytes\n"),
        other => panic!("expected StreamFrame, got: {other:?}"),
    }

    drop(host);
}

#[test]
fn tail_file_returns_oserror_when_path_missing() {
    let agent = TestAgent::new();
    let (mut host, _h) = agent.run_stream_op(HostMessage::TailFile(TailFileArgs {
        path: "/nonexistent/tail/target".into(),
        start: TailStart::Beginning,
    }));
    match recv(&mut host) {
        AgentMessage::TailFileResult(OpResult::Err(e)) => assert_eq!(e.errno, 2),
        other => panic!("{other:?}"),
    }
    drop(host);
}

#[test]
fn tail_file_at_end_skips_existing_content() {
    let tmp = tempfile::NamedTempFile::new().unwrap();
    let path = tmp.path().to_path_buf();
    std::fs::write(&path, b"existing content that should be skipped\n").unwrap();

    let agent = TestAgent::new();
    let (mut host, _h) = agent.run_stream_op(HostMessage::TailFile(TailFileArgs {
        path: path.to_string_lossy().into(),
        start: TailStart::End,
    }));
    match recv(&mut host) {
        AgentMessage::TailFileResult(OpResult::Ok(())) => {}
        other => panic!("{other:?}"),
    }

    // Append after open; first frame should contain only the new bytes.
    {
        let mut f = std::fs::OpenOptions::new().append(true).open(&path).unwrap();
        f.write_all(b"new\n").unwrap();
    }

    match recv(&mut host) {
        AgentMessage::StreamFrame(f) => assert_eq!(f.data, b"new\n"),
        other => panic!("{other:?}"),
    }

    drop(host);
}

#[allow(dead_code)]
fn _stream_end_use(_: &StreamEnd) {}
