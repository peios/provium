//! Streaming-op handlers — currently `TailFile` and `ProcStream`.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;
use std::thread;
use std::time::Duration;

use provium_protocol::wire::{
    AgentMessage, FdStreamArgs, OpResult, ProcStreamArgs, ProcStreamChannel, StreamEnd,
    StreamFrame, TailFileArgs, TailStart,
};

use crate::error::AgentRuntimeError;
use crate::io::write_agent_message;
use crate::state::AgentState;

use super::os_error_from_io;

/// Frames bigger than this would be wasteful for a tail (the file's
/// own write boundaries are usually well below) but we want a sane
/// upper bound. 64 KiB is the typical pipe-buffer size.
const READ_CHUNK: usize = 64 * 1024;

/// Polling interval between attempts to read more bytes from a file
/// that's currently at EOF. 50ms matches the cadence the spike's
/// integrity test validated; tests requiring crisp latency can be
/// sped up if needed.
const IDLE_POLL: Duration = Duration::from_millis(50);

/// `TailFile` — open a file, position to the requested start, send an
/// open-acknowledge result, then push [`StreamFrame`]s as bytes arrive
/// until either the source errors or the host closes the connection.
///
/// Peer-disconnect detection is implicit: the next attempted write
/// after a host close fails with [`std::io::Error`], the loop exits.
/// For a pathologically idle file (no appends ever, host closed), the
/// agent's stream thread sleeps until the file does see activity.
/// Future enhancement: shutdown-detection via a side-channel reader.
pub fn tail_file<R, W>(
    args: TailFileArgs,
    _reader: &mut R,
    writer: &mut W,
) -> Result<(), AgentRuntimeError>
where
    R: Read,
    W: Write,
{
    let mut file = match File::open(Path::new(&args.path)) {
        Ok(f) => f,
        Err(e) => {
            return write_agent_message(
                writer,
                &AgentMessage::TailFileResult(OpResult::Err(os_error_from_io(e))),
            );
        }
    };

    if let Err(e) = seek_to_start(&mut file, args.start) {
        return write_agent_message(
            writer,
            &AgentMessage::TailFileResult(OpResult::Err(os_error_from_io(e))),
        );
    }

    // Open-acknowledge — frames follow.
    write_agent_message(writer, &AgentMessage::TailFileResult(OpResult::Ok(())))?;

    let mut buf = vec![0u8; READ_CHUNK];
    loop {
        match file.read(&mut buf) {
            Ok(0) => {
                // At EOF for now. Detect file truncation: if the
                // current position is greater than the file's
                // current size, the file was shrunk under us
                // (logrotate truncate-style). Emit clean EOF
                // rather than spinning silently forever.
                use std::io::Seek;
                let pos = file.seek(std::io::SeekFrom::Current(0)).unwrap_or(0);
                let file_size = file.metadata().map(|m| m.len()).unwrap_or(pos);
                if pos > file_size {
                    let _ = write_agent_message(
                        writer,
                        &AgentMessage::StreamEnd(StreamEnd::Eof),
                    );
                    return Ok(());
                }
                // At EOF for now — wait briefly and try again.
                thread::sleep(IDLE_POLL);
            }
            Ok(n) => {
                let frame = AgentMessage::StreamFrame(StreamFrame {
                    data: buf[..n].to_vec(),
                });
                if write_agent_message(writer, &frame).is_err() {
                    // Peer closed (or any other write error). Done.
                    return Ok(());
                }
            }
            Err(e) => {
                // Source errored — tell the host and close.
                let _ = write_agent_message(
                    writer,
                    &AgentMessage::StreamEnd(StreamEnd::Error(os_error_from_io(e))),
                );
                return Ok(());
            }
        }
    }
}

/// `ProcStream` — open a streaming subscription to a tracked
/// async-process's captured stdout or stderr. Polls the buffered
/// output (drained by the process's spawn-time pump thread), emits
/// every newly-appended chunk as a [`StreamFrame`], and ends with
/// [`StreamEnd::Eof`] once the process has exited and the buffer is
/// drained.
pub fn proc_stream<R, W>(
    args: ProcStreamArgs,
    state: &AgentState,
    _reader: &mut R,
    writer: &mut W,
) -> Result<(), AgentRuntimeError>
where
    R: Read,
    W: Write,
{
    // Look up the buffer + drain-thread join handle by handle.
    let buf_arc = match state.with_process_mut(args.handle, |slot| {
        let b = match args.channel {
            ProcStreamChannel::Stdout => slot.stdout_buf.clone(),
            ProcStreamChannel::Stderr => slot.stderr_buf.clone(),
        };
        let join_done = match args.channel {
            ProcStreamChannel::Stdout => slot.stdout_join.is_none(),
            ProcStreamChannel::Stderr => slot.stderr_join.is_none(),
        };
        (b, join_done)
    }) {
        Some((buf, _)) => buf,
        None => {
            // Process slot is gone — almost always because the
            // caller already issued `proc:wait` on this handle.
            // Surface that explicitly so the test author doesn't
            // chase a raw EBADF; DESIGN's ordering rule is
            // streams BEFORE wait.
            return write_agent_message(
                writer,
                &AgentMessage::ProcStreamResult(OpResult::Err(
                    provium_protocol::OsError {
                        errno: 9, // EBADF
                        message: "proc stream: process already waited or unknown handle (open streams BEFORE proc:wait)".into(),
                    },
                )),
            );
        }
    };

    write_agent_message(writer, &AgentMessage::ProcStreamResult(OpResult::Ok(())))?;

    // Track our own consumed offset; the buffer keeps growing until
    // the drainer ends + Wait reaps the slot, at which point the
    // slot's `take_process` call will remove it. We keep the Arc to
    // the buffer alive on our side via clone.
    let mut consumed: usize = 0;
    loop {
        // Read whatever's appeared since `consumed`.
        let chunk: Vec<u8> = {
            let g = buf_arc.lock().unwrap();
            if g.len() > consumed {
                let bytes = g[consumed..].to_vec();
                consumed = g.len();
                bytes
            } else {
                Vec::new()
            }
        };
        if !chunk.is_empty() {
            let frame = AgentMessage::StreamFrame(StreamFrame { data: chunk });
            if write_agent_message(writer, &frame).is_err() {
                return Ok(());
            }
            continue;
        }
        // Empty: did the process exit + drain finish?
        let done = state
            .with_process_mut(args.handle, |slot| match args.channel {
                ProcStreamChannel::Stdout => slot.stdout_join.is_none(),
                ProcStreamChannel::Stderr => slot.stderr_join.is_none(),
            })
            .unwrap_or(true);
        if done {
            // Final drain — pick up anything the drainer wrote
            // between our last read and its exit.
            let final_chunk: Vec<u8> = {
                let g = buf_arc.lock().unwrap();
                if g.len() > consumed {
                    g[consumed..].to_vec()
                } else {
                    Vec::new()
                }
            };
            if !final_chunk.is_empty() {
                let _ = write_agent_message(
                    writer,
                    &AgentMessage::StreamFrame(StreamFrame { data: final_chunk }),
                );
            }
            let _ = write_agent_message(writer, &AgentMessage::StreamEnd(StreamEnd::Eof));
            return Ok(());
        }
        thread::sleep(IDLE_POLL);
    }
}

/// `FdStream` — read repeatedly from an open file handle, emit
/// frames until EOF (or host close).
pub fn fd_stream<R, W>(
    args: FdStreamArgs,
    state: &AgentState,
    _reader: &mut R,
    writer: &mut W,
) -> Result<(), AgentRuntimeError>
where
    R: Read,
    W: Write,
{
    // Verify the handle exists. Fail if it doesn't.
    let handle_known = state.with_file_mut(args.handle, |_| ()).is_some();
    if !handle_known {
        return write_agent_message(
            writer,
            &AgentMessage::FdStreamResult(OpResult::Err(
                provium_protocol::OsError::from_errno(9),
            )),
        );
    }

    write_agent_message(writer, &AgentMessage::FdStreamResult(OpResult::Ok(())))?;

    let mut buf = vec![0u8; READ_CHUNK];
    loop {
        let read_result = state
            .with_file_mut(args.handle, |f| f.read(&mut buf))
            .unwrap_or(Ok(0));
        match read_result {
            Ok(0) => {
                // EOF for now; for a regular file this means actual
                // EOF, but for a pipe/socket the writer might still
                // be active. Sleep + retry; host disconnect breaks
                // us out via the next write_agent_message failure.
                thread::sleep(IDLE_POLL);
                // Heuristic exit: if the file is unmapped (closed
                // mid-stream) bail.
                let still_known = state.with_file_mut(args.handle, |_| ()).is_some();
                if !still_known {
                    let _ = write_agent_message(
                        writer,
                        &AgentMessage::StreamEnd(StreamEnd::Eof),
                    );
                    return Ok(());
                }
            }
            Ok(n) => {
                let frame = AgentMessage::StreamFrame(StreamFrame {
                    data: buf[..n].to_vec(),
                });
                if write_agent_message(writer, &frame).is_err() {
                    return Ok(());
                }
            }
            Err(e) => {
                let _ = write_agent_message(
                    writer,
                    &AgentMessage::StreamEnd(StreamEnd::Error(os_error_from_io(e))),
                );
                return Ok(());
            }
        }
    }
}

fn seek_to_start(file: &mut File, start: TailStart) -> std::io::Result<()> {
    match start {
        TailStart::Beginning => {
            file.seek(SeekFrom::Start(0))?;
        }
        TailStart::End => {
            file.seek(SeekFrom::End(0))?;
        }
        TailStart::Offset(o) => {
            file.seek(SeekFrom::Start(o))?;
        }
    }
    Ok(())
}
