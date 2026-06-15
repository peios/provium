//! Error types shared by the wire and event modules.
//!
//! There are three layers here, each with distinct ownership:
//!
//! * [`FrameError`] — codec layer. Local to the reader/writer; never
//!   crosses the wire. Returned by [`crate::frame::read_frame`] and
//!   [`crate::frame::write_frame`].
//! * [`ProtocolError`] — handshake / envelope-layer violations local to
//!   the host or agent. Things like "agent and host disagree on version"
//!   or "received a response when we sent no request".
//! * [`OsError`] — appears *inside* op response payloads to convey OS
//!   failure (errno + a strerror message) back to the test author. Lives
//!   on the wire; derives [`serde::Serialize`] / [`serde::Deserialize`].

use std::io;

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Errors raised by the framing codec ([`crate::frame`]).
///
/// All variants represent unrecoverable conditions for the current
/// connection: the caller should drop the stream and surface the error
/// to the test author as an infrastructure failure.
#[derive(Debug, Error)]
pub enum FrameError {
    /// I/O failure on the underlying byte stream.
    #[error("frame I/O: {0}")]
    Io(#[from] io::Error),

    /// The peer closed the connection before the frame header was
    /// received. Distinguished from a mid-frame EOF because a clean EOF
    /// at frame boundary is the normal end-of-conversation signal for
    /// short ops.
    #[error("connection closed at frame boundary")]
    Eof,

    /// The peer closed the connection partway through a frame's
    /// length-prefix or body. Always an error — implies the peer crashed
    /// or was killed mid-write.
    #[error("connection closed mid-frame after {bytes_read} bytes (expected {expected})")]
    UnexpectedEof {
        /// Bytes read before EOF.
        bytes_read: usize,
        /// Bytes the codec was waiting on.
        expected: usize,
    },

    /// A frame's declared length exceeded the configured cap.
    /// Defensive: prevents a malicious or buggy peer from forcing the
    /// reader to allocate unbounded memory.
    #[error("frame too large: {len} bytes (max {max})")]
    FrameTooLarge {
        /// Length declared in the frame header.
        len: usize,
        /// Configured per-frame cap.
        max: usize,
    },

    /// The frame body did not deserialize as the expected type.
    #[error("decode: {0}")]
    Decode(#[from] rmp_serde::decode::Error),

    /// The message could not be encoded (almost always a programmer
    /// bug — `rmp-serde` only fails on serializer-rejected shapes).
    #[error("encode: {0}")]
    Encode(#[from] rmp_serde::encode::Error),
}

/// Higher-level protocol violations the codec can't catch on its own.
#[derive(Debug, Error)]
pub enum ProtocolError {
    /// Hello handshake declared a different protocol version on each side.
    /// The connection must be torn down; no recovery is possible.
    #[error("protocol version mismatch (host: {host}, agent: {agent})")]
    VersionMismatch {
        /// Version the host advertised.
        host: u32,
        /// Version the agent advertised.
        agent: u32,
    },

    /// First message on the connection was not a [`crate::wire::Hello`].
    #[error("expected hello as first message, got {actual}")]
    HandshakeMissing {
        /// `serde` discriminator of the message that arrived instead.
        actual: &'static str,
    },

    /// A message arrived in a state where it was not valid (e.g. a stream
    /// frame on a connection that never opened a stream).
    #[error("unexpected message in state {state:?}: {actual}")]
    UnexpectedMessage {
        /// Connection state at the time of receipt.
        state: &'static str,
        /// `serde` discriminator of the offending message.
        actual: &'static str,
    },
}

/// An OS-level failure observed by the agent while servicing a wire op.
///
/// Carries the raw errno and the agent's strerror rendering. The errno
/// numbering follows the agent's OS — for the v1 Peios and Linux ports
/// these are identical Linux ABI values; future ports translate at the
/// agent boundary.
///
/// Lives on the wire as part of op response payloads. Lua bindings on
/// the host map `errno` to symbolic names (`"ENOENT"`, `"EACCES"`) for
/// test-author ergonomics.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct OsError {
    /// Numeric errno (Linux ABI on v1).
    pub errno: i32,
    /// strerror-style description. Free-form; do not match against it.
    pub message: String,
}

impl OsError {
    /// Build an [`OsError`] from the current `errno` value at the agent.
    ///
    /// Wraps [`std::io::Error::last_os_error`] and renders its `Display`
    /// implementation as the message.
    pub fn last_os_error() -> Self {
        let err = io::Error::last_os_error();
        Self {
            errno: err.raw_os_error().unwrap_or(0),
            message: err.to_string(),
        }
    }

    /// Build an [`OsError`] from an explicit errno value, deriving the
    /// human-readable message via the platform's `strerror`.
    pub fn from_errno(errno: i32) -> Self {
        let err = io::Error::from_raw_os_error(errno);
        Self {
            errno,
            message: err.to_string(),
        }
    }
}

impl std::fmt::Display for OsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} (errno {})", self.message, self.errno)
    }
}

impl std::error::Error for OsError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn os_error_round_trips_through_msgpack() {
        let original = OsError {
            errno: 2,
            message: "No such file or directory (os error 2)".into(),
        };
        let bytes = rmp_serde::to_vec_named(&original).unwrap();
        let decoded: OsError = rmp_serde::from_slice(&bytes).unwrap();
        assert_eq!(original, decoded);
    }

    #[test]
    fn os_error_from_errno_renders_strerror() {
        let err = OsError::from_errno(2);
        assert_eq!(err.errno, 2);
        // Don't assert exact message text — strerror output varies by
        // libc — but it should be non-empty.
        assert!(!err.message.is_empty());
    }
}
