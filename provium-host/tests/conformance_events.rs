//! Conformance: event schema per `DESIGN.md` § Event categories.
//! The event format is part of the public API consumers depend
//! on. Field names + types must remain stable across versions —
//! these tests serialize each variant and pin the wire shape.

use provium_protocol::events::*;

fn ts() -> i64 { 1_700_000_000_000_000_000 }

fn round_trip(frame: &EventFrame) -> EventFrame {
    let bytes = rmp_serde::to_vec_named(frame).expect("serialize");
    rmp_serde::from_slice::<EventFrame>(&bytes).expect("deserialize")
}

#[test]
fn file_discovered_carries_path_and_refs() {
    let f = EventFrame {
        ts: ts(),
        event: Event::FileDiscovered(FileDiscovered {
            path: "tests/x.test.lua".into(),
            fixture_refs: vec!["fixtures/y".into()],
            declared_claim: None,
        }),
    };
    let r = round_trip(&f);
    assert_eq!(r, f);
}

#[test]
fn file_blocked_uses_memory_bytes_not_memory() {
    // R8 #421: DESIGN docs once said `memory`; code uses
    // `memory_bytes`. Pin the snake_case wire field.
    let f = EventFrame {
        ts: ts(),
        event: Event::FileBlocked(FileBlocked {
            path: "x".into(),
            waiting_for: ResourceAmount {
                memory_bytes: 1024,
                cpus: 2,
            },
            reason: "pool_full".into(),
        }),
    };
    let bytes = rmp_serde::to_vec_named(&f).unwrap();
    // The msgpack bytes carry the field names as raw strings —
    // grep the bytes for `memory_bytes` literally. (Decoding to
    // an untyped Value isn't worth a json/serde_json detour.)
    let mem_bytes_marker = b"memory_bytes";
    let bare_memory_marker = b"\xa6memory";  // msgpack fixstr "memory" (6 chars)
    let has_mem_bytes = bytes.windows(mem_bytes_marker.len())
        .any(|w| w == mem_bytes_marker);
    assert!(has_mem_bytes, "wire schema must use memory_bytes");
    let has_bare_memory = bytes.windows(bare_memory_marker.len())
        .any(|w| w == bare_memory_marker);
    assert!(!has_bare_memory,
        "wire schema must not use bare `memory` as a 6-char string");
}

#[test]
fn fixture_build_done_carries_snapshot_bytes() {
    // DESIGN: fixture_build_done {path, duration_ns, snapshot_bytes}.
    let f = EventFrame {
        ts: ts(),
        event: Event::FixtureBuildDone(FixtureBuildDone {
            path: "fixtures/x".into(),
            duration_ns: 1_000_000,
            snapshot_bytes: 4096,
        }),
    };
    let r = round_trip(&f);
    assert_eq!(r, f);
}

#[test]
fn vm_spawned_carries_memory_bytes() {
    let f = EventFrame {
        ts: ts(),
        event: Event::VmSpawned(VmSpawned {
            file: "x.test.lua".into(),
            vm_name: "a".into(),
            profile: "peios".into(),
            memory_bytes: 1_073_741_824,
            cid: 4,
        }),
    };
    let r = round_trip(&f);
    assert_eq!(r, f);
}

#[test]
fn test_failed_payload_carries_console_excerpt() {
    let f = EventFrame {
        ts: ts(),
        event: Event::TestFailed(TestFailed {
            path: "x.test.lua".into(),
            name: "test 1".into(),
            reason: "assertion failed".into(),
            console_excerpt: "panic: ...".into(),
            duration_ns: 50_000,
            meta: Default::default(),
        }),
    };
    let r = round_trip(&f);
    assert_eq!(r, f);
}

#[test]
fn unknown_event_kind_during_decode_errors() {
    // Wire compatibility guard: a frame with an unknown `kind`
    // tag must produce a clear deserialize error, not silently
    // lose data.
    let bad = b"\x82\xa4kind\xb1unknown_event_xyz\xa7payload\x80";
    let r = rmp_serde::from_slice::<EventFrame>(bad);
    assert!(r.is_err(), "unknown kind should fail to decode");
}

#[test]
fn pool_state_carries_used_and_available() {
    let f = EventFrame {
        ts: ts(),
        event: Event::PoolState(PoolState {
            used: ResourceAmount { memory_bytes: 1024, cpus: 2 },
            available: ResourceAmount { memory_bytes: 8192, cpus: 8 },
        }),
    };
    let r = round_trip(&f);
    assert_eq!(r, f);
}
