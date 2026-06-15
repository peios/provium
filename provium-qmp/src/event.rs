//! QMP async events and the sequence-mark machinery used to wait for
//! them safely on a long-lived connection.

use serde::Deserialize;
use serde_json::Value;

/// One async event emitted by QEMU.
///
/// QEMU's wire shape is `{"event": "<name>", "data": {...},
/// "timestamp": {"seconds": ..., "microseconds": ...}}`. We rename
/// `event` to `name` for ergonomics and flatten the timestamp into
/// nanoseconds since the Unix epoch.
#[derive(Clone, Debug, PartialEq)]
pub struct Event {
    /// The QEMU event name, e.g. `"MIGRATION"`, `"STOP"`, `"RESUME"`.
    pub name: String,
    /// Event payload (the `data` field). May be `Null` for parameterless
    /// events. Consumers traverse this with `serde_json::Value` accessors
    /// or [`serde_json::from_value`] into a typed struct.
    pub data: Value,
    /// Wall-clock timestamp from QEMU in nanoseconds since the Unix
    /// epoch. Computed from QEMU's `seconds` + `microseconds` pair.
    pub timestamp_ns: i64,
}

/// A snapshot of the event-log length, taken before issuing a command
/// whose completion is signalled asynchronously.
///
/// Pass to [`crate::Qmp::wait_event`] as the `since` parameter — only
/// events appended *after* this mark will match. Without this
/// discipline, a long-lived connection re-uses stale events from prior
/// operations: `wait_event("MIGRATION", status="completed")` would
/// instantly match the previous cycle's already-completed event,
/// producing a truncated snapshot.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct EventMark(pub(crate) usize);

// ---------------------------------------------------------------------------
// Wire-shaped types used internally by the reader to parse incoming
// frames before fanning them out as Events / responses.
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
pub(crate) struct WireTimestamp {
    pub(crate) seconds: i64,
    pub(crate) microseconds: i64,
}

impl WireTimestamp {
    pub(crate) fn to_ns(&self) -> i64 {
        // i64 holds ~292 years of nanoseconds — fine for any timestamp
        // QEMU is going to produce.
        self.seconds
            .saturating_mul(1_000_000_000)
            .saturating_add(self.microseconds.saturating_mul(1_000))
    }
}

#[derive(Deserialize)]
pub(crate) struct WireEvent {
    pub(crate) event: String,
    #[serde(default)]
    pub(crate) data: Value,
    pub(crate) timestamp: WireTimestamp,
}

impl From<WireEvent> for Event {
    fn from(w: WireEvent) -> Self {
        Self {
            name: w.event,
            data: w.data,
            timestamp_ns: w.timestamp.to_ns(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timestamp_to_ns_combines_seconds_and_microseconds() {
        let ts = WireTimestamp {
            seconds: 1_700_000_000,
            microseconds: 500_000,
        };
        assert_eq!(ts.to_ns(), 1_700_000_000_500_000_000);
    }

    #[test]
    fn wire_event_parses_full_qmp_shape() {
        let raw = r#"{
            "event": "MIGRATION",
            "data": {"status": "completed"},
            "timestamp": {"seconds": 1234567890, "microseconds": 1000}
        }"#;
        let parsed: WireEvent = serde_json::from_str(raw).unwrap();
        assert_eq!(parsed.event, "MIGRATION");
        assert_eq!(parsed.data["status"], "completed");
        let event: Event = parsed.into();
        // 1234567890s + 1000μs = 1234567890s + 1ms.
        assert_eq!(event.timestamp_ns, 1_234_567_890_001_000_000);
    }

    #[test]
    fn wire_event_parses_with_missing_data() {
        let raw = r#"{
            "event": "STOP",
            "timestamp": {"seconds": 0, "microseconds": 0}
        }"#;
        let parsed: WireEvent = serde_json::from_str(raw).unwrap();
        assert!(parsed.data.is_null());
    }
}
