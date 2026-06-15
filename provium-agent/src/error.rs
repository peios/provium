//! Agent-internal errors — distinct from the wire-shaped
//! [`provium_protocol::OsError`] that op handlers return as part of a
//! [`provium_protocol::wire::OpResult`].
//!
//! [`AgentRuntimeError`] surfaces unrecoverable problems on a *connection*:
//! a frame failed to encode, the peer dropped mid-handshake, etc.
//! These never reach the host as op results — the connection is
//! already gone — so they don't derive [`serde::Serialize`].

use provium_protocol::FrameError;
use thiserror::Error;

/// A failure observed while servicing a single connection.
#[derive(Debug, Error)]
pub enum AgentRuntimeError {
    /// Codec error reading or writing a frame.
    #[error("frame: {0}")]
    Frame(#[from] FrameError),

    /// A frame arrived in a state where it shouldn't have (e.g. an op
    /// before the handshake completed). Connection should be torn down.
    #[error("protocol violation: {0}")]
    Protocol(&'static str),
}
