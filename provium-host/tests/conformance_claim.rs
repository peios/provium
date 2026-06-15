//! Conformance: provium:claim per `DESIGN.md` § Resource model.

#![cfg(feature = "lua")]

mod common;

use common::{assert_one_passed, run_local_lua};

#[test]
fn claim_with_no_pool_succeeds_silently() {
    // run_local_lua wires no pool; `claim` should still succeed
    // per the binding's "no-pool fast path" comment.
    let outcome = run_local_lua(
        r#"
test("claim no-pool", function(t)
    provium:claim({memory_bytes = 1024 * 1024, cpus = 1})
end)
"#,
    );
    assert_one_passed(&outcome);
}

#[test]
fn claim_twice_errors_one_shot() {
    let outcome = run_local_lua(
        r#"
test("claim twice", function(t)
    provium:claim({memory_bytes = 1024, cpus = 1})
    local ok, err = pcall(function()
        provium:claim({memory_bytes = 1024, cpus = 1})
    end)
    t:assert(not ok, "second claim must error (one-shot per file)")
end)
"#,
    );
    assert_one_passed(&outcome);
}

#[test]
fn claim_zero_resources_is_valid() {
    let outcome = run_local_lua(
        r#"
test("claim zero", function(t)
    -- Documented edge case: zero claim is a no-op observation.
    provium:claim({memory_bytes = 0, cpus = 0})
end)
"#,
    );
    assert_one_passed(&outcome);
}

#[test]
fn claim_missing_keys_uses_zero_defaults() {
    let outcome = run_local_lua(
        r#"
test("claim partial", function(t)
    -- cpus omitted — ResourceAmount fields default to 0.
    provium:claim({memory_bytes = 1024})
end)
"#,
    );
    assert_one_passed(&outcome);
}
