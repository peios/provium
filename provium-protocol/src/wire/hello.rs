//! Connection handshake.
//!
//! Every vsock connection from the host begins with a [`Hello`] message.
//! The agent answers with either [`HelloOk`] (versions agreed; here is
//! my build info) or [`HelloErr`] (version mismatch; closing). Only on a
//! successful Hello may the host follow up with an op.
//!
//! The handshake exists in addition to the cache-key version check on
//! fixture snapshots so that a stale snapshot loaded against a newer
//! agent still surfaces a clean diagnostic at connection time, rather
//! than producing mysterious decode failures on later ops.

use serde::{Deserialize, Serialize};

/// First message on every connection: host states its protocol version.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Hello {
    /// [`crate::PROTOCOL_VERSION`] as compiled into the host.
    pub protocol_version: u32,
}

/// Successful handshake reply from the agent.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HelloOk {
    /// [`crate::PROTOCOL_VERSION`] as compiled into the agent.
    /// Always equal to the host's version on success.
    pub protocol_version: u32,
    /// Diagnostic information about the agent build. Surfaced by the
    /// host in failure traces and `provium repl` welcome banners.
    pub agent: AgentInfo,
}

/// Failed handshake reply from the agent, indicating a version mismatch.
///
/// On receipt the host raises [`crate::ProtocolError::VersionMismatch`]
/// and the connection is closed. No op may follow.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HelloErr {
    /// Version the host advertised in [`Hello`].
    pub host_version: u32,
    /// Version compiled into the agent.
    pub agent_version: u32,
}

/// Identifying information about an agent build.
///
/// Free-form; the host displays it but does not switch behaviour on it.
/// Agent-port detection happens via the profile's `guest_os` field, not
/// from this struct, so a malicious or buggy agent cannot lie about its
/// port and trick the host into a different code path.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentInfo {
    /// Agent port name, e.g. `"peios"`. Informational.
    pub os: String,
    /// `provium-agent` build version, semver-shaped. Informational.
    pub agent_version: String,
    /// Optional kernel identifier, typically `uname -r`. Useful when
    /// debugging fixture rebuilds across kernel changes.
    pub kernel: Option<String>,
    /// Forward-compat capability flags. Empty in v1; lets a future
    /// agent advertise op-level extensions without needing a protocol
    /// version bump.
    pub capabilities: Vec<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hello_round_trips() {
        let h = Hello { protocol_version: 1 };
        let bytes = rmp_serde::to_vec_named(&h).unwrap();
        let decoded: Hello = rmp_serde::from_slice(&bytes).unwrap();
        assert_eq!(h, decoded);
    }

    #[test]
    fn hello_ok_round_trips() {
        let ok = HelloOk {
            protocol_version: 1,
            agent: AgentInfo {
                os: "peios".into(),
                agent_version: "0.1.0".into(),
                kernel: Some("6.12.85-peios".into()),
                capabilities: vec![],
            },
        };
        let bytes = rmp_serde::to_vec_named(&ok).unwrap();
        let decoded: HelloOk = rmp_serde::from_slice(&bytes).unwrap();
        assert_eq!(ok, decoded);
    }

    #[test]
    fn hello_err_round_trips() {
        let err = HelloErr {
            host_version: 7,
            agent_version: 6,
        };
        let bytes = rmp_serde::to_vec_named(&err).unwrap();
        let decoded: HelloErr = rmp_serde::from_slice(&bytes).unwrap();
        assert_eq!(err, decoded);
    }
}
