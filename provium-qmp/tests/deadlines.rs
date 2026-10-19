//! QMP deadline regressions use only disposable local sockets.

use std::io::{BufRead, BufReader, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::net::{UnixListener, UnixStream};
use std::sync::{mpsc, Arc};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use provium_qmp::{Qmp, QmpError};

fn handshake(server: &mut UnixStream) {
    server.write_all(b"{\"QMP\":{}}\n").unwrap();
    let mut reader = BufReader::new(server.try_clone().unwrap());
    for _ in 0..2 {
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        server.write_all(b"{\"return\":{}}\n").unwrap();
    }
}

#[test]
fn silent_greeting_respects_the_handshake_deadline() {
    let (client, _server) = UnixStream::pair().unwrap();
    let started = Instant::now();
    let result = Qmp::from_stream_timeout(client, Duration::from_millis(30));
    assert!(matches!(result, Err(QmpError::Timeout(_))), "{result:?}");
    assert!(started.elapsed() < Duration::from_secs(2));
}

#[test]
fn trickled_greeting_does_not_restart_the_deadline() {
    let (client, mut server) = UnixStream::pair().unwrap();
    let (release, wait) = mpsc::channel();
    let peer = std::thread::spawn(move || {
        for _ in 0..100 {
            if server.write_all(b" ").is_err() {
                return;
            }
            if wait.recv_timeout(Duration::from_millis(5)).is_ok() {
                return;
            }
        }
    });
    let result = Qmp::from_stream_timeout(client, Duration::from_millis(40));
    let _ = release.send(());
    peer.join().unwrap();
    assert!(matches!(result, Err(QmpError::Timeout(_))), "{result:?}");
}

#[test]
fn capability_reply_uses_the_handshake_deadline() {
    let (client, mut server) = UnixStream::pair().unwrap();
    server.write_all(b"{\"QMP\":{}}\n").unwrap();
    let result = Qmp::from_stream_timeout(client, Duration::from_millis(30));
    assert!(matches!(result, Err(QmpError::Timeout(_))), "{result:?}");
}

#[test]
fn send_deadline_closes_a_partial_command() {
    let (client, mut server) = UnixStream::pair().unwrap();
    let peer = std::thread::spawn(move || {
        handshake(&mut server);
        server
    });
    let qmp = Qmp::from_stream(client).unwrap();
    let _server = peer.join().unwrap();
    let started = Instant::now();
    let result = qmp.execute_timeout(
        "large",
        json!({"value": "x".repeat(4 * 1024 * 1024)}),
        Duration::from_millis(50),
    );
    assert!(matches!(result, Err(QmpError::Timeout(_))), "{result:?}");
    assert!(started.elapsed() < Duration::from_secs(2));
    assert!(qmp.is_closed());
    assert!(matches!(
        qmp.execute("query-status", Value::Null),
        Err(QmpError::Closed(_))
    ));
}

#[test]
fn waiting_for_another_sender_uses_the_callers_deadline() {
    let (client, mut server) = UnixStream::pair().unwrap();
    let (writing, wait_writing) = mpsc::channel();
    let (release, wait_release) = mpsc::channel();
    let peer = std::thread::spawn(move || {
        handshake(&mut server);
        server.read_exact(&mut [0; 1]).unwrap();
        writing.send(()).unwrap();
        wait_release.recv().unwrap();
    });
    let qmp = Arc::new(Qmp::from_stream(client).unwrap());
    let sender = Arc::clone(&qmp);
    let first = std::thread::spawn(move || {
        sender.execute_timeout(
            "large",
            json!({"value":"x".repeat(4 * 1024 * 1024)}),
            Duration::from_secs(2),
        )
    });
    wait_writing.recv_timeout(Duration::from_secs(3)).unwrap();
    let started = Instant::now();
    let second = qmp.execute_timeout("query-status", Value::Null, Duration::from_millis(30));
    let elapsed = started.elapsed();
    release.send(()).unwrap();
    peer.join().unwrap();
    assert!(first.join().unwrap().is_err());
    assert!(matches!(second, Err(QmpError::Timeout(_))), "{second:?}");
    assert!(elapsed < Duration::from_secs(1));
}

#[test]
fn full_listen_queue_uses_the_connection_deadline() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("qmp");
    let listener = UnixListener::bind(&path).unwrap();
    // SAFETY: listener owns a live socket; backlog 0 leaves one queued connection on Linux.
    assert_eq!(unsafe { libc::listen(listener.as_raw_fd(), 0) }, 0);
    let _queued = UnixStream::connect(&path).unwrap();
    let started = Instant::now();
    let result = Qmp::connect_timeout(&path, Duration::from_millis(30));
    assert!(matches!(result, Err(QmpError::Timeout(_))), "{result:?}");
    assert!(started.elapsed() < Duration::from_secs(2));
}

#[test]
fn unrepresentable_timeout_is_an_input_error() {
    let (client, _server) = UnixStream::pair().unwrap();
    let result = Qmp::from_stream_timeout(client, Duration::MAX);
    assert!(
        matches!(result, Err(QmpError::Io(error)) if error.kind() == std::io::ErrorKind::InvalidInput)
    );
}

#[test]
fn response_timeout_leaves_a_complete_command_connection_usable() {
    let (client, mut server) = UnixStream::pair().unwrap();
    let peer = std::thread::spawn(move || {
        handshake(&mut server);
        let mut reader = BufReader::new(server.try_clone().unwrap());
        let mut first = String::new();
        reader.read_line(&mut first).unwrap();
        let mut second = String::new();
        reader.read_line(&mut second).unwrap();
        let value: Value = serde_json::from_str(&second).unwrap();
        writeln!(
            server,
            "{}",
            json!({"return": {"status":"running"}, "id":value["id"]})
        )
        .unwrap();
        // Keep the socket open until the client drops its connection.
        let mut end = String::new();
        reader.read_line(&mut end).unwrap();
    });
    let qmp = Qmp::from_stream(client).unwrap();
    assert!(matches!(
        qmp.execute_timeout("ignored", Value::Null, Duration::from_millis(30)),
        Err(QmpError::Timeout(_))
    ));
    assert!(!qmp.is_closed());
    assert_eq!(qmp.query_status().unwrap(), "running");
    drop(qmp);
    peer.join().unwrap();
}
