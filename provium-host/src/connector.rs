//! Abstraction over "open a fresh agent connection."
//!
//! Per `DESIGN.md`, short ops are connectionless — every op opens a
//! new connection. This trait lets the agent client target either a
//! live in-VM agent over vsock or, in tests, a paired Unix stream
//! whose other end is fed to `provium_agent::connection::handle_connection`.
//!
//! ## Type-erased streams
//!
//! `connect()` returns `Box<dyn AgentStream>` rather than an
//! associated-type stream. Type erasure here lets the rest of the
//! host (notably the [`crate::vm::Vm`] resource exposed to Lua) hold
//! a single non-generic [`crate::agent_client::AgentClient`] regardless
//! of which backend produced the connection. The boxing cost is
//! one heap allocation per op — negligible against a vsock or in-VM
//! syscall round-trip.
//!
//! ## Error handling
//!
//! `connect()` returns [`std::io::Error`] so production code can
//! propagate the underlying vsock failure verbatim and tests can
//! synthesise readable failure modes with `io::Error::other(...)`.
//! [`crate::ClientError::Connect`] is the wire-side wrapper.

use std::io::{self, Read, Write};

use vsock::{VsockStream, VMADDR_CID_HOST};

/// Marker trait grouping the bounds an agent connection must satisfy:
/// readable + writable + thread-portable + non-borrowed. Includes a
/// best-effort `set_read_timeout` so streaming ops can honour
/// per-call timeouts; the default-impl returns `NotSupported` for
/// types that don't carry a kernel-side timeout knob.
pub trait AgentStream: Read + Write + Send + 'static {
    /// Set the read timeout on the underlying socket. `None`
    /// clears. Default-impl is `NotSupported` so types that don't
    /// support timeouts are handled.
    fn set_read_timeout(
        &self,
        _timeout: Option<std::time::Duration>,
    ) -> io::Result<()> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "set_read_timeout not supported by this stream type",
        ))
    }
}

// Concrete-type impls so VsockStream and UnixStream can carry
// timeouts where the kernel supports it.
impl AgentStream for VsockStream {
    fn set_read_timeout(
        &self,
        timeout: Option<std::time::Duration>,
    ) -> io::Result<()> {
        VsockStream::set_read_timeout(self, timeout)
    }
}
impl AgentStream for std::os::unix::net::UnixStream {
    fn set_read_timeout(
        &self,
        timeout: Option<std::time::Duration>,
    ) -> io::Result<()> {
        std::os::unix::net::UnixStream::set_read_timeout(self, timeout)
    }
}

/// One-shot connector: each call to [`Self::connect`] yields a fresh
/// stream representing a fresh agent connection.
///
/// Implementations must be `Send + Sync` so the agent client can be
/// shared across runner threads without a wrapping lock.
pub trait Connector: Send + Sync {
    /// Open a new connection. Caller does the Hello handshake.
    fn connect(&self) -> io::Result<Box<dyn AgentStream>>;
}

/// Connect to an in-VM agent at `(cid, port)` over vsock.
#[derive(Clone, Copy, Debug)]
pub struct VsockConnector {
    /// vsock CID assigned to the VM at boot time.
    pub cid: u32,
    /// Port the agent is listening on. Default: 1234.
    pub port: u32,
}

impl VsockConnector {
    /// Build a connector for `(cid, port)`.
    pub const fn new(cid: u32, port: u32) -> Self {
        Self { cid, port }
    }
}

impl Connector for VsockConnector {
    fn connect(&self) -> io::Result<Box<dyn AgentStream>> {
        // VMADDR_CID_HOST is *only* valid as a source address; we
        // dial with the VM's CID. Documenting the constant here
        // keeps the field definition unambiguous for future readers.
        let _ = VMADDR_CID_HOST;
        let stream = VsockStream::connect_with_cid_port(self.cid, self.port)?;
        Ok(Box::new(stream))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vsock_connector_carries_cid_and_port() {
        let c = VsockConnector::new(123, 9999);
        assert_eq!(c.cid, 123);
        assert_eq!(c.port, 9999);
    }
}
