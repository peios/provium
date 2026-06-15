//! Conformance: test-level metadata per `DESIGN.md` § Test-level
//! metadata. The 3-arg `test(name, meta, fn)` form attaches an
//! arbitrary table that's surfaced on result events; runner-
//! interpreted keys are `slow`, `tags`, `timeout`.

#![cfg(feature = "lua")]

mod common;

use common::{run_local_lua};
use provium_host::lua::TestStatus;

#[test]
fn meta_passthrough_does_not_break_test() {
    let outcome = run_local_lua(
        r#"
test("with meta", {spec = "PSD-X §1.2"}, function(t)
    t:assert(true)
end)
"#,
    );
    assert_eq!(outcome.tests.len(), 1);
    assert_eq!(outcome.tests[0].status, TestStatus::Passed);
}

#[test]
fn slow_tag_skips_unless_include_slow() {
    // DESIGN: `slow=true` skipped unless --include-slow. Local
    // runner default has --include-slow off → must skip.
    let outcome = run_local_lua(
        r#"
test("slow body", {slow = true}, function(t)
    t:assert(false, "this should not run")
end)
"#,
    );
    assert_eq!(outcome.tests.len(), 1);
    assert_eq!(outcome.tests[0].status, TestStatus::Skipped,
        "slow=true must skip by default");
}

#[test]
fn meta_skip_true_skips() {
    let outcome = run_local_lua(
        r#"
test("explicit skip", {skip = "broken on macOS"}, function(t)
    t:assert(false)
end)
"#,
    );
    assert_eq!(outcome.tests.len(), 1);
    assert_eq!(outcome.tests[0].status, TestStatus::Skipped);
}

#[test]
fn meta_unknown_keys_pass_through_silently() {
    let outcome = run_local_lua(
        r#"
test("unknown meta", {custom_key = "value", other = 42}, function(t)
    t:assert(true)
end)
"#,
    );
    assert_eq!(outcome.tests.len(), 1);
    assert_eq!(outcome.tests[0].status, TestStatus::Passed);
}

#[test]
fn meta_tags_table_accepted() {
    let outcome = run_local_lua(
        r#"
test("tagged", {tags = {"unit", "fast"}}, function(t)
    t:assert(true)
end)
"#,
    );
    assert_eq!(outcome.tests[0].status, TestStatus::Passed);
}

#[test]
fn three_arg_form_with_no_meta_keys_runs() {
    // Empty meta table is valid.
    let outcome = run_local_lua(
        r#"
test("empty meta", {}, function(t)
    t:assert(true)
end)
"#,
    );
    assert_eq!(outcome.tests[0].status, TestStatus::Passed);
}
