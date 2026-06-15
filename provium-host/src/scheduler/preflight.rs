//! Startup pre-flight checks.
//!
//! Per `DESIGN.md` § Components / Host startup pre-flight: validate
//! the systemic prerequisites before scanning tests so any failure
//! surfaces with one actionable diagnostic instead of as a downstream
//! mystery.
//!
//! ## Checks (slice 5)
//!
//! | Check                             | Why                                | Failure message               |
//! |-----------------------------------|------------------------------------|-------------------------------|
//! | `/dev/kvm` exists and accessible  | QEMU needs KVM                     | "load the kvm module …"       |
//! | `/dev/vhost-vsock` exists         | Required for vsock between host + VMs | "modprobe vhost_vsock"   |
//! | `ip` (iproute2) on `PATH`         | Bridge / TAP / tc operations       | "install iproute2 …"          |
//!
//! `CAP_NET_ADMIN` is *not* checked here — it's only required once
//! bridges land (slice 9). The check moves into preflight at that
//! point.

use std::path::Path;

use thiserror::Error;

/// One pre-flight failure. Each carries a `recommendation` that the
/// CLI surfaces verbatim — actionable next-step text per design.
#[derive(Debug, Error)]
pub enum PreflightError {
    /// `/dev/kvm` is missing or not openable.
    #[error("/dev/kvm: {detail}\n  → {recommendation}")]
    Kvm {
        /// Underlying problem (missing, permission denied, …).
        detail: String,
        /// Suggested fix.
        recommendation: &'static str,
    },

    /// `/dev/vhost-vsock` is missing.
    #[error("/dev/vhost-vsock: {detail}\n  → {recommendation}")]
    VhostVsock {
        /// Underlying problem.
        detail: String,
        /// Suggested fix.
        recommendation: &'static str,
    },

    /// iproute2 binaries not on PATH.
    #[error("iproute2: `{program}` not found on PATH\n  → {recommendation}")]
    Iproute2 {
        /// Specific binary that was missing — `ip` in v1.
        program: &'static str,
        /// Suggested fix.
        recommendation: &'static str,
    },

    /// Process lacks `CAP_NET_ADMIN`. Required by bridge / TAP / tc
    /// operations per `DESIGN.md` § Pre-flight check.
    #[error("CAP_NET_ADMIN missing\n  → {recommendation}")]
    NetAdmin {
        /// Suggested fix.
        recommendation: &'static str,
    },

    /// QEMU binary missing from PATH. R9 sched-M2: without this
    /// check, the user got a confusing runtime error from the
    /// first `vm:boot()` instead of a clean upfront diagnostic.
    #[error("qemu: `{program}` not found on PATH\n  → {recommendation}")]
    QemuMissing {
        /// Specific binary that was missing.
        program: &'static str,
        /// Suggested fix.
        recommendation: &'static str,
    },
}

/// Successful preflight result. Captures which checks ran for
/// telemetry / diagnostic; future-proof for "warned but not failed"
/// classifications.
#[derive(Clone, Debug, Default)]
pub struct PreflightReport {
    /// Each check that ran successfully.
    pub passed: Vec<&'static str>,
}

/// Run every check. Returns the first failure or a successful
/// report listing the checks that passed.
pub fn run() -> Result<PreflightReport, PreflightError> {
    let mut report = PreflightReport::default();

    check_kvm()?;
    report.passed.push("/dev/kvm");

    check_vhost_vsock()?;
    report.passed.push("/dev/vhost-vsock");

    check_iproute2()?;
    report.passed.push("iproute2");

    check_qemu()?;
    report.passed.push("qemu");

    check_net_admin()?;
    report.passed.push("CAP_NET_ADMIN");

    // Sweep stale `provium_*` nft tables left over from a prior
    // run that was killed mid-test. Without this, the next run's
    // bridges start with realized=false → refresh_partitions
    // short-circuits → stale DROP rules from the prior run leak
    // into this one and silently break tests.
    sweep_stale_nft_tables();
    report.passed.push("nft cleanup");

    Ok(report)
}

/// Best-effort: enumerate `nft list tables` and delete any
/// `provium_*` table (bridge or ip family). Failures (no nft on
/// PATH, permission denied) are silently ignored — the
/// pre-existing checks above guarantee the tooling is present.
fn sweep_stale_nft_tables() {
    let out = std::process::Command::new("nft")
        .args(["-j", "list", "tables"])
        .output();
    let stdout = match out {
        Ok(o) if o.status.success() => o.stdout,
        _ => return,
    };
    // Parse the JSON minimally — we just need the (family, name)
    // pairs. The shape is `{"nftables":[{"metainfo":...},
    // {"table":{"family":"bridge","name":"provium_lan",...}}, ...]}`.
    let parsed: Result<serde_json::Value, _> = serde_json::from_slice(&stdout);
    let Ok(v) = parsed else {
        return;
    };
    let Some(arr) = v.get("nftables").and_then(|n| n.as_array()) else {
        return;
    };
    for entry in arr {
        let Some(t) = entry.get("table") else { continue };
        let name = t.get("name").and_then(|n| n.as_str()).unwrap_or("");
        let family = t.get("family").and_then(|f| f.as_str()).unwrap_or("");
        if name.starts_with("provium_") {
            let _ = std::process::Command::new("nft")
                .args(["delete", "table", family, name])
                .status();
        }
    }
}

/// Probe `CAP_NET_ADMIN`. Cheap: try to read `/proc/self/status`
/// for the effective capability mask. Falls back to a non-fatal
/// "skipped" if the file isn't readable (kernel without procfs).
fn check_net_admin() -> Result<(), PreflightError> {
    // CAP_NET_ADMIN is bit 12 in the effective-cap mask.
    const CAP_NET_ADMIN_BIT: u64 = 1 << 12;
    let status = match std::fs::read_to_string("/proc/self/status") {
        Ok(s) => s,
        Err(_) => return Ok(()),
    };
    let mut effective: Option<u64> = None;
    for line in status.lines() {
        if let Some(rest) = line.strip_prefix("CapEff:") {
            // hex, possibly with leading whitespace
            if let Ok(v) = u64::from_str_radix(rest.trim(), 16) {
                effective = Some(v);
                break;
            }
        }
    }
    match effective {
        Some(mask) if mask & CAP_NET_ADMIN_BIT != 0 => Ok(()),
        Some(_) => Err(PreflightError::NetAdmin {
            recommendation:
                "run with sudo, or `setcap cap_net_admin,cap_net_raw=eip $(which provium)`",
        }),
        None => Ok(()), // non-Linux or no procfs — accept
    }
}

fn check_kvm() -> Result<(), PreflightError> {
    let path = Path::new("/dev/kvm");
    if !path.exists() {
        return Err(PreflightError::Kvm {
            detail: "device file does not exist".into(),
            recommendation: "load the kvm module: `sudo modprobe kvm-intel` or `kvm-amd` for your CPU",
        });
    }
    // Touch test — open for read so a permission failure surfaces
    // without us holding an fd.
    if let Err(e) = std::fs::File::open(path) {
        return Err(PreflightError::Kvm {
            detail: e.to_string(),
            recommendation: "ensure you're in the `kvm` group: `sudo usermod -aG kvm $USER` then re-login",
        });
    }
    Ok(())
}

fn check_vhost_vsock() -> Result<(), PreflightError> {
    let path = Path::new("/dev/vhost-vsock");
    if !path.exists() {
        return Err(PreflightError::VhostVsock {
            detail: "device file does not exist".into(),
            recommendation: "load the vhost_vsock module: `sudo modprobe vhost_vsock`",
        });
    }
    Ok(())
}

fn check_iproute2() -> Result<(), PreflightError> {
    // R9 mat-m3: bridge layer calls `ip`, `tc`, AND `nft`.
    // Surface each missing binary individually so the user gets
    // actionable diagnostics rather than runtime errors from the
    // first impairment / partition op.
    for prog in ["ip", "tc"] {
        if which_on_path(prog).is_none() {
            return Err(PreflightError::Iproute2 {
                program: match prog {
                    "ip" => "ip",
                    "tc" => "tc",
                    _ => unreachable!(),
                },
                recommendation: "install iproute2 (`apt install iproute2`, `pacman -S iproute2`, or `nix-env -iA nixpkgs.iproute2`)",
            });
        }
    }
    if which_on_path("nft").is_none() {
        return Err(PreflightError::Iproute2 {
            program: "nft",
            recommendation: "install nftables (`apt install nftables`, `pacman -S nftables`, or `nix-env -iA nixpkgs.nftables`)",
        });
    }
    Ok(())
}

/// QEMU binary check (R9 sched-M2). Without this, the user got a
/// confusing error from the first `vm:boot()` instead of a clean
/// preflight diagnostic.
fn check_qemu() -> Result<(), PreflightError> {
    // qemu-system-x86_64 is the v1 default per QemuVmmConfig.
    // Per-arch builds may need a different binary; for now the
    // x86_64 form is what the launcher invokes.
    if which_on_path("qemu-system-x86_64").is_some() {
        return Ok(());
    }
    Err(PreflightError::QemuMissing {
        program: "qemu-system-x86_64",
        recommendation: "install qemu (`apt install qemu-system-x86_64`, `pacman -S qemu-base`, or `nix-env -iA nixpkgs.qemu`)",
    })
}

/// Minimal `which` implementation — stdlib doesn't have one and the
/// `which` crate adds dependency weight we don't need for one
/// lookup. Honours the standard `PATH` environment variable.
fn which_on_path(program: &str) -> Option<std::path::PathBuf> {
    let path_var = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path_var) {
        let candidate = dir.join(program);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    // The system-level checks (`/dev/kvm` etc.) are environment-
    // dependent — we don't run them in the standard test suite.
    // The preflight runner is exercised end-to-end by the slice-5
    // integration tests + the real-QEMU smoke script.

    #[test]
    fn which_on_path_finds_sh() {
        // `sh` is on every POSIX PATH.
        assert!(which_on_path("sh").is_some());
    }

    #[test]
    fn which_on_path_returns_none_for_nonexistent() {
        assert!(which_on_path("definitely-not-on-any-path-xyzzy").is_none());
    }

    #[test]
    fn check_iproute2_recommendation_includes_install_instructions() {
        // We don't run the live check (would fail on minimal CI),
        // but verify the error type is renderable end-to-end.
        let err = PreflightError::Iproute2 {
            program: "ip",
            recommendation: "install iproute2",
        };
        let s = err.to_string();
        assert!(s.contains("ip"));
        assert!(s.contains("install"));
    }
}
