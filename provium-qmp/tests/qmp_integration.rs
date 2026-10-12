//! Integration tests for [`provium_qmp::Qmp`] against a scripted QMP
//! server.
//!
//! The mock server (see [`mock`]) runs on its own thread, listens on a
//! unix socket, performs the standard QMP greeting + capability dance,
//! then responds to commands using a programmable rule set. Each test
//! drives the rules to exercise one wrapper invariant.

use std::time::Duration;

use serde_json::{json, Value};

use provium_qmp::{Event, Qmp, QmpError};

mod mock;
use mock::MockServer;

#[test]
fn event_wait_evaluates_each_new_event_once() {
    use std::cell::RefCell;

    let server = MockServer::start();
    let qmp = Qmp::connect(server.path()).unwrap();
    let mark = qmp.event_mark();
    let seen = RefCell::new(Vec::new());
    server.push_event("PROGRESS", json!({"step": 0}), Duration::ZERO);
    let result = qmp
        .wait_event(
            "PROGRESS",
            mark,
            |event| {
                let step = event.data["step"].as_u64().unwrap();
                let mut seen = seen.borrow_mut();
                let first_visit = !seen.contains(&step);
                seen.push(step);
                if step < 32 && first_visit {
                    server.push_event("PROGRESS", json!({"step": step + 1}), Duration::ZERO);
                }
                step == 32
            },
            Duration::from_secs(5),
        )
        .unwrap();
    assert_eq!(result.data["step"], 32);
    assert_eq!(*seen.borrow(), (0..=32).collect::<Vec<_>>());
    drop(qmp);
    server.shutdown();
}

#[test]
fn connect_completes_handshake() {
    let server = MockServer::start();
    let qmp = Qmp::connect(server.path()).expect("connect");

    // Handshake commands the wrapper sent should now be recorded by
    // the mock server.
    let observed = server.commands_received();
    assert_eq!(observed.len(), 2, "expected 2 handshake commands");
    assert_eq!(observed[0].as_str(), "qmp_capabilities");
    assert_eq!(observed[1].as_str(), "migrate-set-capabilities");

    drop(qmp);
    server.shutdown();
}

#[test]
fn execute_returns_value_on_success() {
    let server = MockServer::start();
    server.respond_to(
        "query-status",
        json!({"status": "running", "running": true}),
    );
    let qmp = Qmp::connect(server.path()).unwrap();

    let result = qmp.execute("query-status", Value::Null).unwrap();
    assert_eq!(result["status"], "running");
    assert_eq!(result["running"], true);

    qmp.close().unwrap();
    server.shutdown();
}

#[test]
fn execute_surfaces_qmp_error_class_and_desc() {
    let server = MockServer::start();
    server.respond_to_with_error("device_add", "GenericError", "Device 'foo' not found");
    let qmp = Qmp::connect(server.path()).unwrap();

    let err = qmp
        .execute("device_add", json!({"driver": "foo"}))
        .unwrap_err();
    match err {
        QmpError::Command {
            command,
            class,
            desc,
        } => {
            assert_eq!(command, "device_add");
            assert_eq!(class, "GenericError");
            assert!(desc.contains("not found"));
        }
        other => panic!("expected Command error, got {other:?}"),
    }

    qmp.close().unwrap();
    server.shutdown();
}

#[test]
fn execute_times_out_when_no_response() {
    let server = MockServer::start();
    // No rule for "stop" → the mock keeps the connection open but
    // never replies. The wrapper should hit its timeout cleanly.
    let qmp = Qmp::connect(server.path()).unwrap();

    let err = qmp
        .execute_timeout("stop", Value::Null, Duration::from_millis(150))
        .unwrap_err();
    assert!(matches!(err, QmpError::Timeout(_)));

    qmp.close().unwrap();
    server.shutdown();
}

#[test]
fn typed_helpers_dispatch_correct_commands() {
    let server = MockServer::start();
    server.respond_to("stop", json!({}));
    server.respond_to("cont", json!({}));
    server.respond_to("query-status", json!({"status": "paused"}));

    let qmp = Qmp::connect(server.path()).unwrap();
    qmp.stop().unwrap();
    qmp.cont().unwrap();
    let status = qmp.query_status().unwrap();
    assert_eq!(status, "paused");

    qmp.close().unwrap();
    server.shutdown();
}

#[test]
fn wait_event_returns_a_matching_event_pushed_after_mark() {
    let server = MockServer::start();
    let qmp = Qmp::connect(server.path()).unwrap();

    let mark = qmp.event_mark();

    // Push a MIGRATION/completed event from the mock side.
    server.push_event(
        "MIGRATION",
        json!({"status": "completed"}),
        Duration::from_millis(50),
    );

    let event = qmp
        .wait_event(
            "MIGRATION",
            mark,
            |e| e.data.get("status").and_then(|s| s.as_str()) == Some("completed"),
            Duration::from_secs(2),
        )
        .unwrap();

    assert_eq!(event.name, "MIGRATION");
    assert_eq!(event.data["status"], "completed");

    qmp.close().unwrap();
    server.shutdown();
}

#[test]
fn wait_event_with_since_mark_ignores_stale_events() {
    // This is the stale-event hazard the spike validated. Two cycles:
    // the first cycle's MIGRATION/completed must NOT match the second
    // cycle's wait_event because we capture a fresh mark.
    let server = MockServer::start();
    let qmp = Qmp::connect(server.path()).unwrap();

    // Cycle 1: emit completed.
    server.push_event(
        "MIGRATION",
        json!({"status": "completed"}),
        Duration::from_millis(20),
    );

    // Wait long enough for the event to be appended to the log.
    std::thread::sleep(Duration::from_millis(80));

    // Cycle 2: capture a fresh mark, then check that an instant
    // wait_event call would NOT spuriously match the cycle-1 event.
    let mark2 = qmp.event_mark();
    let err = qmp
        .wait_event(
            "MIGRATION",
            mark2,
            |e| e.data.get("status").and_then(|s| s.as_str()) == Some("completed"),
            Duration::from_millis(150),
        )
        .unwrap_err();
    assert!(
        matches!(err, QmpError::Timeout(_)),
        "expected Timeout (no event since mark), got {err:?}"
    );

    qmp.close().unwrap();
    server.shutdown();
}

#[test]
fn wait_event_observes_events_appended_during_wait() {
    let server = MockServer::start();
    let qmp = Qmp::connect(server.path()).unwrap();

    let mark = qmp.event_mark();

    // Schedule the event for after we've started waiting.
    server.push_event("STOP", json!({}), Duration::from_millis(100));

    let observed = qmp
        .wait_event("STOP", mark, |_| true, Duration::from_secs(2))
        .unwrap();
    assert_eq!(observed.name, "STOP");

    qmp.close().unwrap();
    server.shutdown();
}

#[test]
fn close_drains_pending_commands_with_closed_error() {
    let server = MockServer::start();
    let qmp = Qmp::connect(server.path()).unwrap();

    // Spawn a thread waiting on a command the server will never answer.
    let qmp_arc = std::sync::Arc::new(qmp);
    let qmp_for_thread = std::sync::Arc::clone(&qmp_arc);
    let handle = std::thread::spawn(move || {
        qmp_for_thread.execute_timeout("stop", Value::Null, Duration::from_secs(10))
    });

    // Give the call time to register its pending entry.
    std::thread::sleep(Duration::from_millis(50));

    // Sever the connection from the server side.
    server.shutdown();

    let result = handle.join().unwrap();
    match result {
        Err(QmpError::Closed(_)) => {}
        other => panic!("expected Closed error, got {other:?}"),
    }
}

#[test]
fn dropping_qmp_without_explicit_close_is_safe() {
    let server = MockServer::start();
    {
        let _qmp = Qmp::connect(server.path()).unwrap();
        // Just drop it. The Drop impl should tear down the reader
        // thread without leaking or panicking.
    }
    server.shutdown();
}

#[test]
fn migrate_helper_uses_file_uri() {
    let server = MockServer::start();
    server.respond_to("migrate", json!({}));
    let qmp = Qmp::connect(server.path()).unwrap();

    qmp.migrate("/tmp/snap.bin").unwrap();

    let arg_value = server
        .last_arguments_for("migrate")
        .expect("migrate should have been recorded");
    let uri = arg_value
        .get("uri")
        .and_then(|v| v.as_str())
        .expect("`uri` argument");
    assert_eq!(uri, "file:/tmp/snap.bin");

    qmp.close().unwrap();
    server.shutdown();
}

#[test]
fn wait_migration_completed_succeeds_on_completed_event() {
    let server = MockServer::start();
    let qmp = Qmp::connect(server.path()).unwrap();

    let mark = qmp.event_mark();
    server.push_event(
        "MIGRATION",
        json!({"status": "completed"}),
        Duration::from_millis(30),
    );

    qmp.wait_migration_completed(mark, Duration::from_secs(2))
        .unwrap();

    qmp.close().unwrap();
    server.shutdown();
}

#[test]
fn wait_migration_completed_returns_command_error_on_failed_event() {
    let server = MockServer::start();
    let qmp = Qmp::connect(server.path()).unwrap();

    let mark = qmp.event_mark();
    server.push_event(
        "MIGRATION",
        json!({"status": "failed"}),
        Duration::from_millis(30),
    );

    let err = qmp
        .wait_migration_completed(mark, Duration::from_secs(2))
        .unwrap_err();
    match err {
        QmpError::Command { class, desc, .. } => {
            assert_eq!(class, "MigrationFailed");
            assert!(desc.contains("failed"));
        }
        other => panic!("expected Command error, got {other:?}"),
    }

    qmp.close().unwrap();
    server.shutdown();
}

#[test]
fn execute_after_close_returns_closed_error() {
    let server = MockServer::start();
    let qmp = Qmp::connect(server.path()).unwrap();

    server.shutdown();
    // Give the reader thread time to observe the EOF.
    std::thread::sleep(Duration::from_millis(50));

    let err = qmp
        .execute_timeout("stop", Value::Null, Duration::from_millis(200))
        .unwrap_err();
    assert!(
        matches!(err, QmpError::Closed(_) | QmpError::Io(_)),
        "expected Closed/Io, got {err:?}"
    );
    assert!(qmp.is_closed());
}

#[test]
fn event_field_passes_through_arbitrary_data() {
    let server = MockServer::start();
    let qmp = Qmp::connect(server.path()).unwrap();

    let mark = qmp.event_mark();
    server.push_event(
        "BLOCK_IO_ERROR",
        json!({
            "device": "ide0-hd0",
            "operation": "write",
            "action": "report",
            "nospace": true,
        }),
        Duration::from_millis(20),
    );

    let event = qmp
        .wait_event("BLOCK_IO_ERROR", mark, |_| true, Duration::from_secs(2))
        .unwrap();
    assert_eq!(event.data["device"], "ide0-hd0");
    assert_eq!(event.data["nospace"], true);

    qmp.close().unwrap();
    server.shutdown();
}

// Convince the compiler that Event is actually used (it is, via the
// closure types above — but tests don't import it directly).
#[allow(dead_code)]
fn _event_use(_: &Event) {}

fn wait_for_powerdown_commands(server: &MockServer, count: usize) {
    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    while server
        .commands_received()
        .iter()
        .filter(|name| name.as_str() == "system_powerdown")
        .count()
        < count
    {
        assert!(
            std::time::Instant::now() < deadline,
            "powerdown command was not sent"
        );
        std::thread::sleep(Duration::from_millis(1));
    }
}

#[test]
fn powerdown_waits_for_fresh_guest_shutdown_not_a_stale_event() {
    let server = MockServer::start();
    server.respond_to("system_powerdown", json!({}));
    server.respond_to("query-status", json!({"status": "running"}));
    let qmp = Qmp::connect(server.path()).unwrap();
    let stale_mark = qmp.event_mark();
    server.push_event(
        "SHUTDOWN",
        json!({"guest": true, "reason": "guest-shutdown"}),
        Duration::ZERO,
    );
    qmp.wait_event("SHUTDOWN", stale_mark, |_| true, Duration::from_secs(2))
        .unwrap();
    assert!(
        matches!(
            qmp.powerdown_and_wait(Duration::from_millis(25)),
            Err(QmpError::GuestShutdownTimeout(_))
        ),
        "the earlier event must not satisfy a later request"
    );

    std::thread::scope(|scope| {
        let call = scope.spawn(|| qmp.powerdown_and_wait(Duration::from_secs(2)));
        wait_for_powerdown_commands(&server, 2);
        assert!(
            !call.is_finished(),
            "a stale event cannot confirm this request"
        );
        server.push_event(
            "SHUTDOWN",
            json!({"guest": true, "reason": "guest-shutdown"}),
            Duration::ZERO,
        );
        call.join().unwrap().unwrap();
    });
    qmp.close().unwrap();
    server.shutdown();
}

#[test]
fn powerdown_acknowledgement_alone_times_out_without_closing_or_quitting() {
    let server = MockServer::start();
    server.respond_to("system_powerdown", json!({}));
    server.respond_to("query-status", json!({"status": "running"}));
    let qmp = Qmp::connect(server.path()).unwrap();
    let error = qmp
        .powerdown_and_wait(Duration::from_millis(25))
        .unwrap_err();
    assert!(
        matches!(error, QmpError::GuestShutdownTimeout(_)),
        "{error:?}"
    );
    assert!(!error.is_terminal());
    assert!(!qmp.is_closed());
    assert!(!server.commands_received().iter().any(|name| name == "quit"));
    qmp.execute("query-status", Value::Null).unwrap();
    qmp.close().unwrap();
    server.shutdown();
}

#[test]
fn powerdown_does_not_accept_host_shutdown_or_guest_reset_as_graceful_poweroff() {
    for data in [
        json!({"guest": false, "reason": "host-signal"}),
        json!({"guest": true, "reason": "guest-reset"}),
        json!({"guest": true, "reason": "guest-panic"}),
    ] {
        let server = MockServer::start();
        server.respond_to("system_powerdown", json!({}));
        server.respond_to("query-status", json!({"status": "running"}));
        let qmp = Qmp::connect(server.path()).unwrap();
        std::thread::scope(|scope| {
            let call = scope.spawn(|| qmp.powerdown_and_wait(Duration::from_millis(100)));
            wait_for_powerdown_commands(&server, 1);
            server.push_event("SHUTDOWN", data, Duration::ZERO);
            assert!(matches!(
                call.join().unwrap(),
                Err(QmpError::GuestShutdownTimeout(_))
            ));
        });
        qmp.close().unwrap();
        server.shutdown();
    }
}

#[test]
fn powerdown_surfaces_command_rejection_and_monitor_loss() {
    let server = MockServer::start();
    server.respond_to("system_powerdown", json!({}));
    server.respond_to("query-status", json!({"status": "running"}));
    server.respond_to_with_error("system_powerdown", "GenericError", "no power button");
    let qmp = Qmp::connect(server.path()).unwrap();
    assert!(matches!(
        qmp.powerdown_and_wait(Duration::from_secs(2)),
        Err(QmpError::Command { .. })
    ));
    qmp.close().unwrap();
    server.shutdown();

    let server = MockServer::start();
    server.respond_to("system_powerdown", json!({}));
    server.respond_to("query-status", json!({"status": "running"}));
    let qmp = Qmp::connect(server.path()).unwrap();
    std::thread::scope(|scope| {
        let call = scope.spawn(|| qmp.powerdown_and_wait(Duration::from_secs(2)));
        wait_for_powerdown_commands(&server, 1);
        server.shutdown();
        assert!(matches!(call.join().unwrap(), Err(QmpError::Closed(_))));
    });
}
