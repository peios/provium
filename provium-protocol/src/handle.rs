//! Typed handles returned by the agent for stateful resources.
//!
//! Each kind of resource (open file, async process, sub-agent worker)
//! gets a distinct newtype. The agent owns the namespace and allocates
//! identifiers; the host stores them in its resource graph and includes
//! them in subsequent ops.
//!
//! Distinct types catch handle-mixing bugs at compile time on the host
//! side: the type signature of `Read` only accepts a [`FileHandle`], so
//! a [`ProcessHandle`] cannot be passed to it by accident. On the wire
//! they all serialize as bare `u64` ([`serde(transparent)`]).
//!
//! # Allocation
//!
//! Within an agent, handle id `0` is reserved as "no handle". Live
//! handles are allocated monotonically per kind. Handle ids are not
//! recycled within a single agent process — the u64 namespace is
//! effectively unbounded.

use serde::{Deserialize, Serialize};
use std::fmt;

macro_rules! define_handle {
    ($(#[$meta:meta])* $name:ident, $kind:literal) => {
        $(#[$meta])*
        #[derive(Copy, Clone, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name(pub u64);

        impl $name {
            /// Construct from a raw id.
            #[inline]
            pub const fn new(id: u64) -> Self { Self(id) }

            /// Raw id value.
            #[inline]
            pub const fn get(self) -> u64 { self.0 }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, "{}#{}", $kind, self.0)
            }
        }
    };
}

define_handle! {
    /// Identifier for an open file in the agent's open-file table.
    /// Returned by `OpenFile`; consumed by `Read`, `Write`, `Seek`,
    /// `Close`, `Stat`-of-fd, syscall/ioctl with an `fd` argument.
    FileHandle, "file"
}

define_handle! {
    /// Identifier for an asynchronously-launched process in the
    /// agent's job table. Returned by `RunAsync`; consumed by
    /// process-control ops (`Wait`, `Kill`, `Signal`,
    /// `StdinWrite`, `StdinClose`).
    ProcessHandle, "proc"
}

define_handle! {
    /// Identifier for a spawned sub-agent (worker). Workers are
    /// addressable via the same op set as the top-level agent, so
    /// op messages targeting a worker carry both the worker handle
    /// and the inner op.
    WorkerHandle, "worker"
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn handles_are_serde_transparent() {
        // FileHandle on the wire must be just a u64, not a struct
        // with a `0` field. This is what makes the typed wrapper
        // free at the protocol layer.
        let handle = FileHandle::new(42);
        let bytes = rmp_serde::to_vec_named(&handle).unwrap();
        let raw: u64 = rmp_serde::from_slice(&bytes).unwrap();
        assert_eq!(raw, 42);

        let from_raw: FileHandle = rmp_serde::from_slice(&bytes).unwrap();
        assert_eq!(from_raw, handle);
    }

    #[test]
    fn distinct_types_round_trip_independently() {
        let f = FileHandle::new(1);
        let p = ProcessHandle::new(1);
        let w = WorkerHandle::new(1);

        // Same underlying id but distinct types — confirmed at compile
        // time by the lack of a coercion between them.
        assert_eq!(f.get(), p.get());
        assert_eq!(p.get(), w.get());
    }

    #[test]
    fn display_includes_kind_for_debugging() {
        assert_eq!(FileHandle::new(7).to_string(), "file#7");
        assert_eq!(ProcessHandle::new(7).to_string(), "proc#7");
        assert_eq!(WorkerHandle::new(7).to_string(), "worker#7");
    }
}
