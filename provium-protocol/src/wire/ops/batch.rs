//! Op batching — multiple ops carried by one wire round-trip.
//!
//! Per `DESIGN.md` § Performance / Op batching, the wire form is:
//! a [`BatchOpArgs`] carries an ordered list of generic
//! [`crate::wire::HostMessage`]s; the agent dispatches each in order
//! and returns a [`BatchOpResult`] with the matching list of
//! [`crate::wire::AgentMessage`]s, paired by index.
//!
//! [`BatchExecArgs`] is the legacy single-op form retained for
//! call sites that only ever batch [`super::ExecArgs`]. Both shapes
//! coexist; new code should prefer the generic form.

use serde::{Deserialize, Serialize};

use super::{ExecArgs, ExecResult};

/// Multiple [`ExecArgs`] in one wire op (legacy single-kind form).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct BatchExecArgs {
    /// Ordered list of exec requests.
    pub items: Vec<ExecArgs>,
}

/// Multiple [`ExecResult`]s, paired by index with the request's `items`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct BatchExecResult {
    /// Ordered list of exec results — `results[i]` is the outcome of
    /// `items[i]`.
    pub results: Vec<ExecResult>,
}

/// Generic op-batch envelope. Carries an ordered list of arbitrary
/// (non-streaming, non-handshake, non-batch) host messages; the
/// agent dispatches each in turn against the same `AgentState`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct BatchOpArgs {
    /// Inner messages. Stream / handshake / nested-batch variants
    /// are rejected by the agent with an `AgentError`.
    pub items: Vec<crate::wire::HostMessage>,
}

/// Paired-by-index responses for a [`BatchOpArgs`].
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct BatchOpResult {
    /// One response per input item. Stream variants never appear
    /// here — the agent rejects them as input.
    pub responses: Vec<crate::wire::AgentMessage>,
}
