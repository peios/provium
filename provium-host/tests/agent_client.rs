//! End-to-end integration tests for [`provium_host::agent_client`].
//!
//! Each test wires the host's [`AgentClient`] up to a *real*
//! `provium-agent` connection handler running on a worker thread, with
//! the wire transport implemented as paired [`UnixStream`]s. This
//! exercises the entire host ↔ agent code path — frame codec, hello
//! handshake, op dispatch, OS interaction in the agent — without
//! needing a live KVM guest.

use std::io::{self, Write as _};
use std::os::unix::net::UnixStream;
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use provium_agent::AgentState;

use provium_host::agent_client::{AgentClient, TailFileOutcome};
use provium_host::connector::{AgentStream, Connector};
use provium_host::ClientError;

use provium_protocol::handle::FileHandle;
use provium_protocol::wire::{
    AgentErrorKind, CloseArgs, EntryType, ExecArgs, ExecResult, ExitStatus, OpResult, OpenFileArgs,
    OpenMode, ReadArgs, ReadFileArgs, StatArgs, TailFileArgs, TailStart, WriteArgs, WriteFileArgs,
    WriteFileMode,
};

// ---------------------------------------------------------------------------
// Test connector — drives a real provium-agent over a UnixStream pair.
// ---------------------------------------------------------------------------

#[derive(Clone)]
struct AgentBackedConnector {
    state: Arc<AgentState>,
}

impl AgentBackedConnector {
    fn new() -> Self {
        Self {
            state: Arc::new(AgentState::new()),
        }
    }

    fn agent_state(&self) -> &Arc<AgentState> {
        &self.state
    }
}

impl Connector for AgentBackedConnector {
    fn connect(&self) -> io::Result<Box<dyn AgentStream>> {
        let (host_side, agent_side) = UnixStream::pair()?;
        let state = Arc::clone(&self.state);

        let mut reader = agent_side.try_clone()?;
        let mut writer = agent_side;
        thread::spawn(move || {
            let _ = provium_agent::connection::handle_connection(&mut reader, &mut writer, state);
        });
        Ok(Box::new(host_side))
    }
}

fn fresh_client() -> AgentClient {
    AgentClient::new(AgentBackedConnector::new())
}

fn exec_args(cmd: &str, args: &[&str]) -> ExecArgs {
    ExecArgs {
        cmd: cmd.into(),
        args: args.iter().copied().map(String::from).collect(),
        env: Default::default(),
        env_clear: false,
        stdin: vec![],
        cwd: None,
        timeout_ms: None,
    }
}

// ---------------------------------------------------------------------------
// Hello handshake
// ---------------------------------------------------------------------------

#[test]
fn handshake_succeeds_against_real_agent() {
    let client = fresh_client();
    // Any short op is enough to exercise the handshake; pick stat
    // since it's pure metadata.
    let r = client
        .stat(StatArgs {
            path: "/".into(),
            follow_symlinks: true,
        })
        .expect("op completed");
    assert!(matches!(r, OpResult::Ok(_)));
}

#[test]
fn version_mismatch_surfaces_clean_error() {
    // A connector that lies about being an agent: greets the host
    // with HelloErr regardless of the host's version.
    struct LyingConnector;
    impl Connector for LyingConnector {
        fn connect(&self) -> io::Result<Box<dyn AgentStream>> {
            let (host_side, mut agent_side) = UnixStream::pair()?;
            thread::spawn(move || {
                use provium_protocol::frame::{read_frame, write_frame, DEFAULT_MAX_FRAME_BYTES};
                use provium_protocol::wire::{AgentMessage, HelloErr, HostMessage};
                let _hello: HostMessage =
                    read_frame(&mut agent_side, DEFAULT_MAX_FRAME_BYTES).unwrap();
                let _ = write_frame(
                    &mut agent_side,
                    &AgentMessage::HelloErr(HelloErr {
                        host_version: 999,
                        agent_version: 1,
                    }),
                    DEFAULT_MAX_FRAME_BYTES,
                );
            });
            Ok(Box::new(host_side))
        }
    }

    let client = AgentClient::new(LyingConnector);
    let err = client
        .stat(StatArgs {
            path: "/".into(),
            follow_symlinks: true,
        })
        .unwrap_err();
    match err {
        ClientError::VersionMismatch { host: 999, agent: 1 } => {}
        other => panic!("expected VersionMismatch, got {other:?}"),
    }
}

#[test]
fn connect_failure_surfaces_as_client_error_connect() {
    struct BadConnector;
    impl Connector for BadConnector {
        fn connect(&self) -> io::Result<Box<dyn AgentStream>> {
            Err(io::Error::new(io::ErrorKind::ConnectionRefused, "no agent"))
        }
    }

    let client = AgentClient::new(BadConnector);
    let err = client
        .stat(StatArgs {
            path: "/".into(),
            follow_symlinks: true,
        })
        .unwrap_err();
    match err {
        ClientError::Connect(io_err) => {
            assert_eq!(io_err.kind(), io::ErrorKind::ConnectionRefused);
        }
        other => panic!("expected Connect, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// exec
// ---------------------------------------------------------------------------

#[test]
fn exec_returns_stdout_and_exit_code() {
    let client = fresh_client();
    let result = client
        .exec(exec_args("sh", &["-c", "printf 'hi\\n'"]))
        .unwrap();
    match result {
        ExecResult::Ok(ok) => {
            assert_eq!(ok.status, ExitStatus::Exited(0));
            assert_eq!(&ok.stdout, b"hi\n");
        }
        other => panic!("expected Ok, got {other:?}"),
    }
}

#[test]
fn exec_propagates_oserror_when_binary_missing() {
    let client = fresh_client();
    let result = client
        .exec(exec_args("/nonexistent/never/exists", &[]))
        .unwrap();
    match result {
        ExecResult::Err(e) => assert_eq!(e.errno, 2),
        other => panic!("expected Err, got {other:?}"),
    }
}

#[test]
fn exec_with_timeout_reports_timed_out() {
    let client = fresh_client();
    let result = client
        .exec(ExecArgs {
            cmd: "sh".into(),
            args: vec!["-c".into(), "sleep 30".into()],
            env: Default::default(),
            env_clear: false,
            stdin: vec![],
            cwd: None,
            timeout_ms: Some(150),
        })
        .unwrap();
    match result {
        ExecResult::Ok(ok) => assert_eq!(ok.status, ExitStatus::TimedOut),
        other => panic!("expected Ok with TimedOut, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// File ops — exercise stateful handle reuse across connections.
// ---------------------------------------------------------------------------

#[test]
fn open_then_read_then_close_round_trips_through_state() {
    let tmp = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(tmp.path(), b"client-side roundtrip").unwrap();

    let connector = AgentBackedConnector::new();
    let agent_state = Arc::clone(connector.agent_state());
    let client = AgentClient::new(connector);

    let handle = match client
        .open_file(OpenFileArgs {
            path: tmp.path().to_string_lossy().into(),
            mode: OpenMode {
                read: true,
                ..Default::default()
            },
            create_perm: None,
        })
        .unwrap()
    {
        OpResult::Ok(h) => h,
        OpResult::Err(e) => panic!("open failed: {e:?}"),
    };

    // The handle is in the agent's table after a fresh connection.
    assert_eq!(agent_state.open_file_count(), 1);

    let read_ok = match client
        .read(ReadArgs {
            handle,
            max_bytes: 4096,
        })
        .unwrap()
    {
        OpResult::Ok(ok) => ok,
        OpResult::Err(e) => panic!("read failed: {e:?}"),
    };
    assert_eq!(&read_ok.data, b"client-side roundtrip");

    match client.close(CloseArgs { handle }).unwrap() {
        OpResult::Ok(()) => {}
        OpResult::Err(e) => panic!("close failed: {e:?}"),
    }
    assert_eq!(agent_state.open_file_count(), 0);
}

#[test]
fn read_with_unknown_handle_surfaces_agent_error() {
    let client = fresh_client();
    let err = client
        .read(ReadArgs {
            handle: FileHandle::new(99_999),
            max_bytes: 64,
        })
        .unwrap_err();
    match err {
        ClientError::Agent(agent_err) => {
            assert_eq!(agent_err.kind, AgentErrorKind::UnknownHandle);
        }
        other => panic!("expected Agent error, got {other:?}"),
    }
}

#[test]
fn read_file_returns_full_contents() {
    let tmp = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(tmp.path(), b"composite read").unwrap();

    let client = fresh_client();
    match client
        .read_file(ReadFileArgs {
            path: tmp.path().to_string_lossy().into(),
        })
        .unwrap()
    {
        OpResult::Ok(ok) => assert_eq!(&ok.data, b"composite read"),
        OpResult::Err(e) => panic!("{e:?}"),
    }
}

#[test]
fn write_file_creates_path_with_requested_mode() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("created.txt");

    let client = fresh_client();
    match client
        .write_file(WriteFileArgs {
            path: path.to_string_lossy().into(),
            data: b"created via composite".to_vec(),
            mode: WriteFileMode::Replace,
            create_perm: Some(0o600),
        })
        .unwrap()
    {
        OpResult::Ok(()) => {}
        OpResult::Err(e) => panic!("{e:?}"),
    }
    assert_eq!(std::fs::read(&path).unwrap(), b"created via composite");
}

#[test]
fn write_file_exclusive_rejects_existing() {
    let tmp = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(tmp.path(), b"original").unwrap();

    let client = fresh_client();
    match client
        .write_file(WriteFileArgs {
            path: tmp.path().to_string_lossy().into(),
            data: b"replacement".to_vec(),
            mode: WriteFileMode::Exclusive,
            create_perm: None,
        })
        .unwrap()
    {
        OpResult::Err(e) => assert_eq!(e.errno, 17), // EEXIST
        other => panic!("{other:?}"),
    }
    assert_eq!(std::fs::read(tmp.path()).unwrap(), b"original");
}

#[test]
fn write_then_read_back_through_agent() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("rw.txt");
    let client = fresh_client();

    let handle = match client
        .open_file(OpenFileArgs {
            path: path.to_string_lossy().into(),
            mode: OpenMode {
                write: true,
                create: true,
                truncate: true,
                ..Default::default()
            },
            create_perm: None,
        })
        .unwrap()
    {
        OpResult::Ok(h) => h,
        OpResult::Err(e) => panic!("{e:?}"),
    };

    match client
        .write(WriteArgs {
            handle,
            data: b"round-trip\n".to_vec(),
        })
        .unwrap()
    {
        OpResult::Ok(ok) => assert_eq!(ok.written, 11),
        OpResult::Err(e) => panic!("{e:?}"),
    }

    let _ = client.close(CloseArgs { handle }).unwrap();
    assert_eq!(std::fs::read(&path).unwrap(), b"round-trip\n");
}

// ---------------------------------------------------------------------------
// stat
// ---------------------------------------------------------------------------

#[test]
fn stat_returns_typed_metadata() {
    let tmp = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(tmp.path(), b"abcde").unwrap();

    let client = fresh_client();
    match client
        .stat(StatArgs {
            path: tmp.path().to_string_lossy().into(),
            follow_symlinks: true,
        })
        .unwrap()
    {
        OpResult::Ok(m) => {
            assert_eq!(m.size, 5);
            assert_eq!(m.entry_type, EntryType::File);
        }
        OpResult::Err(e) => panic!("{e:?}"),
    }
}

#[test]
fn stat_returns_oserror_for_missing_path() {
    let client = fresh_client();
    match client
        .stat(StatArgs {
            path: "/no/such/path/exists/anywhere".into(),
            follow_symlinks: true,
        })
        .unwrap()
    {
        OpResult::Err(e) => assert_eq!(e.errno, 2),
        other => panic!("{other:?}"),
    }
}

// ---------------------------------------------------------------------------
// tail_file streaming
// ---------------------------------------------------------------------------

#[test]
fn tail_file_returns_oserror_for_missing_path() {
    let client = fresh_client();
    let outcome = client
        .tail_file(TailFileArgs {
            path: "/no/such/log".into(),
            start: TailStart::Beginning,
        })
        .unwrap();
    match outcome {
        TailFileOutcome::Err(os) => assert_eq!(os.errno, 2),
        TailFileOutcome::Ok(_) => panic!("expected Err"),
    }
}

#[test]
fn tail_file_streams_existing_then_appended_bytes() {
    let tmp = tempfile::NamedTempFile::new().unwrap();
    let path = tmp.path().to_path_buf();
    std::fs::write(&path, b"first\n").unwrap();

    let client = fresh_client();
    let mut session = match client
        .tail_file(TailFileArgs {
            path: path.to_string_lossy().into(),
            start: TailStart::Beginning,
        })
        .unwrap()
    {
        TailFileOutcome::Ok(s) => s,
        TailFileOutcome::Err(e) => panic!("{e:?}"),
    };

    let f1 = session
        .next_frame()
        .expect("frame")
        .expect("frame present");
    assert_eq!(f1.data, b"first\n");

    {
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        f.write_all(b"second\n").unwrap();
    }

    let f2 = session
        .next_frame()
        .expect("frame")
        .expect("frame present");
    assert_eq!(f2.data, b"second\n");

    // Drop the session — closes the connection from the host side.
    drop(session);
}

#[test]
fn tail_file_at_end_skips_existing_content() {
    let tmp = tempfile::NamedTempFile::new().unwrap();
    let path = tmp.path().to_path_buf();
    std::fs::write(&path, b"already there\n").unwrap();

    let client = fresh_client();
    let mut session = match client
        .tail_file(TailFileArgs {
            path: path.to_string_lossy().into(),
            start: TailStart::End,
        })
        .unwrap()
    {
        TailFileOutcome::Ok(s) => s,
        TailFileOutcome::Err(e) => panic!("{e:?}"),
    };

    {
        let mut f = std::fs::OpenOptions::new().append(true).open(&path).unwrap();
        f.write_all(b"after\n").unwrap();
    }

    let f = session.next_frame().unwrap().unwrap();
    assert_eq!(f.data, b"after\n");
    drop(session);
}

#[test]
fn agent_client_is_send_and_sync() {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<AgentClient>();
}

// Compile-time check: ClientError variants are exhaustively matchable
// for downstream consumers.
#[allow(dead_code)]
fn _client_error_match(err: ClientError) -> &'static str {
    match err {
        ClientError::Connect(_) => "connect",
        ClientError::Frame(_) => "frame",
        ClientError::VersionMismatch { .. } => "version",
        ClientError::UnexpectedMessage { .. } => "unexpected",
        ClientError::Agent(_) => "agent",
        ClientError::StreamSource(_) => "source",
    }
}

// Suppress unused-Duration warning; reserved for cadence-sensitive
// tests added in later slices.
#[allow(dead_code)]
fn _hold_duration() -> Duration {
    Duration::from_millis(50)
}
