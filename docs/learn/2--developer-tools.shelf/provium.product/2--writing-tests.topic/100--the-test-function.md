---
title: Writing Tests
type: how-to
description:
  How to register tests, organize a file, use assertions, skip cleanly, log
  diagnostic data, and pick per-test metadata.
related:
  - provium/reference/test-framework
  - provium/reference/meta-tags
  - provium/writing-tests/labs-and-scope
---

A Provium test file is a sequence of `test(...)` calls. The harness runs them
in declaration order, gives each one a fresh `t` context, and records pass /
fail / skip. This page covers the patterns for getting the most out of that
loop.

The exhaustive reference is on
[test framework](~provium/reference/test-framework) and
[meta tags](~provium/reference/meta-tags).

## Anatomy of a test file

```lua
-- tests/file_handles.test.lua

provium.timeout = "10s"        -- file-default per-test timeout

test("baseline I/O round-trips", function(t)
    local vm = provium:vm("v", "peios"):boot()
    vm:write_file("/tmp/data", "hello")
    t:assert_eq(vm:read_file("/tmp/data"), "hello")
end)

test("permission bits stick on create", {tags = {"perms"}}, function(t)
    local vm = provium:vm("v", "peios"):boot()
    local h = vm:open_file("/tmp/secret", {write=true, create=true, perm=0x180})  -- 0o600
    h:close()
    t:assert_eq(vm:stat("/tmp/secret").perm, 0x180)
end)
```

Two things to know up front:

1. **Each `test()` body runs in its own scope.** The `provium:vm("v", …)`
   calls in the two tests above each create their own VM in their test scope;
   they're independent and do not share state. The VMs are silently shut down
   at test end. To share a VM across tests, declare it at file scope
   (top-level `local vm = provium:vm("v", "peios"):boot()`) and look it up by
   name (`provium:vm("v")`) or capture the userdata as a Lua local.
2. **Each test gets a fresh `t` context.** `t.name` and `t.meta` are per-test;
   assertions and skips are scoped to the running test.

For the full scoping rules (lookup fallthrough, shadow detection, fixtures),
see [labs and scope](~provium/writing-tests/labs-and-scope).

## Naming tests

Test names must be unique within a file. The harness raises at registration
time on a duplicate:

```text
test: duplicate name `boots` in this file
```

Pick names that read like the assertion, not the implementation.
`"boots and runs uname"` is better than `"test_boot_uname"`, the renderer
prefixes them with status (`PASS`, `FAIL`) and indents under the file path, so
the name reads as a sentence.

## Assertions

The `t` context exposes:

| Assertion                                   | Use when                                                                |
| ------------------------------------------- | ----------------------------------------------------------------------- |
| `t:assert(cond, msg?)`                      | You're checking any boolean condition.                                  |
| `t:assert_eq(a, b, msg?)`                   | Two values must be equal. The error message includes both values.       |
| `t:assert_neq(a, b, msg?)`                  | Two values must be unequal.                                             |
| `t:assert_contains(haystack, needle, msg?)` | A string must appear inside another string. Both args must be strings.  |
| `t:assert_raises(fn, msg?)`                 | Calling `fn` must raise; returns the error value.                       |
| `t:fail(msg?)`                              | You've decided the test failed for reasons that don't fit an assertion. |

Every assertion that fires raises (with `error(..., 2)` so the call site, not
the assertion implementation, is in the message). The harness catches the
raise and records the test as Failed.

Even when you wrap a body in `pcall` and swallow the error, the harness still
detects the failure, an assertion marks the test failed before it raises, and
that mark is sticky:

```lua
test("expected to detect a failure", function(t)
    local ok = pcall(function() t:assert(false, "should fail") end)
    -- ok is false (we caught the assert), but the test still
    -- reports Failed: the failure was recorded before the raise.
end)
```

Use `t:assert_raises(fn)` if you want the inverse, "this should raise":

```lua
test("write to a closed handle raises", function(t)
    local h = vm:open_file("/tmp/x", {write=true, create=true})
    h:close()
    local err = t:assert_raises(function() h:write("oops") end)
    t:assert_contains(tostring(err), "closed")
end)
```

## Skipping

Three ways to skip:

### Inline (`t:skip(reason)`)

```lua
test("only on btrfs", function(t)
    if not is_btrfs(vm) then t:skip("not a btrfs root") end
    -- … rest of test body …
end)
```

`t:skip` raises an internal sentinel that the harness treats as Skipped.
Useful when the skip condition can only be evaluated at runtime.

### Declarative (`{skip = …}`)

```lua
test("not implemented yet", {skip = true}, function(t) … end)
test("waiting on RFC", {skip = "see issue #42"}, function(t) … end)
```

The body never runs. Cleaner than inline when the test is permanently disabled
or pending an unrelated change.

### File-scope (`todo("reason")`)

```lua
todo("waiting on the spec")
test("a", function(t) … end)
test("b", function(t) … end)
```

Every registered test is reported Skipped with the given reason. Use this when
an entire test file is non-applicable temporarily, do not delete the tests,
mark the file pending.

## Logging diagnostic data

`t:log(msg)` appends to a per-test log array. The harness includes these
messages in the file outcome and the `TestPassed` / `TestFailed` events,
useful for diagnostic context when something goes wrong:

```lua
test("retries until success", function(t)
    for i = 1, 5 do
        local r = vm:run("flaky-command")
        t:log("attempt " .. i .. " -> exit " .. r.exit_code)
        if r:ok() then return end
    end
    t:fail("flaky-command failed 5 times")
end)
```

Avoid `print` in tests, `print` writes to stdout and gets interleaved with the
harness's own output, while `t:log` is structured and tied to the specific
test.

## Per-test metadata

The optional second arg to `test(...)` is a metadata table. Provium inspects a
handful of well-known keys:

| Key               | Purpose                                    |
| ----------------- | ------------------------------------------ |
| `slow = true`     | Skip unless `--include-slow` is passed.    |
| `skip = …`        | Declarative skip.                          |
| `tags = {...}`    | Tag-based filtering (`--tag`, `--no-tag`). |
| `timeout = "30s"` | Per-test wall-clock timeout.               |
| `spec = "PSD-…"`  | Spec linkage for `provium-coverage`.       |

Anything else passes through to event consumers. See
[meta tags](~provium/reference/meta-tags) for the full reference.

```lua
test("federation handshake", {
    tags    = {"federation", "slow"},
    timeout = "5m",
    spec    = "PSD-FEDERATION §3.1",
}, function(t) … end)
```

## Polling with `wait_until`

`wait_until(predicate, opts?)` calls `predicate` repeatedly until it returns
truthy. Use it for guest-side conditions that don't have a stream interface:

```lua
test("observes a file written by the guest", function(t)
    local vm = provium:vm("v", "peios"):boot()
    vm:write_file("/tmp/ready", "ready")
    local value = wait_until(function()
        return vm:read_file("/tmp/ready") == "ready"
    end, {timeout = "30s", interval = "200ms", desc = "ready file"})
    t:assert(value)
end)
```

For things that produce a stream (logs, console output, captured stdout),
prefer `:expect` on the stream over `wait_until`, it gets event-driven
semantics and much tighter feedback.

## File-default timeouts

Set `provium.timeout` at file scope to put a default on every test:

```lua
provium.timeout = "30s"

test("…", function(t) … end)        -- 30s
test("…", {timeout = "5m"}, function(t) … end)  -- per-test wins; 5m
```

Per-test `meta.timeout` always wins over the file default. When a per-test
timeout fires, the watchdog tears down the **entire file's lab**, there's no
finer-grained cancellation in v1. See
[time and timeouts](~provium/reference/test-framework#time-and-timeouts) for
the scope-limitation note.

## Reset-between-tests

Set `provium.reset_between_tests = true` at file scope to take a baseline
snapshot after the file's top-level chunk and restore it between every test:

```lua
provium.reset_between_tests = true

local vm = provium:vm("v", "peios"):boot()
vm:write_file("/etc/marker", "baseline")  -- snapshot baseline includes this

test("a", function(t)
    vm:run("rm /etc/marker"):assert_ok()
    -- After this test, the lab is restored: the marker is back.
end)

test("b", function(t)
    vm:run("test -f /etc/marker"):assert_ok()  -- still present, baseline restored
end)
```

**Mutually exclusive with file-scope open streams.** Opening a `tail_file`,
`console:read`, `bridge:capture`, and so on at top-level errors at chunk load
with the offending stream's creation site named. Move stream opens into
`test()` bodies.

## Common patterns

### One-test-per-VM

When tests are independent and a fresh VM per test is acceptable, opt into
reset-between-tests:

```lua
provium.reset_between_tests = true

test("a", function(t)
    local vm = provium:vm("v", "peios"):boot()
    -- … tests against fresh state …
end)
```

### One-VM-many-tests, ordered

When tests build on each other, leave `reset_between_tests` off and let state
accumulate:

```lua
local vm = provium:vm("v", "peios"):boot()

test("create user", function(t)
    vm:run("useradd alice"):assert_ok()
end)

test("user appears in /etc/passwd", function(t)
    t:assert(vm:read_file("/etc/passwd"):find("^alice:"))
end)
```

Order matters here. If you delete the first test, the second will fail; that's
intentional.

### Fixture-backed setup

When the setup is expensive (install packages, fetch data, build a config),
put it in a `*.fixture.lua` file and call `provium:vm_fixture("name")`:

```lua
-- tests/fixtures/corpus.fixture.lua
local vm = provium:vm("base", "peios"):boot()
vm:run("seq 1000000 > /srv/corpus"):assert_ok()
return vm:snapshot()

-- tests/uses-corpus.test.lua
test("corpus is pre-built", function(t)
    local vm = provium:vm_fixture("fixtures/corpus")
    vm:run("test -s /srv/corpus"):assert_ok()
end)
```

The fixture is built once, cached on disk, and restored per test that asks for
it. See
[fixtures and dependencies](~provium/running-tests/fixtures-and-dependencies)
for the cache lifecycle.

## See also

- [Test framework reference](~provium/reference/test-framework): every method
  on `t` and the helpers `wait_until` and `todo`.
- [Meta tags reference](~provium/reference/meta-tags): every well-known meta
  key.
- [Labs and scope](~provium/writing-tests/labs-and-scope): `lab:claim`,
  `lab:barrier`, `reset_between_tests`.
