//! # provium-host
//!
//! Host-side core for provium. This crate is the home of the
//! scheduler, the VM resource model, the agent client, and (in a
//! future slice) the mlua bindings + test runner. It is consumed by
//! the `provium` binary in the same crate's `src/main.rs`.
//!
//! ## Slice 1 scope (this crate at v0.1)
//!
//! Foundational pieces, no scheduler or test runner yet:
//!
//! * [`profile`] — TOML loading of the `[profiles.<name>]` table.
//! * [`cid`] — monotonic vsock CID allocator.
//! * [`connector`] — abstraction over the agent connection: vsock in
//!   production, paired Unix streams in tests.
//! * [`agent_client`] — typed wrappers for every short op + a
//!   streaming `tail_file` client.
//! * [`vmm`] — the [`vmm::Vmm`] trait per `DESIGN.md` Architecture §;
//!   the QEMU implementation is stubbed for slice 2.
//!
//! Tests live under `tests/` and drive the agent client against a
//! real `provium-agent` running on the host (via `UnixStream::pair`),
//! so the full host-↔-agent code path is exercised end-to-end without
//! needing KVM or a guest kernel.

#![deny(missing_debug_implementations)]
#![warn(missing_docs)]
#![warn(rust_2018_idioms)]
#![warn(unreachable_pub)]

pub mod agent_client;
pub mod bridge;
pub mod bridge_realize;
pub mod build;
pub mod cid;
pub mod console;
pub mod connector;
pub mod fixture;
pub mod lab;
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
