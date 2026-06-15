//! Conformance: typed handle wrappers per `provium_protocol::handle`.
//! All three handle types (file/process/worker) round-trip
//! transparently as `u64` on the wire while staying distinct
//! types at the Rust API.

use provium_protocol::handle::{FileHandle, ProcessHandle, WorkerHandle};

#[test]
fn file_handle_serializes_as_bare_u64() {
    let h = FileHandle::new(42);
    let bytes = rmp_serde::to_vec_named(&h).unwrap();
    let raw: u64 = rmp_serde::from_slice(&bytes).unwrap();
    assert_eq!(raw, 42);
}

#[test]
fn process_handle_serializes_as_bare_u64() {
    let h = ProcessHandle::new(7);
    let bytes = rmp_serde::to_vec_named(&h).unwrap();
    let raw: u64 = rmp_serde::from_slice(&bytes).unwrap();
    assert_eq!(raw, 7);
}

#[test]
fn worker_handle_serializes_as_bare_u64() {
    let h = WorkerHandle::new(99);
    let bytes = rmp_serde::to_vec_named(&h).unwrap();
    let raw: u64 = rmp_serde::from_slice(&bytes).unwrap();
    assert_eq!(raw, 99);
}

#[test]
fn handles_round_trip_back_to_typed() {
    let h = FileHandle::new(123);
    let bytes = rmp_serde::to_vec_named(&h).unwrap();
    let back: FileHandle = rmp_serde::from_slice(&bytes).unwrap();
    assert_eq!(back, h);
}

#[test]
fn display_includes_kind_prefix() {
    assert_eq!(FileHandle::new(7).to_string(), "file#7");
    assert_eq!(ProcessHandle::new(7).to_string(), "proc#7");
    assert_eq!(WorkerHandle::new(7).to_string(), "worker#7");
}

#[test]
fn handles_with_same_id_compare_equal_within_type() {
    let a = FileHandle::new(5);
    let b = FileHandle::new(5);
    assert_eq!(a, b);
}

#[test]
fn handles_with_different_ids_are_distinct() {
    let a = FileHandle::new(5);
    let b = FileHandle::new(6);
    assert_ne!(a, b);
}

#[test]
fn handle_zero_is_reserved_no_handle() {
    // The module docstring documents `0` as "no handle". The
    // type doesn't enforce it but downstream code MUST treat
    // zero as the sentinel — the wire shape allows it.
    let h = FileHandle::new(0);
    assert_eq!(h.get(), 0);
}

#[test]
fn handle_default_is_zero() {
    let h: FileHandle = Default::default();
    assert_eq!(h.get(), 0,
        "Default must be the reserved no-handle id");
}

#[test]
fn handle_get_returns_underlying_u64() {
    assert_eq!(FileHandle::new(u64::MAX).get(), u64::MAX);
    assert_eq!(WorkerHandle::new(1).get(), 1);
}

#[test]
fn handle_is_copy_and_clone() {
    let h = FileHandle::new(1);
    let h2 = h;     // Copy
    let h3 = h.clone();
    assert_eq!(h, h2);
    assert_eq!(h, h3);
}

#[test]
fn distinct_kinds_serialize_compatibly_at_wire_layer() {
    // Same underlying id; same wire bytes; different Rust types.
    // Confirms that the kind discrimination is host-side only.
    let bytes_file = rmp_serde::to_vec_named(&FileHandle::new(7)).unwrap();
    let bytes_proc = rmp_serde::to_vec_named(&ProcessHandle::new(7)).unwrap();
    let bytes_worker = rmp_serde::to_vec_named(&WorkerHandle::new(7)).unwrap();
    assert_eq!(bytes_file, bytes_proc);
    assert_eq!(bytes_proc, bytes_worker);
}
