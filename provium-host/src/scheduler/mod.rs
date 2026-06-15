//! Resource pool + multi-file dispatcher + pre-flight checks.
//!
//! Per `DESIGN.md` § Scheduler:
//!
//! * [`pool::Pool`] is the global memory + cpus reservation table.
//!   Acquires block until the request fits; releases happen via the
//!   RAII [`pool::Reservation`].
//! * [`dispatch::dispatch_files`] takes a list of `*.test.lua` paths
//!   plus a configured VMM and runs each on its own runner thread,
//!   gated by per-file pool reservations + timeout watchdogs +
//!   panic isolation.
//! * [`preflight::run`] validates the systemic prerequisites
//!   (`/dev/kvm`, `/dev/vhost-vsock`, iproute2) at startup with
//!   actionable failure messages.
//!
//! Slice 5 ships the foundations; slice 5.5 (later) layers
//! `claim()`, SCHED_BATCH, and PSI-driven adaptive throttling on top
//! without changing the public surface.

#[cfg(feature = "lua")]
pub mod dispatch;
pub mod events;
pub mod pool;
pub mod preflight;
pub mod psi;

#[cfg(feature = "lua")]
pub use dispatch::{
    dispatch_files, dispatch_files_with_progress, DispatchOpts, FileTimeout,
    FileTimeoutOutcome,
};
pub use events::{EventSink, MultiSink, NullSink, UnixSocketSink, WriteSink};
pub use pool::{Pool, ResourceAmount, Reservation};
pub use preflight::{run as run_preflight, PreflightError, PreflightReport};
pub use psi::{spawn as spawn_psi_monitor, PressureFlag};
