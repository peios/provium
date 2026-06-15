//! Resource pool — memory + cpus reservation table with blocking
//! acquire.
//!
//! Per `DESIGN.md` § Scheduler / Pool:
//!
//! ```text
//! Pool = { memory, cpus }
//! ```
//!
//! Memory is bytes, CPU is integer vCPU count, both hard-gated. The
//! design's "vsock CIDs are u32 — effectively unlimited, not tracked
//! as a pool resource" is honoured by [`crate::cid::CidAllocator`]
//! living separately. KVM fds, TAP devices, file descriptors are
//! provisioned high enough at startup that they don't bottleneck —
//! see [`super::preflight`].
//!
//! Acquire is blocking: the call parks on a [`Condvar`] until the
//! request fits in `available`. Release wakes every waiter; the
//! kernel's wakeup ordering is not guaranteed fair but practical
//! starvation is bounded (every waiter gets a chance every release
//! and only the smallest-fitting requests block briefly).

use std::sync::{Arc, Condvar, Mutex, Weak};

/// Memory + CPU pair. Mirrors
/// [`provium_protocol::events::ResourceAmount`] which the event
/// stream emits — re-exported here for ergonomics, with arithmetic
/// helpers the wire type intentionally lacks.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ResourceAmount {
    /// Memory in bytes.
    pub memory_bytes: u64,
    /// vCPU count.
    pub cpus: u32,
}

impl ResourceAmount {
    /// Build from an explicit pair.
    pub const fn new(memory_bytes: u64, cpus: u32) -> Self {
        Self { memory_bytes, cpus }
    }

    /// `true` if `self` fits within `budget` along both axes.
    pub fn fits_in(self, budget: ResourceAmount) -> bool {
        self.memory_bytes <= budget.memory_bytes && self.cpus <= budget.cpus
    }

    /// Saturating addition along both axes.
    pub fn saturating_add(self, other: ResourceAmount) -> ResourceAmount {
        ResourceAmount {
            memory_bytes: self.memory_bytes.saturating_add(other.memory_bytes),
            cpus: self.cpus.saturating_add(other.cpus),
        }
    }

    /// Saturating subtraction along both axes.
    pub fn saturating_sub(self, other: ResourceAmount) -> ResourceAmount {
        ResourceAmount {
            memory_bytes: self.memory_bytes.saturating_sub(other.memory_bytes),
            cpus: self.cpus.saturating_sub(other.cpus),
        }
    }
}

/// Resource pool. Construct once per provium run; share via [`Arc`]
/// across runner threads.
#[derive(Debug)]
pub struct Pool {
    state: Mutex<PoolState>,
    cond: Condvar,
}

#[derive(Debug)]
struct PoolState {
    total: ResourceAmount,
    available: ResourceAmount,
    /// Telemetry — number of acquirers currently parked on the
    /// condvar. Cheap to maintain, surfaces in
    /// [`Pool::pending_count`] for `file_blocked` events.
    pending: usize,
}

impl Pool {
    /// Build with `total` capacity.
    pub fn new(total: ResourceAmount) -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(PoolState {
                total,
                available: total,
                pending: 0,
            }),
            cond: Condvar::new(),
        })
    }

    /// Total budget the pool was constructed with.
    pub fn total(&self) -> ResourceAmount {
        self.state.lock().unwrap().total
    }

    /// Currently-available budget (total − sum of live reservations).
    pub fn available(&self) -> ResourceAmount {
        self.state.lock().unwrap().available
    }

    /// Number of acquirers currently parked waiting.
    pub fn pending_count(&self) -> usize {
        self.state.lock().unwrap().pending
    }

    /// Block until `amount` is available, then reserve it. The
    /// returned [`Reservation`] holds the resources for its lifetime;
    /// dropping it releases them and wakes one waiter.
    ///
    /// Returns `None` if `amount` exceeds the *total* pool capacity —
    /// that request can never be satisfied, so the caller should fail
    /// fast rather than block forever. Tests + the dispatcher surface
    /// this as a per-file ("budget exceeded") failure.
    pub fn acquire(self: &Arc<Self>, amount: ResourceAmount) -> Option<Reservation> {
        let mut state = self.state.lock().unwrap();
        if !amount.fits_in(state.total) {
            return None;
        }
        // R9 sched-M1: only count toward `pending` while we're
        // ACTUALLY blocked. Pre-incrementing made non-blocking
        // acquires transiently visible as pending, which the
        // dispatcher uses to decide whether to emit
        // `file_blocked` events — spurious increments produced
        // spurious events.
        let mut counted_pending = false;
        while !amount.fits_in(state.available) {
            if !counted_pending {
                state.pending += 1;
                counted_pending = true;
            }
            state = self.cond.wait(state).unwrap();
        }
        if counted_pending {
            state.pending -= 1;
        }
        state.available = state.available.saturating_sub(amount);
        Some(Reservation {
            amount,
            pool: Arc::downgrade(self),
        })
    }

    /// Try to acquire without blocking. Returns `None` on
    /// "would block" (request exceeds available right now, but might
    /// fit later) and on "exceeds total" alike — call sites that need
    /// to distinguish use [`Self::acquire`] which only returns `None`
    /// for the latter.
    pub fn try_acquire(self: &Arc<Self>, amount: ResourceAmount) -> Option<Reservation> {
        let mut state = self.state.lock().unwrap();
        if !amount.fits_in(state.available) {
            return None;
        }
        state.available = state.available.saturating_sub(amount);
        Some(Reservation {
            amount,
            pool: Arc::downgrade(self),
        })
    }

    fn release(&self, amount: ResourceAmount) {
        let mut state = self.state.lock().unwrap();
        state.available = state.available.saturating_add(amount);
        // Cap at total — defense against accidental double-release.
        if !state.available.fits_in(state.total) {
            state.available = state.total;
        }
        // notify_all rather than notify_one because the smallest
        // waiter might not be at the front of the condvar queue.
        self.cond.notify_all();
    }
}

/// RAII handle for an active reservation. Drops release the
/// resources back to the pool.
#[derive(Debug)]
pub struct Reservation {
    amount: ResourceAmount,
    pool: Weak<Pool>,
}

impl Reservation {
    /// Bytes + cpus held by this reservation.
    pub fn amount(&self) -> ResourceAmount {
        self.amount
    }
}

impl Drop for Reservation {
    fn drop(&mut self) {
        if let Some(pool) = self.pool.upgrade() {
            pool.release(self.amount);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::thread;
    use std::time::{Duration, Instant};

    fn budget(memory_bytes: u64, cpus: u32) -> ResourceAmount {
        ResourceAmount { memory_bytes, cpus }
    }

    #[test]
    fn acquire_succeeds_within_budget() {
        let pool = Pool::new(budget(1024, 4));
        let r = pool.acquire(budget(512, 2)).unwrap();
        assert_eq!(pool.available(), budget(512, 2));
        drop(r);
        assert_eq!(pool.available(), budget(1024, 4));
    }

    #[test]
    fn acquire_returns_none_when_request_exceeds_total() {
        let pool = Pool::new(budget(1024, 4));
        assert!(pool.acquire(budget(2048, 4)).is_none());
        assert!(pool.acquire(budget(1024, 8)).is_none());
    }

    #[test]
    fn acquire_blocks_until_release() {
        let pool = Pool::new(budget(1024, 2));
        let first = pool.acquire(budget(1024, 2)).unwrap();

        let pool_for_thread = Arc::clone(&pool);
        let started = Arc::new(AtomicUsize::new(0));
        let started_for_thread = Arc::clone(&started);
        let handle = thread::spawn(move || {
            started_for_thread.store(1, Ordering::SeqCst);
            let r = pool_for_thread.acquire(budget(1024, 2)).unwrap();
            started_for_thread.store(2, Ordering::SeqCst);
            drop(r);
        });

        // Spin briefly to confirm the thread parked rather than
        // proceeded.
        thread::sleep(Duration::from_millis(40));
        assert_eq!(started.load(Ordering::SeqCst), 1, "should be parked");
        assert_eq!(pool.pending_count(), 1);

        drop(first);
        handle.join().unwrap();
        assert_eq!(started.load(Ordering::SeqCst), 2);
        assert_eq!(pool.pending_count(), 0);
        assert_eq!(pool.available(), budget(1024, 2));
    }

    #[test]
    fn try_acquire_returns_none_on_overflow() {
        let pool = Pool::new(budget(1024, 4));
        let _hold = pool.acquire(budget(900, 3)).unwrap();
        assert!(pool.try_acquire(budget(200, 2)).is_none());
        assert!(pool.try_acquire(budget(100, 1)).is_some());
    }

    #[test]
    fn parallel_acquires_serialize_via_condvar() {
        let pool = Pool::new(budget(2048, 8));
        let mut handles = Vec::new();
        for _ in 0..16 {
            let pool = Arc::clone(&pool);
            handles.push(thread::spawn(move || {
                let r = pool.acquire(budget(1024, 4)).unwrap();
                thread::sleep(Duration::from_millis(20));
                drop(r);
            }));
        }
        let started = Instant::now();
        for h in handles {
            h.join().unwrap();
        }
        // 16 reservations, only 2 fit at a time → 8 sequential
        // batches × 20ms ≈ 160ms minimum. Generous upper bound to
        // avoid flakiness on slow CI.
        let elapsed = started.elapsed();
        assert!(
            elapsed >= Duration::from_millis(150),
            "should have serialized: {elapsed:?}"
        );
        assert_eq!(pool.available(), budget(2048, 8));
    }

    #[test]
    fn reservation_release_cant_overflow_total() {
        let pool = Pool::new(budget(1024, 4));
        let r = pool.acquire(budget(512, 2)).unwrap();
        drop(r);
        assert_eq!(pool.available(), budget(1024, 4));
    }
}
