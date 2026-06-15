//! # provium-protocol
//!
//! The wire protocol shared between the provium host and `provium-agent`.
//!
//! Two independent message streams live in this crate:
//!
//! * [`wire`] — the bidirectional msgpack protocol the host speaks to the
//!   in-guest agent over vsock. Defines [`wire::HostMessage`],
//!   [`wire::AgentMessage`], the Hello handshake, and the per-op
//!   request/response types.
//! * [`events`] — the unidirectional msgpack stream the host emits to
//!   observability consumers (`provium-coverage`, the deferred TUI,
//!   user-written tools).
//!
//! Both streams use length-prefixed msgpack frames; see [`frame`] for the
//! shared codec.
//!
//! ## Versioning
//!
//! [`PROTOCOL_VERSION`] is bumped whenever any wire-facing struct changes
//! shape. Host and agent compare versions in the Hello exchange and abort
//! on mismatch. Cache keys for fixtures embed the same constant so a
//! stale snapshot is treated as a cache miss rather than restored against
//! an incompatible agent.
//!
//! See `DESIGN.md` (§ Wire protocol, § Observability) for the full
//! rationale.

#![deny(missing_debug_implementations)]
#![warn(missing_docs)]
#![warn(rust_2018_idioms)]
#![warn(unreachable_pub)]

pub mod events;
pub mod frame;
pub mod handle;
pub mod wire;

mod error;

pub use error::{FrameError, OsError, ProtocolError};

/// Wire protocol version. Bumped whenever any struct in [`wire`] or
/// [`events`] changes shape.
///
/// The agent rejects connections whose [`wire::Hello::protocol_version`]
/// does not match this constant. Fixture cache keys include it so stale
/// snapshots are detected as cache misses rather than restored against an
/// incompatible agent.
pub const PROTOCOL_VERSION: u32 = 1;
