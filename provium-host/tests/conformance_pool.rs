//! Conformance: scheduler `Pool` per `DESIGN.md` § Resource model.
//! Pool gates parallel test execution; acquire blocks until the
//! request fits, release wakes waiters, exceed-total fails fast.

use std::sync::Arc;
use std::time::Duration;

use provium_host::scheduler::pool::{Pool, ResourceAmount};

fn small() -> ResourceAmount {
    ResourceAmount { memory_bytes: 1024, cpus: 1 }
}

fn pool_of(memory_bytes: u64, cpus: u32) -> Arc<Pool> {
    Pool::new(ResourceAmount { memory_bytes, cpus })
}

#[test]
fn acquire_within_budget_returns_reservation() {
    let pool = pool_of(8192, 4);
    let r = pool.acquire(small());
    assert!(r.is_some());
}

#[test]
fn acquire_exceeding_total_returns_none_immediately() {
    // Per Pool docs: requests exceeding TOTAL fail-fast (vs.
    // exceeding AVAILABLE which would block).
    let pool = pool_of(1024, 1);
    let r = pool.acquire(ResourceAmount { memory_bytes: 1_000_000, cpus: 1 });
    assert!(r.is_none(), "exceed-total request must short-circuit");
}

#[test]
fn release_via_drop_replenishes_available() {
    let pool = pool_of(2048, 2);
    let initial = pool.available();
    {
        let _r = pool.acquire(small()).unwrap();
        assert_ne!(pool.available(), initial,
            "available must drop while holding reservation");
    }
    assert_eq!(pool.available(), initial,
        "drop must release back to initial");
}

#[test]
fn try_acquire_returns_none_when_full() {
    let pool = pool_of(1024, 1);
    let _hold = pool.acquire(small()).unwrap();
    let r = pool.try_acquire(small());
    assert!(r.is_none(), "try_acquire must not block / must say no");
}

#[test]
fn try_acquire_succeeds_after_release() {
    let pool = pool_of(1024, 1);
    {
        let _r = pool.acquire(small()).unwrap();
    }
    let r = pool.try_acquire(small());
    assert!(r.is_some());
}

#[test]
fn parallel_acquires_serialize_through_release() {
    // One thread holds; another acquires; release the first;
    // the second proceeds.
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::thread;

    let pool = pool_of(1024, 1);
    let pool_for_first = Arc::clone(&pool);
    let pool_for_second = Arc::clone(&pool);
    let second_done = Arc::new(AtomicBool::new(false));
    let second_done_clone = Arc::clone(&second_done);

    let r1 = pool_for_first.acquire(small()).unwrap();
    let h = thread::spawn(move || {
        let _r2 = pool_for_second.acquire(small()).unwrap();
        second_done_clone.store(true, Ordering::SeqCst);
    });
    // Give the second thread time to enter the wait. It must NOT
    // have completed yet — we still hold r1.
    thread::sleep(Duration::from_millis(50));
    assert!(!second_done.load(Ordering::SeqCst),
        "second acquire must block while first holds");
    drop(r1);
    h.join().unwrap();
    assert!(second_done.load(Ordering::SeqCst));
}

#[test]
fn pending_count_reflects_blocked_waiters() {
    use std::thread;
    let pool = pool_of(1024, 1);
    let _hold = pool.acquire(small()).unwrap();
    let pool2 = Arc::clone(&pool);
    let h = thread::spawn(move || {
        let _r = pool2.acquire(small()).unwrap();
    });
    // Wait briefly for the second thread to actually enter the
    // wait loop.
    thread::sleep(Duration::from_millis(50));
    assert_eq!(pool.pending_count(), 1,
        "pending_count must include blocked waiters");
    drop(_hold);
    h.join().unwrap();
    assert_eq!(pool.pending_count(), 0);
}

#[test]
fn resource_amount_addition_is_saturating() {
    let a = ResourceAmount { memory_bytes: u64::MAX - 10, cpus: u32::MAX - 1 };
    let b = ResourceAmount { memory_bytes: 100, cpus: 5 };
    let s = a.saturating_add(b);
    assert_eq!(s.memory_bytes, u64::MAX);
    assert_eq!(s.cpus, u32::MAX);
}

#[test]
fn resource_amount_subtraction_is_saturating() {
    let a = ResourceAmount { memory_bytes: 100, cpus: 1 };
    let b = ResourceAmount { memory_bytes: 1000, cpus: 5 };
    let s = a.saturating_sub(b);
    assert_eq!(s.memory_bytes, 0);
    assert_eq!(s.cpus, 0);
}

#[test]
fn fits_in_strict_inequality_only_at_boundary() {
    let budget = ResourceAmount { memory_bytes: 1024, cpus: 4 };
    let exact = ResourceAmount { memory_bytes: 1024, cpus: 4 };
    let over_mem = ResourceAmount { memory_bytes: 1025, cpus: 4 };
    let over_cpus = ResourceAmount { memory_bytes: 1024, cpus: 5 };
    assert!(exact.fits_in(budget), "exact equal must fit");
    assert!(!over_mem.fits_in(budget));
    assert!(!over_cpus.fits_in(budget));
}
