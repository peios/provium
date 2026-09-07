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
//!
//! # Owners, and the deadlock the pool can prove
//!
//! A reservation may be attributed to an [`OwnerId`] — one per test
//! file. The pool then knows, for every owner, what it holds and
//! whether it is parked waiting for more. That is enough to *prove* a
//! hold-and-wait deadlock: when every owner holding part of the pool is
//! itself waiting, and no waiting request fits in what is free, nothing
//! will ever be released again. The pool declares it on the request
//! that closes the cycle, which fails with [`AcquireError::Deadlock`]
//! instead of parking forever (PEI-810: thirty-six files each holding
//! one VM and waiting for a second stalled a whole run until the file
//! watchdogs killed them).
//!
//! An anonymous reservation — one made through [`Pool::acquire`] with
//! no owner — may be released at any time by code the pool cannot see,
//! so while one exists no deadlock is ever declared.
//!
//! # Accounts and claims
//!
//! A file's [`Account`] carries its owner id and, once the file has
//! called `provium:claim`, its claim. A claim is the file's whole VM
//! budget: every boot in the file takes a [`ClaimSlice`] from it and
//! touches the pool not at all, so a claimed file can never block
//! mid-file — the queueing happened once, up front, at the claim. A
//! boot the claim cannot cover fails at once with [`ClaimExceeded`]
//! rather than falling back to the pool, because falling back would
//! reintroduce exactly the hold-and-wait the claim exists to prevent.

use std::cell::RefCell;
use std::collections::HashMap;
use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, Weak};
use std::time::{Duration, Instant};

// ---------------------------------------------------------------------------
// Wait sink — how long this thread has spent parked in the pool.
// ---------------------------------------------------------------------------

/// Accumulates the time a thread spends parked in [`Pool::acquire`].
///
/// The scheduler runs one thread per test file and starts a wall-clock
/// watchdog for each. Time a file spends parked here is time it is not
/// running, so charging it to the file's timeout kills files for
/// queueing rather than for being slow. The dispatcher installs a sink
/// on the file's thread and extends the deadline by [`Self::waited`].
///
/// A wait still in progress counts: `waited` includes the time since
/// the current park began, not only waits that have already ended. A
/// previous version settled the time only once the acquire returned,
/// which meant a wait that never ended was never counted — and it was
/// exactly those waits, the deadlocked ones, that mattered.
#[derive(Debug, Default)]
pub struct WaitSink {
    settled_ns: AtomicU64,
    parked_since: Mutex<Option<Instant>>,
}

impl WaitSink {
    /// A fresh, empty sink.
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Total time parked so far, the current park included.
    pub fn waited(&self) -> Duration {
        let settled = Duration::from_nanos(self.settled_ns.load(Ordering::Relaxed));
        let live = self
            .parked_since
            .lock()
            .unwrap()
            .map(|since| since.elapsed())
            .unwrap_or_default();
        settled + live
    }

    fn park(&self) {
        *self.parked_since.lock().unwrap() = Some(Instant::now());
    }

    fn unpark(&self) {
        if let Some(since) = self.parked_since.lock().unwrap().take() {
            self.settled_ns
                .fetch_add(since.elapsed().as_nanos() as u64, Ordering::Relaxed);
        }
    }
}

thread_local! {
    /// Where this thread reports time it spends parked in
    /// [`Pool::acquire`]. `None` unless something installed one.
    static WAIT_SINK: RefCell<Option<Arc<WaitSink>>> = const { RefCell::new(None) };
}

/// Count this thread's pool waits into `sink` until the returned
/// guard drops.
///
/// A thread with no sink installed — the REPL, a fixture build, a unit
/// test — records nothing and behaves as before.
pub fn install_wait_sink(sink: Arc<WaitSink>) -> WaitSinkGuard {
    WAIT_SINK.with(|s| *s.borrow_mut() = Some(sink));
    WaitSinkGuard(())
}

/// Removes this thread's wait sink on drop.
#[derive(Debug)]
pub struct WaitSinkGuard(());

impl Drop for WaitSinkGuard {
    fn drop(&mut self) {
        WAIT_SINK.with(|s| *s.borrow_mut() = None);
    }
}

/// This thread's wait sink, if one is installed.
///
/// For code that starts its own watchdog on the file's thread and needs
/// to make the same deadline adjustment — the per-test watchdog in the
/// Lua runner does exactly this.
pub fn current_wait_sink() -> Option<Arc<WaitSink>> {
    WAIT_SINK.with(|s| s.borrow().clone())
}

fn sink_park() {
    WAIT_SINK.with(|s| {
        if let Some(sink) = s.borrow().as_ref() {
            sink.park();
        }
    });
}

fn sink_unpark() {
    WAIT_SINK.with(|s| {
        if let Some(sink) = s.borrow().as_ref() {
            sink.unpark();
        }
    });
}

// ---------------------------------------------------------------------------
// ResourceAmount
// ---------------------------------------------------------------------------

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

    /// `true` when both axes are zero.
    pub fn is_zero(self) -> bool {
        self.memory_bytes == 0 && self.cpus == 0
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

fn fmt_bytes(b: u64) -> String {
    const MIB: u64 = 1024 * 1024;
    const GIB: u64 = 1024 * MIB;
    if b >= GIB {
        format!("{:.1} GiB", b as f64 / GIB as f64)
    } else if b >= MIB {
        format!("{} MiB", b / MIB)
    } else {
        format!("{b} B")
    }
}

impl fmt::Display for ResourceAmount {
    /// Human form for error messages: `1.1 GiB and 1 cpu`.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = if self.cpus == 1 { "" } else { "s" };
        write!(f, "{} and {} cpu{s}", fmt_bytes(self.memory_bytes), self.cpus)
    }
}

// ---------------------------------------------------------------------------
// Owners and errors
// ---------------------------------------------------------------------------

/// Who a reservation belongs to — one per test file. Minted by
/// [`Pool::new_owner`]; carried by the file's [`Account`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct OwnerId(u64);

/// Why an attributed acquire did not produce a reservation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AcquireError {
    /// The request is larger than the whole pool. No amount of
    /// waiting will satisfy it.
    ExceedsTotal {
        /// What was asked for.
        requested: ResourceAmount,
        /// The pool's capacity.
        total: ResourceAmount,
    },
    /// Parking would have completed a proven hold-and-wait cycle.
    Deadlock(Deadlock),
}

impl fmt::Display for AcquireError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ExceedsTotal { requested, total } => {
                write!(f, "requested {requested}, which exceeds the pool's total of {total}")
            }
            Self::Deadlock(d) => d.fmt(f),
        }
    }
}

impl std::error::Error for AcquireError {}

/// The state of the pool at the moment a deadlock was proven.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Deadlock {
    /// What the failing acquire wanted.
    pub requested: ResourceAmount,
    /// What was free — enough for none of the waiting requests.
    pub available: ResourceAmount,
    /// The pool's capacity.
    pub total: ResourceAmount,
    /// Owners holding part of the pool. Every one of them was parked
    /// waiting for more; that is what makes it a deadlock.
    pub holders: usize,
    /// Parked requests, the failing one included.
    pub waiters: usize,
}

impl fmt::Display for Deadlock {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let held = self.total.saturating_sub(self.available);
        let files = if self.holders == 1 { "file holds" } else { "files hold" };
        write!(
            f,
            "the pool is deadlocked: {} {files} {held} of its {} and every one of them is \
             waiting for more, while the {} still free serves none of the {} waiting \
             requests (this one wants {})",
            self.holders, self.total, self.available, self.waiters, self.requested
        )
    }
}

// ---------------------------------------------------------------------------
// Pool
// ---------------------------------------------------------------------------

/// Resource pool. Construct once per provium run; share via [`Arc`]
/// across runner threads.
#[derive(Debug)]
pub struct Pool {
    state: Mutex<PoolState>,
    cond: Condvar,
}

#[derive(Debug, Default)]
struct OwnerState {
    /// Sum of this owner's live reservations.
    held: ResourceAmount,
    /// Threads of this owner currently parked in an acquire.
    waiting: usize,
}

impl OwnerState {
    fn is_idle(&self) -> bool {
        self.held.is_zero() && self.waiting == 0
    }
}

#[derive(Debug)]
struct PoolState {
    total: ResourceAmount,
    available: ResourceAmount,
    /// Per-owner holdings and parked-thread counts. An entry exists
    /// only while its owner holds or waits for something.
    owners: HashMap<OwnerId, OwnerState>,
    /// The request of every parked acquirer, keyed by ticket. Its
    /// length is what [`Pool::pending_count`] reports for
    /// `file_blocked` events.
    waiting: HashMap<u64, ResourceAmount>,
    next_id: u64,
}

impl PoolState {
    fn next_id(&mut self) -> u64 {
        self.next_id += 1;
        self.next_id
    }

    fn owner_mut(&mut self, owner: OwnerId) -> &mut OwnerState {
        self.owners.entry(owner).or_default()
    }

    fn prune(&mut self, owner: OwnerId) {
        if self.owners.get(&owner).is_some_and(OwnerState::is_idle) {
            self.owners.remove(&owner);
        }
    }

    /// The verdict, taken under the lock. `Some` only when nothing in
    /// the pool can ever be released again without outside help.
    fn deadlock(&self, requested: ResourceAmount) -> Option<Deadlock> {
        // Anything held anonymously may be released by code the pool
        // cannot see, so nothing can be concluded while it is out.
        let owned = self
            .owners
            .values()
            .fold(ResourceAmount::default(), |acc, o| acc.saturating_add(o.held));
        if owned != self.total.saturating_sub(self.available) {
            return None;
        }
        // Every holder must be parked. One that is still running will
        // release what it holds when it is done.
        let mut holders = 0;
        for o in self.owners.values() {
            if !o.held.is_zero() {
                if o.waiting == 0 {
                    return None;
                }
                holders += 1;
            }
        }
        if holders == 0 {
            return None;
        }
        // And no parked request may fit — one that does is progress the
        // next wake-up will make.
        if self.waiting.values().any(|w| w.fits_in(self.available)) {
            return None;
        }
        Some(Deadlock {
            requested,
            available: self.available,
            total: self.total,
            holders,
            waiters: self.waiting.len(),
        })
    }
}

impl Pool {
    /// Build with `total` capacity.
    pub fn new(total: ResourceAmount) -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(PoolState {
                total,
                available: total,
                owners: HashMap::new(),
                waiting: HashMap::new(),
                next_id: 0,
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
        self.state.lock().unwrap().waiting.len()
    }

    /// Mint an owner id. One per test file.
    pub fn new_owner(&self) -> OwnerId {
        OwnerId(self.state.lock().unwrap().next_id())
    }

    /// Block until `amount` is available, then reserve it. The
    /// returned [`Reservation`] holds the resources for its lifetime;
    /// dropping it releases them and wakes every waiter.
    ///
    /// Returns `None` if `amount` exceeds the *total* pool capacity —
    /// that request can never be satisfied, so the caller should fail
    /// fast rather than block forever. Tests + the dispatcher surface
    /// this as a per-file ("budget exceeded") failure.
    ///
    /// The reservation is anonymous: the pool cannot tell whose it is,
    /// so it never declares a deadlock while one is held. Code that
    /// runs inside a test file uses [`Self::acquire_for`].
    pub fn acquire(self: &Arc<Self>, amount: ResourceAmount) -> Option<Reservation> {
        self.acquire_inner(None, amount).ok()
    }

    /// [`Self::acquire`] on behalf of `owner`.
    ///
    /// Attributed acquires are what make the deadlock verdict possible:
    /// if parking this request would leave every holder in the pool
    /// waiting, with nothing free that any of them could use, the
    /// request fails with [`AcquireError::Deadlock`] instead.
    pub fn acquire_for(
        self: &Arc<Self>,
        owner: OwnerId,
        amount: ResourceAmount,
    ) -> Result<Reservation, AcquireError> {
        self.acquire_inner(Some(owner), amount)
    }

    fn acquire_inner(
        self: &Arc<Self>,
        owner: Option<OwnerId>,
        amount: ResourceAmount,
    ) -> Result<Reservation, AcquireError> {
        let mut state = self.state.lock().unwrap();
        if !amount.fits_in(state.total) {
            return Err(AcquireError::ExceedsTotal {
                requested: amount,
                total: state.total,
            });
        }
        // R9 sched-M1: only count as waiting while ACTUALLY parked.
        // Pre-registering made non-blocking acquires transiently
        // visible as pending, which the dispatcher uses to decide
        // whether to emit `file_blocked` events — spurious
        // registrations produced spurious events.
        let mut ticket = None;
        while !amount.fits_in(state.available) {
            if ticket.is_none() {
                let t = state.next_id();
                state.waiting.insert(t, amount);
                if let Some(o) = owner {
                    state.owner_mut(o).waiting += 1;
                }
                ticket = Some(t);
                sink_park();
            }
            if owner.is_some() {
                if let Some(deadlock) = state.deadlock(amount) {
                    Self::withdraw(&mut state, owner, ticket.take());
                    return Err(AcquireError::Deadlock(deadlock));
                }
            }
            state = self.cond.wait(state).unwrap();
        }
        Self::withdraw(&mut state, owner, ticket);
        state.available = state.available.saturating_sub(amount);
        if let Some(o) = owner {
            let held = state.owner_mut(o).held;
            state.owner_mut(o).held = held.saturating_add(amount);
        }
        Ok(Reservation {
            amount,
            owner,
            pool: Arc::downgrade(self),
        })
    }

    /// Leave the waiting set, if this acquire was ever in it.
    fn withdraw(state: &mut PoolState, owner: Option<OwnerId>, ticket: Option<u64>) {
        let Some(t) = ticket else { return };
        state.waiting.remove(&t);
        if let Some(o) = owner {
            let w = state.owner_mut(o);
            w.waiting = w.waiting.saturating_sub(1);
            state.prune(o);
        }
        sink_unpark();
    }

    /// Try to acquire without blocking. Returns `None` on
    /// "would block" (request exceeds available right now, but might
    /// fit later) and on "exceeds total" alike — call sites that need
    /// to distinguish use [`Self::acquire`] which only returns `None`
    /// for the latter.
    pub fn try_acquire(self: &Arc<Self>, amount: ResourceAmount) -> Option<Reservation> {
        self.try_acquire_inner(None, amount)
    }

    /// [`Self::try_acquire`] on behalf of `owner`.
    pub fn try_acquire_for(
        self: &Arc<Self>,
        owner: OwnerId,
        amount: ResourceAmount,
    ) -> Option<Reservation> {
        self.try_acquire_inner(Some(owner), amount)
    }

    fn try_acquire_inner(
        self: &Arc<Self>,
        owner: Option<OwnerId>,
        amount: ResourceAmount,
    ) -> Option<Reservation> {
        let mut state = self.state.lock().unwrap();
        if !amount.fits_in(state.available) {
            return None;
        }
        state.available = state.available.saturating_sub(amount);
        if let Some(o) = owner {
            let held = state.owner_mut(o).held;
            state.owner_mut(o).held = held.saturating_add(amount);
        }
        Some(Reservation {
            amount,
            owner,
            pool: Arc::downgrade(self),
        })
    }

    fn release(&self, amount: ResourceAmount, owner: Option<OwnerId>) {
        let mut state = self.state.lock().unwrap();
        state.available = state.available.saturating_add(amount);
        // Cap at total — defense against accidental double-release.
        if !state.available.fits_in(state.total) {
            state.available = state.total;
        }
        if let Some(o) = owner {
            let held = state.owner_mut(o).held;
            state.owner_mut(o).held = held.saturating_sub(amount);
            state.prune(o);
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
    owner: Option<OwnerId>,
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
            pool.release(self.amount, self.owner);
        }
    }
}

// ---------------------------------------------------------------------------
// Accounts, claims and slices
// ---------------------------------------------------------------------------

/// A test file's standing with the pool: its owner id, and its claim
/// once it has made one. Shared by the file's root lab, its sub-labs
/// and every per-test scope, so a VM anywhere in the file draws from
/// the same claim and is attributed to the same owner.
#[derive(Debug)]
pub struct Account {
    owner: Option<OwnerId>,
    claim: Mutex<AccountClaim>,
}

#[derive(Debug, Default)]
struct AccountClaim {
    /// Set by the first `provium:claim`, and never cleared — the
    /// design's one-shot-per-file rule holds even after release.
    taken: bool,
    budget: Option<ClaimBudget>,
}

#[derive(Debug)]
struct ClaimBudget {
    /// The pool reservation behind the claim. Released at file end.
    _reservation: Reservation,
    total: ResourceAmount,
    used: ResourceAmount,
}

/// A boot asked for more than the file's claim has left.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClaimExceeded {
    /// What the boot wanted.
    pub requested: ResourceAmount,
    /// The file's claim.
    pub claimed: ResourceAmount,
    /// How much of the claim other live VMs already use.
    pub in_use: ResourceAmount,
}

impl fmt::Display for ClaimExceeded {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "needs {} but the file's claim is {} with {} already in use; raise \
             provium:claim to the file's peak",
            self.requested, self.claimed, self.in_use
        )
    }
}

impl std::error::Error for ClaimExceeded {}

impl Account {
    /// An account against `pool`, or a pool-less one (REPL, ad-hoc
    /// runs) that only remembers whether a claim was made.
    pub fn new(pool: Option<&Arc<Pool>>) -> Arc<Self> {
        Arc::new(Self {
            owner: pool.map(|p| p.new_owner()),
            claim: Mutex::new(AccountClaim::default()),
        })
    }

    /// The owner every reservation of this file is attributed to.
    /// `None` when there is no pool.
    pub fn owner(&self) -> Option<OwnerId> {
        self.owner
    }

    /// Whether `provium:claim` has been called for this file.
    pub fn claim_taken(&self) -> bool {
        self.claim.lock().unwrap().taken
    }

    /// Record a claim with no reservation behind it (no pool is
    /// wired). Returns `false` if one was already taken.
    pub fn mark_claim_taken(&self) -> bool {
        let mut c = self.claim.lock().unwrap();
        if c.taken {
            return false;
        }
        c.taken = true;
        true
    }

    /// Install the file's claim. Returns `false`, dropping
    /// `reservation`, if one was already taken.
    pub fn set_claim(&self, reservation: Reservation, amount: ResourceAmount) -> bool {
        let mut c = self.claim.lock().unwrap();
        if c.taken {
            return false;
        }
        c.taken = true;
        c.budget = Some(ClaimBudget {
            _reservation: reservation,
            total: amount,
            used: ResourceAmount::default(),
        });
        true
    }

    /// The claim's size and how much of it is in use, if one is held.
    pub fn claim(&self) -> Option<(ResourceAmount, ResourceAmount)> {
        self.claim
            .lock()
            .unwrap()
            .budget
            .as_ref()
            .map(|b| (b.total, b.used))
    }

    /// Give the claim's reservation back to the pool. Returns its size
    /// so the caller can emit `claim_released`; `None` if none was
    /// held. Slices still out simply expire with their VMs.
    pub fn release_claim(&self) -> Option<ResourceAmount> {
        self.claim.lock().unwrap().budget.take().map(|b| b.total)
    }

    /// Take `amount` out of the claim.
    ///
    /// `Ok(None)` when the file has no claim — the caller reserves from
    /// the pool instead. `Err` when it has one that cannot cover the
    /// request: the claim is the file's declared peak, and a boot past
    /// it is a mistake to report, not a wait to join.
    pub fn slice(
        self: &Arc<Self>,
        amount: ResourceAmount,
    ) -> Result<Option<ClaimSlice>, ClaimExceeded> {
        let mut c = self.claim.lock().unwrap();
        let Some(budget) = c.budget.as_mut() else {
            return Ok(None);
        };
        let after = budget.used.saturating_add(amount);
        if !after.fits_in(budget.total) {
            return Err(ClaimExceeded {
                requested: amount,
                claimed: budget.total,
                in_use: budget.used,
            });
        }
        budget.used = after;
        Ok(Some(ClaimSlice {
            amount,
            account: Arc::downgrade(self),
        }))
    }

    fn return_slice(&self, amount: ResourceAmount) {
        if let Some(budget) = self.claim.lock().unwrap().budget.as_mut() {
            budget.used = budget.used.saturating_sub(amount);
        }
    }
}

/// Part of a file's claim, out on loan to one VM. Returns to the
/// claim on drop.
#[derive(Debug)]
pub struct ClaimSlice {
    amount: ResourceAmount,
    account: Weak<Account>,
}

impl Drop for ClaimSlice {
    fn drop(&mut self) {
        if let Some(account) = self.account.upgrade() {
            account.return_slice(self.amount);
        }
    }
}

/// What a VM boot holds for its lifetime: a slice of the file's claim
/// when it has one, otherwise a reservation of its own.
#[derive(Debug)]
pub enum Hold {
    /// Reserved from the pool directly.
    Pool(Reservation),
    /// Lent from the file's claim.
    Claim(ClaimSlice),
}

impl Hold {
    /// Bytes + cpus held.
    pub fn amount(&self) -> ResourceAmount {
        match self {
            Self::Pool(r) => r.amount,
            Self::Claim(s) => s.amount,
        }
    }
}

/// Why [`reserve`] could not produce a [`Hold`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ReserveError {
    /// The file has a claim and this boot does not fit in it.
    Claim(ClaimExceeded),
    /// The pool refused: too big for it, or a proven deadlock.
    Pool(AcquireError),
}

impl fmt::Display for ReserveError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Claim(e) => e.fmt(f),
            // A boot-time deadlock has one remedy, and it is the test
            // file's: a claim is reserved whole, up front, so a claimed
            // file never holds one VM while it waits for another.
            Self::Pool(e @ AcquireError::Deadlock(_)) => write!(
                f,
                "{e}; declare this file's peak with provium:claim so it is scheduled whole \
                 and no boot waits mid-file"
            ),
            Self::Pool(e) => e.fmt(f),
        }
    }
}

impl std::error::Error for ReserveError {}

/// Reserve `amount` for a boot: from the file's claim when it has one,
/// otherwise from the pool on the file's behalf. This is the one place
/// the two are chosen between, so every boot path — a solo `vm:boot`,
/// a lab's joint boot — makes the same choice.
pub fn reserve(
    pool: &Arc<Pool>,
    account: Option<&Arc<Account>>,
    amount: ResourceAmount,
) -> Result<Hold, ReserveError> {
    if let Some(account) = account {
        if let Some(slice) = account.slice(amount).map_err(ReserveError::Claim)? {
            return Ok(Hold::Claim(slice));
        }
        if let Some(owner) = account.owner() {
            return pool
                .acquire_for(owner, amount)
                .map(Hold::Pool)
                .map_err(ReserveError::Pool);
        }
    }
    let total = pool.total();
    pool.acquire(amount)
        .map(Hold::Pool)
        .ok_or(ReserveError::Pool(AcquireError::ExceedsTotal {
            requested: amount,
            total,
        }))
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

    // -- the deadlock verdict ------------------------------------------

    /// Park an attributed acquire on another thread and give it time
    /// to settle. Returns the handle; join it to get the result.
    fn park(
        pool: &Arc<Pool>,
        owner: OwnerId,
        amount: ResourceAmount,
    ) -> thread::JoinHandle<Result<Reservation, AcquireError>> {
        let pool = Arc::clone(pool);
        let h = thread::spawn(move || pool.acquire_for(owner, amount));
        thread::sleep(Duration::from_millis(40));
        h
    }

    /// The PEI-810 shape: every file holds one VM and wants a second.
    /// The request that closes the cycle fails; the others are served
    /// once something is released.
    #[test]
    fn a_cycle_of_holders_all_waiting_fails_the_request_that_closes_it() {
        let pool = Pool::new(budget(2048, 2));
        let a = pool.new_owner();
        let b = pool.new_owner();
        let a_first = pool.acquire_for(a, budget(1024, 1)).unwrap();
        let b_first = pool.acquire_for(b, budget(1024, 1)).unwrap();

        let a_second = park(&pool, a, budget(1024, 1));
        assert_eq!(pool.pending_count(), 1, "a parks: b is still running");

        let err = pool.acquire_for(b, budget(1024, 1)).unwrap_err();
        let AcquireError::Deadlock(d) = err else {
            panic!("expected a deadlock verdict, got {err:?}");
        };
        assert_eq!(d.holders, 2);
        assert_eq!(d.waiters, 2, "the failing request counts itself");
        assert_eq!(d.available, budget(0, 0));
        assert_eq!(pool.pending_count(), 1, "the loser left the waiting set");

        // b gives up its VM; a's second boot goes through.
        drop(b_first);
        let a_second = a_second.join().unwrap().unwrap();
        assert_eq!(pool.available(), budget(0, 0));
        drop(a_second);
        drop(a_first);
        assert_eq!(pool.available(), budget(2048, 2));
    }

    /// A holder that is still running will release what it holds, so a
    /// waiter behind it is queued, not deadlocked.
    #[test]
    fn a_holder_that_is_not_waiting_means_no_deadlock() {
        let pool = Pool::new(budget(2048, 2));
        let a = pool.new_owner();
        let b = pool.new_owner();
        let a_first = pool.acquire_for(a, budget(1024, 1)).unwrap();
        let _b_first = pool.acquire_for(b, budget(1024, 1)).unwrap();

        let b_second = park(&pool, b, budget(1024, 1));
        assert!(!b_second.is_finished(), "b should be parked, not failed");

        drop(a_first);
        assert!(b_second.join().unwrap().is_ok());
    }

    /// The pool cannot see who will release an anonymous reservation,
    /// so it never calls a deadlock while one is out.
    #[test]
    fn an_anonymous_holder_withholds_the_verdict() {
        let pool = Pool::new(budget(2048, 2));
        let anon = pool.acquire(budget(1024, 1)).unwrap();
        let b = pool.new_owner();
        let _b_first = pool.acquire_for(b, budget(1024, 1)).unwrap();

        let b_second = park(&pool, b, budget(1024, 1));
        assert!(!b_second.is_finished());

        drop(anon);
        assert!(b_second.join().unwrap().is_ok());
    }

    /// A waiter that holds nothing is not part of any cycle: it is
    /// simply queued behind a running holder.
    #[test]
    fn a_first_boot_queued_behind_a_running_file_is_not_a_deadlock() {
        let pool = Pool::new(budget(2048, 2));
        let a = pool.new_owner();
        let c = pool.new_owner();
        let a_all = pool.acquire_for(a, budget(2048, 2)).unwrap();

        let c_first = park(&pool, c, budget(1024, 1));
        assert!(!c_first.is_finished());

        drop(a_all);
        assert!(c_first.join().unwrap().is_ok());
    }

    /// A request larger than the pool is refused before anything is
    /// parked, attributed or not.
    #[test]
    fn an_attributed_request_over_the_total_is_refused_outright() {
        let pool = Pool::new(budget(1024, 2));
        let a = pool.new_owner();
        assert!(matches!(
            pool.acquire_for(a, budget(4096, 1)),
            Err(AcquireError::ExceedsTotal { .. })
        ));
        assert_eq!(pool.pending_count(), 0);
    }

    // -- claims ----------------------------------------------------------

    /// A claimed file's boots come out of the claim and leave the pool
    /// untouched; one past the claim fails rather than waits.
    #[test]
    fn a_boot_draws_from_the_claim_and_a_boot_past_it_fails_fast() {
        let pool = Pool::new(budget(4096, 4));
        let account = Account::new(Some(&pool));
        let claim = budget(2048, 2);
        let res = pool.acquire_for(account.owner().unwrap(), claim).unwrap();
        assert!(account.set_claim(res, claim));
        assert_eq!(pool.available(), budget(2048, 2));

        let one = reserve(&pool, Some(&account), budget(1024, 1)).unwrap();
        assert!(matches!(one, Hold::Claim(_)));
        assert_eq!(pool.available(), budget(2048, 2), "the pool is not charged again");
        assert_eq!(account.claim(), Some((claim, budget(1024, 1))));

        let err = reserve(&pool, Some(&account), budget(2048, 2)).unwrap_err();
        let ReserveError::Claim(e) = err else {
            panic!("expected the claim to refuse, got {err:?}");
        };
        assert_eq!(e.in_use, budget(1024, 1));

        drop(one);
        assert_eq!(account.claim(), Some((claim, budget(0, 0))));
        let two = reserve(&pool, Some(&account), budget(2048, 2)).unwrap();
        assert!(matches!(two, Hold::Claim(_)));
    }

    /// Without a claim the boot is a pool reservation in the file's
    /// name — which is what lets the pool see a deadlock.
    #[test]
    fn a_boot_without_a_claim_is_a_pool_reservation_for_the_owner() {
        let pool = Pool::new(budget(4096, 4));
        let account = Account::new(Some(&pool));
        let hold = reserve(&pool, Some(&account), budget(1024, 1)).unwrap();
        assert!(matches!(hold, Hold::Pool(_)));
        assert_eq!(pool.available(), budget(3072, 3));
        drop(hold);
        assert_eq!(pool.available(), budget(4096, 4));
    }

    /// The claim is one-shot per file, and stays taken after release.
    #[test]
    fn a_claim_is_one_shot_per_account() {
        let pool = Pool::new(budget(4096, 4));
        let account = Account::new(Some(&pool));
        let owner = account.owner().unwrap();
        let res = pool.acquire_for(owner, budget(1024, 1)).unwrap();
        assert!(account.set_claim(res, budget(1024, 1)));
        let res = pool.acquire_for(owner, budget(1024, 1)).unwrap();
        assert!(!account.set_claim(res, budget(1024, 1)));
        assert_eq!(pool.available(), budget(3072, 3), "the refused one is released");
        assert_eq!(account.release_claim(), Some(budget(1024, 1)));
        assert_eq!(pool.available(), budget(4096, 4));
        assert!(account.claim_taken());
        assert!(!account.mark_claim_taken());
    }

    // -- the wait sink ---------------------------------------------------

    /// A wait still in progress is visible: the watchdogs read the sink
    /// while the file is parked, and a wait that never ends must still
    /// count.
    #[test]
    fn a_wait_in_progress_is_visible_before_the_acquire_returns() {
        let pool = Pool::new(budget(1024, 2));
        let held = pool.acquire(budget(1024, 2)).unwrap();

        let sink = WaitSink::new();
        let waiter_pool = Arc::clone(&pool);
        let waiter_sink = Arc::clone(&sink);
        let waiter = thread::spawn(move || {
            let _guard = install_wait_sink(waiter_sink);
            waiter_pool.acquire(budget(1024, 2)).unwrap()
        });

        thread::sleep(Duration::from_millis(120));
        let live = sink.waited();
        assert!(
            live >= Duration::from_millis(80),
            "the wait should already be counted while it is in progress, got {live:?}"
        );
        drop(held);
        drop(waiter.join().unwrap());

        let settled = sink.waited();
        assert!(settled >= live, "settling never loses time: {settled:?} < {live:?}");
        assert!(settled < Duration::from_secs(5), "and it is the wait, not the test");
        thread::sleep(Duration::from_millis(20));
        assert_eq!(sink.waited(), settled, "nothing accrues once the acquire has returned");
    }

    /// An acquire that never parks costs the caller nothing, so a file
    /// that simply runs slowly still hits its deadline.
    #[test]
    fn an_unblocked_acquire_records_nothing() {
        let pool = Pool::new(budget(1024, 4));
        let sink = WaitSink::new();
        let _guard = install_wait_sink(Arc::clone(&sink));
        let r = pool.acquire(budget(512, 2)).unwrap();
        drop(r);
        assert_eq!(sink.waited(), Duration::ZERO);
    }

    /// A deadlock verdict ends the park like a grant does.
    #[test]
    fn a_refused_wait_is_settled_too() {
        let pool = Pool::new(budget(2048, 2));
        let a = pool.new_owner();
        let b = pool.new_owner();
        let _a_first = pool.acquire_for(a, budget(1024, 1)).unwrap();
        let _b_first = pool.acquire_for(b, budget(1024, 1)).unwrap();
        let _a_second = park(&pool, a, budget(1024, 1));

        let sink = WaitSink::new();
        let _guard = install_wait_sink(Arc::clone(&sink));
        assert!(pool.acquire_for(b, budget(1024, 1)).is_err());
        let after = sink.waited();
        thread::sleep(Duration::from_millis(20));
        assert_eq!(sink.waited(), after, "no park is left open");
    }

    /// The guard is scoped: a sink stops collecting when it drops, so
    /// one file's queueing is never charged to the next file that
    /// happens to reuse the thread.
    #[test]
    fn the_sink_is_removed_when_its_guard_drops() {
        let pool = Pool::new(budget(1024, 2));
        let sink = WaitSink::new();
        {
            let _guard = install_wait_sink(Arc::clone(&sink));
        }
        assert!(current_wait_sink().is_none());
        let held = pool.acquire(budget(1024, 2)).unwrap();
        let waiter_pool = Arc::clone(&pool);
        let waiter = thread::spawn(move || waiter_pool.acquire(budget(1024, 2)).unwrap());
        thread::sleep(Duration::from_millis(30));
        drop(held);
        drop(waiter.join().unwrap());
        assert_eq!(sink.waited(), Duration::ZERO);
    }
}
