//! Conformance: bridge topology — route + uplink — per
//! `DESIGN.md` § Bridge.

#![cfg(feature = "lua")]

mod common;

use common::{assert_one_passed, run_local_lua};

#[test]
fn route_accepts_string_target() {
    let outcome = run_local_lua(
        r#"
test("route string", function(t)
    local lan = provium:bridge("lan", {})
    -- v1 route is graph-state only — should accept without raising.
    lan:route("dmz")
    local routes = lan:routes()
    local found = false
    for _, r in ipairs(routes) do
        if r == "dmz" then found = true; break end
    end
    t:assert(found, "route 'dmz' must appear in routes()")
end)
"#,
    );
    assert_one_passed(&outcome);
}

#[test]
fn route_accepts_bridge_userdata() {
    let outcome = run_local_lua(
        r#"
test("route ud", function(t)
    local lan = provium:bridge("lan", {})
    local dmz = provium:bridge("dmz", {})
    lan:route(dmz)
    local routes = lan:routes()
    local found = false
    for _, r in ipairs(routes) do
        if r == "dmz" then found = true; break end
    end
    t:assert(found)
end)
"#,
    );
    assert_one_passed(&outcome);
}

#[test]
fn route_accepts_list() {
    let outcome = run_local_lua(
        r#"
test("route list", function(t)
    local lan = provium:bridge("lan", {})
    lan:route({"dmz", "guest"})
    local routes = lan:routes()
    t:assert(#routes >= 2, "list form must add both routes")
end)
"#,
    );
    assert_one_passed(&outcome);
}

#[test]
#[ignore = "iptables/nft requires root to install MASQUERADE rules"]
fn enable_uplink_does_not_panic() {
    let outcome = run_local_lua(
        r#"
test("enable_uplink", function(t)
    local lan = provium:bridge("lan", {})
    -- LocalAgent: graph-state only — must not raise.
    lan:enable_uplink()
end)
"#,
    );
    assert_one_passed(&outcome);
}

#[test]
#[ignore = "iptables/nft requires root to install MASQUERADE rules"]
fn disable_uplink_does_not_panic() {
    let outcome = run_local_lua(
        r#"
test("disable_uplink", function(t)
    local lan = provium:bridge("lan", {})
    lan:enable_uplink()
    lan:disable_uplink()
end)
"#,
    );
    assert_one_passed(&outcome);
}

#[test]
fn routes_returns_empty_table_initially() {
    let outcome = run_local_lua(
        r#"
test("routes empty", function(t)
    local lan = provium:bridge("lan", {})
    local routes = lan:routes()
    t:assert(type(routes) == "table")
    t:assert_eq(#routes, 0)
end)
"#,
    );
    assert_one_passed(&outcome);
}

#[test]
fn isolate_then_unisolate_round_trips() {
    let outcome = run_local_lua(
        r#"
test("isolate round-trip", function(t)
    local lan = provium:bridge("lan", {})
    lan:attach({"a", "b"})
    lan:isolate("a")
    t:assert(lan:is_isolated("a"))
    lan:unisolate("a")
    t:assert(not lan:is_isolated("a"))
end)
"#,
    );
    assert_one_passed(&outcome);
}
