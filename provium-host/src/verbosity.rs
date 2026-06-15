//! Process-global verbosity flag for CLI diagnostics.
//!
//! Per-VM lifecycle chatter ("agent overlay injected", "agent up
//! after Xs") is useful when debugging a boot but drowns the test
//! output on a healthy run — a 40-file suite emits hundreds of
//! lines. Those call sites live deep in the VMM layer (and in free
//! functions with no config handle), so threading a `verbose` field
//! through every constructor would be invasive. A process-global
//! flag set once at startup is the idiomatic shape for scattered
//! CLI diagnostics.
//!
//! Genuine warnings (a slow agent boot, a failed `SCHED_BATCH`) are
//! NOT gated — they stay unconditional because they signal a real
//! problem regardless of verbosity.

use std::sync::atomic::{AtomicBool, Ordering};

static VERBOSE: AtomicBool = AtomicBool::new(false);

/// Enable or disable verbose diagnostic output. Called once at
/// startup from the CLI's `--verbose` flag. Defaults to `false`.
pub fn set_verbose(on: bool) {
    VERBOSE.store(on, Ordering::Relaxed);
}

/// `true` when verbose diagnostics should be printed.
pub fn is_verbose() -> bool {
    VERBOSE.load(Ordering::Relaxed)
}
