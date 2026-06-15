//! `test()` registration and the `t` context object.
//!
//! Implemented mostly in Lua — the registration table and the
//! assertion helpers are easier to reason about and customise that
//! side. The Rust runner ([`super::runner`]) drives execution by
//! retrieving the registry, building a fresh `t` per test, and
//! calling the test fn under `pcall`-equivalent semantics.

use mlua::Lua;

/// Outcome of one test.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TestStatus {
    /// Test fn returned without raising.
    Passed,
    /// Test fn raised an error not classified as a skip.
    Failed,
    /// Test fn called `t:skip(...)`.
    Skipped,
}

/// Sentinel string raised by `t:skip(...)` so the runner can
/// distinguish skips from real failures. The check is wrapped:
/// runner code never inspects raw Lua error strings.
pub(crate) const SKIP_SENTINEL: &str = "__PROVIUM_SKIP__";

/// Lua source for the test framework. Loaded once per
/// [`Lua`] state by [`install`]. Test authors see the
/// [`test`] global; the `_provium_*` helpers are runner-internal.
const FRAMEWORK_LUA: &str = r#"
local _registered = {}
local _file_skipped = nil  -- string when todo() was called
local _test_scope_resources = {}
local _file_scope_resources = {}

-- Resource graph tracking — stream/proc/file/worker userdata register
-- here at creation. Auto-close walks this list in reverse-dependency
-- order at scope end. Per `DESIGN.md` § Auto-close ordering.
function _provium_register_resource(ud, kind)
    -- Inside test() block: scope-local. At file scope: file-local.
    local target = _provium_in_test and _test_scope_resources
                                     or _file_scope_resources
    table.insert(target, {ud = ud, kind = kind})
end

local _close_order = {stream = 1, proc = 2, file = 3, worker = 4,
                      bridge = 5, vm = 6}

local function _close_scope(list)
    -- Stable sort by kind priority. Within a kind, original
    -- declaration order is preserved.
    local sorted = {}
    for i, r in ipairs(list) do
        sorted[i] = {prio = _close_order[r.kind] or 99, idx = i, r = r}
    end
    table.sort(sorted, function(a, b)
        if a.prio ~= b.prio then return a.prio < b.prio end
        return a.idx < b.idx
    end)
    for _, entry in ipairs(sorted) do
        local ud = entry.r.ud
        if ud and type(ud) == "userdata" then
            local mt = getmetatable(ud)
            if mt and (mt.close or (ud.close ~= nil)) then
                pcall(function() ud:close() end)
            end
        end
    end
end

function _provium_close_test_scope()
    _close_scope(_test_scope_resources)
    _test_scope_resources = {}
end

function _provium_close_file_scope()
    -- Test-scope first (just in case), then file-scope.
    _close_scope(_test_scope_resources)
    _test_scope_resources = {}
    _close_scope(_file_scope_resources)
    _file_scope_resources = {}
end

-- `test(name, [meta,] fn)` — register a test in declaration order.
-- Duplicate names within the same file raise an error so a
-- copy-paste typo doesn't silently produce two TestOutcome rows
-- with identical names that downstream consumers may merge.
function test(name, meta_or_fn, maybe_fn)
    for _, existing in ipairs(_registered) do
        if existing.name == name then
            error("test: duplicate name `" .. tostring(name) .. "` in this file", 2)
        end
    end
    if type(meta_or_fn) == "function" then
        table.insert(_registered, {name = name, meta = {}, fn = meta_or_fn})
    elseif type(meta_or_fn) == "table" and type(maybe_fn) == "function" then
        table.insert(_registered, {name = name, meta = meta_or_fn, fn = maybe_fn})
    else
        error("test(name, [meta,] fn): bad arguments", 2)
    end
end

-- `todo([reason])` — declarative file-scope skip. The runner
-- reads `_provium_file_skipped()` after the chunk runs; if set,
-- every registered test is reported Skipped with this reason
-- and no test body executes.
function todo(reason)
    _file_skipped = reason or "todo"
end

function _provium_file_skipped()
    return _file_skipped
end

-- `wait_until(predicate[, opts])` — call `predicate` repeatedly
-- until it returns truthy (and return that value), or until the
-- timeout lapses. opts: {timeout=10, interval=0.1, desc="…"}.
local function _parse_duration(field, value)
    if type(value) == "number" then return value end
    if type(value) ~= "string" then
        error("wait_until: " .. field .. " must be a number or string, got " ..
              type(value), 3)
    end
    local n, unit = value:match("^(%d+%.?%d*)(%a+)$")
    if not n then
        error("wait_until: bad " .. field .. " string `" .. tostring(value) ..
              "` (expected number with ms/s/m/h suffix)", 3)
    end
    n = tonumber(n)
    if unit == "ms" then return n / 1000
    elseif unit == "s" then return n
    elseif unit == "m" then return n * 60
    elseif unit == "h" then return n * 3600
    else error("wait_until: bad " .. field .. " unit `" .. unit .. "`", 3) end
end

function wait_until(predicate, opts)
    opts = opts or {}
    local timeout = _parse_duration("timeout", opts.timeout or 10)
    local interval = _parse_duration("interval", opts.interval or 0.1)
    local desc = opts.desc or "condition"
    -- Use wall-clock seconds (`_provium_now`) so the deadline
    -- elapses while `_provium_sleep` is pausing — `os.clock()`
    -- returns CPU time and would never advance.
    local now = _provium_now or os.clock
    local deadline = now() + timeout
    while true do
        local ok, value = pcall(predicate)
        if not ok then
            error(value, 2)
        end
        if value then return value end
        if now() >= deadline then
            error("wait_until: " .. desc .. " not met within " .. tostring(timeout) .. "s", 2)
        end
        if _provium_sleep then
            _provium_sleep(interval)
        else
            local target = now() + interval
            while now() < target do end
        end
    end
end

-- Runner-private accessor. Returns the registry as an array.
function _provium_get_tests()
    return _registered
end

function _provium_clear_tests()
    _registered = {}
end

-- Build a fresh `t` context for one test.
function _provium_make_t(name, meta)
    local t = {
        name = name,
        meta = meta or {},
        _outcome = "pass",
        _skip_reason = nil,
        _log = {},
        -- Sticky failure flag. Set BEFORE error() in every
        -- assertion so a user-side `pcall(function() t:assert(false) end)`
        -- still leaves enough state for the runner to mark the
        -- test Failed. Inspected on the Ok() return path.
        _failed = false,
        _failed_msg = nil,
    }

    -- Helper: record a sticky failure message before raising.
    local function _mark_fail(msg)
        if not t._failed then
            t._failed = true
            t._failed_msg = msg
        end
    end

    function t:assert(cond, msg)
        if not cond then
            local m = msg or "assertion failed"
            _mark_fail(m)
            error(m, 2)
        end
    end

    function t:assert_eq(a, b, msg)
        if a ~= b then
            local prefix = msg or "assert_eq failed"
            local full = prefix .. ": " .. tostring(a) .. " ~= " .. tostring(b)
            _mark_fail(full)
            error(full, 2)
        end
    end

    function t:assert_neq(a, b, msg)
        if a == b then
            local prefix = msg or "assert_neq failed"
            local full = prefix .. ": " .. tostring(a) .. " == " .. tostring(b)
            _mark_fail(full)
            error(full, 2)
        end
    end

    function t:assert_contains(haystack, needle, msg)
        if type(haystack) ~= "string" or type(needle) ~= "string" then
            local m = "assert_contains: both arguments must be strings (slice 2 limit)"
            _mark_fail(m)
            error(m, 2)
        end
        if not string.find(haystack, needle, 1, true) then
            local prefix = msg or "assert_contains failed"
            local full = prefix .. ": " .. tostring(needle) .. " not found in: " .. tostring(haystack)
            _mark_fail(full)
            error(full, 2)
        end
    end

    function t:assert_raises(fn, msg)
        -- Save the sticky failure flags before invoking `fn` so a
        -- t:assert/t:fail INSIDE fn (used to trigger the expected
        -- raise) doesn't leave _failed=true behind. We restore the
        -- pre-call values when fn raised as expected.
        local saved_failed = t._failed
        local saved_failed_msg = t._failed_msg
        local ok, err = pcall(fn)
        if ok then
            -- fn did NOT raise — the assertion itself failed, keep
            -- whatever flag state fn left (could be sticky from
            -- inner asserts) and add our own.
            local m = (msg or "assert_raises") .. ": expected to raise"
            _mark_fail(m)
            error(m, 2)
        end
        -- fn raised as expected — restore pre-call flags so a
        -- caller-side pcall(fn) inside fn that fired t:assert
        -- doesn't poison the rest of the test.
        t._failed = saved_failed
        t._failed_msg = saved_failed_msg
        return err
    end

    function t:fail(msg)
        local m = msg or "explicit failure (t:fail)"
        _mark_fail(m)
        error(m, 2)
    end

    function t:skip(reason)
        t._outcome = "skip"
        t._skip_reason = reason
        error("__PROVIUM_SKIP__", 2)
    end

    function t:log(msg)
        table.insert(t._log, tostring(msg))
    end

    return t
end
"#;

/// Install [`FRAMEWORK_LUA`] into `lua`. Idempotent.
pub(crate) fn install(lua: &Lua) -> mlua::Result<()> {
    // Sleep primitive used by `wait_until` — replaces the prior
    // `os.execute("sleep …")` shell-out so polling intervals don't
    // depend on /bin/sh, are sub-millisecond accurate, and don't
    // emit child processes at high frequency.
    let sleep = lua.create_function(|_, secs: f64| {
        let ns = (secs.max(0.0) * 1_000_000_000.0) as u64;
        std::thread::sleep(std::time::Duration::from_nanos(ns));
        Ok(())
    })?;
    lua.globals().set("_provium_sleep", sleep)?;
    // Wall-clock seconds since process start. Used by `wait_until`'s
    // deadline math — `os.clock()` returns CPU time which doesn't
    // advance during `_provium_sleep`, so a sleeping wait_until
    // would never time out.
    static EPOCH: std::sync::OnceLock<std::time::Instant> =
        std::sync::OnceLock::new();
    let now = lua.create_function(|_, ()| {
        let epoch = EPOCH.get_or_init(std::time::Instant::now);
        Ok(epoch.elapsed().as_secs_f64())
    })?;
    lua.globals().set("_provium_now", now)?;
    lua.load(FRAMEWORK_LUA).set_name("provium-test-framework").exec()
}
