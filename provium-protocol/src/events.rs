//! Observability event stream emitted by the host scheduler and runners.
//!
//! Provium's text and JSON outputs, the deferred TUI, `provium-coverage`,
//! and any third-party tool consume the same stream — these types are
//! the **public API** for that contract. Adding a field is a strict
//! version bump (see [`crate::PROTOCOL_VERSION`]); removing or renaming
//! one is a breaking change.
//!
//! Each frame on the wire is an [`EventFrame`] encoded as msgpack via
//! the same length-prefixed framing used for the wire protocol
//! ([`crate::frame`]).
//!
//! # Wire shape
//!
//! [`EventFrame`] is `{ts: i64, kind: ..., payload: ...}`. The `ts` is
//! nanoseconds since the Unix epoch on the host, captured at emission
//! time. The `kind` discriminator is taken from the inner [`Event`]
//! variant; the matching payload struct lives alongside in this module.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// One frame in the event stream — a timestamp paired with an [`Event`].
///
/// Serializes as `{ts, kind, payload}` (the inner enum is flattened
/// into the envelope), matching the design-document specification.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct EventFrame {
    /// Nanoseconds since the Unix epoch when the host emitted the
    /// event. Use a signed integer so pre-epoch is at least
    /// representable for unusual host clocks.
    pub ts: i64,
    /// The event payload itself. Flattened into this struct so the
    /// `kind` and `payload` keys appear at the top level of the frame.
    #[serde(flatten)]
    pub event: Event,
}

/// The taxonomy of events the scheduler and runners emit.
///
/// Variants are added (never reordered or removed without a version
/// bump). Adding a field within a payload struct is also a version
/// bump — consumers depend on the shape.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "payload", rename_all = "snake_case")]
pub enum Event {
    /// A test file was discovered during the pre-run scan. Emitted
    /// once per `*.test.lua` file before any dispatching begins.
    FileDiscovered(FileDiscovered),
    /// A test file was picked up by a runner thread.
    FileDispatched(FileDispatched),
    /// A test file is waiting on the resource pool.
    FileBlocked(FileBlocked),
    /// A test file finished — successfully, with failures, or
    /// terminated by timeout / panic.
    FileCompleted(FileCompleted),

    /// A test (a `test()` block within a file) started.
    TestStarted(TestStarted),
    /// Test was filtered or marked Skipped (declarative `meta.skip`,
    /// inline `t:skip()`, file-scope `todo()`, or tag/slow filter).
    /// Distinct from [`Self::TestPassed`] so coverage / dashboard
    /// consumers can differentiate.
    TestSkipped(TestSkipped),
    /// A test passed.
    TestPassed(TestPassed),
    /// A test failed (assertion, exception, panic, or
    /// poisoned-state cascade).
    TestFailed(TestFailed),

    /// A VM was spawned for a test file.
    VmSpawned(VmSpawned),
    /// A VM was shut down or otherwise reaped.
    VmShutdown(VmShutdown),

    /// Periodic resource-pool usage snapshot.
    PoolState(PoolState),

    /// A file successfully claimed resources via `provium:claim(...)`.
    ClaimAcquired(ClaimAcquired),
    /// A file released its claim (file end).
    ClaimReleased(ClaimReleased),

    /// Started building a fixture from its `.fixture.lua`.
    FixtureBuildStarted(FixtureBuildStarted),
    /// Finished building a fixture; its snapshot is now cached.
    FixtureBuildDone(FixtureBuildDone),
    /// A second file is waiting on the build lock another file holds.
    FixtureBuildWaiting(FixtureBuildWaiting),
    /// A fixture was resumed from its cached snapshot — no rebuild
    /// required.
    FixtureCacheHit(FixtureCacheHit),
}

// ---------------------------------------------------------------------------
// File-lifecycle events
// ---------------------------------------------------------------------------

/// Payload for [`Event::FileDiscovered`].
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct FileDiscovered {
    /// Test root–relative path of the file.
    pub path: String,
    /// Fixtures referenced by the file (paths, with `.fixture.lua`
    /// already stripped per the design).
    pub fixture_refs: Vec<String>,
    /// `provium:claim(...)` declared at file scope, if any.
    pub declared_claim: Option<ResourceAmount>,
}

/// Payload for [`Event::FileDispatched`].
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct FileDispatched {
    /// Test root–relative path of the file.
    pub path: String,
    /// Resources reserved by the file at dispatch (sum of runner
    /// overhead + claim).
    pub reservation: ResourceAmount,
}

/// Payload for [`Event::FileBlocked`].
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct FileBlocked {
    /// Test root–relative path of the file.
    pub path: String,
    /// What the file is waiting on.
    pub waiting_for: ResourceAmount,
    /// Why the file blocked: `pool_full`, `psi_pressure`, …
    #[serde(default)]
    pub reason: String,
}

/// Payload for [`Event::FileCompleted`].
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct FileCompleted {
    /// Test root–relative path of the file.
    pub path: String,
    /// Final disposition.
    pub status: FileStatus,
    /// Wall-clock duration in nanoseconds, host-side.
    pub duration_ns: u64,
}

/// Final disposition of a test file.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FileStatus {
    /// All tests passed.
    Passed,
    /// At least one test failed; runner exited cleanly.
    Failed,
    /// Runner exceeded its per-file timeout and was killed.
    TimedOut,
    /// Runner panicked or otherwise hit a test-infrastructure failure;
    /// remaining tests in the file are marked
    /// `failed-due-to-poisoned-state`.
    Crashed,
}

// ---------------------------------------------------------------------------
// Test-lifecycle events
// ---------------------------------------------------------------------------

/// Payload for [`Event::TestStarted`].
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TestStarted {
    /// Test root–relative path of the containing file.
    pub path: String,
    /// `test()` block name.
    pub name: String,
    /// Per-test metadata (`spec`, `tags`, `timeout`, anything else).
    /// Always present; empty if the test had no metadata.
    #[serde(default)]
    pub meta: MetaMap,
}

/// Payload for [`Event::TestSkipped`].
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TestSkipped {
    /// Test root–relative path of the containing file.
    pub path: String,
    /// `test()` block name.
    pub name: String,
    /// Skip reason: filter expression, `t:skip()` argument, or
    /// `todo()` argument.
    pub reason: String,
    /// Per-test metadata.
    #[serde(default)]
    pub meta: MetaMap,
}

/// Payload for [`Event::TestPassed`].
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TestPassed {
    /// Test root–relative path of the containing file.
    pub path: String,
    /// `test()` block name.
    pub name: String,
    /// Wall-clock duration in nanoseconds.
    pub duration_ns: u64,
    /// Per-test metadata.
    #[serde(default)]
    pub meta: MetaMap,
}

/// Payload for [`Event::TestFailed`].
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TestFailed {
    /// Test root–relative path of the containing file.
    pub path: String,
    /// `test()` block name.
    pub name: String,
    /// Wall-clock duration up to the failure point.
    pub duration_ns: u64,
    /// Failure description (assertion message, exception text, panic
    /// payload).
    pub reason: String,
    /// Recent console output captured at the failure moment, useful
    /// for diagnostic. May be empty.
    pub console_excerpt: String,
    /// Per-test metadata.
    #[serde(default)]
    pub meta: MetaMap,
}

// ---------------------------------------------------------------------------
// VM-lifecycle events
// ---------------------------------------------------------------------------

/// Payload for [`Event::VmSpawned`].
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct VmSpawned {
    /// File that owns this VM.
    pub file: String,
    /// VM name as given to `lab:vm(...)`.
    pub vm_name: String,
    /// Profile name used to boot.
    pub profile: String,
    /// VM memory cap in bytes.
    pub memory_bytes: u64,
    /// vsock CID assigned to this VM.
    pub cid: u32,
}

/// Payload for [`Event::VmShutdown`].
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct VmShutdown {
    /// File that owned this VM.
    pub file: String,
    /// VM name.
    pub vm_name: String,
    /// VM uptime in nanoseconds.
    pub duration_ns: u64,
}

// ---------------------------------------------------------------------------
// Resource events
// ---------------------------------------------------------------------------

/// Payload for [`Event::PoolState`]. Emitted on a periodic cadence
/// (default 1 Hz).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PoolState {
    /// Currently in use.
    pub used: ResourceAmount,
    /// Currently free.
    pub available: ResourceAmount,
}

/// Payload for [`Event::ClaimAcquired`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClaimAcquired {
    /// Human-readable path / name of the file or lab the claim
    /// belongs to.
    #[serde(default)]
    pub path: String,
    /// Amount claimed.
    pub amount: ResourceAmount,
}

/// Payload for [`Event::ClaimReleased`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClaimReleased {
    /// Human-readable path / name of the file or lab the claim
    /// belonged to. Pairs with the [`ClaimAcquired.path`].
    #[serde(default)]
    pub path: String,
    /// Amount released.
    pub amount: ResourceAmount,
}

// ---------------------------------------------------------------------------
// Fixture events
// ---------------------------------------------------------------------------

/// Payload for [`Event::FixtureBuildStarted`].
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct FixtureBuildStarted {
    /// Fixture path (test-root relative, no `.fixture.lua`).
    pub path: String,
}

/// Payload for [`Event::FixtureBuildDone`].
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct FixtureBuildDone {
    /// Fixture path.
    pub path: String,
    /// Build duration in nanoseconds.
    pub duration_ns: u64,
    /// Cached snapshot size in bytes (post-compression).
    pub snapshot_bytes: u64,
}

/// Payload for [`Event::FixtureBuildWaiting`].
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct FixtureBuildWaiting {
    /// Fixture path the file is waiting for.
    pub path: String,
    /// File currently holding the build lock.
    pub held_by_file: String,
}

/// Payload for [`Event::FixtureCacheHit`].
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct FixtureCacheHit {
    /// Fixture path that was resumed from cache.
    pub path: String,
}

// ---------------------------------------------------------------------------
// Shared scalar / map types
// ---------------------------------------------------------------------------

/// Memory + CPU pair used in pool / claim / reservation events.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResourceAmount {
    /// Memory in bytes.
    pub memory_bytes: u64,
    /// vCPUs.
    pub cpus: u32,
}

/// Per-test metadata blob.
///
/// Provium core treats this as opaque; the consumer (e.g.
/// `provium-coverage`) reads whatever keys it cares about. The runner
/// itself only inspects a handful of well-known keys before storing the
/// blob (`slow`, `tags`, `timeout`).
pub type MetaMap = BTreeMap<String, MetaValue>;

/// Recursive value type for [`MetaMap`].
///
/// Self-contained — no `serde_json::Value` dependency, so the agent
/// build (which does not consume events) is unaffected.
///
/// On the wire this encodes to native msgpack types (`bin` distinct
/// from `str`, `int` distinct from `float`); the manual [`Deserialize`]
/// impl below dispatches on the input type rather than relying on
/// `#[serde(untagged)]` declaration-order fallthrough, which would
/// mis-classify valid-UTF-8 byte sequences as strings.
#[derive(Clone, Debug, PartialEq)]
pub enum MetaValue {
    /// `nil` in Lua, `null` in JSON.
    Null,
    /// Boolean.
    Bool(bool),
    /// Signed integer.
    Int(i64),
    /// Floating-point. Treated as `PartialEq` only — NaN comparisons
    /// follow IEEE rules.
    Float(f64),
    /// Text.
    Str(String),
    /// Raw bytes.
    Bytes(Vec<u8>),
    /// Sequence of values.
    Array(Vec<MetaValue>),
    /// Keyed map of values.
    Map(BTreeMap<String, MetaValue>),
}

impl serde::Serialize for MetaValue {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        match self {
            MetaValue::Null => s.serialize_unit(),
            MetaValue::Bool(b) => s.serialize_bool(*b),
            MetaValue::Int(i) => s.serialize_i64(*i),
            MetaValue::Float(f) => s.serialize_f64(*f),
            MetaValue::Str(t) => s.serialize_str(t),
            MetaValue::Bytes(b) => s.serialize_bytes(b),
            MetaValue::Array(a) => a.serialize(s),
            MetaValue::Map(m) => m.serialize(s),
        }
    }
}

impl<'de> serde::Deserialize<'de> for MetaValue {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct V;

        impl<'de> serde::de::Visitor<'de> for V {
            type Value = MetaValue;

            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("a meta value (null/bool/int/float/str/bytes/array/map)")
            }

            fn visit_unit<E>(self) -> Result<MetaValue, E> {
                Ok(MetaValue::Null)
            }
            fn visit_none<E>(self) -> Result<MetaValue, E> {
                Ok(MetaValue::Null)
            }
            fn visit_some<D: serde::Deserializer<'de>>(self, d: D) -> Result<MetaValue, D::Error> {
                MetaValue::deserialize(d)
            }

            fn visit_bool<E>(self, v: bool) -> Result<MetaValue, E> {
                Ok(MetaValue::Bool(v))
            }

            fn visit_i64<E>(self, v: i64) -> Result<MetaValue, E> {
                Ok(MetaValue::Int(v))
            }
            fn visit_i128<E: serde::de::Error>(self, v: i128) -> Result<MetaValue, E> {
                i64::try_from(v)
                    .map(MetaValue::Int)
                    .map_err(|_| E::custom("i128 out of range for MetaValue::Int"))
            }
            fn visit_u64<E: serde::de::Error>(self, v: u64) -> Result<MetaValue, E> {
                i64::try_from(v)
                    .map(MetaValue::Int)
                    .map_err(|_| E::custom("u64 out of range for MetaValue::Int"))
            }
            fn visit_u128<E: serde::de::Error>(self, v: u128) -> Result<MetaValue, E> {
                i64::try_from(v)
                    .map(MetaValue::Int)
                    .map_err(|_| E::custom("u128 out of range for MetaValue::Int"))
            }

            fn visit_f64<E>(self, v: f64) -> Result<MetaValue, E> {
                Ok(MetaValue::Float(v))
            }

            fn visit_str<E>(self, v: &str) -> Result<MetaValue, E> {
                Ok(MetaValue::Str(v.to_owned()))
            }
            fn visit_string<E>(self, v: String) -> Result<MetaValue, E> {
                Ok(MetaValue::Str(v))
            }

            fn visit_bytes<E>(self, v: &[u8]) -> Result<MetaValue, E> {
                Ok(MetaValue::Bytes(v.to_vec()))
            }
            fn visit_byte_buf<E>(self, v: Vec<u8>) -> Result<MetaValue, E> {
                Ok(MetaValue::Bytes(v))
            }

            fn visit_seq<A: serde::de::SeqAccess<'de>>(
                self,
                mut seq: A,
            ) -> Result<MetaValue, A::Error> {
                let mut items = Vec::with_capacity(seq.size_hint().unwrap_or(0));
                while let Some(item) = seq.next_element()? {
                    items.push(item);
                }
                Ok(MetaValue::Array(items))
            }

            fn visit_map<A: serde::de::MapAccess<'de>>(
                self,
                mut map: A,
            ) -> Result<MetaValue, A::Error> {
                let mut out = BTreeMap::new();
                while let Some((k, v)) = map.next_entry::<String, MetaValue>()? {
                    out.insert(k, v);
                }
                Ok(MetaValue::Map(out))
            }
        }

        d.deserialize_any(V)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn round_trip<T>(value: &T) -> T
    where
        T: serde::Serialize + serde::de::DeserializeOwned,
    {
        let bytes = rmp_serde::to_vec_named(value).unwrap();
        rmp_serde::from_slice(&bytes).unwrap()
    }

    #[test]
    fn event_frame_carries_ts_kind_and_payload() {
        let frame = EventFrame {
            ts: 1_700_000_000_000_000_000,
            event: Event::TestPassed(TestPassed {
                path: "tests/foo.test.lua".into(),
                name: "happy path".into(),
                duration_ns: 12_500_000,
                meta: MetaMap::new(),
            }),
        };

        let bytes = rmp_serde::to_vec_named(&frame).unwrap();
        let decoded: EventFrame = rmp_serde::from_slice(&bytes).unwrap();
        assert_eq!(frame, decoded);
    }

    #[test]
    fn meta_value_round_trips_each_variant() {
        let cases = vec![
            MetaValue::Null,
            MetaValue::Bool(true),
            MetaValue::Int(-7),
            MetaValue::Float(2.5),
            MetaValue::Str("hello".into()),
            MetaValue::Bytes(vec![1, 2, 3]),
            MetaValue::Array(vec![MetaValue::Int(1), MetaValue::Str("two".into())]),
            MetaValue::Map({
                let mut m = BTreeMap::new();
                m.insert("k".into(), MetaValue::Int(1));
                m
            }),
        ];
        for v in cases {
            assert_eq!(v, round_trip(&v));
        }
    }

    #[test]
    fn meta_with_spec_key_round_trips() {
        let mut meta = MetaMap::new();
        meta.insert(
            "spec".into(),
            MetaValue::Str("PSD-KACS §4.2.1.1".into()),
        );
        meta.insert(
            "tags".into(),
            MetaValue::Array(vec![MetaValue::Str("federation".into())]),
        );

        let event = Event::TestPassed(TestPassed {
            path: "tests/kacs/issue.test.lua".into(),
            name: "creates a token".into(),
            duration_ns: 50_000_000,
            meta,
        });

        let bytes = rmp_serde::to_vec_named(&event).unwrap();
        let decoded: Event = rmp_serde::from_slice(&bytes).unwrap();
        assert_eq!(event, decoded);
    }

    #[test]
    fn pool_state_is_a_pure_value_type() {
        let s = PoolState {
            used: ResourceAmount {
                memory_bytes: 4 * 1024 * 1024 * 1024,
                cpus: 8,
            },
            available: ResourceAmount {
                memory_bytes: 12 * 1024 * 1024 * 1024,
                cpus: 24,
            },
        };
        assert_eq!(s, round_trip(&s));
    }

    #[test]
    fn file_completed_status_variants_round_trip() {
        for status in [
            FileStatus::Passed,
            FileStatus::Failed,
            FileStatus::TimedOut,
            FileStatus::Crashed,
        ] {
            let f = Event::FileCompleted(FileCompleted {
                path: "tests/x.test.lua".into(),
                status,
                duration_ns: 0,
            });
            assert_eq!(f, round_trip(&f));
        }
    }

    #[test]
    fn frame_codec_round_trips_events() {
        use crate::frame::{read_frame, write_frame, DEFAULT_MAX_FRAME_BYTES};
        use std::io::Cursor;

        let mut buf = Vec::new();
        let f1 = EventFrame {
            ts: 100,
            event: Event::FileDiscovered(FileDiscovered {
                path: "a.test.lua".into(),
                fixture_refs: vec![],
                declared_claim: None,
            }),
        };
        let f2 = EventFrame {
            ts: 200,
            event: Event::PoolState(PoolState {
                used: ResourceAmount::default(),
                available: ResourceAmount {
                    memory_bytes: 1024,
                    cpus: 4,
                },
            }),
        };
        write_frame(&mut buf, &f1, DEFAULT_MAX_FRAME_BYTES).unwrap();
        write_frame(&mut buf, &f2, DEFAULT_MAX_FRAME_BYTES).unwrap();

        let mut cursor = Cursor::new(&buf);
        let d1: EventFrame = read_frame(&mut cursor, DEFAULT_MAX_FRAME_BYTES).unwrap();
        let d2: EventFrame = read_frame(&mut cursor, DEFAULT_MAX_FRAME_BYTES).unwrap();
        assert_eq!(d1, f1);
        assert_eq!(d2, f2);
    }
}
