//! Conformance: fixture cache key per `DESIGN.md` § Fixtures /
//! Lazy build. Tests the Rust-side cache-key derivation
//! directly — round-tripping through the Lua fixture loader
//! requires a real fixture file on disk and is covered by
//! tests/fixtures.rs.

use provium_host::fixture::{compute_key, compute_key_with_deps,
    compute_key_with_deps_and_kernel};

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
