//! `tail_file` session — owns the long-lived connection to the agent
//! and yields [`StreamFrame`]s as the agent emits them.

use provium_protocol::frame::{FrameReader, DEFAULT_MAX_FRAME_BYTES};
use provium_protocol::wire::{AgentMessage, StreamEnd, StreamFrame};

use crate::connector::AgentStream;
use crate::ClientError;

use super::unexpected;

/// A `tail_file` session: an open vsock-or-paired connection that
/// belongs to one streaming op for its lifetime.
///
/// Drop the session to close the connection; the agent's stream
/// thread observes the EOF on its next write attempt and exits.
pub struct TailFileSession {
    stream: Box<dyn AgentStream>,
    decoder: FrameReader,
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
        Self {
            stream,
            decoder: FrameReader::new(DEFAULT_MAX_FRAME_BYTES),
            eof: false,
        }
    }

    /// Best-effort read-timeout on the underlying stream. Used by
    /// `Stream:next(timeout)` to honour the design's timeout
    /// argument. Returns the underlying `set_read_timeout` error
    /// (`NotSupported` for backends that can't honour it).
    pub fn set_read_timeout(&self, timeout: Option<std::time::Duration>) -> std::io::Result<()> {
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
        let msg = match self
            .decoder
            .read(&mut self.stream)
            .map_err(ClientError::Frame)
        {
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

#[cfg(test)]
mod tests {
    use super::*;
    use provium_protocol::frame::{write_frame, DEFAULT_MAX_FRAME_BYTES};
    use std::io::{self, Cursor, Read, Write};

    struct PausingStream {
        bytes: Cursor<Vec<u8>>,
        pause_at: usize,
        pause: Option<io::ErrorKind>,
    }

    impl Read for PausingStream {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            if self.pause.is_some() {
                let remaining = self.pause_at - self.bytes.position() as usize;
                if remaining == 0 {
                    return Err(self.pause.take().unwrap().into());
                }
                let len = buf.len().min(remaining);
                return self.bytes.read(&mut buf[..len]);
            }
            self.bytes.read(buf)
        }
    }

    impl Write for PausingStream {
        fn write(&mut self, _: &[u8]) -> io::Result<usize> {
            unreachable!("stream subscription only reads")
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    impl AgentStream for PausingStream {}

    #[test]
    fn timeout_at_every_frame_offset_preserves_bytes_and_next_frame() {
        let mut bytes = Vec::new();
        let payload = b"stream data";
        write_frame(
            &mut bytes,
            &AgentMessage::StreamFrame(StreamFrame {
                data: payload.to_vec(),
            }),
            DEFAULT_MAX_FRAME_BYTES,
        )
        .unwrap();
        let frame_len = bytes.len();
        write_frame(
            &mut bytes,
            &AgentMessage::StreamEnd(StreamEnd::Eof),
            DEFAULT_MAX_FRAME_BYTES,
        )
        .unwrap();

        for kind in [io::ErrorKind::TimedOut, io::ErrorKind::WouldBlock] {
            for pause_at in 0..frame_len {
                let mut session = TailFileSession::new(Box::new(PausingStream {
                    bytes: Cursor::new(bytes.clone()),
                    pause_at,
                    pause: Some(kind),
                }));
                assert!(matches!(session.next_frame(),
                    Err(ClientError::Frame(provium_protocol::FrameError::Io(e)))
                    if e.kind() == kind));
                assert!(!session.is_eof());
                let frame = session
                    .next_frame()
                    .unwrap_or_else(|e| panic!("resume after {kind:?} at offset {pause_at}: {e}"))
                    .expect("data frame");
                assert_eq!(frame.data, payload);
                assert!(session.next_frame().unwrap().is_none());
                assert!(session.is_eof());
                assert!(session.next_frame().unwrap().is_none());
            }
        }
    }
}
