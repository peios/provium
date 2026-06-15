//! Conformance: Config / Profile loader per `DESIGN.md` §
//! Profiles.

use std::path::Path;

use provium_host::profile::{Config, ConfigError};

#[test]
fn empty_config_parses_to_no_profiles() {
    let cfg = Config::from_toml_str("", Path::new("test.toml")).unwrap();
    assert!(cfg.profiles.is_empty());
}

#[test]
fn minimal_profile_round_trips() {
    let toml = r#"
[profiles.peios]
kernel = "/boot/vmlinuz"
initrd = "/boot/initrd"
cmdline = "console=hvc0"
"#;
    let cfg = Config::from_toml_str(toml, Path::new("test.toml")).unwrap();
    let p = cfg.profile("peios").expect("profile present");
    assert_eq!(p.kernel.to_str().unwrap(), "/boot/vmlinuz");
    assert_eq!(p.initrd.to_str().unwrap(), "/boot/initrd");
    assert_eq!(p.cmdline, "console=hvc0");
}

#[test]
fn empty_cmdline_fails_validation() {
    let toml = r#"
[profiles.peios]
kernel = "/k"
initrd = "/i"
cmdline = ""
"#;
    match Config::from_toml_str(toml, Path::new("test.toml")) {
        Err(ConfigError::Validation { message, .. }) => {
            assert!(message.contains("cmdline"),
                "validation error must mention cmdline: {message}");
        }
        other => panic!("expected Validation error, got {other:?}"),
    }
}

#[test]
fn missing_kernel_fails_parse() {
    let toml = r#"
[profiles.peios]
initrd = "/i"
cmdline = "x"
"#;
    let r = Config::from_toml_str(toml, Path::new("test.toml"));
    assert!(matches!(r, Err(ConfigError::Parse { .. })),
        "missing kernel must fail at parse time: {r:?}");
}

#[test]
fn missing_file_returns_io_error() {
    match Config::load("/no/such/provium.toml") {
        Err(ConfigError::Io { path, .. }) => {
            assert_eq!(path.to_str().unwrap(), "/no/such/provium.toml");
        }
        other => panic!("expected Io, got {other:?}"),
    }
}

#[test]
fn parse_error_carries_file_context() {
    let toml = "this is not toml = !!! [";
    let r = Config::from_toml_str(toml, Path::new("bad.toml"));
    match r {
        Err(ConfigError::Parse { path, .. }) => {
            assert_eq!(path.to_str().unwrap(), "bad.toml");
        }
        other => panic!("expected Parse, got {other:?}"),
    }
}

#[test]
fn guest_os_defaults_to_peios() {
    // The default function in profile.rs returns "peios" when
    // the field is omitted.
    let toml = r#"
[profiles.x]
kernel = "/k"
initrd = "/i"
cmdline = "console=hvc0"
"#;
    let cfg = Config::from_toml_str(toml, Path::new("t.toml")).unwrap();
    assert_eq!(cfg.profile("x").unwrap().guest_os, "peios");
}

#[test]
fn provium_section_defaults_when_omitted() {
    let toml = r#"
[profiles.x]
kernel = "/k"
initrd = "/i"
cmdline = "console=hvc0"
"#;
    let cfg = Config::from_toml_str(toml, Path::new("t.toml")).unwrap();
    assert!(cfg.provium.roots.is_empty());
    assert!(cfg.provium.cache_dir.is_none());
}

#[test]
fn multiple_profiles_round_trip() {
    let toml = r#"
[profiles.alpha]
kernel = "/a/k"
initrd = "/a/i"
cmdline = "x"

[profiles.beta]
kernel = "/b/k"
initrd = "/b/i"
cmdline = "y"
"#;
    let cfg = Config::from_toml_str(toml, Path::new("t.toml")).unwrap();
    assert_eq!(cfg.profiles.len(), 2);
    assert_eq!(cfg.profile("alpha").unwrap().cmdline, "x");
    assert_eq!(cfg.profile("beta").unwrap().cmdline, "y");
}

#[test]
fn provium_section_roots_round_trip() {
    let toml = r#"
[provium]
roots = ["tests", "more-tests"]

[profiles.x]
kernel = "/k"
initrd = "/i"
cmdline = "x"
"#;
    let cfg = Config::from_toml_str(toml, Path::new("t.toml")).unwrap();
    assert_eq!(cfg.provium.roots, vec!["tests".to_string(), "more-tests".to_string()]);
}

#[test]
fn unknown_top_level_section_does_not_error() {
    // Forward-compat: schema additions in newer provium versions
    // shouldn't break older configs reading them. TOML parser
    // ignores unknown fields by default.
    let toml = r#"
[provium]
roots = ["tests"]

[future_section]
new_key = 42

[profiles.x]
kernel = "/k"
initrd = "/i"
cmdline = "x"
"#;
    let r = Config::from_toml_str(toml, Path::new("t.toml"));
    assert!(r.is_ok(), "unknown sections should not error: {r:?}");
}
