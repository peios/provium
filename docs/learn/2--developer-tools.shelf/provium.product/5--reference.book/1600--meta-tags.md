---
title: Meta Tags
description:
  The metadata table on test(), the well-known keys (slow, skip, tags,
  timeout, spec), arbitrary fields, and how --tag-meta filtering works.
related:
  - provium/reference/test-framework
  - provium/writing-tests/the-test-function
  - provium/reference/cli
  - provium/reference/events
---

Each `test(name, meta, fn)` call can include a metadata table. Provium's
runner inspects a handful of well-known keys, but every key is preserved in
the `MetaMap` it ships in `TestStarted` / `TestPassed` / `TestFailed` /
`TestSkipped` events. Consumers like `provium-coverage` use this for spec
linkage, classification, and filtering.

## Well-known keys

### `meta.slow`

```lua
test("big batch import", {slow = true}, function(t) … end)
```

When `--include-slow` is **not** passed to the CLI, tests with truthy
`meta.slow` (`true` or any non-zero integer) are skipped with reason
`filtered: slow tests skipped (pass --include-slow)`.

The default behavior is "skip slow tests" so `provium tests/` is fast by
default. CI and explicit slow runs pass `--include-slow`.

### `meta.skip`

```lua
test("not implemented yet", {skip = true}, function(t) … end)
test("waiting on RFC", {skip = "see issue #42"}, function(t) … end)
```

Declarative skip. Three accepted forms:

| Value                        | Skip reason             |
| ---------------------------- | ----------------------- |
| `true` (or any non-zero int) | `"skipped (meta.skip)"` |
| String `"<reason>"`          | `"skipped: <reason>"`   |
| `false` / `nil` / `0`        | Not skipped.            |

### `meta.tags`

```lua
test("dns lookup", {tags = {"net", "dns"}}, function(t) … end)
test("ipv6 only", {tags = "ipv6"}, function(t) … end)  -- single string also accepted
```

Tag filtering works through the CLI:

- `provium tests/ --tag net`, run only tests tagged `net`. Repeatable:
  `--tag a --tag b` is "a OR b".
- `provium tests/ --no-tag dns`, run every test EXCEPT those tagged `dns`.
  Repeatable; OR'd. Wins over `--tag` (intersect).

A single-string `tags = "smoke"` is accepted alongside the canonical array
form. Without the single-string convenience, a one-tag test was silently
filtered out by `--tag` with the misleading "did not match --tag" message
instead of seeing its lone tag.

### `meta.subsystems` and other arbitrary fields

`meta.tags` is the de-facto field for orthogonal cross-cutting flags (`slow`,
`flaky`, `perf`, `federation`). For declaring **what a test exercises**,
particularly when test files are organized by user-visible scenario rather
than by code subsystem, use a separate field with array values:

```lua
test("DNS service survives peinit restart", {
    subsystems = {"peinit", "loregd", "networking"},
    tags = {"dns", "services"},
    slow = true,
}, function(t) … end)
```

Filter through `--tag-meta KEY=VALUE`:

- `provium tests/ --tag-meta subsystems=peinit`, every test that touches
  peinit, regardless of where it lives in the directory tree.
- `provium tests/ --tag-meta subsystems=peinit --tag-meta subsystems=loregd`,
  touches peinit OR loregd (OR within key).
- `provium tests/ --tag-meta subsystems=peinit --tag-meta area=boot`, touches
  peinit AND is in the boot area (AND across keys).
- `provium tests/ --no-tag-meta flaky=true`, exclude anything tagged
  flaky=true.

`--tag-meta` accepts string-scalar (`{flaky = "true"}`) and array
(`{subsystems = {"a", "b"}}`) meta values; nested-map and integer values don't
match. The KEY can be any meta field name; the harness doesn't reserve
`subsystems` or any other name.

The convention split:

- **`tags`**, orthogonal flags. "How does this test behave?" (slow, flaky,
  perf).
- **arbitrary keys (`subsystems`, `area`, `feature`)**, "What does this test
  exercise?" Filter through `--tag-meta`.

Why two mechanisms? `tags` is for things you'd cumulatively-enable ("run me
everything tagged slow OR perf"). Arbitrary keys are for orthogonal axes you'd
intersect ("run everything that touches peinit AND is in the boot area").
`--tag-meta`'s AND-across-keys is what makes intersection work.

### `meta.timeout`

```lua
test("flaky network probe", {timeout = "30s"}, function(t) … end)
test("quick", {timeout = 0.5}, function(t) … end)
```

Per-test wall-clock timeout. Numbers are seconds. Strings carry a suffix:
`"500ms"` / `"5s"` / `"5m"` / `"2h"`. Per-test wins over the file-default
`provium.timeout`.

When the timeout fires, the watchdog tears down the entire file's lab. See
[test framework / Time and timeouts](~provium/reference/test-framework#time-and-timeouts)
for the scope-limitation note.

### `meta.spec`

```lua
test("token issue follows §4.2.1", {spec = "PSD-KACS §4.2.1"}, function(t) … end)
```

Provium's runner does not interpret `meta.spec`, it ships the value through to
consumers. `provium-coverage` reads it to map tests to spec-document sections
for coverage reports.

By convention: a stable identifier that names the spec section a test
exercises. Useful for cross-referencing tests against normative documents.

## Arbitrary keys

Any other key in the meta table is passed through to consumers as-is.
Provium's `MetaValue` enum covers:

- `null` / `nil`
- bool
- int (i64)
- float (f64)
- string
- bytes (raw `Vec<u8>`)
- array of meta values
- string→meta map

Nested tables come through as either `Array` (when the table has integer keys
1..N) or `Map` (when string keys are present). The decision is made per
nesting level; a mixed table is unusual and not specifically handled.

## Examples

### Multi-tag with skip-on-condition

```lua
test("federation handshake (multi-DC)", {
    tags = {"federation", "slow", "multi-dc"},
    timeout = "5m",
    spec = "PSD-FEDERATION §3.1",
}, function(t)
    if not have_two_dcs() then
        t:skip("requires two DCs")
    end
    -- … real test body …
end)
```

### Conditional gate through `wait_until`

```lua
test("sysctl knob takes effect", {
    spec = "PSD-PEINIT §6.5",
    timeout = "10s",
}, function(t)
    vm:run("sysctl -w kernel.shm_rmid_forced=1"):assert_ok()
    wait_until(function()
        return vm:read_file("/proc/sys/kernel/shm_rmid_forced") == "1\n"
    end, {timeout = "5s", desc = "sysctl knob propagated"})
end)
```

## See also

- [Test framework](~provium/reference/test-framework): `test()`, the `t`
  context.
- [CLI](~provium/reference/cli): `--tag`, `--no-tag`, `--tag-meta`,
  `--no-tag-meta`, `--include-slow`.
- [Events](~provium/reference/events): how meta is emitted in test-lifecycle
  events.
