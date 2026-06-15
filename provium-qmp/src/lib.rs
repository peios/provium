//! # provium-qmp
//!
//! Synchronous Rust client for QEMU's [Machine Protocol][qmp] (QMP),
//! used by the provium host to drive snapshot, restore, pause / resume,
//! and any other VMM control flow that goes through QEMU's monitor.
//!
//! The crate is **host-only** by intent: the provium agent runs *inside*
//! QEMU and never speaks QMP, so the QMP code is split out from
//! `provium-protocol` to keep the agent's musl-static binary as small
//! as possible.
//!
//! ## Threading model
//!
//! Each [`Qmp`] connection owns one reader thread that pulls JSON
//! lines off the socket, parses them, and dispatches:
//!
//! * Command responses (`return` / `error`) are matched by id to the
//!   waiting [`Qmp::execute`] caller, which is unblocked via a condvar.
//! * Async [`Event`]s are appended to a shared event log and broadcast
//!   to any [`Qmp::wait_event`] callers.
//!
//! Senders (`execute`, `event_mark`, `wait_event`) all return
//! synchronously; the reader thread is opaque to callers.
//!
//! ## Production gotchas baked in
//!
//! Three patterns from the QEMU spike are mandatory and the crate
//! handles them so the consumer doesn't have to:
//!
//! 1. **`migrate-set-capabilities` with `events: true`** is sent
//!    automatically after `qmp_capabilities`. Without it, MIGRATION
//!    state transitions are not emitted and the only signal is polling
//!    `query-migrate`.
//!
//! 2. **Event sequence marks.** [`Qmp::event_mark`] returns the current
//!    event-log length; [`Qmp::wait_event`] takes a mark and only
//!    matches events appended *after* it. Without this discipline a
//!    long-lived QMP connection re-uses stale `MIGRATION completed`
//!    events from earlier cycles and returns immediately, producing
//!    truncated snapshots.
//!
//! 3. **`migrate file:<path>`, not `exec:cat > file`.** The exec form
//!    has a flush race because the shell child may still be writing
//!    when the migration-completed event fires. Use [`Qmp::migrate`]
//!    with a regular path; the `file:` URI is built internally.
//!
//! [qmp]: https://wiki.qemu.org/Documentation/QMP

#![deny(missing_debug_implementations)]
#![warn(missing_docs)]
#![warn(rust_2018_idioms)]
#![warn(unreachable_pub)]

mod connection;
mod error;
mod event;

pub use connection::{Qmp, DEFAULT_COMMAND_TIMEOUT};
pub use error::QmpError;
pub use event::{Event, EventMark};
