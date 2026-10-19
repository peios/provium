//! Host-side VM test runner: Lua bindings, scheduling, profiles, fixture caches,
//! and agent connections. The `provium` CLI lives in `src/bin/provium.rs`.
//!
//! [`vmm::qemu::QemuVmm`] controls real guest machines through QMP and vsock.
//! With the `local-agent` feature, [`vmm::local_agent::LocalAgentVmm`] runs
//! agent operations in the host process for unprivileged library tests.
//! The local backend does not provide guest isolation or VM lifecycle coverage.
//!
//! The default `lua` and `local-agent` features provide the CLI and its tests.
//! Consumers that only need the client and resource model can disable them.

#![deny(missing_debug_implementations)]
#![warn(missing_docs)]
#![warn(rust_2018_idioms)]
#![warn(unreachable_pub)]

pub mod agent_client;
pub mod bridge;
pub mod bridge_realize;
pub mod build;
pub mod cid;
pub mod connector;
pub mod console;
pub mod fixture;
pub mod lab;
pub mod nbd;
pub mod perf;
pub mod profile;
pub mod scheduler;
pub mod verbosity;
pub mod vm;
pub mod vmm;

#[cfg(feature = "lua")]
pub mod lsp_meta;

#[cfg(feature = "lua")]
pub mod lua;

#[cfg(feature = "lua")]
pub mod repl;

mod error;

pub use error::ClientError;
pub use lab::{Lab, LabError};
pub use vm::{RunResult, StatMeta, Vm, VmError, VmState};

/// Re-export the protocol crate so consumers don't have to add their
/// own dependency.
pub use provium_protocol as protocol;
