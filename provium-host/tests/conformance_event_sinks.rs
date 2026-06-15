//! Conformance: EventSink implementations per `DESIGN.md` §
//! Event categories. NullSink discards, WriteSink persists,
//! MultiSink fans out — pin the contract so a future refactor
//! that swaps the trait can't quietly drop frames.

use std::io::Read;
use std::sync::Arc;

use provium_host::scheduler::events::{
    EventSink, MultiSink, NullSink, WriteSink,
};
use provium_protocol::events::{Event, EventFrame, FileDiscovered};
use provium_protocol::frame::{read_frame, DEFAULT_MAX_FRAME_BYTES};

fn sample_event() -> Event {
    Event::FileDiscovered(FileDiscovered {
        path: "tests/x.test.lua".into(),
        fixture_refs: vec![],
        declared_claim: None,
    })
}

#[test]
fn null_sink_emit_is_a_noop() {
    let sink = NullSink;
    // No assertion possible besides "doesn't panic" — pin that.
    sink.emit(sample_event());
}

#[test]
fn write_sink_persists_emitted_events_to_file() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("events.msgpack");
    {
        let sink = WriteSink::file(&path).unwrap();
        sink.emit(sample_event());
        // Drop sink to flush.
    }
    let mut bytes = Vec::new();
    std::fs::File::open(&path).unwrap().read_to_end(&mut bytes).unwrap();
    assert!(!bytes.is_empty(), "emitted event must hit disk");

    // Decode back — must be a complete EventFrame.
    let mut cursor = std::io::Cursor::new(&bytes);
    let frame: EventFrame = read_frame(&mut cursor, DEFAULT_MAX_FRAME_BYTES)
        .expect("frame must round-trip from disk");
    if let Event::FileDiscovered(payload) = frame.event {
        assert_eq!(payload.path, "tests/x.test.lua");
    } else {
        panic!("wrong event variant decoded");
    }
}

#[test]
fn write_sink_appends_multiple_frames() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("events.msgpack");
    {
        let sink = WriteSink::file(&path).unwrap();
        sink.emit(sample_event());
        sink.emit(sample_event());
        sink.emit(sample_event());
    }
    let mut bytes = Vec::new();
    std::fs::File::open(&path).unwrap().read_to_end(&mut bytes).unwrap();
    let mut cursor = std::io::Cursor::new(&bytes);
    let mut count = 0;
    while cursor.position() < bytes.len() as u64 {
        let _: EventFrame = read_frame(&mut cursor, DEFAULT_MAX_FRAME_BYTES)
            .expect("decode frame");
        count += 1;
    }
    assert_eq!(count, 3, "all three frames must persist");
}

#[test]
fn multi_sink_fans_out_to_every_inner() {
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[derive(Default)]
    struct Counter {
        n: AtomicUsize,
    }
    impl EventSink for Counter {
        fn emit(&self, _event: Event) {
            self.n.fetch_add(1, Ordering::SeqCst);
        }
    }

    let a: Arc<Counter> = Arc::new(Counter::default());
    let b: Arc<Counter> = Arc::new(Counter::default());
    let multi = MultiSink::new(vec![
        Box::new(WrappedArc(Arc::clone(&a))),
        Box::new(WrappedArc(Arc::clone(&b))),
    ]);
    multi.emit(sample_event());
    multi.emit(sample_event());
    assert_eq!(a.n.load(Ordering::SeqCst), 2);
    assert_eq!(b.n.load(Ordering::SeqCst), 2);

    struct WrappedArc(Arc<Counter>);
    impl EventSink for WrappedArc {
        fn emit(&self, e: Event) {
            self.0.emit(e);
        }
    }
}

#[test]
fn multi_sink_with_zero_inner_is_a_noop() {
    let multi = MultiSink::new(vec![]);
    multi.emit(sample_event());
}

#[test]
fn write_sink_file_truncate_resets_existing_log() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("events.msgpack");
    // Pre-fill with garbage.
    std::fs::write(&path, b"OLD GARBAGE").unwrap();
    {
        let sink = WriteSink::file_with_mode(&path, true).unwrap();
        sink.emit(sample_event());
    }
    let bytes = std::fs::read(&path).unwrap();
    // The first byte should NOT be 'O' (would be from "OLD GARBAGE")
    // — truncate must have reset before our emit.
    assert_ne!(bytes.first(), Some(&b'O'),
        "truncate=true must reset the log before emit");
}
