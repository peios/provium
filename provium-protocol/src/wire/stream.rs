//! Stream-mode response frames.
//!
//! Streaming ops (file tail, console read, packet capture, subscribe)
//! hold their connection open after the initial open-acknowledge, then
//! push [`StreamFrame`] messages until either the source reaches EOF
//! or the host closes the connection. A clean source EOF is followed by
//! a [`StreamEnd`] message before the agent closes its side.
//!
//! Frame data is opaque bytes at the protocol layer. Stream-specific
//! framing (line buffering, pcap headers, msgpack-inside-msgpack for
//! structured-event streams) is the responsibility of the open-op pair,
//! which agrees on encoding when the stream opens.

use serde::{Deserialize, Serialize};

use crate::error::OsError;

/// One chunk of stream data.
///
/// `data` is interpreted by the originating op type — bytes for file
/// tails and console reads, pcap-shaped bytes for nic captures, and so
/// on. The protocol layer makes no claim about its contents.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StreamFrame {
    /// Frame payload, encoded with the op-specific framing convention.
    #[serde(with = "serde_bytes")]
    pub data: Vec<u8>,
}

/// Terminal message marking the end of a stream.
///
/// Sent by the agent when the source ends naturally; followed by a
/// clean connection close. If the host closes the connection first the
/// agent does **not** synthesise a [`StreamEnd`] — the disconnect itself
/// is the signal.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "reason", content = "data", rename_all = "snake_case")]
pub enum StreamEnd {
    /// Source reached end-of-data normally (process exited cleanly,
    /// file truncated to zero, capture interface released, etc.).
    Eof,
    /// Source reported an error mid-stream.
    Error(OsError),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_round_trips_with_compact_byte_encoding() {
        let frame = StreamFrame {
            data: vec![0u8, 1, 2, 3, 4, 5],
        };
        let bytes = rmp_serde::to_vec_named(&frame).unwrap();
        let decoded: StreamFrame = rmp_serde::from_slice(&bytes).unwrap();
        assert_eq!(frame, decoded);

        // serde_bytes encodes Vec<u8> as msgpack `bin` (1-byte tag +
        // length + raw bytes), not an array of u8 — much more compact
        // than the default. For 6 bytes that's ~9 bytes wire size.
        assert!(bytes.len() < 32);
    }

    #[test]
    fn stream_end_eof_round_trips() {
        let end = StreamEnd::Eof;
        let bytes = rmp_serde::to_vec_named(&end).unwrap();
        let decoded: StreamEnd = rmp_serde::from_slice(&bytes).unwrap();
        assert_eq!(end, decoded);
    }

    #[test]
    fn stream_end_error_round_trips() {
        let end = StreamEnd::Error(OsError::from_errno(5));
        let bytes = rmp_serde::to_vec_named(&end).unwrap();
        let decoded: StreamEnd = rmp_serde::from_slice(&bytes).unwrap();
        assert_eq!(end, decoded);
    }
}
