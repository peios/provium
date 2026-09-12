//! Conformance: fixture cache key per `DESIGN.md` § Fixtures /
//! Lazy build. Tests the Rust-side cache-key derivation
//! directly — round-tripping through the Lua fixture loader
//! requires a real fixture file on disk and is covered by
//! tests/fixtures.rs.

use provium_host::fixture::{canonical_profile_paths_all, compute_key,
    compute_key_with_deps, compute_key_with_deps_and_kernel,
    compute_key_with_deps_and_kernels};
use provium_host::profile::{Config, Profile, ProviumSection};

#[test]
fn cache_key_changes_with_source() {
    let k1 = compute_key(b"return 1");
    let k2 = compute_key(b"return 2");
    assert_ne!(k1.as_str(), k2.as_str(),
        "different sources must hash differently");
}

#[test]
fn cache_key_is_deterministic() {
    let k1 = compute_key(b"return 1");
    let k2 = compute_key(b"return 1");
    assert_eq!(k1.as_str(), k2.as_str(),
        "same source must hash same way across calls");
}

#[test]
fn cache_key_changes_with_dep_keys() {
    let k_no_deps = compute_key_with_deps(b"return 1", &[]);
    let k_with_dep = compute_key_with_deps(b"return 1", &[compute_key(b"helper")]);
    assert_ne!(
        k_no_deps.as_str(),
        k_with_dep.as_str(),
        "dep folding must affect the digest",
    );
}

#[test]
fn cache_key_changes_with_kernel_path() {
    use std::path::Path;
    let k_no_kernel = compute_key_with_deps_and_kernel(b"return 1", &[], None, None);
    let k_with_kernel = compute_key_with_deps_and_kernel(
        b"return 1",
        &[],
        Some(Path::new("/dev/null")),
        None,
    );
    assert_ne!(
        k_no_kernel.as_str(),
        k_with_kernel.as_str(),
        "kernel identifier must affect the digest",
    );
}

#[test]
fn cache_key_dep_order_independent() {
    // R8-derived: deps must hash sorted-equivalent so reordering
    // a fixture's `requires` list doesn't invalidate the cache.
    let a = compute_key(b"a");
    let b = compute_key(b"b");
    let k_ab = compute_key_with_deps(b"x", &[a.clone(), b.clone()]);
    let k_ba = compute_key_with_deps(b"x", &[b, a]);
    // CURRENT BEHAVIOUR: dep order does affect the key. This
    // test documents that — flip to assert_ne! → assert_eq!
    // if/when sorting is added (and rebuild every cached
    // fixture once at the cutover).
    assert_ne!(k_ab.as_str(), k_ba.as_str(),
        "dep order is currently load-bearing — see fixture.rs comment");
}

// ---------------------------------------------------------------------------
// Multi-profile folding — PEI-228
// ---------------------------------------------------------------------------

fn profile(kernel: &str, initrd: &str) -> Profile {
    Profile {
        kernel: kernel.into(),
        initrd: initrd.into(),
        root: None,
        cmdline: "console=hvc0".into(),
        guest_os: "peios".into(),
        inject_agent: true,
        agent_overlay_path: None,
        agent_boot_timeout: None,
        disks: Vec::new(),
        cmdline_file: None,
        build: None,
        build_out: None,
        dir: None,
    }
}

fn config_with(profiles: &[(&str, &str, &str)]) -> Config {
    let mut map = std::collections::BTreeMap::new();
    for (name, kernel, initrd) in profiles {
        map.insert((*name).into(), profile(kernel, initrd));
    }
    Config {
        provium: ProviumSection {
            roots: vec!["tests".into()],
            cache_dir: None,
            cache_max_size: None,
        },
        profiles: map,
    }
}

#[test]
fn canonical_paths_cover_every_profile_in_name_order() {
    // One helper answers "which images does this config's key fold
    // in?" for the runner, the `fixture` subcommands and the REPL,
    // so all three address the same cache entry.
    let config = config_with(&[
        ("prelude", "/img/prelude/kernel", "/img/prelude/initrd"),
        ("kernel-only", "/img/ko/kernel", "/img/ko/initrd"),
    ]);
    let (kernels, initrds) = canonical_profile_paths_all(&config);
    assert_eq!(
        kernels,
        vec![
            std::path::PathBuf::from("/img/ko/kernel"),
            std::path::PathBuf::from("/img/prelude/kernel"),
        ],
        "every profile's kernel, in profile-name order",
    );
    assert_eq!(
        initrds,
        vec![
            std::path::PathBuf::from("/img/ko/initrd"),
            std::path::PathBuf::from("/img/prelude/initrd"),
        ],
        "every profile's initrd, in profile-name order",
    );
}

#[test]
fn cache_key_changes_with_a_later_profiles_kernel() {
    // PEI-228: the key must react to a kernel swap on a profile that
    // is not the lex-first one. Folding only the first profile made
    // these two configs share a key, so `provium fixture build` on a
    // multi-profile config pre-warmed an entry the runner never read.
    let a = config_with(&[
        ("aaa", "/img/a/kernel", "/img/a/initrd"),
        ("zzz", "/img/z/kernel", "/img/z/initrd"),
    ]);
    let b = config_with(&[
        ("aaa", "/img/a/kernel", "/img/a/initrd"),
        ("zzz", "/img/z2/kernel", "/img/z2/initrd"),
    ]);

    let key_of = |config: &Config| {
        let (kernels, initrds) = canonical_profile_paths_all(config);
        let kernel_refs: Vec<&std::path::Path> =
            kernels.iter().map(|p| p.as_path()).collect();
        let initrd_refs: Vec<&std::path::Path> =
            initrds.iter().map(|p| p.as_path()).collect();
        compute_key_with_deps_and_kernels(
            b"return 1",
            &[],
            &kernel_refs,
            &initrd_refs,
        )
    };

    assert_ne!(
        key_of(&a).as_str(),
        key_of(&b).as_str(),
        "a kernel swap on the second profile must invalidate the cache",
    );
}

#[test]
fn cache_key_is_stable_across_profile_insertion_order() {
    // The config is a name-keyed map, so the order the profiles were
    // written in must not reach the digest — otherwise two machines
    // reading the same provium.toml could disagree about the key.
    let forwards = config_with(&[
        ("aaa", "/img/a/kernel", "/img/a/initrd"),
        ("zzz", "/img/z/kernel", "/img/z/initrd"),
    ]);
    let backwards = config_with(&[
        ("zzz", "/img/z/kernel", "/img/z/initrd"),
        ("aaa", "/img/a/kernel", "/img/a/initrd"),
    ]);
    assert_eq!(
        canonical_profile_paths_all(&forwards),
        canonical_profile_paths_all(&backwards),
    );
}
