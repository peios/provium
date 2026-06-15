//! Envelope-level agent errors.
//!
//! Distinct from [`crate::OsError`] which lives *inside* op result
//! payloads to convey OS failures. [`AgentError`] is for failures of the
//! agent itself: malformed requests, unsupported ops, unknown handles,
//! or internal bugs. They arrive as their own variant on
//! [`super::AgentMessage`] and indicate the op had no meaningful result.

use serde::{Deserialize, Serialize};

/// An agent-side failure with no op-result payload to attach it to.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentError {
    /// Coarse classification used by the host to decide between
    /// "test-author bug" and "infrastructure failure".
    pub kind: AgentErrorKind,
    /// Free-form context. Surfaced verbatim in the host's error
    /// rendering; do not pattern-match against it.
    pub message: String,
}

/// Categorisation for an [`AgentError`].
///
/// Errors of kind [`AgentErrorKind::Internal`] always raise a
/// test-infrastructure failure. The other kinds may surface as test
/// failures depending on the calling op.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentErrorKind {
    /// The agent could not parse or validate the request body.
    /// Usually a host-side encoding bug.
    BadRequest,

    /// The op is recognised by the protocol but not implemented on
    /// this agent port. Future-proofing for cross-OS ports where
    /// some ops have no native equivalent.
    Unsupported,

    /// A handle quoted by the host does not exist in the agent's
    /// open-handle table (closed, never allocated, or wrong kind).
    UnknownHandle,

    /// Unexpected internal failure inside the agent. Always a bug;
    /// the message carries enough context to file an issue.
    Internal,
}

impl std::fmt::Display for AgentError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "agent error ({:?}): {}", self.kind, self.message)
    }
}

impl std::error::Error for AgentError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips() {
        let err = AgentError {
            kind: AgentErrorKind::UnknownHandle,
            message: "file#42 was already closed".into(),
        };
        let bytes = rmp_serde::to_vec_named(&err).unwrap();
        let decoded: AgentError = rmp_serde::from_slice(&bytes).unwrap();
        assert_eq!(err, decoded);
    }
}
