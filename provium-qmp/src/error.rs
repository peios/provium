//! Error types for QMP operations.

use std::io;
use std::time::Duration;

use thiserror::Error;

/// Failure modes when talking to a QEMU monitor.
#[derive(Debug, Error)]
pub enum QmpError {
    /// I/O failure on the unix socket.
    #[error("qmp I/O: {0}")]
    Io(#[from] io::Error),

    /// QEMU sent something that is not valid JSON or doesn't match
    /// the QMP shape we expect.
    #[error("malformed QMP message: {0}")]
    Decode(#[from] serde_json::Error),

    /// QEMU rejected the command — `error.class` and `error.desc` from
    /// the QMP error reply are surfaced verbatim. Typical examples:
    /// `GenericError`, `CommandNotFound`, `DeviceNotFound`.
    #[error("qemu rejected `{command}`: {class}: {desc}")]
    Command {
        /// Name of the command that was rejected, for context.
        command: String,
        /// QEMU's error class.
        class: String,
        /// QEMU's free-form error description.
        desc: String,
    },

    /// The connection has been closed (clean QMP `quit`, the QEMU
    /// process exited, the socket was severed, or the reader thread
    /// observed an unrecoverable I/O error). All further operations on
    /// this connection will return this error.
    #[error("QMP connection closed: {0}")]
    Closed(&'static str),

    /// A blocking call (`execute`, `wait_event`) did not complete
    /// inside its configured timeout.
    #[error("QMP operation timed out after {0:?}")]
    Timeout(Duration),

    /// Greeting / capability negotiation failed at connect time.
    #[error("QMP handshake failed: {0}")]
    Handshake(String),
}

impl QmpError {
    /// `true` when the connection is no longer usable. Helpful for the
    /// host scheduler's "treat the VM as dead" branch.
    pub fn is_terminal(&self) -> bool {
        matches!(self, Self::Closed(_) | Self::Io(_))
    }
}
