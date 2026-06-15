//! Dynamic-profile build hook.
//!
//! A profile may declare a `build` command (see [`crate::profile::Profile::build`]).
//! When the profile is used — `provium test`, `console`, `repl`, or the
//! explicit `provium prepare` — provium runs that command **once**, before
//! any VM boots, so the suite tests a freshly-built image instead of
//! whatever stale artifacts happen to be on disk.
//!
//! provium deliberately holds **no** staleness logic: it runs the command
//! every invocation and trusts the builder to be cheap when nothing
//! changed (incremental rebuilds are the builder's job). `--no-build`
//! skips the hook for runs where you know the artifacts are current.
//!
//! Contracts:
//! * The command runs with `sh -c` from provium's cwd, inheriting stdio
//!   (build output streams straight to the terminal) and environment.
//! * A non-zero exit is fatal — the caller aborts the run rather than
//!   booting whatever stale outputs remain on disk.
//! * The profile passed in must already be `{out}`-expanded (see
//!   [`crate::profile::Config::expand_build_outputs`]), so the command's
//!   `--out` and the path fields the VM later reads point at the same
//!   directory.

use std::process::Command;

use thiserror::Error;

use crate::profile::{Config, Profile};

/// Failure modes of [`run_build`].
#[derive(Debug, Error)]
pub enum BuildError {
    /// Could not create the profile's `{out}` directory.
    #[error("profile `{profile}`: create build output dir `{dir}`: {source}")]
    OutDir {
        /// Profile whose build was running.
        profile: String,
        /// The output directory we failed to create.
        dir: std::path::PathBuf,
        /// Underlying I/O error.
        #[source]
        source: std::io::Error,
    },
    /// The `sh -c` process could not be spawned at all (e.g. no `sh`).
    #[error("profile `{profile}`: spawn build command: {source}")]
    Spawn {
        /// Profile whose build was running.
        profile: String,
        /// Underlying I/O error.
        #[source]
        source: std::io::Error,
    },
    /// The build command ran but exited non-zero (or was signalled).
    #[error("profile `{profile}`: build command failed ({status})")]
    Failed {
        /// Profile whose build failed.
        profile: String,
        /// `exit code N` or `killed by signal`.
        status: String,
    },
}

/// Run `profile.build` (if any) with `sh -c`, streaming its output to the
/// inherited stdio. No-op when the profile declares no build command.
///
/// `profile` must already be `{out}`-expanded. The profile's
/// [`Profile::out_dir`] is created first so the command's `--out` has
/// somewhere to write; provium never wipes it.
pub fn run_build(profile_name: &str, profile: &Profile) -> Result<(), BuildError> {
    let Some(cmd) = profile.build.as_deref() else {
        return Ok(());
    };

    let out = profile.out_dir(profile_name);
    std::fs::create_dir_all(&out).map_err(|e| BuildError::OutDir {
        profile: profile_name.to_string(),
        dir: out.clone(),
        source: e,
    })?;

    eprintln!(
        "provium: building profile `{profile_name}` → {}",
        out.display()
    );

    let status = Command::new("sh")
        .arg("-c")
        .arg(cmd)
        .status()
        .map_err(|e| BuildError::Spawn {
            profile: profile_name.to_string(),
            source: e,
        })?;

    if !status.success() {
        return Err(BuildError::Failed {
            profile: profile_name.to_string(),
            status: status
                .code()
                .map(|c| format!("exit code {c}"))
                .unwrap_or_else(|| "killed by signal".into()),
        });
    }
    Ok(())
}

/// Run the build command of every profile that declares one. Used by the
/// test runner (which can't statically know which profiles a Lua run will
/// boot) and by `provium prepare` with no profile argument. Stops at the
/// first failure.
pub fn run_all_builds(config: &Config) -> Result<(), BuildError> {
    for (name, profile) in &config.profiles {
        run_build(name, profile)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn profile(build: Option<&str>, build_out: Option<PathBuf>) -> Profile {
        Profile {
            kernel: PathBuf::from("/k"),
            initrd: PathBuf::from("/i"),
            cmdline: "console=ttyS0".into(),
            cmdline_file: None,
            guest_os: "peios".into(),
            inject_agent: true,
            agent_overlay_path: None,
            build: build.map(str::to_owned),
            build_out,
        }
    }

    #[test]
    fn no_build_command_is_a_noop() {
        let p = profile(None, None);
        run_build("peios", &p).expect("no build = ok");
    }

    #[test]
    fn successful_command_writes_into_out_dir() {
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("peios");
        // The command can rely on {out} already existing.
        let p = profile(Some("touch \"$PWD/marker\" && test -d ."), Some(out.clone()));
        run_build("peios", &p).expect("command succeeds");
        assert!(out.exists(), "out_dir was created before the build ran");
    }

    #[test]
    fn nonzero_exit_is_fatal() {
        let dir = tempfile::tempdir().unwrap();
        let p = profile(Some("exit 3"), Some(dir.path().join("peios")));
        let err = run_build("peios", &p).unwrap_err();
        match err {
            BuildError::Failed { status, profile } => {
                assert_eq!(profile, "peios");
                assert!(status.contains("3"), "carries exit code: {status}");
            }
            other => panic!("expected Failed, got {other:?}"),
        }
    }

    #[test]
    fn run_all_builds_skips_profiles_without_a_command() {
        // A config with one static + one dynamic profile: only the
        // dynamic one runs, and a no-op static profile doesn't error.
        let dir = tempfile::tempdir().unwrap();
        let mut config = Config::default();
        config
            .profiles
            .insert("static".into(), profile(None, None));
        config.profiles.insert(
            "dynamic".into(),
            profile(Some("true"), Some(dir.path().join("dynamic"))),
        );
        run_all_builds(&config).expect("all builds ok");
    }
}
