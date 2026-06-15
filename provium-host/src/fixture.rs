//! Fixture cache — lazy, file-locked, content-addressable.
//!
//! Per `DESIGN.md` § Fixtures: `*.fixture.lua` files build a VM,
//! return a [`Snapshot`], and the framework persists the snapshot
//! into a cache keyed by a hash of the builder + provium version.
//! Subsequent calls to `provium.vm_fixture("path")` resume from
//! cache instead of rebuilding.
//!
//! ## Slice 8 minimum
//!
//! * Cache key = SHA-256(file content + protocol version).
//!   Transitive helper hashing + kernel/agent identifier are
//!   listed in the design and slated for slice 8.5.
//! * File-locked build: per-fixture lockfile via `flock(2)`. A
//!   second file asking for the same fixture parks until the first
//!   one finishes, then re-checks the cache (will hit).
//! * LRU eviction at startup. Entries sorted by access time;
//!   oldest evicted until total bytes ≤ cap.
//! * Cache directory: `$PROVIUM_FIXTURE_CACHE` env var, or
//!   `$XDG_CACHE_HOME/provium/fixtures/`, or
//!   `$HOME/.cache/provium/fixtures/`.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use sha2::{Digest, Sha256};
use thiserror::Error;

/// Default LRU cap. Matches `DESIGN.md` § Fixtures / Eviction policy.
pub const DEFAULT_CACHE_MAX_BYTES: u64 = 20 * 1024 * 1024 * 1024;

/// Failures specific to fixture-cache management.
#[derive(Debug, Error)]
pub enum FixtureError {
    /// Underlying I/O failure (cache directory create, file copy, …).
    #[error("fixture cache I/O: {0}")]
    Io(#[from] io::Error),

    /// `*.fixture.lua` not found at the requested path.
    #[error("fixture `{0}` not found")]
    NotFound(PathBuf),

    /// Build failed — the .fixture.lua's chunk raised, or returned
    /// a value that wasn't a Snapshot.
    #[error("fixture `{path}` build failed: {detail}")]
    BuildFailed {
        /// Fixture path.
        path: PathBuf,
        /// Free-form description.
        detail: String,
    },
}

// ---------------------------------------------------------------------------
// Cache directory resolution
// ---------------------------------------------------------------------------

/// Resolve the active fixture-cache directory. Honour
/// `$PROVIUM_FIXTURE_CACHE` first, then XDG, then `~/.cache/`.
pub fn default_cache_dir() -> PathBuf {
    if let Some(p) = std::env::var_os("PROVIUM_FIXTURE_CACHE") {
        return PathBuf::from(p);
    }
    if let Some(p) = std::env::var_os("XDG_CACHE_HOME") {
        return PathBuf::from(p).join("provium").join("fixtures");
    }
    if let Some(p) = std::env::var_os("HOME") {
        return PathBuf::from(p)
            .join(".cache")
            .join("provium")
            .join("fixtures");
    }
    PathBuf::from("/tmp/provium-fixtures")
}

// ---------------------------------------------------------------------------
// Cache key
// ---------------------------------------------------------------------------

/// 64-character lowercase hex SHA-256 hash that uniquely identifies
/// a fixture-cache entry.
#[derive(Clone, Debug, Eq, PartialEq, Hash)]
pub struct CacheKey(pub String);

impl std::fmt::Display for CacheKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl CacheKey {
    /// Hex string view without ownership transfer.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Pick the first profile (by sorted name) and return its kernel +
/// initrd paths for cache-key folding. Mirrors the
/// per-binary helper used by `provium fixture build`. Used by the
/// REPL `--fixture` resume path so its key matches the runner's
/// (otherwise builds would always look stale to `--fixture`).
pub fn canonical_profile_paths(
    config: &crate::profile::Config,
) -> (Option<std::path::PathBuf>, Option<std::path::PathBuf>) {
    let mut names: Vec<&String> = config.profiles.keys().collect();
    names.sort();
    let Some(first) = names.first() else {
        return (None, None);
    };
    let Some(p) = config.profiles.get(*first) else {
        return (None, None);
    };
    (Some(p.kernel.clone()), Some(p.initrd.clone()))
}

/// Compute the cache key for a fixture file.
///
/// The key includes the [`provium_protocol::PROTOCOL_VERSION`] so
/// rebuilds happen automatically when the wire shape changes —
/// matching the design's "stale snapshot is a cache miss" guarantee.
pub fn compute_key(fixture_source: &[u8]) -> CacheKey {
    compute_key_with_deps(fixture_source, &[])
}

/// As [`compute_key`] but folds the keys of transitively-referenced
/// fixtures into the digest. Per `DESIGN.md` § Fixtures /
/// Dependency tracking — rebuilding a parent invalidates every
/// derivative.
pub fn compute_key_with_deps(
    fixture_source: &[u8],
    dep_keys: &[CacheKey],
) -> CacheKey {
    compute_key_with_deps_and_kernel(fixture_source, dep_keys, None, None)
}

/// Full-form cache key that also folds in the kernel + initrd
/// identifier (path + mtime + size). Per `DESIGN.md` § Fixtures /
/// Lazy build — "cache key = hash of (builder file +
/// transitively-required helpers + provium version + kernel/agent
/// identifier)". Either path argument may be `None` for callers
/// that don't have a profile context (legacy single-arg form).
///
/// **Agent binary identifier:** the agent is baked into the
/// initrd at build time, so the initrd's mtime+size is the
/// agent's identifier too. If a workflow ever updates the agent
/// without rebuilding the initrd (e.g. an out-of-band patch
/// that preserves mtime), the cache will not invalidate.
/// Mitigated by `provium_protocol::PROTOCOL_VERSION` being
/// folded in — any wire-shape change forces a rebuild.
pub fn compute_key_with_deps_and_kernel(
    fixture_source: &[u8],
    dep_keys: &[CacheKey],
    kernel: Option<&Path>,
    initrd: Option<&Path>,
) -> CacheKey {
    let kernels: Vec<&Path> = kernel.into_iter().collect();
    let initrds: Vec<&Path> = initrd.into_iter().collect();
    compute_key_with_deps_and_kernels(
        fixture_source, dep_keys, &kernels, &initrds,
    )
}

/// Multi-profile variant of [`compute_key_with_deps_and_kernel`].
/// Folds EVERY profile's kernel + initrd identifier into the
/// digest so a kernel swap on any profile invalidates the cache.
/// R9 sched-m4 fix: the prior single-profile helper picked the
/// first profile by sorted name and silently missed kernel
/// changes on later profiles. Over-invalidation is the safe
/// failure mode here; under-invalidation (the prior behavior)
/// could resume guests against a stale kernel.
pub fn compute_key_with_deps_and_kernels(
    fixture_source: &[u8],
    dep_keys: &[CacheKey],
    kernels: &[&Path],
    initrds: &[&Path],
) -> CacheKey {
    compute_key_with_deps_kernels_and_externals(
        fixture_source,
        dep_keys,
        kernels,
        initrds,
        &[],
    )
}

/// Full-form cache key that also folds external host-file
/// dependencies (declared via `vm:push_file` and
/// `lab:depends_on_file`) into the digest. Each external is folded
/// as path + mtime + size — the same shape used for kernel/initrd
/// identifiers. Editing the external file (which bumps mtime and
/// usually size) invalidates the cache.
pub fn compute_key_with_deps_kernels_and_externals(
    fixture_source: &[u8],
    dep_keys: &[CacheKey],
    kernels: &[&Path],
    initrds: &[&Path],
    externals: &[&Path],
) -> CacheKey {
    let mut hasher = Sha256::new();
    hasher.update(fixture_source);
    hasher.update(b"|protocol=");
    hasher.update(provium_protocol::PROTOCOL_VERSION.to_le_bytes());
    for dep in dep_keys {
        hasher.update(b"|dep=");
        hasher.update(dep.as_str().as_bytes());
    }
    let fold = |hasher: &mut Sha256, label: &str, path: &Path| {
        hasher.update(b"|");
        hasher.update(label.as_bytes());
        hasher.update(b"=");
        hasher.update(path.to_string_lossy().as_bytes());
        if let Ok(m) = path.metadata() {
            hasher.update(b"@");
            hasher.update(m.len().to_le_bytes());
            if let Ok(modified) = m.modified() {
                if let Ok(d) = modified.duration_since(std::time::UNIX_EPOCH) {
                    hasher.update(b":");
                    hasher.update(d.as_nanos().to_le_bytes());
                }
            }
        }
    };
    for path in kernels {
        fold(&mut hasher, "kernel", path);
    }
    for path in initrds {
        fold(&mut hasher, "initrd", path);
    }
    for path in externals {
        fold(&mut hasher, "external", path);
    }
    let digest = hasher.finalize();
    CacheKey(format!("{:x}", digest))
}

/// Static scan of a fixture's source for `provium.vm_fixture("…")`
/// and `provium.lab_fixture("…")` references. Returns the names
/// (test-root-relative, no `.fixture.lua` suffix) in source order
/// — i.e. the order each call appears in the file, with both
/// markers interleaved by position. Duplicates are preserved so a
/// fixture that calls the same dep twice still hashes
/// consistently.
///
/// Source-level scanning is the cheapest accurate option: a true
/// static analysis would need a Lua parser, and dynamic tracking
/// has the chicken-and-egg problem (the key drives cache lookup
/// before the fixture runs).
pub fn scan_fixture_deps(source: &str) -> Vec<String> {
    let mut hits: Vec<(usize, String)> = Vec::new();
    for marker in ["vm_fixture", "lab_fixture"] {
        scan_quoted_arg_with_pos(source, marker, &mut hits);
    }
    hits.sort_by_key(|(pos, _)| *pos);
    hits.into_iter().map(|(_, name)| name).collect()
}

/// Static scan of a fixture's source for `require("…")` calls.
/// Returns module names in declaration order. Used to fold helper
/// modules into the cache key per `DESIGN.md` § Fixtures /
/// transitively-required helpers.
pub fn scan_require_deps(source: &str) -> Vec<String> {
    let mut deps = Vec::new();
    scan_quoted_arg(source, "require", &mut deps);
    deps
}

fn scan_quoted_arg(source: &str, marker: &str, out: &mut Vec<String>) {
    let mut hits = Vec::new();
    scan_quoted_arg_with_pos(source, marker, &mut hits);
    out.extend(hits.into_iter().map(|(_, s)| s));
}

/// Like [`scan_quoted_arg`] but also records the absolute byte
/// offset of each marker hit so callers can stable-sort across
/// multiple markers and recover true source order.
fn scan_quoted_arg_with_pos(
    source: &str,
    marker: &str,
    out: &mut Vec<(usize, String)>,
) {
    let mut idx = 0;
    while let Some(found) = source[idx..].find(marker) {
        let abs = idx + found;
        // Identifier-boundary check: char before must not be alnum/_
        if abs > 0 {
            let prev = source.as_bytes()[abs - 1];
            if prev.is_ascii_alphanumeric() || prev == b'_' {
                idx = abs + marker.len();
                continue;
            }
        }
        let after = &source[abs + marker.len()..];
        let after_trim = after.trim_start();
        let Some(after_open) = after_trim.strip_prefix('(') else {
            idx = abs + marker.len();
            continue;
        };
        let after_open = after_open.trim_start();
        let (quote, body) = if let Some(rest) = after_open.strip_prefix('"') {
            ('"', rest)
        } else if let Some(rest) = after_open.strip_prefix('\'') {
            ('\'', rest)
        } else {
            idx = abs + marker.len();
            continue;
        };
        if let Some(end) = body.find(quote) {
            out.push((abs, body[..end].to_string()));
        }
        idx = abs + marker.len();
    }
}

/// Static scan of a fixture's source for external host-file deps:
/// `vm:push_file("host", "guest", …)` and
/// `lab:depends_on_file("host")` / `provium:depends_on_file("host")`
/// calls. Returns the literal host-path strings in source order.
///
/// Auto-tracking is on by default; a `vm:push_file` call whose
/// argument list contains a literal `auto_dep = false` (or
/// `auto_dep=false`, whitespace-tolerant) is skipped. Non-literal
/// host paths (variables, concatenation) are not detected — those
/// callers must use `lab:depends_on_file("…")` with a string
/// literal to opt back in.
///
/// Resolution to absolute paths is the caller's job — paths can be
/// relative and only the source-containing-file knows the right
/// base directory.
pub fn scan_external_file_deps(source: &str) -> Vec<String> {
    let mut out: Vec<(usize, String)> = Vec::new();
    scan_push_file_calls(source, &mut out);
    let mut depends_hits: Vec<(usize, String)> = Vec::new();
    scan_quoted_arg_with_pos(source, "depends_on_file", &mut depends_hits);
    out.append(&mut depends_hits);
    out.sort_by_key(|(pos, _)| *pos);
    out.into_iter().map(|(_, s)| s).collect()
}

/// Find `:push_file("…", …)` call sites. Filters out calls whose
/// argument list contains a literal `auto_dep = false`.
fn scan_push_file_calls(source: &str, out: &mut Vec<(usize, String)>) {
    let marker = "push_file";
    let mut idx = 0;
    while let Some(found) = source[idx..].find(marker) {
        let abs = idx + found;
        if abs > 0 {
            let prev = source.as_bytes()[abs - 1];
            if prev.is_ascii_alphanumeric() || prev == b'_' {
                idx = abs + marker.len();
                continue;
            }
        }
        let after = &source[abs + marker.len()..];
        let after_trim = after.trim_start();
        let Some(after_open) = after_trim.strip_prefix('(') else {
            idx = abs + marker.len();
            continue;
        };
        let call_body_start = after_open.as_ptr() as usize - source.as_ptr() as usize;
        let close = match find_matching_close_paren(source, call_body_start) {
            Some(p) => p,
            None => {
                idx = abs + marker.len();
                continue;
            }
        };
        let call_args = &source[call_body_start..close];
        if has_auto_dep_false(call_args) {
            idx = close;
            continue;
        }
        let after_open_trim = after_open.trim_start();
        let (quote, body) = if let Some(rest) = after_open_trim.strip_prefix('"') {
            ('"', rest)
        } else if let Some(rest) = after_open_trim.strip_prefix('\'') {
            ('\'', rest)
        } else {
            idx = abs + marker.len();
            continue;
        };
        if let Some(end) = body.find(quote) {
            out.push((abs, body[..end].to_string()));
        }
        idx = close;
    }
}

/// Return the byte offset of the `)` that closes the call whose
/// opening `(` content begins at `start`. Skips Lua string literals
/// (`"…"` and `'…'`) so a `)` inside a string doesn't trip the
/// match. Does not handle long-bracket strings (`[[…]]`) — fixtures
/// don't use them for push_file args.
fn find_matching_close_paren(source: &str, start: usize) -> Option<usize> {
    let bytes = source.as_bytes();
    let mut depth: i32 = 1;
    let mut i = start;
    while i < bytes.len() {
        let c = bytes[i];
        if c == b'"' || c == b'\'' {
            i += 1;
            while i < bytes.len() {
                if bytes[i] == b'\\' {
                    i += 2;
                    continue;
                }
                if bytes[i] == c {
                    i += 1;
                    break;
                }
                i += 1;
            }
            continue;
        }
        if c == b'(' {
            depth += 1;
        } else if c == b')' {
            depth -= 1;
            if depth == 0 {
                return Some(i);
            }
        }
        i += 1;
    }
    None
}

/// True if `args` contains a literal `auto_dep = false` (whitespace
/// around `=` is tolerated). Anything more complex (variables,
/// conditional expressions) conservatively does NOT match — the
/// default is to track, so a missed opt-out errs on the safe side.
fn has_auto_dep_false(args: &str) -> bool {
    let Some(idx) = args.find("auto_dep") else {
        return false;
    };
    let rest = args[idx + "auto_dep".len()..].trim_start();
    let Some(rest) = rest.strip_prefix('=') else {
        return false;
    };
    let rest = rest.trim_start();
    rest.starts_with("false")
        && rest[5..]
            .chars()
            .next()
            .map(|c| !(c.is_ascii_alphanumeric() || c == '_'))
            .unwrap_or(true)
}

// ---------------------------------------------------------------------------
// Per-fixture paths inside the cache directory
// ---------------------------------------------------------------------------

/// On-disk paths for one cache entry.
#[derive(Clone, Debug)]
pub struct CacheEntryPaths {
    /// Snapshot bytes. The VMM's `restore` reads this.
    pub snapshot: PathBuf,
    /// Lockfile used by the build phase. Holds an `flock` exclusive
    /// while a builder is producing the snapshot.
    pub lock: PathBuf,
}

impl CacheEntryPaths {
    /// Construct from cache root + key.
    pub fn for_key(cache_dir: &Path, key: &CacheKey) -> Self {
        let stem = cache_dir.join(&key.0);
        Self {
            snapshot: stem.with_extension("snap"),
            lock: stem.with_extension("lock"),
        }
    }
}

// ---------------------------------------------------------------------------
// File-locked build
// ---------------------------------------------------------------------------

/// RAII guard for an exclusive lock on a fixture's lockfile.
///
/// Construct via [`acquire_build_lock`]; drop releases the lock.
pub struct BuildLock {
    file: File,
}

impl std::fmt::Debug for BuildLock {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BuildLock").finish()
    }
}

impl Drop for BuildLock {
    fn drop(&mut self) {
        // Release the advisory lock. flock(LOCK_UN) is best-effort
        // — failures here only matter if the file descriptor is
        // already broken.
        // SAFETY: the file is owned and the fd is valid.
        unsafe {
            libc::flock(self.file.as_raw_fd(), libc::LOCK_UN);
        }
    }
}

/// Acquire an exclusive `flock` on `path`. Blocks if another
/// process holds the lock. Creates the lockfile if it doesn't
/// exist.
pub fn acquire_build_lock(path: &Path) -> Result<BuildLock, FixtureError> {
    Ok(acquire_build_lock_observed(path)?.0)
}

/// As [`acquire_build_lock`] but reports whether the call had to
/// block on contention. The bool is `true` when another process
/// held the lock and we waited. Used by the dispatcher to emit
/// `fixture_build_waiting` only in the contended case per
/// `DESIGN.md` § Failure mode catalogue / Fixture rebuild storm.
pub fn acquire_build_lock_observed(
    path: &Path,
) -> Result<(BuildLock, bool), FixtureError> {
    acquire_build_lock_observed_notify(path, |_| {})
}

/// Like [`acquire_build_lock_observed`] but invokes `on_wait`
/// AFTER detecting contention but BEFORE the blocking flock.
/// The callback receives the holder identity (pid + cmdline)
/// from the lockfile, suitable for emitting a
/// `fixture_build_waiting` event in real time so a TUI / CI
/// dashboard sees the wait as it begins instead of when it ends.
pub fn acquire_build_lock_observed_notify(
    path: &Path,
    on_wait: impl FnOnce(Option<String>),
) -> Result<(BuildLock, bool), FixtureError> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(path)?;
    // SAFETY: the file's fd is valid for the duration of the call.
    // Try non-blocking first to detect contention; fall back to a
    // blocking acquire if it would have to wait.
    let nb = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    let blocked = if nb == 0 {
        false
    } else {
        // Notify the caller about the imminent wait + the
        // holder, so consumers see `fixture_build_waiting`
        // events in real time rather than after the wait
        // resolves.
        on_wait(read_lock_holder(path));
        let r = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) };
        if r != 0 {
            return Err(FixtureError::Io(io::Error::last_os_error()));
        }
        true
    };
    // Now-locked: stamp the lockfile with our holder identity so
    // a concurrent peer that calls [`read_lock_holder`] sees who
    // is mid-build. Best-effort — failure to write doesn't block
    // the build path.
    let _ = file.set_len(0);
    let _ = file.write_all(holder_info().as_bytes());
    let _ = file.flush();
    Ok((BuildLock { file }, blocked))
}

/// Bump `path`'s access time to "now" so the LRU eviction
/// comparator sorts it as freshly-used. Without this, `relatime`
/// (the kernel default) only updates atime once per day, so
/// long-running CI runs see every cache entry's atime collapse to
/// the same value and eviction degenerates to filesystem order
/// instead of true LRU. Best-effort: failure is silently ignored
/// — the fixture itself is still served from cache.
pub fn bump_atime(path: &Path) {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;
    let Ok(c) = CString::new(path.as_os_str().as_bytes()) else {
        return;
    };
    // [atime, mtime] — UTIME_NOW for atime, UTIME_OMIT for mtime
    // so we don't churn modification timestamps that callers may
    // care about (e.g. cache-key staleness checks).
    let times = [
        libc::timespec {
            tv_sec: 0,
            tv_nsec: libc::UTIME_NOW,
        },
        libc::timespec {
            tv_sec: 0,
            tv_nsec: libc::UTIME_OMIT,
        },
    ];
    // SAFETY: c is a valid C string for the path's lifetime; times
    // is a 2-element struct timespec array as required.
    // Try AT_SYMLINK_NOFOLLOW first (correct for the symlink case)
    // and fall back to flag 0 if the kernel returns EINVAL — older
    // kernels (< ~5.2) don't support the flag on regular files,
    // and we'd otherwise silently fail to update the atime,
    // breaking LRU.
    unsafe {
        let r = libc::utimensat(
            libc::AT_FDCWD,
            c.as_ptr(),
            times.as_ptr(),
            libc::AT_SYMLINK_NOFOLLOW,
        );
        if r != 0 {
            let errno = *libc::__errno_location();
            if errno == libc::EINVAL {
                let _ = libc::utimensat(libc::AT_FDCWD, c.as_ptr(), times.as_ptr(), 0);
            }
        }
    }
}

/// Read the holder identity (pid + cmdline) stamped into a fixture
/// lockfile by [`acquire_build_lock_observed`]. Returns `None` if
/// the lockfile is empty, missing, or unreadable. Used by
/// `provium fixture` diagnostics to answer "what's holding this?".
pub fn read_lock_holder(path: &Path) -> Option<String> {
    let s = fs::read_to_string(path).ok()?;
    let trimmed = s.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

/// Build a one-line holder identifier of the form
/// `pid=<n> host=<h> started=<rfc3339> argv=<…>`. Used by the
/// build-lock writer so a concurrent observer can attribute a
/// contended lock back to a process.
fn holder_info() -> String {
    let pid = std::process::id();
    let host = hostname_or("unknown");
    let started = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let argv = std::env::args().collect::<Vec<_>>().join(" ");
    format!("pid={pid} host={host} started={started} argv={argv}\n")
}

fn hostname_or(default: &str) -> String {
    std::fs::read_to_string("/etc/hostname")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| default.to_string())
}

// ---------------------------------------------------------------------------
// LRU eviction
// ---------------------------------------------------------------------------

/// Evict cache entries until total size ≤ `max_bytes`.
///
/// Sorts entries by access time (oldest first) and removes
/// matching `<key>.snap` + `<key>.lock` pairs until the budget is
/// satisfied. Failures on individual files are logged to stderr but
/// do not abort the run — eviction is best-effort.
pub fn evict_to(cache_dir: &Path, max_bytes: u64) -> io::Result<EvictionReport> {
    if !cache_dir.exists() {
        return Ok(EvictionReport::default());
    }

    let mut entries: Vec<EvictionEntry> = Vec::new();
    let mut total: u64 = 0;
    for dirent in fs::read_dir(cache_dir)? {
        let dirent = dirent?;
        let path = dirent.path();
        let metadata = match dirent.metadata() {
            Ok(m) => m,
            Err(_) => continue,
        };
        let is_lab = metadata.is_dir()
            && path
                .file_name()
                .and_then(|s| s.to_str())
                .map(|s| s.ends_with(".lab"))
                .unwrap_or(false);
        let is_snap = path.extension().and_then(|s| s.to_str()) == Some("snap");
        if !is_lab && !is_snap {
            continue;
        }
        let size = if is_lab {
            // Sum file sizes inside the lab dir.
            let mut s = 0u64;
            if let Ok(rd) = fs::read_dir(&path) {
                for inner in rd.flatten() {
                    if let Ok(im) = inner.metadata() {
                        s = s.saturating_add(im.len());
                    }
                }
            }
            s
        } else {
            metadata.len()
        };
        total += size;
        let atime = metadata.accessed().unwrap_or(std::time::UNIX_EPOCH);
        entries.push(EvictionEntry { path, size, atime });
    }

    if total <= max_bytes {
        return Ok(EvictionReport {
            evicted_count: 0,
            evicted_bytes: 0,
            kept_bytes: total,
        });
    }

    // Oldest first.
    entries.sort_by_key(|e| e.atime);

    let mut evicted_count = 0u64;
    let mut evicted_bytes = 0u64;
    let mut kept_bytes = total;
    for entry in entries {
        if kept_bytes <= max_bytes {
            break;
        }
        let is_lab_dir = entry.path.is_dir();
        let r = if is_lab_dir {
            fs::remove_dir_all(&entry.path)
        } else {
            fs::remove_file(&entry.path)
        };
        if let Err(e) = r {
            eprintln!(
                "provium: evicting `{}` failed: {e}",
                entry.path.display()
            );
            continue;
        }
        // Lockfile cleanup — for `<key>.snap` it's `<key>.lock`,
        // for `<key>.lab/` it's `<key>.lab.lock`.
        let lock = if is_lab_dir {
            let mut s = entry.path.clone().into_os_string();
            s.push(".lock");
            std::path::PathBuf::from(s)
        } else {
            entry.path.with_extension("lock")
        };
        let _ = fs::remove_file(&lock);
        evicted_count += 1;
        evicted_bytes += entry.size;
        kept_bytes = kept_bytes.saturating_sub(entry.size);
    }

    Ok(EvictionReport {
        evicted_count,
        evicted_bytes,
        kept_bytes,
    })
}

/// Summary of one eviction pass.
#[derive(Clone, Debug, Default)]
pub struct EvictionReport {
    /// How many cache entries were removed.
    pub evicted_count: u64,
    /// Bytes freed by eviction.
    pub evicted_bytes: u64,
    /// Bytes still on disk after eviction.
    pub kept_bytes: u64,
}

struct EvictionEntry {
    path: PathBuf,
    size: u64,
    atime: std::time::SystemTime,
}

// ---------------------------------------------------------------------------
// Read fixture source for cache-key derivation
// ---------------------------------------------------------------------------

/// Unique scratch path for a snapshot decompression.
///
/// The path is keyed only by pid + a process-local counter, never by
/// the cache key — two parallel restores of the same fixture must
/// not collide on this file. Placing it outside the cache dir also
/// keeps eviction passes away from in-flight decompresses.
pub fn unique_restore_scratch() -> PathBuf {
    static N: AtomicU64 = AtomicU64::new(0);
    let n = N.fetch_add(1, Ordering::Relaxed);
    let pid = std::process::id();
    let dir = std::env::temp_dir().join("provium-restore-staging");
    let _ = fs::create_dir_all(&dir);
    dir.join(format!("snap-{pid}-{n}.restore"))
}

/// Read the fixture file's bytes for hashing. Materialises the
/// whole file in memory; fixtures are expected to be small.
pub fn read_fixture_source(path: &Path) -> Result<Vec<u8>, FixtureError> {
    if !path.is_file() {
        return Err(FixtureError::NotFound(path.into()));
    }
    let mut buf = Vec::new();
    File::open(path)?.read_to_end(&mut buf)?;
    Ok(buf)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn cache_key_is_deterministic_per_content() {
        let k1 = compute_key(b"some lua source");
        let k2 = compute_key(b"some lua source");
        assert_eq!(k1, k2);
        assert_eq!(k1.0.len(), 64);
    }

    #[test]
    fn cache_key_changes_with_content() {
        assert_ne!(compute_key(b"a"), compute_key(b"b"));
    }

    #[test]
    fn scan_external_finds_push_file() {
        let src = r#"
            local vm = provium:vm("a", "p"):boot()
            vm:push_file("../bin/foo", "/usr/bin/foo")
            return vm:snapshot()
        "#;
        let hits = scan_external_file_deps(src);
        assert_eq!(hits, vec!["../bin/foo".to_string()]);
    }

    #[test]
    fn scan_external_finds_depends_on_file() {
        let src = r#"
            provium:depends_on_file("templates/foo.conf")
            local vm = provium:vm("a", "p"):boot()
            return vm:snapshot()
        "#;
        let hits = scan_external_file_deps(src);
        assert_eq!(hits, vec!["templates/foo.conf".to_string()]);
    }

    #[test]
    fn scan_external_skips_push_file_with_auto_dep_false() {
        let src = r#"
            vm:push_file("../big.tar", "/data.tar", {auto_dep = false})
            vm:push_file("../bin/foo", "/usr/bin/foo")
        "#;
        let hits = scan_external_file_deps(src);
        assert_eq!(hits, vec!["../bin/foo".to_string()]);
    }

    #[test]
    fn scan_external_handles_whitespace_in_opt_out() {
        let src = r#"vm:push_file("a", "b", { auto_dep   =   false })"#;
        assert!(scan_external_file_deps(src).is_empty());
    }

    #[test]
    fn scan_external_preserves_source_order() {
        let src = r#"
            vm:push_file("a", "/a")
            lab:depends_on_file("b")
            vm:push_file("c", "/c")
        "#;
        let hits = scan_external_file_deps(src);
        assert_eq!(hits, vec!["a".to_string(), "b".to_string(), "c".to_string()]);
    }

    #[test]
    fn scan_external_ignores_substring_matches() {
        // `my_push_file` is not push_file, `set_depends_on_file` is not depends_on_file.
        let src = r#"
            my_push_file("a", "b")
            set_depends_on_file("c")
        "#;
        assert!(scan_external_file_deps(src).is_empty());
    }

    #[test]
    fn cache_key_folds_external_path() {
        // Two files: edit one, key changes.
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a.bin");
        fs::write(&a, b"version-one").unwrap();
        let externals: Vec<&Path> = vec![a.as_path()];
        let k1 = compute_key_with_deps_kernels_and_externals(
            b"src", &[], &[], &[], &externals,
        );
        // Bump mtime+content.
        std::thread::sleep(Duration::from_millis(15));
        fs::write(&a, b"version-two-longer").unwrap();
        let k2 = compute_key_with_deps_kernels_and_externals(
            b"src", &[], &[], &[], &externals,
        );
        assert_ne!(k1, k2);
    }

    #[test]
    fn cache_key_external_vs_no_external_differs() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a.bin");
        fs::write(&a, b"x").unwrap();
        let with =
            compute_key_with_deps_kernels_and_externals(b"src", &[], &[], &[], &[a.as_path()]);
        let without =
            compute_key_with_deps_kernels_and_externals(b"src", &[], &[], &[], &[]);
        assert_ne!(with, without);
    }

    #[test]
    fn cache_entry_paths_share_stem() {
        let dir = Path::new("/var/cache/provium");
        let entry = CacheEntryPaths::for_key(
            dir,
            &CacheKey("deadbeef".into()),
        );
        assert_eq!(
            entry.snapshot,
            PathBuf::from("/var/cache/provium/deadbeef.snap")
        );
        assert_eq!(
            entry.lock,
            PathBuf::from("/var/cache/provium/deadbeef.lock")
        );
    }

    #[test]
    fn build_lock_is_exclusive_within_process() {
        let dir = tempfile::tempdir().unwrap();
        let lock_path = dir.path().join("test.lock");

        let lock = acquire_build_lock(&lock_path).unwrap();
        // Hold the lock; spawn a thread that should park.
        let lock_path_clone = lock_path.clone();
        let parked = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let parked_for_thread = std::sync::Arc::clone(&parked);
        let handle = std::thread::spawn(move || {
            let _l = acquire_build_lock(&lock_path_clone).unwrap();
            parked_for_thread.store(true, std::sync::atomic::Ordering::SeqCst);
        });
        std::thread::sleep(Duration::from_millis(40));
        assert!(
            !parked.load(std::sync::atomic::Ordering::SeqCst),
            "second lock should park"
        );
        drop(lock);
        handle.join().unwrap();
        assert!(parked.load(std::sync::atomic::Ordering::SeqCst));
    }

    #[test]
    fn evict_to_does_nothing_when_under_budget() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("a.snap"), b"abcde").unwrap();
        fs::write(dir.path().join("b.snap"), b"fghij").unwrap();

        let report = evict_to(dir.path(), 1024).unwrap();
        assert_eq!(report.evicted_count, 0);
        assert!(dir.path().join("a.snap").exists());
        assert!(dir.path().join("b.snap").exists());
    }

    #[test]
    fn evict_to_drops_oldest_until_under_budget() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a.snap");
        let b = dir.path().join("b.snap");
        let c = dir.path().join("c.snap");
        fs::write(&a, vec![0u8; 1000]).unwrap();
        std::thread::sleep(Duration::from_millis(10));
        fs::write(&b, vec![0u8; 1000]).unwrap();
        std::thread::sleep(Duration::from_millis(10));
        fs::write(&c, vec![0u8; 1000]).unwrap();

        // Make atimes distinct by reading them in order.
        let _ = std::fs::read(&a);
        std::thread::sleep(Duration::from_millis(20));
        let _ = std::fs::read(&b);
        std::thread::sleep(Duration::from_millis(20));
        let _ = std::fs::read(&c);

        // Cap at 2000 bytes — one entry must be evicted.
        let report = evict_to(dir.path(), 2000).unwrap();
        assert!(report.evicted_count >= 1);
        assert!(!a.exists(), "oldest should be gone");
    }
}
