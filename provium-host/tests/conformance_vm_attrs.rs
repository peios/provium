//! Conformance: VM accessor methods per `DESIGN.md` § VM. The
//! documented surface includes name/state/profile/cid; missing
//! getters are flagged so they can be added (the test names
//! pinned here document the contract regardless of impl).

#![cfg(feature = "lua")]

mod common;

use common::{assert_one_passed, run_local_lua};

#[test]
fn name_returns_declared_name() {
    let outcome = run_local_lua(
        r#"
test("vm:name", function(t)
    local vm = provium:vm("web", "peios")
    t:assert_eq(vm:name(), "web")
end)
"#,
    );
    assert_one_passed(&outcome);
}

#[test]
fn profile_returns_declared_profile() {
    let outcome = run_local_lua(
        r#"
test("vm:profile", function(t)
    local vm = provium:vm("a", "peios")
    t:assert_eq(vm:profile(), "peios")
end)
"#,
    );
    assert_one_passed(&outcome);
}

#[test]
fn state_reflects_lifecycle() {
    let outcome = run_local_lua(
        r#"
test("vm:state lifecycle", function(t)
    local vm = provium:vm("a", "peios")
    t:assert_eq(vm:state(), "created")
    vm:boot()
    t:assert_eq(vm:state(), "booted")
end)
"#,
    );
    assert_one_passed(&outcome);
}

#[test]
fn cid_is_nil_before_boot() {
    let outcome = run_local_lua(
        r#"
test("cid pre-boot", function(t)
    local vm = provium:vm("a", "peios")
    -- cid only exists post-boot. Either nil or unset.
    local cid = vm:cid()
    t:assert(cid == nil, "pre-boot cid must be nil, got " .. tostring(cid))
end)
"#,
    );
    assert_one_passed(&outcome);
}

#[test]
fn cid_is_positive_after_boot() {
    let outcome = run_local_lua(
        r#"
test("cid post-boot", function(t)
    local vm = provium:vm("a", "peios"):boot()
    local cid = vm:cid()
    t:assert(cid ~= nil and cid > 0,
        "post-boot cid must be positive, got " .. tostring(cid))
end)
"#,
    );
    assert_one_passed(&outcome);
}

#[test]
fn vm_tostring_carries_state_and_cid() {
    // The __tostring metamethod is what `print(vm)` and
    // `tostring(vm)` produce — must include state for diagnostic.
    let outcome = run_local_lua(
        r#"
test("vm tostring", function(t)
    local vm = provium:vm("named", "peios"):boot()
    local s = tostring(vm)
    t:assert(s:find("named"), "tostring must include name: " .. s)
    t:assert(s:find("booted"), "tostring must include state: " .. s)
end)
"#,
    );
    assert_one_passed(&outcome);
}

#[test]
fn open_file_count_starts_at_zero() {
    let outcome = run_local_lua(
        r#"
test("open_file_count zero", function(t)
    local vm = provium:vm("a", "peios"):boot()
    t:assert_eq(vm:open_file_count(), 0)
end)
"#,
    );
    assert_one_passed(&outcome);
}

#[test]
fn open_file_count_increments_then_decrements() {
    let outcome = run_local_lua(
        r#"
test("open_file_count tracking", function(t)
    local tmp = os.tmpname()
    io.open(tmp, "w"):close()
    local vm = provium:vm("a", "peios"):boot()
    local h = vm:open_file(tmp, {read=true})
    t:assert_eq(vm:open_file_count(), 1)
    h:close()
    t:assert_eq(vm:open_file_count(), 0,
        "count must drop after close")
end)
"#,
    );
    assert_one_passed(&outcome);
}

#[test]
fn open_stream_count_tracks_tail() {
    let outcome = run_local_lua(
        r#"
test("open_stream_count tracking", function(t)
    local tmp = os.tmpname()
    io.open(tmp, "w"):close()
    local vm = provium:vm("a", "peios"):boot()
    t:assert_eq(vm:open_stream_count(), 0)
    local s = vm:tail_file(tmp)
    t:assert_eq(vm:open_stream_count(), 1)
    s:close()
    t:assert_eq(vm:open_stream_count(), 0)
end)
"#,
    );
    assert_one_passed(&outcome);
}

#[test]
fn is_quiescent_true_with_no_open_resources() {
    let outcome = run_local_lua(
        r#"
test("is_quiescent baseline", function(t)
    local vm = provium:vm("a", "peios"):boot()
    t:assert(vm:is_quiescent(), "fresh VM must be quiescent")
end)
"#,
    );
    assert_one_passed(&outcome);
}

#[test]
fn is_quiescent_false_with_open_stream() {
    let outcome = run_local_lua(
        r#"
test("is_quiescent open stream", function(t)
    local tmp = os.tmpname()
    io.open(tmp, "w"):close()
    local vm = provium:vm("a", "peios"):boot()
    local s = vm:tail_file(tmp)
    t:assert(not vm:is_quiescent(),
        "open stream must invalidate quiescence")
    s:close()
end)
"#,
    );
    assert_one_passed(&outcome);
}
