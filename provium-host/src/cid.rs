//! vsock Context-ID allocation.
//!
//! Per `DESIGN.md` Scheduler § Pool: vsock CIDs are an [`AtomicU64`]
//! counter starting at 100 (well-known ids 0/1/2 plus headroom) that
//! is **never recycled within a session**. The u32 namespace would
//! take centuries to exhaust at any realistic test rate, and the
//! never-reuse policy sidesteps the kernel's ~330 ms post-VMM-exit
//! grace before a CID can be re-bound.

use std::sync::atomic::{AtomicU64, Ordering};

/// Smallest CID the allocator will hand out. 0 = `VMADDR_CID_ANY`,
/// 1 = `VMADDR_CID_HYPERVISOR`, 2 = `VMADDR_CID_HOST`. Skipping past
/// these plus a small buffer keeps logs and tests free of confusion.
pub const FIRST_CID: u32 = 100;

/// Monotonic vsock CID allocator. Cheap to construct and clone via
/// [`std::sync::Arc`].
#[derive(Debug)]
pub struct CidAllocator {
    next: AtomicU64,
}

impl CidAllocator {
    /// Build a fresh allocator starting at [`FIRST_CID`].
    pub fn new() -> Self {
        Self::starting_at(FIRST_CID)
    }

    /// Build an allocator whose next-allocated CID is `start`. Tests
    /// use this to make CID values predictable.
    pub fn starting_at(start: u32) -> Self {
        Self {
            next: AtomicU64::new(u64::from(start)),
        }
    }

    /// Allocate the next CID.
    ///
    /// Panics only if the u32 namespace is exhausted (unreachable in
    /// any realistic provium run).
    pub fn allocate(&self) -> u32 {
        let n = self.next.fetch_add(1, Ordering::Relaxed);
        u32::try_from(n).expect("vsock CID space exhausted (u32)")
    }

    /// Peek at the next CID without allocating it. Test introspection
    /// only.
    pub fn peek(&self) -> u32 {
        u32::try_from(self.next.load(Ordering::Relaxed))
            .expect("vsock CID space exhausted (u32)")
    }
}

impl Default for CidAllocator {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::thread;

    #[test]
    fn first_allocation_is_first_cid_constant() {
        let a = CidAllocator::new();
        assert_eq!(a.allocate(), FIRST_CID);
    }

    #[test]
    fn allocations_are_monotonic() {
        let a = CidAllocator::new();
        let mut prev = 0u32;
        for _ in 0..16 {
            let c = a.allocate();
            assert!(c > prev, "{c} should exceed {prev}");
            prev = c;
        }
    }

    #[test]
    fn peek_does_not_advance() {
        let a = CidAllocator::starting_at(500);
        assert_eq!(a.peek(), 500);
        assert_eq!(a.peek(), 500);
        assert_eq!(a.allocate(), 500);
        assert_eq!(a.peek(), 501);
    }

    #[test]
    fn parallel_allocators_produce_no_duplicates() {
        let a = Arc::new(CidAllocator::new());
        let mut handles = Vec::new();
        for _ in 0..8 {
            let a = Arc::clone(&a);
            handles.push(thread::spawn(move || {
                let mut got = Vec::new();
                for _ in 0..100 {
                    got.push(a.allocate());
                }
                got
            }));
        }
        let mut all: Vec<u32> = handles.into_iter().flat_map(|h| h.join().unwrap()).collect();
        all.sort_unstable();
        for w in all.windows(2) {
            assert_ne!(w[0], w[1], "duplicate CID allocated: {}", w[0]);
        }
        assert_eq!(all.len(), 800);
    }
}
