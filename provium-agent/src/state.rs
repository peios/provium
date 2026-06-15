//! Process-wide agent state shared by every connection handler.
//!
//! The agent has a small amount of inter-connection state — the
//! open-file table is the v1 example — that must survive between op
//! invocations. A handle returned by [`crate::ops::file::open_file`] on
//! one connection is consumed by [`crate::ops::file::read`] on a later
//! connection, and the table sits here.
//!
//! All access goes through methods on [`AgentState`]. The handle
//! allocator is an [`AtomicU64`]; the file table is a [`Mutex`]. The
//! split lets allocation happen without contending on the table lock,
//! but every table mutation still serializes — see
//! `with_file_mut` for the common idiom.

use std::collections::HashMap;
use std::fs::File;
use std::process::Child;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::thread::JoinHandle;

use provium_protocol::handle::{FileHandle, ProcessHandle, WorkerHandle};

/// Shared agent state. Wrap in [`std::sync::Arc`] and clone into each
/// handler thread.
#[derive(Debug)]
pub struct AgentState {
    /// Source for fresh handles. Starts at 1 so a `FileHandle::default()`
    /// (== 0) is never confusable with an allocated handle.
    next_handle: AtomicU64,
    /// Open-file table.
    files: Mutex<HashMap<FileHandle, File>>,
    /// Async-process table — populated by `RunAsync`, drained by
    /// `Wait`, mutated by `Kill`. Each entry owns the [`Child`] plus
    /// the join handles for the stdout/stderr drain threads spawned
    /// at `RunAsync` time.
    processes: Mutex<HashMap<ProcessHandle, ProcessSlot>>,
    /// Worker (sub-agent) registry. Each worker owns its own
    /// file table + process table — Phase E isolated this from
    /// the parent agent's state so worker:open_file doesn't
    /// collide with the host VM's open-file namespace.
    workers: Mutex<HashMap<WorkerHandle, std::sync::Arc<AgentState>>>,
    /// Membership map: worker handle → set of process handles
    /// spawned via `WorkerRunAsync`. Used by `worker_kill` /
    /// `worker_join` to target only that worker's children even
    /// though the actual `ProcessSlot` lives in the parent's
    /// process table (see `worker_run_async` rationale comment).
    worker_processes: Mutex<HashMap<WorkerHandle, std::collections::HashSet<ProcessHandle>>>,
}

/// Per-process state held while the child is alive.
#[derive(Debug)]
pub struct ProcessSlot {
    /// `None` once `take_process` consumes it.
    pub child: Option<Child>,
    /// stdout drainer's accumulated bytes. The thread reads to
    /// EOF (process exit closes the pipe) and writes here.
    pub stdout_buf: std::sync::Arc<Mutex<Vec<u8>>>,
    /// stderr drainer.
    pub stderr_buf: std::sync::Arc<Mutex<Vec<u8>>>,
    /// Drain-thread handles. Joined during `Wait` so we get
    /// the full output without blocking the agent's accept loop.
    pub stdout_join: Option<JoinHandle<()>>,
    /// Drain-thread handle for stderr.
    pub stderr_join: Option<JoinHandle<()>>,
    /// Live stdin pipe — present until `ProcStdinClose` or the
    /// child exits. `None` after stdin is dropped.
    pub stdin: Option<std::process::ChildStdin>,
}

impl AgentState {
    /// Build an empty agent state.
    pub fn new() -> Self {
        Self {
            next_handle: AtomicU64::new(1),
            files: Mutex::new(HashMap::new()),
            processes: Mutex::new(HashMap::new()),
            workers: Mutex::new(HashMap::new()),
            worker_processes: Mutex::new(HashMap::new()),
        }
    }

    /// Record `process` as belonging to `worker`'s namespace.
    /// Called from `worker_run_async` immediately after a successful
    /// spawn so a later `worker_kill` / `worker_join` can find this
    /// child even though the `ProcessSlot` itself sits in the
    /// parent's process table.
    pub fn bind_process_to_worker(
        &self,
        worker: WorkerHandle,
        process: ProcessHandle,
    ) {
        self.worker_processes
            .lock()
            .unwrap()
            .entry(worker)
            .or_default()
            .insert(process);
    }

    /// Pop the set of process handles bound to `worker`. Used by
    /// `worker_kill` (which signals the live ones) and
    /// `worker_join` (which reaps + sums exit codes).
    pub fn take_worker_processes(
        &self,
        worker: WorkerHandle,
    ) -> std::collections::HashSet<ProcessHandle> {
        self.worker_processes
            .lock()
            .unwrap()
            .remove(&worker)
            .unwrap_or_default()
    }

    /// Read the current set of process handles for `worker`
    /// without removing the entry. Used by `worker_kill` so a
    /// subsequent `worker_join` still has the same set to reap.
    pub fn worker_process_handles(
        &self,
        worker: WorkerHandle,
    ) -> std::collections::HashSet<ProcessHandle> {
        self.worker_processes
            .lock()
            .unwrap()
            .get(&worker)
            .cloned()
            .unwrap_or_default()
    }

    /// Allocate a new worker handle. The worker's state has its
    /// own file + process tables (separate from `self`'s).
    pub fn insert_worker(self: &std::sync::Arc<Self>) -> WorkerHandle {
        let id = self.next_handle.fetch_add(1, Ordering::Relaxed);
        let handle = WorkerHandle::new(id);
        let worker_state = std::sync::Arc::new(AgentState::new());
        self.workers.lock().unwrap().insert(handle, worker_state);
        handle
    }

    /// Look up a worker's state. Used by [`with_worker_state`].
    pub fn worker_state(&self, handle: WorkerHandle) -> Option<std::sync::Arc<AgentState>> {
        self.workers.lock().unwrap().get(&handle).cloned()
    }

    /// Remove a worker's state from the registry.
    pub fn remove_worker(&self, handle: WorkerHandle) -> bool {
        self.workers.lock().unwrap().remove(&handle).is_some()
    }

    /// `true` if `handle` is a live worker.
    pub fn has_worker(&self, handle: WorkerHandle) -> bool {
        self.workers.lock().unwrap().contains_key(&handle)
    }

    /// Number of currently-tracked workers.
    /// Send `signal` to every process in this state's process
    /// table. Returns the count of processes signalled. Used by
    /// `worker_kill` to broadcast termination to a worker's
    /// children.
    pub fn signal_all(&self, signal: i32) -> u32 {
        let mut count = 0u32;
        for slot in self.processes.lock().unwrap().values_mut() {
            if let Some(child) = slot.child.as_mut() {
                let pid = child.id() as libc::pid_t;
                // SAFETY: pid valid for child lifetime; signal is
                // a POSIX integer.
                unsafe {
                    libc::kill(pid, signal);
                }
                count += 1;
            }
        }
        count
    }

    /// Send `signal` to each process named in `handles` whose
    /// slot is still live in this state's process table. Returns
    /// the count of signalled processes. Used by `worker_kill` to
    /// target only that worker's children when the slots actually
    /// live in the parent's process table (see worker_run_async
    /// rationale).
    pub fn signal_processes(
        &self,
        handles: &std::collections::HashSet<ProcessHandle>,
        signal: i32,
    ) -> u32 {
        let mut count = 0u32;
        let procs = self.processes.lock().unwrap();
        for h in handles {
            if let Some(slot) = procs.get(h) {
                if let Some(child) = slot.child.as_ref() {
                    let pid = child.id() as libc::pid_t;
                    // SAFETY: pid is valid for child lifetime.
                    unsafe {
                        libc::kill(pid, signal);
                    }
                    count += 1;
                }
            }
        }
        count
    }

    /// Wait for each process named in `handles` to exit, draining
    /// its stdout/stderr threads. Returns the worst exit status
    /// (or 0 when no children ran / all exited cleanly). Mirrors
    /// [`Self::reap_all_processes`] but scoped to a worker's
    /// child set.
    pub fn reap_processes(
        &self,
        handles: &std::collections::HashSet<ProcessHandle>,
    ) -> i32 {
        let drained: Vec<ProcessSlot> = {
            let mut procs = self.processes.lock().unwrap();
            handles.iter().filter_map(|h| procs.remove(h)).collect()
        };
        reap_slots(drained)
    }

    /// Wait for every process registered in this state to exit
    /// and report the worst (max) exit status seen. A signalled
    /// child contributes `128 + signo`, matching shell convention.
    /// Used by `worker_join` to give the host a single
    /// `exit_status` it can hand back to Lua.
    pub fn reap_all_processes(&self) -> i32 {
        let drained: Vec<ProcessSlot> = {
            let mut procs = self.processes.lock().unwrap();
            procs.drain().map(|(_, slot)| slot).collect()
        };
        reap_slots(drained)
    }

    pub fn open_worker_count(&self) -> usize {
        self.workers.lock().unwrap().len()
    }

    /// Insert a [`ProcessSlot`] under a freshly-allocated handle.
    pub fn insert_process(&self, slot: ProcessSlot) -> ProcessHandle {
        let id = self.next_handle.fetch_add(1, Ordering::Relaxed);
        let handle = ProcessHandle::new(id);
        self.processes.lock().unwrap().insert(handle, slot);
        handle
    }

    /// Remove a [`ProcessSlot`] from the table — typically called
    /// by `Wait` once the child has exited.
    pub fn take_process(&self, handle: ProcessHandle) -> Option<ProcessSlot> {
        self.processes.lock().unwrap().remove(&handle)
    }

    /// Run `f` against the slot for `handle` while holding the
    /// table lock. Used by `Kill` to inspect the child's pid.
    pub fn with_process_mut<R>(
        &self,
        handle: ProcessHandle,
        f: impl FnOnce(&mut ProcessSlot) -> R,
    ) -> Option<R> {
        let mut table = self.processes.lock().unwrap();
        let slot = table.get_mut(&handle)?;
        Some(f(slot))
    }

    /// Number of currently-tracked async processes.
    pub fn open_process_count(&self) -> usize {
        self.processes.lock().unwrap().len()
    }

    /// Insert `file` and return its freshly-allocated handle.
    pub fn insert_file(&self, file: File) -> FileHandle {
        let id = self.next_handle.fetch_add(1, Ordering::Relaxed);
        let handle = FileHandle::new(id);
        self.files.lock().unwrap().insert(handle, file);
        handle
    }

    /// Remove `handle` from the table. Returns the [`File`] so the
    /// caller can drop it (closing the underlying fd) or otherwise
    /// dispose of it.
    pub fn take_file(&self, handle: FileHandle) -> Option<File> {
        self.files.lock().unwrap().remove(&handle)
    }

    /// Run `f` against the file behind `handle`, holding the table
    /// lock for the duration. The lock is held to keep the borrow
    /// safe; the per-op work (a single read or write) is fast.
    pub fn with_file_mut<R>(
        &self,
        handle: FileHandle,
        f: impl FnOnce(&mut File) -> R,
    ) -> Option<R> {
        let mut table = self.files.lock().unwrap();
        let file = table.get_mut(&handle)?;
        Some(f(file))
    }

    /// Number of currently-open files. Test / introspection only.
    pub fn open_file_count(&self) -> usize {
        self.files.lock().unwrap().len()
    }
}

impl Default for AgentState {
    fn default() -> Self {
        Self::new()
    }
}

/// Drain a list of [`ProcessSlot`]s, waiting for each child to
/// exit and joining its stdout/stderr drain threads. Returns the
/// worst (max) exit status, where a signalled child contributes
/// `128 + signo` (shell convention). Used by both
/// [`AgentState::reap_all_processes`] and
/// [`AgentState::reap_processes`] so the per-worker and
/// agent-wide reap paths share their semantics.
fn reap_slots(slots: Vec<ProcessSlot>) -> i32 {
    let mut worst: i32 = 0;
    for mut slot in slots {
        if let Some(mut child) = slot.child.take() {
            match child.wait() {
                Ok(status) => {
                    let s = status.code().unwrap_or_else(|| {
                        use std::os::unix::process::ExitStatusExt;
                        128 + status.signal().unwrap_or(0)
                    });
                    if s > worst {
                        worst = s;
                    }
                }
                Err(_) => {
                    if 1 > worst {
                        worst = 1;
                    }
                }
            }
        }
        if let Some(j) = slot.stdout_join.take() {
            let _ = j.join();
        }
        if let Some(j) = slot.stderr_join.take() {
            let _ = j.join();
        }
    }
    worst
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn insert_and_take_round_trips() {
        let state = AgentState::new();
        let mut tmpfile = tempfile::NamedTempFile::new().unwrap();
        tmpfile.write_all(b"hi").unwrap();

        let path = tmpfile.path().to_path_buf();
        let handle = state.insert_file(File::open(&path).unwrap());
        assert_eq!(state.open_file_count(), 1);

        let taken = state.take_file(handle);
        assert!(taken.is_some());
        assert_eq!(state.open_file_count(), 0);

        // Taking an unknown handle returns None.
        assert!(state.take_file(handle).is_none());
    }

    #[test]
    fn handles_are_monotonic() {
        let state = AgentState::new();
        let f1 = tempfile::NamedTempFile::new().unwrap();
        let f2 = tempfile::NamedTempFile::new().unwrap();

        let h1 = state.insert_file(File::open(f1.path()).unwrap());
        let h2 = state.insert_file(File::open(f2.path()).unwrap());
        assert!(h2.get() > h1.get(), "handles should increase");
    }

    #[test]
    fn with_file_mut_reads_via_borrow() {
        let state = AgentState::new();
        let mut tmpfile = tempfile::NamedTempFile::new().unwrap();
        tmpfile.write_all(b"contents").unwrap();
        let handle = state.insert_file(File::open(tmpfile.path()).unwrap());

        let len = state
            .with_file_mut(handle, |f| f.metadata().unwrap().len())
            .unwrap();
        assert_eq!(len, 8);
    }
}
