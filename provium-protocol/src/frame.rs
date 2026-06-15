//! Length-prefixed msgpack framing.
//!
//! Both the wire (host ↔ agent) and the event stream (host → consumers)
//! use the same framing: a 4-byte big-endian length header, then exactly
//! that many bytes of msgpack-encoded body. This module exposes the
//! synchronous read/write helpers; the message types live in
//! [`crate::wire`] and [`crate::events`].
//!
//! # Encoding choice
//!
//! Bodies are encoded with `rmp_serde::to_vec_named`, which preserves
//! struct field names. Combined with the [`crate::PROTOCOL_VERSION`]
//! check at handshake, this gives defence-in-depth against version skew:
//! adding a field is a strict version bump, but a buggy "compatible"
//! decode is at least named-field-tolerant rather than silently
//! reinterpreting positional data.
//!
//! # Frame-size cap
//!
//! Every read enforces a per-frame cap (see
//! [`DEFAULT_MAX_FRAME_BYTES`]) to prevent a buggy or hostile peer from
//! forcing the reader to pre-allocate arbitrary memory. The cap covers
//! the body only, not the 4-byte header.
//!
//! Use chunked Layer-1 ops (`open_file` + `read`) for payloads larger
//! than the cap; the composite `read_file` / `write_file` ops are sized
//! for "small-to-medium" files and the cap is the upper bound on what
//! can be sent in one frame.

use std::io::{self, Read, Write};

use serde::{de::DeserializeOwned, Serialize};

use crate::error::FrameError;

/// Default frame-body size cap: 128 MiB.
///
/// Sized to comfortably hold composite ops (`read_file`, `write_file`)
/// for typical config-file scale data, while bounding worst-case
/// allocation. Long-running streams keep their per-frame chunks well
/// under this; the host scheduler's claim accounting is the budget for
/// total Lua-side memory, not this codec.
pub const DEFAULT_MAX_FRAME_BYTES: usize = 128 * 1024 * 1024;

const HEADER_LEN: usize = 4;

/// Read one frame from `reader` and decode it as `T`.
///
/// Returns [`FrameError::Eof`] if the reader is at a clean frame
/// boundary (zero bytes read for the header) — this is the normal
/// end-of-conversation signal for a short op. Returns
/// [`FrameError::UnexpectedEof`] if the connection closes part-way
/// through a header or body.
pub fn read_frame<R, T>(reader: &mut R, max_body_bytes: usize) -> Result<T, FrameError>
where
    R: Read,
    T: DeserializeOwned,
{
    let mut header = [0u8; HEADER_LEN];
    match read_full(reader, &mut header)? {
        0 => return Err(FrameError::Eof),
        n if n < HEADER_LEN => {
            return Err(FrameError::UnexpectedEof {
                bytes_read: n,
                expected: HEADER_LEN,
            });
        }
        _ => {}
    }

    let body_len = u32::from_be_bytes(header) as usize;
    if body_len > max_body_bytes {
        return Err(FrameError::FrameTooLarge {
            len: body_len,
            max: max_body_bytes,
        });
    }

    let mut body = vec![0u8; body_len];
    let read = read_full(reader, &mut body)?;
    if read < body_len {
        return Err(FrameError::UnexpectedEof {
            bytes_read: read,
            expected: body_len,
        });
    }

    Ok(rmp_serde::from_slice(&body)?)
}

/// Encode `message` as a single msgpack frame and write it to `writer`.
///
/// Does **not** flush: callers wrapping `writer` in a [`std::io::BufWriter`]
/// must flush themselves when a peer is waiting on the frame. For
/// unbuffered sockets (the default vsock case) `write_all` translates
/// directly to a `send` syscall and no flush is required.
pub fn write_frame<W, T>(
    writer: &mut W,
    message: &T,
    max_body_bytes: usize,
) -> Result<(), FrameError>
where
    W: Write,
    T: Serialize,
{
    let body = rmp_serde::to_vec_named(message)?;
    if body.len() > max_body_bytes {
        return Err(FrameError::FrameTooLarge {
            len: body.len(),
            max: max_body_bytes,
        });
    }
    // u32 fit guaranteed by the cap check above; the cap fits in u32.
    // Coalesce header + body into a single write call so any
    // advisory `flock(LOCK_EX)` on the underlying writer (see
    // `provium-host`'s `LockedFile`) covers the whole frame
    // atomically. With two separate write_all calls a concurrent
    // process could insert its own header between ours and the
    // body — leaving torn frames that crash readers.
    let mut buf = Vec::with_capacity(4 + body.len());
    buf.extend_from_slice(&(body.len() as u32).to_be_bytes());
    buf.extend_from_slice(&body);
    writer.write_all(&buf)?;
    Ok(())
}

/// Read until `buf` is full or the reader returns 0 bytes.
///
/// Returns the total number of bytes read. Distinct from
/// [`Read::read_exact`] in that it surfaces a *partial* read as a
/// returned count rather than an error, which the caller uses to
/// distinguish clean EOF (0 bytes) from mid-message EOF (1..len).
fn read_full<R: Read>(reader: &mut R, buf: &mut [u8]) -> io::Result<usize> {
    let mut total = 0;
    while total < buf.len() {
        match reader.read(&mut buf[total..])? {
            0 => break,
            n => total += n,
        }
    }
    Ok(total)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::{Deserialize, Serialize};
    use std::io::Cursor;

    #[derive(Debug, PartialEq, Serialize, Deserialize)]
    struct Sample {
        n: u32,
        s: String,
        v: Vec<u8>,
    }

    fn sample() -> Sample {
        Sample {
            n: 42,
            s: "hello".into(),
            v: vec![1, 2, 3, 4],
        }
    }

    #[test]
    fn round_trips_a_single_frame() {
        let mut buf = Vec::new();
        write_frame(&mut buf, &sample(), DEFAULT_MAX_FRAME_BYTES).unwrap();

        let mut cursor = Cursor::new(&buf);
        let decoded: Sample = read_frame(&mut cursor, DEFAULT_MAX_FRAME_BYTES).unwrap();
        assert_eq!(decoded, sample());
    }

    #[test]
    fn round_trips_multiple_frames_back_to_back() {
        let mut buf = Vec::new();
        for i in 0..5 {
            let msg = Sample {
                n: i,
                s: format!("frame {i}"),
                v: vec![i as u8; i as usize],
            };
            write_frame(&mut buf, &msg, DEFAULT_MAX_FRAME_BYTES).unwrap();
        }

        let mut cursor = Cursor::new(&buf);
        for i in 0..5 {
            let msg: Sample = read_frame(&mut cursor, DEFAULT_MAX_FRAME_BYTES).unwrap();
            assert_eq!(msg.n, i);
            assert_eq!(msg.s, format!("frame {i}"));
        }

        // Sixth read returns clean EOF.
        let result = read_frame::<_, Sample>(&mut cursor, DEFAULT_MAX_FRAME_BYTES);
        assert!(matches!(result, Err(FrameError::Eof)));
    }

    #[test]
    fn rejects_frame_exceeding_cap() {
        // Build a header that claims a body larger than the cap.
        let header = u32::to_be_bytes(1024);
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&header);
        bytes.extend_from_slice(&vec![0u8; 1024]);

        let mut cursor = Cursor::new(&bytes);
        let result = read_frame::<_, Sample>(&mut cursor, 512);
        match result {
            Err(FrameError::FrameTooLarge { len, max }) => {
                assert_eq!(len, 1024);
                assert_eq!(max, 512);
            }
            other => panic!("expected FrameTooLarge, got {other:?}"),
        }
    }

    #[test]
    fn write_rejects_oversized_payload() {
        let huge = Sample {
            n: 0,
            s: "x".repeat(2048),
            v: vec![],
        };
        let mut buf = Vec::new();
        let result = write_frame(&mut buf, &huge, 512);
        assert!(matches!(result, Err(FrameError::FrameTooLarge { .. })));
        assert!(buf.is_empty(), "no bytes should have been written");
    }

    #[test]
    fn partial_header_is_unexpected_eof() {
        let bytes = vec![0u8, 1u8]; // 2 of 4 header bytes
        let mut cursor = Cursor::new(&bytes);
        let result = read_frame::<_, Sample>(&mut cursor, DEFAULT_MAX_FRAME_BYTES);
        match result {
            Err(FrameError::UnexpectedEof { bytes_read, expected }) => {
                assert_eq!(bytes_read, 2);
                assert_eq!(expected, HEADER_LEN);
            }
            other => panic!("expected UnexpectedEof, got {other:?}"),
        }
    }

    #[test]
    fn truncated_body_is_unexpected_eof() {
        // Header says 16 bytes, only 8 follow.
        let mut bytes = u32::to_be_bytes(16).to_vec();
        bytes.extend_from_slice(&[0u8; 8]);

        let mut cursor = Cursor::new(&bytes);
        let result = read_frame::<_, Sample>(&mut cursor, DEFAULT_MAX_FRAME_BYTES);
        match result {
            Err(FrameError::UnexpectedEof { bytes_read, expected }) => {
                assert_eq!(bytes_read, 8);
                assert_eq!(expected, 16);
            }
            other => panic!("expected UnexpectedEof, got {other:?}"),
        }
    }

    #[test]
    fn empty_stream_at_boundary_is_clean_eof() {
        let bytes: Vec<u8> = vec![];
        let mut cursor = Cursor::new(&bytes);
        let result = read_frame::<_, Sample>(&mut cursor, DEFAULT_MAX_FRAME_BYTES);
        assert!(matches!(result, Err(FrameError::Eof)));
    }
}
