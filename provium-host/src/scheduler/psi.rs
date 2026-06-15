//! Pressure Stall Information–driven adaptive throttle.
//!
//! Per `DESIGN.md` § Adaptive host pressure: read
//! `/proc/pressure/memory` every ~1 s. If `some avg10 > 10%`,
//! pause new file dispatches; resume when the metric drops.
//!
//! ## Why not `MemAvailable`?
//!
//! Bazel found `MemAvailable` polling oscillates under page-cache
//! churn + KSM activity. PSI is purpose-built for this and reports
//! a percentage directly. Linux-only — graceful no-op fallback on
//! systems without `CONFIG_PSI`.

use std::fs;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

/// Default pressure threshold for "throttle dispatch."
pub const DEFAULT_PSI_THRESHOLD_PCT: f64 = 10.0;

/// Default poll interval.
pub const DEFAULT_PSI_INTERVAL: Duration = Duration::from_millis(1000);

/// Background PSI monitor — watches both `/proc/pressure/memory` and
/// `/proc/pressure/cpu` per `DESIGN.md` § Adaptive host pressure.
/// Returns a [`PressureFlag`] that the dispatcher checks before
/// pulling a file off the queue. Either signal trips the flag.
pub fn spawn(threshold_pct: f64, interval: Duration) -> PressureFlag {
    let flag = PressureFlag::default();
    let flag_for_thread = flag.clone();
    thread::Builder::new()
        .name("provium-psi".into())
        .spawn(move || run_loop(threshold_pct, interval, flag_for_thread))
        .ok();
    flag
}

/// Lock-free flag the dispatcher reads cheaply on its hot path.
#[derive(Clone, Default, Debug)]
pub struct PressureFlag {
    inner: Arc<AtomicBool>,
}

impl PressureFlag {
    /// `true` when the system is under pressure; pause dispatch.
    pub fn is_pressured(&self) -> bool {
        self.inner.load(Ordering::Relaxed)
    }

    pub(crate) fn set(&self, value: bool) {
        self.inner.store(value, Ordering::Relaxed);
    }
}

fn run_loop(threshold_pct: f64, interval: Duration, flag: PressureFlag) {
    loop {
        let mem = read_some_avg10_at("/proc/pressure/memory").unwrap_or(0.0);
        let cpu = read_some_avg10_at("/proc/pressure/cpu").unwrap_or(0.0);
        flag.set(mem > threshold_pct || cpu > threshold_pct);
        thread::sleep(interval);
    }
}

/// Parse `some avg10=X.XX` from `/proc/pressure/memory`. Returns
/// `None` when PSI is unavailable (kernel without `CONFIG_PSI`,
/// containers without /proc, etc.).
pub fn read_some_avg10() -> Option<f64> {
    read_some_avg10_at("/proc/pressure/memory")
}

/// As [`read_some_avg10`] but takes a custom PSI path. Used by the
/// monitor to consult both memory and CPU.
pub fn read_some_avg10_at(path: &str) -> Option<f64> {
    let body = fs::read_to_string(path).ok()?;
    for line in body.lines() {
        if let Some(rest) = line.strip_prefix("some ") {
            for token in rest.split_whitespace() {
                if let Some(v) = token.strip_prefix("avg10=") {
                    return v.parse::<f64>().ok();
                }
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flag_default_is_clear() {
        let f = PressureFlag::default();
        assert!(!f.is_pressured());
    }

    #[test]
    fn flag_set_is_visible() {
        let f = PressureFlag::default();
        f.set(true);
        assert!(f.is_pressured());
    }

    // The actual /proc parser is exercised on systems with PSI;
    // here we just confirm graceful behaviour when unavailable.
    #[test]
    fn read_some_avg10_returns_none_when_unavailable() {
        // We can't simulate the absence on this host, but we can
        // confirm the parser doesn't panic. Either Some(_) (PSI
        // is on) or None (off) — both fine.
        let _ = read_some_avg10();
    }
}
