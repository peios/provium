//! Thin typed wrapper around the wire codec, used by every op handler.
//!
//! The agent reads exactly one [`HostMessage`] per accept (the Hello
//! plus the op fit in two frames; no pipelining), but stream-mode ops
//! emit many [`AgentMessage`]s on the same connection. Centralising
//! the framing here keeps op handlers focused on their semantics.

use std::io::{Read, Write};

use provium_protocol::frame::{read_frame, write_frame, DEFAULT_MAX_FRAME_BYTES};
use provium_protocol::wire::{AgentMessage, HostMessage};

use crate::error::AgentRuntimeError;

/// Read a single [`HostMessage`] frame from `reader`.
pub(crate) fn read_host_message<R: Read>(
    reader: &mut R,
) -> Result<HostMessage, AgentRuntimeError> {
    Ok(read_frame(reader, DEFAULT_MAX_FRAME_BYTES)?)
}

/// Write a single [`AgentMessage`] frame to `writer`.
pub(crate) fn write_agent_message<W: Write>(
    writer: &mut W,
    message: &AgentMessage,
) -> Result<(), AgentRuntimeError> {
    write_frame(writer, message, DEFAULT_MAX_FRAME_BYTES)?;
    Ok(())
}
