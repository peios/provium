//! `tail_file` session — owns the long-lived connection to the agent
//! and yields [`StreamFrame`]s as the agent emits them.

use provium_protocol::wire::{AgentMessage, StreamEnd, StreamFrame};

use crate::connector::AgentStream;
use crate::ClientError;

use super::{recv, unexpected};

/// A `tail_file` session: an open vsock-or-paired connection that
/// belongs to one streaming op for its lifetime.
///
/// Drop the session to close the connection; the agent's stream
/// thread observes the EOF on its next write attempt and exits.
pub struct TailFileSession {
    stream: Box<dyn AgentStream>,
    /// Set once we've seen a clean [`StreamEnd::Eof`]. Subsequent
    /// `next_frame` calls return `Ok(None)` without re-reading.
    eof: bool,
}

impl std::fmt::Debug for TailFileSession {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TailFileSession")
            .field("eof", &self.eof)
            .finish()
    }
}

impl TailFileSession {
    /// Wrap a connection. Caller must already have read the
    /// open-acknowledge result.
    pub(crate) fn new(stream: Box<dyn AgentStream>) -> Self {
        Self { stream, eof: false }
    }

    /// Best-effort read-timeout on the underlying stream. Used by
    /// `Stream:next(timeout)` to honour the design's timeout
    /// argument. Returns the underlying `set_read_timeout` error
    /// (`NotSupported` for backends that can't honour it).
    pub fn set_read_timeout(
        &self,
        timeout: Option<std::time::Duration>,
    ) -> std::io::Result<()> {
        self.stream.set_read_timeout(timeout)
    }

    /// Pull the next stream frame.
    ///
    /// * `Ok(Some(frame))` — got a frame.
    /// * `Ok(None)` — clean EOF, either from a [`StreamEnd::Eof`]
    ///   message or from the agent closing the connection without
    ///   sending one.
    /// * `Err(ClientError::StreamSource(_))` — the agent reported
    ///   a [`StreamEnd::Error`].
    /// * `Err(_)` — wire-level or envelope-level failure.
    pub fn next_frame(&mut self) -> Result<Option<StreamFrame>, ClientError> {
        if self.eof {
            return Ok(None);
        }
        let msg = match recv(&mut self.stream) {
            Ok(m) => m,
            Err(ClientError::Frame(provium_protocol::FrameError::Eof)) => {
                // Agent closed at frame boundary without sending
                // StreamEnd. Treat as clean EOF.
                self.eof = true;
                return Ok(None);
            }
            Err(e) => return Err(e),
        };
        match msg {
            AgentMessage::StreamFrame(f) => Ok(Some(f)),
            AgentMessage::StreamEnd(StreamEnd::Eof) => {
                self.eof = true;
                Ok(None)
            }
            AgentMessage::StreamEnd(StreamEnd::Error(os)) => {
                self.eof = true;
                Err(ClientError::StreamSource(os))
            }
            AgentMessage::AgentError(e) => Err(e.into()),
            other => Err(unexpected("stream_frame|stream_end", &other)),
        }
    }

    /// Returns `true` once the session has seen a [`StreamEnd::Eof`]
    /// or a clean connection close.
    pub fn is_eof(&self) -> bool {
        self.eof
    }
}
