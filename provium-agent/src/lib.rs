//! # provium-agent
//!
//! Guest-side agent for provium. Built as a static `musl` binary, runs
//! as PID 2 inside a test VM, and services ops the host issues over
//! vsock.
//!
//! ## Connection lifecycle
//!
//! Per [`provium_protocol::wire`]:
//!
//! 1. Host opens a vsock connection.
//! 2. Host sends a `Hello`; the agent replies `HelloOk` (matching
//!    version) or `HelloErr` (mismatch).
//! 3. Host sends one op; the agent dispatches it.
//!     * **Short ops** — agent sends one matching result variant; both
//!       sides close.
//!     * **Stream ops** — agent sends an open-acknowledge result, then
//!       pushes [`provium_protocol::wire::StreamFrame`]s until the
//!       source ends or the host closes the connection.
//!
//! ## Threading
//!
//! One handler thread per accept(). Shared state ([`state::AgentState`])
//! is `Arc<Mutex<…>>` — the open-file table, handle allocator, and
//! anything else that must persist across connections lives there.
//!
//! ## Testing without vsock
//!
//! [`connection::handle_connection`] is generic over a single read +
//! write pair (typically a [`std::os::unix::net::UnixStream`] in tests
//! and a [`vsock::VsockStream`] in production). Integration tests in
//! `tests/` use paired Unix-domain sockets to exercise the full
//! handshake + op-dispatch path without needing a vsock device.

#![warn(missing_debug_implementations)]
#![warn(rust_2018_idioms)]
#![warn(unreachable_pub)]

pub mod connection;
pub mod init;
pub mod ops;
pub mod state;

mod error;
mod io;

pub use error::AgentRuntimeError;
pub use state::AgentState;

/// Build identifier surfaced in `HelloOk.agent.agent_version`.
pub const AGENT_VERSION: &str = env!("CARGO_PKG_VERSION");
