//! `provium.toml` loading.
//!
//! Two top-level sections matter at this slice:
//!
//! ```toml
//! [provium]
//! roots = ["tests"]              # scan paths (used by slice 2 runner)
//!
//! [profiles.peios]
//! kernel  = "/path/to/peios/test-kernel"
//! initrd  = "/path/to/peios-test-initrd"
//! cmdline = "console=hvc0 quiet"
//! guest_os = "peios"             # optional, defaults "peios"
//! ```
//!
//! `[profiles.<name>]` is what the VMM reads to spawn a VM.
//! Path validation (does the kernel actually exist?) happens at
//! VM-boot time rather than config-load time so a `provium.toml` can
//! be portable across machines.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Top-level `provium.toml` shape.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct Config {
    /// `[provium]` section — global runner settings. Most fields are
    /// only consumed by the slice-2 test runner / fixture cache;
    /// they're parsed here so an unknown-key error doesn't fire on a
    /// well-formed file.
    #[serde(default)]
    pub provium: ProviumSection,

    /// `[profiles.<name>]` — at least one is required for any
    /// non-trivial run.
    #[serde(default)]
    pub profiles: BTreeMap<String, Profile>,
}

/// `[provium]` section.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct ProviumSection {
    /// Scan roots for `*.test.lua` / `*.fixture.lua` files. Empty in
    /// slice 1; the test runner uses these in slice 2.
    #[serde(default)]
    pub roots: Vec<String>,

    /// Override the fixture cache directory. Defaults to
    /// `~/.cache/provium/fixtures/` per design.
    #[serde(default)]
    pub cache_dir: Option<PathBuf>,

    /// Override the fixture cache cap. Format mirrors the
    /// time/size convention: number-or-string ("20G", `21474836480`).
    #[serde(default)]
    pub cache_max_size: Option<String>,
}

/// One named `[profiles.<name>]` entry.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Profile {
    /// Kernel image. Direct-boot via QEMU's `-kernel`.
    pub kernel: PathBuf,
    /// Initramfs image. By default the agent-overlay is concatenated
    /// onto this at launch — set `inject_agent = false` to use it
    /// as-is (e.g. when the initrd already contains the agent).
    pub initrd: PathBuf,
    /// Inline kernel command line. May be empty (or omitted) when
    /// `cmdline_file` is set — the two compose, file first with this
    /// appended after. Boot opts can override the whole thing per-VM.
    #[serde(default)]
    pub cmdline: String,
    /// Optional path to a file whose contents form the *base* kernel
    /// command line — e.g. an image builder's generated `cmdline`
    /// (`peiso` writes one to `out/root/boot/cmdline`). Whitespace,
    /// including newlines, is collapsed to single spaces; then the
    /// inline `cmdline` (if any) is appended, so inline tokens win for
    /// last-wins kernel params (`init=`, `loglevel=`, …). Resolved
    /// relative to the cwd like `kernel`/`initrd`, and read at
    /// VM-boot time. At least one of `cmdline`/`cmdline_file` must be
    /// set; pointing at the builder's file keeps the two from drifting.
    #[serde(default)]
    pub cmdline_file: Option<PathBuf>,
    /// Agent port. For Peios, the v1 default. Exposing the field
    /// future-proofs the schema for ports that pick a different one.
    #[serde(default = "default_guest_os")]
    pub guest_os: String,
    /// When true (the default), the QEMU launch path concatenates
    /// the agent-overlay cpio onto `initrd`, caches the result, and
    /// appends `rdinit=/sbin/provium-agent` to the kernel cmdline.
    /// Set to `false` if the initrd already contains the agent at
    /// the path the kernel will exec.
    #[serde(default = "default_inject_agent")]
    pub inject_agent: bool,
    /// Override the path to the agent-overlay cpio. Defaults to
    /// `<provium-binary-dir>/../share/provium/agent-overlay.cpio.gz`
    /// when unset, with fallback to `dist/agent-overlay.cpio.gz`
    /// relative to the provium workspace for in-development runs.
    #[serde(default)]
    pub agent_overlay_path: Option<PathBuf>,
    /// Optional **build command** that produces this profile's boot
    /// artifacts. Run with `sh -c` from provium's cwd, **once** before
    /// any VM boots, when the profile is used (`provium test`,
    /// `console`, `repl`, or `provium prepare`). A non-zero exit aborts
    /// the run — provium never falls through to booting stale outputs.
    ///
    /// provium tracks **no** staleness: the command runs every
    /// invocation (skip with `--no-build`); making rebuilds cheap when
    /// nothing changed is the builder's job, not provium's. The literal
    /// token `{out}` (here and in the path fields) expands to this
    /// profile's [`Profile::out_dir`], so the build writes there and
    /// `kernel`/`initrd`/`cmdline_file` read from the same place without
    /// drifting. See [`crate::build`].
    #[serde(default)]
    pub build: Option<String>,
    /// Override the `{out}` directory. When unset it defaults to a
    /// per-profile dir under the provium cache (see
    /// [`default_build_base`]); the profile name is appended. provium
    /// never wipes it — the `build` command owns its contents.
    #[serde(default)]
    pub build_out: Option<PathBuf>,
}

fn default_guest_os() -> String {
    "peios".into()
}

fn default_inject_agent() -> bool {
    true
}

/// Base directory under which a dynamic profile's `{out}` build
/// directory lives (the profile name is appended). Mirrors the
/// fixture-cache resolution: `$PROVIUM_BUILD_DIR`, then XDG, then
/// `~/.cache/`, then `/tmp`. provium never wipes these — the build
/// command owns its output dir.
pub fn default_build_base() -> PathBuf {
    if let Some(p) = std::env::var_os("PROVIUM_BUILD_DIR") {
        return PathBuf::from(p);
    }
    if let Some(p) = std::env::var_os("XDG_CACHE_HOME") {
        return PathBuf::from(p).join("provium").join("builds");
    }
    if let Some(p) = std::env::var_os("HOME") {
        return PathBuf::from(p).join(".cache").join("provium").join("builds");
    }
    PathBuf::from("/tmp/provium-builds")
}

impl Profile {
    /// The base kernel command line, before `ensure_serial_console`
    /// and agent-overlay `rdinit=` injection layer on top.
    ///
    /// When `cmdline_file` is set, its contents (with all whitespace —
    /// including newlines — collapsed to single spaces) form the base,
    /// and the inline `cmdline` is appended after. Appending-after
    /// means inline tokens win for last-wins kernel params. When
    /// `cmdline_file` is unset this is just the inline `cmdline`,
    /// normalised the same way.
    ///
    /// Reads the file lazily at call time (VM-boot), so a config can
    /// reference a builder output that doesn't exist yet at load time.
    pub fn resolve_cmdline(&self) -> std::io::Result<String> {
        let mut parts: Vec<String> = Vec::new();
        if let Some(file) = &self.cmdline_file {
            let raw = fs::read_to_string(file).map_err(|e| {
                std::io::Error::new(
                    e.kind(),
                    format!("read cmdline_file `{}`: {e}", file.display()),
                )
            })?;
            parts.extend(raw.split_whitespace().map(str::to_owned));
        }
        parts.extend(self.cmdline.split_whitespace().map(str::to_owned));
        Ok(parts.join(" "))
    }

    /// The resolved `{out}` build-output directory for this profile.
    /// `build_out` when set, else `<default_build_base>/<profile_name>`.
    pub fn out_dir(&self, profile_name: &str) -> PathBuf {
        self.build_out
            .clone()
            .unwrap_or_else(|| default_build_base().join(profile_name))
    }
}

/// Errors raised by [`Config::load`].
#[derive(Debug, Error)]
pub enum ConfigError {
    /// Reading the file from disk failed.
    #[error("read `{path}`: {source}")]
    Io {
        /// File whose read failed.
        path: PathBuf,
        /// Underlying I/O error.
        #[source]
        source: std::io::Error,
    },

    /// The TOML did not parse.
    #[error("parse `{path}`: {source}")]
    Parse {
        /// File that failed to parse.
        path: PathBuf,
        /// Underlying TOML error.
        #[source]
        source: toml::de::Error,
    },

    /// The TOML parsed but failed semantic validation (e.g. an empty
    /// `cmdline` or an unknown `guest_os`).
    #[error("invalid config in `{path}`: {message}")]
    Validation {
        /// File whose contents failed validation.
        path: PathBuf,
        /// Human-readable description of the violation.
        message: String,
    },
}

impl Config {
    /// Load and validate a `provium.toml` from disk.
    pub fn load(path: impl AsRef<Path>) -> Result<Self, ConfigError> {
        let path = path.as_ref();
        let raw = fs::read_to_string(path).map_err(|e| ConfigError::Io {
            path: path.into(),
            source: e,
        })?;
        Self::from_toml_str(&raw, path)
    }

    /// Parse + validate a config from in-memory TOML. The `path` is
    /// used purely for error messages.
    pub fn from_toml_str(raw: &str, path: &Path) -> Result<Self, ConfigError> {
        let config: Config = toml::from_str(raw).map_err(|e| ConfigError::Parse {
            path: path.into(),
            source: e,
        })?;
        config.validate(path)?;
        Ok(config)
    }

    /// Look up a profile by name.
    pub fn profile(&self, name: &str) -> Option<&Profile> {
        self.profiles.get(name)
    }

    /// Expand the literal `{out}` token in every profile's `build`
    /// command and path fields to that profile's resolved
    /// [`Profile::out_dir`]. Call once right after [`Config::load`],
    /// before the config is used to boot or build — every downstream
    /// consumer then sees concrete paths, so the build's `--out` and
    /// the `kernel`/`initrd`/`cmdline_file` it reads can never drift.
    /// A no-op for profiles that don't use `{out}`.
    pub fn expand_build_outputs(&mut self) {
        for (name, profile) in self.profiles.iter_mut() {
            let out = profile.out_dir(name).to_string_lossy().into_owned();
            let sub = |s: &str| s.replace("{out}", &out);

            let new_build = profile.build.as_deref().map(&sub);
            let new_kernel = PathBuf::from(sub(&profile.kernel.to_string_lossy()));
            let new_initrd = PathBuf::from(sub(&profile.initrd.to_string_lossy()));
            let new_cmdline = sub(&profile.cmdline);
            let new_cmdline_file = profile
                .cmdline_file
                .as_deref()
                .map(|f| PathBuf::from(sub(&f.to_string_lossy())));
            let new_overlay = profile
                .agent_overlay_path
                .as_deref()
                .map(|a| PathBuf::from(sub(&a.to_string_lossy())));

            profile.build = new_build;
            profile.kernel = new_kernel;
            profile.initrd = new_initrd;
            profile.cmdline = new_cmdline;
            profile.cmdline_file = new_cmdline_file;
            profile.agent_overlay_path = new_overlay;
        }
    }

    fn validate(&self, path: &Path) -> Result<(), ConfigError> {
        for (name, profile) in &self.profiles {
            if profile.cmdline.trim().is_empty() && profile.cmdline_file.is_none() {
                return Err(ConfigError::Validation {
                    path: path.into(),
                    message: format!(
                        "profile `{name}` has empty cmdline and no `cmdline_file`; \
                         set at least one"
                    ),
                });
            }
            // v1: only the Peios agent port exists. Surface a clear
            // diagnostic rather than booting and watching the agent
            // fail to come up.
            if profile.guest_os != "peios" {
                return Err(ConfigError::Validation {
                    path: path.into(),
                    message: format!(
                        "profile `{name}` has guest_os = `{}`, only `peios` is supported in v1",
                        profile.guest_os,
                    ),
                });
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn fake_path() -> PathBuf {
        PathBuf::from("provium.toml")
    }

    #[test]
    fn parses_minimal_profile() {
        let raw = r#"
[profiles.peios]
kernel  = "/k"
initrd  = "/i"
cmdline = "console=hvc0"
"#;
        let cfg = Config::from_toml_str(raw, &fake_path()).unwrap();
        let p = cfg.profile("peios").expect("profile present");
        assert_eq!(p.kernel, PathBuf::from("/k"));
        assert_eq!(p.initrd, PathBuf::from("/i"));
        assert_eq!(p.cmdline, "console=hvc0");
        assert_eq!(p.guest_os, "peios", "default applied");
    }

    #[test]
    fn parses_provium_section_and_multiple_profiles() {
        let raw = r#"
[provium]
roots = ["tests", "more-tests"]

[profiles.peios]
kernel  = "/k"
initrd  = "/i"
cmdline = "console=hvc0"

[profiles.peios-server]
kernel  = "/k"
initrd  = "/i2"
cmdline = "console=hvc0 role=server"
"#;
        let cfg = Config::from_toml_str(raw, &fake_path()).unwrap();
        assert_eq!(cfg.provium.roots, vec!["tests", "more-tests"]);
        assert_eq!(cfg.profiles.len(), 2);
        assert!(cfg.profile("peios-server").is_some());
    }

    #[test]
    fn rejects_empty_cmdline() {
        let raw = r#"
[profiles.peios]
kernel  = "/k"
initrd  = "/i"
cmdline = "   "
"#;
        let err = Config::from_toml_str(raw, &fake_path()).unwrap_err();
        match err {
            ConfigError::Validation { message, .. } => {
                assert!(message.contains("empty cmdline"));
            }
            other => panic!("expected Validation, got {other:?}"),
        }
    }

    #[test]
    fn rejects_unsupported_guest_os() {
        let raw = r#"
[profiles.linux]
kernel  = "/k"
initrd  = "/i"
cmdline = "console=hvc0"
guest_os = "linux"
"#;
        let err = Config::from_toml_str(raw, &fake_path()).unwrap_err();
        match err {
            ConfigError::Validation { message, .. } => {
                assert!(message.contains("guest_os"));
                assert!(message.contains("`peios`"));
            }
            other => panic!("expected Validation, got {other:?}"),
        }
    }

    #[test]
    fn parse_error_carries_file_context() {
        let raw = "this is not = valid toml [";
        let err = Config::from_toml_str(raw, &fake_path()).unwrap_err();
        match err {
            ConfigError::Parse { path, .. } => {
                assert_eq!(path, fake_path());
            }
            other => panic!("expected Parse, got {other:?}"),
        }
    }

    #[test]
    fn load_io_error_when_path_missing() {
        let err = Config::load("/nonexistent/path/provium.toml").unwrap_err();
        assert!(matches!(err, ConfigError::Io { .. }));
    }

    #[test]
    fn empty_config_parses_to_no_profiles() {
        let cfg = Config::from_toml_str("", &fake_path()).unwrap();
        assert!(cfg.profiles.is_empty());
    }

    fn profile_with(cmdline: &str, cmdline_file: Option<PathBuf>) -> Profile {
        Profile {
            kernel: PathBuf::from("/k"),
            initrd: PathBuf::from("/i"),
            cmdline: cmdline.to_string(),
            cmdline_file,
            guest_os: "peios".into(),
            inject_agent: true,
            agent_overlay_path: None,
            build: None,
            build_out: None,
        }
    }

    #[test]
    fn resolve_cmdline_inline_only() {
        let p = profile_with("console=hvc0  quiet", None);
        assert_eq!(p.resolve_cmdline().unwrap(), "console=hvc0 quiet");
    }

    #[test]
    fn resolve_cmdline_file_only_collapses_whitespace() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("cmdline");
        // Multi-line / extra whitespace must collapse to single spaces.
        fs::write(&file, "console=ttyS0 loglevel=7\n  init=/usr/bin/protoinit \n").unwrap();
        let p = profile_with("", Some(file));
        assert_eq!(
            p.resolve_cmdline().unwrap(),
            "console=ttyS0 loglevel=7 init=/usr/bin/protoinit"
        );
    }

    #[test]
    fn resolve_cmdline_composes_file_then_inline() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("cmdline");
        fs::write(&file, "console=ttyS0 init=/usr/bin/protoinit").unwrap();
        // Inline appended after the file: inline wins for last-wins
        // params (here a second console + an override of init=).
        let p = profile_with("console=hvc0 init=/sbin/other", Some(file));
        assert_eq!(
            p.resolve_cmdline().unwrap(),
            "console=ttyS0 init=/usr/bin/protoinit console=hvc0 init=/sbin/other"
        );
    }

    #[test]
    fn resolve_cmdline_missing_file_errors_with_path() {
        let p = profile_with("", Some(PathBuf::from("/nonexistent/cmdline")));
        let err = p.resolve_cmdline().unwrap_err();
        assert!(
            err.to_string().contains("/nonexistent/cmdline"),
            "error names the file: {err}"
        );
    }

    #[test]
    fn accepts_profile_with_only_cmdline_file() {
        let raw = r#"
[profiles.peios]
kernel       = "/k"
initrd       = "/i"
cmdline_file = "../peiso/out/root/boot/cmdline"
"#;
        let cfg = Config::from_toml_str(raw, &fake_path()).expect("valid: file supplies cmdline");
        let p = cfg.profile("peios").unwrap();
        assert_eq!(p.cmdline, "", "inline cmdline defaults empty");
        assert_eq!(
            p.cmdline_file.as_deref(),
            Some(Path::new("../peiso/out/root/boot/cmdline"))
        );
    }

    #[test]
    fn expand_build_outputs_substitutes_out_token() {
        let raw = r#"
[profiles.peios]
build        = "peiso build manifests/peios.toml --out {out}"
build_out    = "/custom/build/peios"
kernel       = "{out}/root/boot/vmlinuz"
initrd       = "{out}/initrd.img"
cmdline_file = "{out}/root/boot/cmdline"
"#;
        let mut cfg = Config::from_toml_str(raw, &fake_path()).unwrap();
        cfg.expand_build_outputs();
        let p = cfg.profile("peios").unwrap();
        assert_eq!(
            p.build.as_deref(),
            Some("peiso build manifests/peios.toml --out /custom/build/peios")
        );
        assert_eq!(
            p.kernel,
            PathBuf::from("/custom/build/peios/root/boot/vmlinuz")
        );
        assert_eq!(p.initrd, PathBuf::from("/custom/build/peios/initrd.img"));
        assert_eq!(
            p.cmdline_file.as_deref(),
            Some(Path::new("/custom/build/peios/root/boot/cmdline"))
        );
    }

    #[test]
    fn expand_build_outputs_default_dir_folds_in_profile_name() {
        let raw = r#"
[profiles.peios-full]
build        = "build --out {out}"
kernel       = "{out}/vmlinuz"
initrd       = "{out}/initrd.img"
cmdline_file = "{out}/cmdline"
"#;
        let mut cfg = Config::from_toml_str(raw, &fake_path()).unwrap();
        cfg.expand_build_outputs();
        let p = cfg.profile("peios-full").unwrap();
        // No build_out → default base with the profile name appended.
        assert!(
            p.kernel
                .to_string_lossy()
                .ends_with("builds/peios-full/vmlinuz"),
            "kernel lands under the per-profile build dir: {}",
            p.kernel.display()
        );
        assert!(
            p.build.as_deref().unwrap().contains("builds/peios-full"),
            "build `--out` points at the same per-profile dir: {:?}",
            p.build
        );
    }

    #[test]
    fn expand_build_outputs_noop_without_out_token() {
        let raw = r#"
[profiles.peios]
kernel  = "../peiso/out/root/boot/vmlinuz"
initrd  = "../peiso/out/initrd.img"
cmdline = "console=ttyS0"
"#;
        let mut cfg = Config::from_toml_str(raw, &fake_path()).unwrap();
        cfg.expand_build_outputs();
        let p = cfg.profile("peios").unwrap();
        // A static profile is untouched.
        assert_eq!(p.kernel, PathBuf::from("../peiso/out/root/boot/vmlinuz"));
        assert_eq!(p.cmdline, "console=ttyS0");
        assert!(p.build.is_none());
    }

    #[test]
    fn rejects_profile_with_neither_cmdline_source() {
        let raw = r#"
[profiles.peios]
kernel = "/k"
initrd = "/i"
"#;
        let err = Config::from_toml_str(raw, &fake_path()).unwrap_err();
        match err {
            ConfigError::Validation { message, .. } => {
                assert!(message.contains("cmdline"), "got: {message}");
            }
            other => panic!("expected Validation, got {other:?}"),
        }
    }
}
