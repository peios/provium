//! Errors raised by the host's agent client.

use std::io;

use thiserror::Error;

use provium_protocol::wire::AgentError;
use provium_protocol::FrameError;

/// Failure modes when talking to the in-VM agent.
#[derive(Debug, Error)]
pub enum ClientError {
    /// Failed to open a connection to the agent (vsock connect, dial,
    /// etc.).
    #[error("connect to agent: {0}")]
    Connect(io::Error),

    /// Codec failure on a frame.
    #[error("frame: {0}")]
    Frame(#[from] FrameError),

    /// Hello handshake failed because the host and agent disagree on
    /// the protocol version.
    #[error("protocol version mismatch (host: {host}, agent: {agent})")]
    VersionMismatch {
        /// Version compiled into the host.
        host: u32,
        /// Version the agent reports.
        agent: u32,
    },

    /// The agent responded with a different message than the op
    /// expected. Indicates a bug in the host or the agent — the
    /// connection is unsalvageable.
    #[error("agent sent `{actual}` where `{expected}` was expected")]
    UnexpectedMessage {
        /// `kind` of the message we wanted.
        expected: &'static str,
        /// `kind` of the message we got.
        actual: &'static str,
    },

    /// Envelope-level agent error (UnknownHandle, Unsupported,
    /// BadRequest, Internal). Distinct from per-op
    /// [`provium_protocol::OsError`] returns inside [`OpResult`]s.
    ///
    /// [`OpResult`]: provium_protocol::wire::OpResult
    #[error("agent error: {0}")]
    Agent(#[from] AgentError),

    /// A streaming op observed a [`provium_protocol::wire::StreamEnd::Error`]
    /// — the source on the agent side reported a mid-stream OS
    /// failure. Distinct from a wire-codec error so the host can
    /// surface the underlying errno cleanly.
    #[error("stream source error: {0}")]
    StreamSource(provium_protocol::OsError),
}
