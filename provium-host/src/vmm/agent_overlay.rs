//! Concatenate the agent-overlay cpio onto a user-supplied initrd.
//!
//! The Linux kernel happily unpacks a sequence of gzipped cpio
//! archives concatenated into a single initrd file: each archive is
//! unpacked in order into the same rootfs and later entries overwrite
//! earlier ones by path. We exploit that here — the overlay places
//! `/sbin/provium-agent` plus the pseudo-FS mountpoint dirs without
//! touching the user's `/init`, then `rdinit=/sbin/provium-agent` on
//! the kernel cmdline tells the kernel to run the agent as PID 1.
//! The agent itself forks the user's `/init` so userspace still runs
//! (see `provium-agent/src/init.rs`).
//!
//! Cached by `sha256(user_initrd_bytes || overlay_bytes)` — second
//! and subsequent boots of the same (initrd, overlay) pair pay
//! nothing beyond a hash check.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

use crate::profile::Profile;
use crate::vmm::VmmError;

/// Hard-wired mountpoint inside the overlay where we drop the agent.
/// Set on the kernel cmdline as `rdinit=...` so the kernel exec's
/// the agent as PID 1.
pub const OVERLAY_AGENT_PATH: &str = "/sbin/provium-agent";

/// Result of [`prepare_initrd`] — describes what to pass to QEMU.
#[derive(Debug, Clone)]
pub struct PreparedInitrd {
    /// Path to feed to QEMU's `-initrd`. Either the original profile
    /// initrd (when injection was skipped) or the cached merged file.
    pub initrd_path: PathBuf,
    /// Cmdline to feed to QEMU's `-append`. Either the input cmdline
    /// unchanged (no injection) or the input + ` rdinit=...`.
    pub cmdline: String,
    /// `true` when injection actually ran. Used by callers that want
    /// to log the merged path / cache hit-or-miss.
    pub injected: bool,
}

/// Locate the agent-overlay cpio. Order:
/// 1. `profile.agent_overlay_path` if set
/// 2. `PROVIUM_OVERLAY` env var if set
/// 3. `<provium-binary-dir>/../share/provium/agent-overlay.cpio.gz`
/// 4. `<workspace>/dist/agent-overlay.cpio.gz` walking up from the
///    binary location — for in-development runs out of `target/`.
pub fn locate_overlay(profile: &Profile) -> Option<PathBuf> {
    if let Some(p) = &profile.agent_overlay_path {
        return Some(p.clone());
    }
    if let Ok(env_path) = std::env::var("PROVIUM_OVERLAY") {
        if !env_path.is_empty() {
            return Some(PathBuf::from(env_path));
        }
    }
    let exe = std::env::current_exe().ok()?;
    let exe_dir = exe.parent()?;
    let installed = exe_dir.join("../share/provium/agent-overlay.cpio.gz");
    if installed.exists() {
        return Some(installed);
    }
    // Walk up looking for `dist/agent-overlay.cpio.gz` — covers the
    // `target/{debug,release}/provium` and `~/.cargo/bin/provium`
    // (when symlinked from the workspace) cases. Five hops is plenty.
    let mut cur = exe_dir.to_path_buf();
    for _ in 0..6 {
        let candidate = cur.join("dist/agent-overlay.cpio.gz");
        if candidate.exists() {
            return Some(candidate);
        }
        let Some(parent) = cur.parent() else { break };
        cur = parent.to_path_buf();
    }
    None
}

/// Apply agent-overlay injection per `profile.inject_agent`. When
/// disabled, returns the inputs unchanged. When enabled:
///
/// 1. Refuse if `cmdline` already contains `rdinit=` set to anything
///    other than our path — silently overriding would mask a real
///    config conflict.
/// 2. Locate the overlay (see [`locate_overlay`]).
/// 3. Compute `sha256(user_initrd || overlay)` as the cache key.
/// 4. On cache miss, write `<cache_dir>/<key>.cpio.gz` = literal
///    byte concat of the two files.
/// 5. Append `rdinit=/sbin/provium-agent` to the cmdline.
pub fn prepare_initrd(
    profile: &Profile,
    cmdline: &str,
    cache_root: &Path,
) -> Result<PreparedInitrd, VmmError> {
    if !profile.inject_agent {
        return Ok(PreparedInitrd {
            initrd_path: profile.initrd.clone(),
            cmdline: cmdline.to_owned(),
            injected: false,
        });
    }

    if let Some(existing) = parse_rdinit(cmdline) {
        if existing != OVERLAY_AGENT_PATH {
            return Err(VmmError::AgentOverlay(format!(
                "profile cmdline already pins rdinit={existing}; either remove it \
                 or set inject_agent = false in this profile"
            )));
        }
        // Already set to our path (e.g. cmdline pinned by a previous
        // user). Keep cmdline untouched but still inject the overlay
        // so /sbin/provium-agent exists in the unpacked rootfs.
    }

    let overlay = locate_overlay(profile).ok_or_else(|| {
        VmmError::AgentOverlay(
            "could not find agent-overlay.cpio.gz — run \
             scripts/build-overlay.sh, or set agent_overlay_path in the profile, \
             or set inject_agent = false"
                .into(),
        )
    })?;

    let user_bytes = fs::read(&profile.initrd).map_err(|e| {
        VmmError::AgentOverlay(format!(
            "read user initrd `{}`: {e}",
            profile.initrd.display()
        ))
    })?;
    let overlay_bytes = fs::read(&overlay).map_err(|e| {
        VmmError::AgentOverlay(format!(
            "read overlay `{}`: {e}",
            overlay.display()
        ))
    })?;

    let mut hasher = Sha256::new();
    hasher.update(&user_bytes);
    hasher.update(&overlay_bytes);
    let key = format!("{:x}", hasher.finalize());

    let cache_dir = cache_root.join("agent-overlay-cache");
    fs::create_dir_all(&cache_dir).map_err(|e| {
        VmmError::AgentOverlay(format!(
            "create cache dir `{}`: {e}",
            cache_dir.display()
        ))
    })?;
    let merged_path = cache_dir.join(format!("{key}.cpio.gz"));

    if !merged_path.exists() {
        // Write to a temp file in the same dir, then rename — avoids
        // a partial-write being seen as a "cache hit" by a sibling
        // launch racing the same key.
        let tmp = cache_dir.join(format!(".{key}.partial"));
        let mut f = fs::File::create(&tmp).map_err(|e| {
            VmmError::AgentOverlay(format!(
                "create cache file `{}`: {e}",
                tmp.display()
            ))
        })?;
        f.write_all(&user_bytes).map_err(|e| {
            VmmError::AgentOverlay(format!("write merged initrd: {e}"))
        })?;
        f.write_all(&overlay_bytes).map_err(|e| {
            VmmError::AgentOverlay(format!("write merged initrd: {e}"))
        })?;
        drop(f);
        fs::rename(&tmp, &merged_path).map_err(|e| {
            VmmError::AgentOverlay(format!(
                "rename `{}` -> `{}`: {e}",
                tmp.display(),
                merged_path.display(),
            ))
        })?;
    }

    let augmented_cmdline = if parse_rdinit(cmdline).is_some() {
        cmdline.to_owned()
    } else {
        format!("{} rdinit={OVERLAY_AGENT_PATH}", cmdline.trim_end())
    };

    Ok(PreparedInitrd {
        initrd_path: merged_path,
        cmdline: augmented_cmdline,
        injected: true,
    })
}

/// Extract the `rdinit=PATH` value from a kernel cmdline, if present.
/// Whitespace-delimited tokens; the first match wins (matches the
/// kernel's own parser, which uses the last `init=` / `rdinit=` it
/// sees but practical configs don't repeat them).
fn parse_rdinit(cmdline: &str) -> Option<&str> {
    cmdline
        .split_ascii_whitespace()
        .find_map(|tok| tok.strip_prefix("rdinit="))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn tmp_profile(initrd: PathBuf, inject: bool) -> Profile {
        Profile {
            kernel: "/k".into(),
            initrd,
            cmdline: "console=hvc0 quiet".into(),
            guest_os: "peios".into(),
            inject_agent: inject,
            agent_overlay_path: None,
            cmdline_file: None,
            build: None,
            build_out: None,
        }
    }

    #[test]
    fn parse_rdinit_finds_token() {
        assert_eq!(parse_rdinit("foo rdinit=/x bar"), Some("/x"));
        assert_eq!(parse_rdinit("rdinit=/y"), Some("/y"));
        assert_eq!(parse_rdinit("foo bar"), None);
    }

    #[test]
    fn skip_when_disabled() {
        let dir = tempfile::tempdir().unwrap();
        let initrd = dir.path().join("user.cpio.gz");
        std::fs::write(&initrd, b"USER").unwrap();
        let profile = tmp_profile(initrd.clone(), false);
        let prep = prepare_initrd(&profile, "console=hvc0", dir.path()).unwrap();
        assert!(!prep.injected);
        assert_eq!(prep.initrd_path, initrd);
        assert_eq!(prep.cmdline, "console=hvc0");
    }

    #[test]
    fn refuses_conflicting_rdinit() {
        let dir = tempfile::tempdir().unwrap();
        let initrd = dir.path().join("user.cpio.gz");
        std::fs::write(&initrd, b"USER").unwrap();
        let profile = tmp_profile(initrd, true);
        let err = prepare_initrd(
            &profile,
            "rdinit=/different/init console=hvc0",
            dir.path(),
        )
        .unwrap_err();
        match err {
            VmmError::AgentOverlay(msg) => assert!(msg.contains("rdinit=")),
            other => panic!("expected AgentOverlay, got {other:?}"),
        }
    }

    #[test]
    fn merged_file_concats_inputs() {
        let dir = tempfile::tempdir().unwrap();
        let initrd = dir.path().join("user.cpio.gz");
        let overlay = dir.path().join("overlay.cpio.gz");
        std::fs::write(&initrd, b"USERDATA").unwrap();
        std::fs::write(&overlay, b"OVERLAYDATA").unwrap();
        let mut profile = tmp_profile(initrd, true);
        profile.agent_overlay_path = Some(overlay);
        let prep = prepare_initrd(&profile, "console=hvc0", dir.path()).unwrap();
        assert!(prep.injected);
        let merged = std::fs::read(&prep.initrd_path).unwrap();
        assert_eq!(merged, b"USERDATAOVERLAYDATA");
        assert!(prep.cmdline.contains("rdinit=/sbin/provium-agent"));
    }

    #[test]
    fn cache_hit_skips_rewrite() {
        let dir = tempfile::tempdir().unwrap();
        let initrd = dir.path().join("user.cpio.gz");
        let overlay = dir.path().join("overlay.cpio.gz");
        std::fs::write(&initrd, b"USERDATA").unwrap();
        std::fs::write(&overlay, b"OVERLAYDATA").unwrap();
        let mut profile = tmp_profile(initrd, true);
        profile.agent_overlay_path = Some(overlay);
        let p1 = prepare_initrd(&profile, "console=hvc0", dir.path()).unwrap();
        let mtime1 = std::fs::metadata(&p1.initrd_path)
            .unwrap()
            .modified()
            .unwrap();
        std::thread::sleep(std::time::Duration::from_millis(20));
        let p2 = prepare_initrd(&profile, "console=hvc0", dir.path()).unwrap();
        let mtime2 = std::fs::metadata(&p2.initrd_path)
            .unwrap()
            .modified()
            .unwrap();
        assert_eq!(p1.initrd_path, p2.initrd_path);
        assert_eq!(mtime1, mtime2, "cache hit should not rewrite");
    }
}
