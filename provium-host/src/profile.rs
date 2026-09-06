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
//! Profiles may also be discovered from a directory instead of being
//! written inline, which is what a repository holding many testsets
//! wants — one directory per testset, self-contained:
//!
//! ```toml
//! [profiles]
//! from_dir = "profiles"          # each subdir with a
//!                                # profile.provium.toml is a profile,
//!                                # named after the directory
//! ```
//!
//! A discovered profile's relative paths resolve against its own
//! directory, and its `build` runs there, so the directory can be moved
//! or copied without rewriting what is inside it. Inline profiles keep
//! resolving against provium's cwd, as they always have.
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
///
/// Built by [`Config::from_toml_str`] rather than deserialized
/// directly: profile discovery needs the directory the config was read
/// from, which serde has no way to know. By the time a `Config` exists,
/// `profiles` holds the inline and the discovered profiles alike and
/// nothing downstream can tell which was which.
#[derive(Clone, Debug, Default, Serialize)]
pub struct Config {
    /// `[provium]` section — global runner settings. Most fields are
    /// only consumed by the slice-2 test runner / fixture cache;
    /// they're parsed here so an unknown-key error doesn't fire on a
    /// well-formed file.
    #[serde(default)]
    pub provium: ProviumSection,

    /// `[profiles.<name>]` — at least one is required for any
    /// non-trivial run. Includes anything found via
    /// `[profiles] from_dir`.
    #[serde(default)]
    pub profiles: BTreeMap<String, Profile>,
}

/// `provium.toml` as written, before profile discovery.
#[derive(Deserialize)]
struct RawConfig {
    #[serde(default)]
    provium: ProviumSection,
    #[serde(default)]
    profiles: RawProfiles,
}

/// The `[profiles]` table: a directory to discover profiles in, plus
/// any written inline beside it. Both may appear; a discovered profile
/// whose name collides with an inline one is an error rather than a
/// silent winner.
#[derive(Default, Deserialize)]
struct RawProfiles {
    #[serde(default)]
    from_dir: Option<PathBuf>,
    #[serde(flatten)]
    inline: BTreeMap<String, Profile>,
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
    /// Kernel image. Direct-boot via QEMU's `-kernel`. May be omitted
    /// when `root` is set, in which case the kernel is found inside the
    /// root — see [`Profile::resolve_kernel`].
    #[serde(default)]
    pub kernel: PathBuf,
    /// Initramfs image. By default the agent-overlay is concatenated
    /// onto this at launch — set `inject_agent = false` to use it
    /// as-is (e.g. when the initrd already contains the agent).
    ///
    /// May be omitted, which means the VM has no userspace of its own:
    /// the agent overlay becomes the whole initramfs and the agent
    /// comes up as PID 1. That is the shape a kernel conformance
    /// testset wants, and omitting the field is how it says so without
    /// vendoring an initrd it would never otherwise need.
    #[serde(default)]
    pub initrd: PathBuf,
    /// A composed Peios root — what `peiso root` writes. Set this
    /// instead of `kernel` to let provium find the kernel inside it,
    /// so a profile whose build composes a root does not also have to
    /// know where in that root a kernel lands.
    #[serde(default)]
    pub root: Option<PathBuf>,
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
    /// How long, in seconds, to wait for the in-VM agent after QEMU
    /// is up before the boot is declared failed. Unset means the VMM
    /// default (30 s). A profile whose guest is expected to reach the
    /// agent in two seconds, and whose tests deliberately boot
    /// configurations that halt instead, sets this low so those
    /// failures are reported promptly. `vm:boot({agent_timeout = …})`
    /// overrides it for one boot.
    #[serde(default)]
    pub agent_boot_timeout: Option<f64>,
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
    /// The directory this profile was discovered in, for a profile
    /// found via `[profiles] from_dir`. Its relative paths have already
    /// been resolved against it by the time anything reads them; this
    /// is retained because the `build` command runs here rather than in
    /// provium's cwd. `None` for a profile written inline, whose
    /// paths and build belong to provium's cwd.
    #[serde(skip)]
    pub dir: Option<PathBuf>,
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

    /// The kernel to boot: `kernel` when set, otherwise the one inside
    /// `root`.
    ///
    /// A composed Peios root carries its kernel beside its modules, at
    /// `usr/lib/modules/<release>/vmlinuz-<release>` — the same place
    /// peiso itself looks when it builds a medium. Resolving it here
    /// rather than in each profile spares every testset the same
    /// symlink incantation, and means a kernel version bump moves
    /// nothing in any config.
    ///
    /// Deliberately lazy: a profile's `build` composes the root, so
    /// this is called at boot time, after the build has run. Two
    /// kernels in one root is an error rather than a choice made
    /// silently — a conformance result that does not say which kernel
    /// produced it is not a result.
    pub fn resolve_kernel(&self) -> Result<PathBuf, KernelError> {
        if !self.kernel.as_os_str().is_empty() {
            return Ok(self.kernel.clone());
        }
        let Some(root) = &self.root else {
            return Err(KernelError::NoSource);
        };
        let modules = root.join("usr/lib/modules");
        let mut found: Vec<PathBuf> = Vec::new();
        let entries = fs::read_dir(&modules).map_err(|source| KernelError::Read {
            path: modules.clone(),
            source,
        })?;
        for entry in entries.flatten() {
            let release = entry.path();
            let Ok(inner) = fs::read_dir(&release) else {
                continue;
            };
            for file in inner.flatten() {
                let name = file.file_name();
                if name.to_string_lossy().starts_with("vmlinuz-") {
                    found.push(file.path());
                }
            }
        }
        found.sort();
        match found.len() {
            0 => Err(KernelError::NotFound { root: root.clone() }),
            1 => Ok(found.remove(0)),
            _ => Err(KernelError::Ambiguous {
                root: root.clone(),
                found,
            }),
        }
    }

    /// Resolve this profile's relative paths against `dir`.
    ///
    /// Called once, when the profile is discovered. `{out}` is not
    /// expanded yet, so a path that starts with the token is left
    /// alone: it names a place in the build output directory, which is
    /// absolute already and has nothing to do with where the profile
    /// happens to live.
    fn rebase(&mut self, dir: &Path) {
        let fix = |p: &Path| -> PathBuf {
            if p.as_os_str().is_empty()
                || p.is_absolute()
                || p.to_string_lossy().starts_with("{out}")
            {
                p.to_path_buf()
            } else {
                dir.join(p)
            }
        };
        self.kernel = fix(&self.kernel);
        self.initrd = fix(&self.initrd);
        self.root = self.root.as_deref().map(fix);
        self.cmdline_file = self.cmdline_file.as_deref().map(fix);
        self.agent_overlay_path = self.agent_overlay_path.as_deref().map(fix);
    }

    /// The directory a profile's `build` command runs in: its own
    /// directory when it was discovered, else provium's cwd.
    pub fn build_dir(&self) -> Option<&Path> {
        self.dir.as_deref()
    }

    /// The resolved `{out}` build-output directory for this profile.
    /// `build_out` when set, else `<default_build_base>/<profile_name>`.
    pub fn out_dir(&self, profile_name: &str) -> PathBuf {
        self.build_out
            .clone()
            .unwrap_or_else(|| default_build_base().join(profile_name))
    }
}

/// The file that makes a directory a profile.
const PROFILE_FILE: &str = "profile.provium.toml";

/// Read every profile under `dir`.
///
/// A subdirectory holding a [`PROFILE_FILE`] is a profile, named after
/// the directory; a subdirectory without one is not, and is skipped
/// rather than rejected, so a profile directory may keep whatever else
/// it needs beside its config. Ordering is by name, so a run is the
/// same whatever order the filesystem hands entries back in.
///
/// Everything relative inside a discovered profile resolves against
/// that profile's own directory rather than provium's cwd — the paths
/// it names, and the directory its `build` runs in. That is what makes
/// a testset one self-contained directory: it can be moved, copied or
/// renamed and still mean what it says.
fn discover_profiles(
    dir: &Path,
    config_path: &Path,
) -> Result<Vec<(String, Profile)>, ConfigError> {
    let entries = fs::read_dir(dir).map_err(|source| ConfigError::Io {
        path: dir.into(),
        source,
    })?;
    let mut found: Vec<(String, Profile)> = Vec::new();
    for entry in entries.flatten() {
        let profile_dir = entry.path();
        let file = profile_dir.join(PROFILE_FILE);
        if !file.is_file() {
            continue;
        }
        let Some(name) = profile_dir.file_name().map(|n| n.to_string_lossy().into_owned()) else {
            continue;
        };
        let raw = fs::read_to_string(&file).map_err(|source| ConfigError::Io {
            path: file.clone(),
            source,
        })?;
        let mut profile: Profile = toml::from_str(&raw).map_err(|source| ConfigError::Parse {
            path: file.clone(),
            source,
        })?;
        profile.dir = Some(profile_dir.clone());
        profile.rebase(&profile_dir);
        found.push((name, profile));
    }
    if found.is_empty() {
        return Err(ConfigError::Validation {
            path: config_path.into(),
            message: format!(
                "`{}` holds no profile: a profile is a directory containing a {PROFILE_FILE}",
                dir.display()
            ),
        });
    }
    found.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(found)
}

/// Why [`Profile::resolve_kernel`] could not name a kernel.
#[derive(Debug, Error)]
pub enum KernelError {
    /// The profile sets neither `kernel` nor `root`.
    #[error("profile sets neither `kernel` nor `root`, so there is no kernel to boot")]
    NoSource,

    /// The root's modules directory could not be read — usually
    /// because the build did not produce the root it promised.
    #[error("read `{path}`: {source}")]
    Read {
        /// Directory whose read failed.
        path: PathBuf,
        /// Underlying I/O error.
        #[source]
        source: std::io::Error,
    },

    /// The root exists but holds no kernel.
    #[error("no kernel at usr/lib/modules/<release>/vmlinuz-* in root `{root}`")]
    NotFound {
        /// The root that was searched.
        root: PathBuf,
    },

    /// More than one kernel: the profile must say which.
    #[error("root `{root}` holds {} kernels ({}); set `kernel` to choose one", found.len(), found.iter().map(|p| p.display().to_string()).collect::<Vec<_>>().join(", "))]
    Ambiguous {
        /// The root that was searched.
        root: PathBuf,
        /// Every kernel found in it.
        found: Vec<PathBuf>,
    },
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

    /// Parse + validate a config from in-memory TOML. `path` names the
    /// config's own location: it is used for error messages, and — when
    /// `[profiles] from_dir` is set — as the directory discovery
    /// resolves against. A config with no `from_dir` never touches the
    /// filesystem here.
    pub fn from_toml_str(raw: &str, path: &Path) -> Result<Self, ConfigError> {
        let raw: RawConfig = toml::from_str(raw).map_err(|e| ConfigError::Parse {
            path: path.into(),
            source: e,
        })?;
        let base = path.parent().unwrap_or_else(|| Path::new("."));
        let mut config = Config {
            provium: raw.provium,
            profiles: raw.profiles.inline,
        };
        if let Some(from_dir) = &raw.profiles.from_dir {
            let dir = if from_dir.is_absolute() {
                from_dir.clone()
            } else {
                base.join(from_dir)
            };
            for (name, profile) in discover_profiles(&dir, path)? {
                if config.profiles.contains_key(&name) {
                    return Err(ConfigError::Validation {
                        path: path.into(),
                        message: format!(
                            "profile `{name}` is both written inline and discovered                              in `{}`; rename one",
                            dir.display()
                        ),
                    });
                }
                config.profiles.insert(name, profile);
            }
        }
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
            let new_root = profile
                .root
                .as_deref()
                .map(|r| PathBuf::from(sub(&r.to_string_lossy())));
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
            profile.root = new_root;
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
            if let Some(t) = profile.agent_boot_timeout {
                if !(t.is_finite() && t > 0.0) {
                    return Err(ConfigError::Validation {
                        path: path.into(),
                        message: format!(
                            "profile `{name}`: agent_boot_timeout must be a positive \
                             number of seconds, got {t}"
                        ),
                    });
                }
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
            root: None,
            cmdline: cmdline.to_string(),
            cmdline_file,
            guest_os: "peios".into(),
            inject_agent: true,
            agent_overlay_path: None,
            agent_boot_timeout: None,
            build: None,
            build_out: None,
            dir: None,
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
        fs::write(&file, "console=ttyS0 loglevel=7\n  init=/bin/protoinit \n").unwrap();
        let p = profile_with("", Some(file));
        assert_eq!(
            p.resolve_cmdline().unwrap(),
            "console=ttyS0 loglevel=7 init=/bin/protoinit"
        );
    }

    #[test]
    fn resolve_cmdline_composes_file_then_inline() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("cmdline");
        fs::write(&file, "console=ttyS0 init=/bin/protoinit").unwrap();
        // Inline appended after the file: inline wins for last-wins
        // params (here a second console + an override of init=).
        let p = profile_with("console=hvc0 init=/sbin/other", Some(file));
        assert_eq!(
            p.resolve_cmdline().unwrap(),
            "console=ttyS0 init=/bin/protoinit console=hvc0 init=/sbin/other"
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
kernel       = "{out}/root/usr/lib/modules/7.0.9-peios/vmlinuz-7.0.9-peios"
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
            PathBuf::from("/custom/build/peios/root/usr/lib/modules/7.0.9-peios/vmlinuz-7.0.9-peios")
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
kernel  = "../peiso/out/root/usr/lib/modules/7.0.9-peios/vmlinuz-7.0.9-peios"
initrd  = "../peiso/out/initrd.img"
cmdline = "console=ttyS0"
"#;
        let mut cfg = Config::from_toml_str(raw, &fake_path()).unwrap();
        cfg.expand_build_outputs();
        let p = cfg.profile("peios").unwrap();
        // A static profile is untouched.
        assert_eq!(p.kernel, PathBuf::from("../peiso/out/root/usr/lib/modules/7.0.9-peios/vmlinuz-7.0.9-peios"));
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

    // -- Profile discovery ------------------------------------------------

    /// A `provium.toml` at `<tmp>/provium.toml` plus a profiles dir.
    fn discovery_tree() -> tempfile::TempDir {
        let tmp = tempfile::tempdir().unwrap();
        let profiles = tmp.path().join("profiles");
        for name in ["kernel-only", "full"] {
            std::fs::create_dir_all(profiles.join(name)).unwrap();
            std::fs::write(
                profiles.join(name).join(PROFILE_FILE),
                format!("build = \"echo {name}\"\nroot = \"{{out}}/root\"\ncmdline = \"console=hvc0\"\n"),
            )
            .unwrap();
        }
        // A directory that is not a profile: skipped, not rejected.
        std::fs::create_dir_all(profiles.join("shared-lua")).unwrap();
        tmp
    }

    #[test]
    fn profiles_are_discovered_from_a_directory() {
        let tmp = discovery_tree();
        let cfg = Config::from_toml_str(
            "[provium]\nroots = [\"tests\"]\n\n[profiles]\nfrom_dir = \"profiles\"\n",
            &tmp.path().join("provium.toml"),
        )
        .expect("discovery");
        let names: Vec<&str> = cfg.profiles.keys().map(String::as_str).collect();
        assert_eq!(names, vec!["full", "kernel-only"], "named after their dirs");
        let ko = cfg.profile("kernel-only").unwrap();
        assert_eq!(ko.build.as_deref(), Some("echo kernel-only"));
        assert_eq!(ko.dir.as_deref(), Some(tmp.path().join("profiles/kernel-only").as_path()));
    }

    #[test]
    fn discovered_profiles_may_sit_beside_inline_ones() {
        let tmp = discovery_tree();
        let cfg = Config::from_toml_str(
            "[profiles]\nfrom_dir = \"profiles\"\n\n[profiles.inline]\n\
             kernel = \"/k\"\ninitrd = \"/i\"\ncmdline = \"console=hvc0\"\n",
            &tmp.path().join("provium.toml"),
        )
        .expect("both shapes");
        assert_eq!(cfg.profiles.len(), 3);
        // An inline profile keeps resolving against provium's cwd.
        assert!(cfg.profile("inline").unwrap().dir.is_none());
    }

    #[test]
    fn a_name_cannot_be_both_inline_and_discovered() {
        let tmp = discovery_tree();
        let err = Config::from_toml_str(
            "[profiles]\nfrom_dir = \"profiles\"\n\n[profiles.full]\n\
             kernel = \"/k\"\ninitrd = \"/i\"\ncmdline = \"console=hvc0\"\n",
            &tmp.path().join("provium.toml"),
        )
        .unwrap_err();
        assert!(
            format!("{err}").contains("both written inline and discovered"),
            "{err}"
        );
    }

    /// The rule that makes a profile directory movable: what it names
    /// is relative to itself, not to wherever provium was run.
    #[test]
    fn discovered_paths_resolve_against_the_profile_directory() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("profiles").join("vendored");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join(PROFILE_FILE),
            "kernel = \"boot/vmlinuz\"\ninitrd = \"boot/initrd.cpio.gz\"\n\
             cmdline_file = \"cmdline\"\ncmdline = \"console=hvc0\"\n",
        )
        .unwrap();
        let cfg = Config::from_toml_str(
            "[profiles]\nfrom_dir = \"profiles\"\n",
            &tmp.path().join("provium.toml"),
        )
        .unwrap();
        let p = cfg.profile("vendored").unwrap();
        assert_eq!(p.kernel, dir.join("boot/vmlinuz"));
        assert_eq!(p.initrd, dir.join("boot/initrd.cpio.gz"));
        assert_eq!(p.cmdline_file.as_deref(), Some(dir.join("cmdline").as_path()));
    }

    /// `{out}` is an absolute build directory, so it must survive
    /// rebasing untouched and still expand afterwards.
    #[test]
    fn out_token_survives_rebasing() {
        let tmp = discovery_tree();
        let mut cfg = Config::from_toml_str(
            "[profiles]\nfrom_dir = \"profiles\"\n",
            &tmp.path().join("provium.toml"),
        )
        .unwrap();
        cfg.expand_build_outputs();
        let root = cfg.profile("kernel-only").unwrap().root.clone().unwrap();
        assert!(root.is_absolute(), "{}", root.display());
        assert!(!root.to_string_lossy().contains("{out}"), "{}", root.display());
        assert!(root.ends_with("kernel-only/root"), "{}", root.display());
    }

    #[test]
    fn a_from_dir_with_no_profiles_is_an_error() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join("profiles").join("notaprofile")).unwrap();
        let err = Config::from_toml_str(
            "[profiles]\nfrom_dir = \"profiles\"\n",
            &tmp.path().join("provium.toml"),
        )
        .unwrap_err();
        assert!(format!("{err}").contains("holds no profile"), "{err}");
    }

    // -- Kernel discovery -------------------------------------------------

    fn kernel_profile(kernel: &str, root: Option<PathBuf>) -> Profile {
        let mut p = profile_with("console=hvc0", None);
        p.kernel = PathBuf::from(kernel);
        p.root = root;
        p
    }

    /// Lay out a composed root the way peiso writes one.
    fn composed_root(releases: &[&str]) -> tempfile::TempDir {
        let tmp = tempfile::tempdir().unwrap();
        for release in releases {
            let dir = tmp.path().join("usr/lib/modules").join(release);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join(format!("vmlinuz-{release}")), b"kernel").unwrap();
        }
        tmp
    }

    #[test]
    fn an_explicit_kernel_wins_over_a_root() {
        let root = composed_root(&["7.0.9-peios"]);
        let p = kernel_profile("/explicit/vmlinuz", Some(root.path().to_path_buf()));
        assert_eq!(p.resolve_kernel().unwrap(), PathBuf::from("/explicit/vmlinuz"));
    }

    #[test]
    fn a_kernel_is_found_inside_a_composed_root() {
        let root = composed_root(&["7.0.9-peios-0.20.1-rc12"]);
        let p = kernel_profile("", Some(root.path().to_path_buf()));
        assert_eq!(
            p.resolve_kernel().unwrap(),
            root.path()
                .join("usr/lib/modules/7.0.9-peios-0.20.1-rc12/vmlinuz-7.0.9-peios-0.20.1-rc12"),
        );
    }

    #[test]
    fn neither_kernel_nor_root_is_an_error() {
        let p = kernel_profile("", None);
        assert!(matches!(p.resolve_kernel(), Err(KernelError::NoSource)));
    }

    #[test]
    fn a_root_with_no_kernel_is_an_error() {
        let root = composed_root(&[]);
        std::fs::create_dir_all(root.path().join("usr/lib/modules")).unwrap();
        let p = kernel_profile("", Some(root.path().to_path_buf()));
        assert!(matches!(p.resolve_kernel(), Err(KernelError::NotFound { .. })));
    }

    /// Picking one of two silently would make a conformance result
    /// unattributable, so it is refused instead.
    #[test]
    fn two_kernels_in_one_root_is_an_error() {
        let root = composed_root(&["7.0.9-peios", "7.0.10-peios"]);
        let p = kernel_profile("", Some(root.path().to_path_buf()));
        match p.resolve_kernel() {
            Err(KernelError::Ambiguous { found, .. }) => assert_eq!(found.len(), 2),
            other => panic!("expected Ambiguous, got {other:?}"),
        }
    }
}
