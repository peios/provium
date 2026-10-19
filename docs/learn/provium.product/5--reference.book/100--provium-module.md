---
title: Provium Global
description:
  The `provium` global is the entry point to every Provium API. It is itself a
  Lab, so every method documented on Lab is reachable directly off provium.
related:
  - provium/reference/lab
  - provium/reference/test-framework
  - provium/reference/vm
  - provium/writing-tests/labs-and-scope
---

The `provium` global is installed onto every test file's Lua state. It is the
root [Lab](~provium/reference/lab) and the only API surface a test author
needs to import, there is no `require`.

```lua
provium:vm("a", "peios")              -- create a VM in the root lab
provium:bridge("lan")                 -- create a bridge in the root lab
provium:lab("subset")                 -- create a sub-lab
provium:vm_fixture("base")            -- restore a fixture into a VM
provium:lab_fixture("ha-pair")        -- restore a fixture into a sub-lab
provium:claim({memory="4G", cpus=2})  -- file-level resource reservation
provium:boot()                        -- batch-boot every VM in the lab
provium:shutdown()                    -- batch-shutdown every VM in the lab
provium:snapshot()                    -- whole-lab snapshot
provium:restore(snap)                 -- whole-lab restore
provium:pack(...) / provium:unpack(...)  -- Lua 5.4 string.pack/unpack
```

See [Lab reference](~provium/reference/lab) for the canonical enumeration of
these methods. This page documents only the things that are unique to the
top-level `provium` global.

## File-scope configuration globals

Two top-level Lua globals on the file's `provium` table change harness
behavior for every test in the file:

### `provium.reset_between_tests`

```lua
provium.reset_between_tests = true

test("a", function(t) provium:vm("v", "peios"):boot() end)
test("b", function(t) provium:vm("v", "peios"):boot() end)  -- v was reset
```

When set to `true` at file scope, Provium takes a baseline snapshot of the
root lab right after the file's top-level chunk finishes (and before the first
test runs). After every test's body returns, the lab is restored to that
baseline. Each `test()` runs against an identical starting state.

Mutually exclusive with file-scope open streams. If the top-level chunk opens
a stream (`vm:tail_file`, `vm:console():read()`, `bridge:capture()`, and so
on) and `reset_between_tests = true`, the file errors at chunk-load time with
an explicit "would auto-snapshot a live stream" message naming the stream type
and creation site.

### `provium.timeout`

```lua
provium.timeout = "30s"

test("a", function(t) … end)         -- per-test deadline = 30s
test("b", {timeout = "5m"}, function(t) … end)  -- per-test wins; 5m
```

File-default per-test timeout. Per-test `meta.timeout` wins. Accepts numeric
seconds or a duration string with `ms`/`s`/`m`/`h` suffix. When a per-test
timeout fires, the watchdog tears the entire root lab down (see
[time and timeouts](~provium/reference/test-framework#time-and-timeouts) for
the scope-limitation note).

## Dot-sugar lookup

`provium.<name>` is shorthand for whichever named resource matches `<name>` in
the root lab. The lookup order is:

| Reserved keys                               | Resolves to                                                                   |
| ------------------------------------------- | ----------------------------------------------------------------------------- |
| `provium.pack`, `provium.unpack`            | Lua's standard `string.pack` / `string.unpack`. Reserved against shadowing.   |
| `provium.vm_fixture`, `provium.lab_fixture` | Functions equivalent to the `:vm_fixture(...)` / `:lab_fixture(...)` methods. |

After reserved keys, the lookup tries:

1. A VM declared by `provium:vm("<name>", ...)`, returns the VM userdata.
2. A bridge declared by `provium:bridge("<name>")`, returns the Bridge
   userdata.
3. A sub-lab declared by `provium:lab("<name>")`, returns the LabUd userdata.
4. `nil` if nothing matches.

Examples:

```lua
local lan = provium:bridge("lan")
local a   = provium:vm("a", "peios")
local b   = provium:vm("b", "peios")

-- Later in the test:
provium.lan:attach({provium.a, provium.b})
provium.a:boot()
if provium.dc1 then provium.dc1:boot() end
```

The dot form is a strict lookup. It does not create new resources; calling
`provium.unknown` returns `nil`. Use the method form
(`provium:vm("name", "profile")`) to create.

## Fixture sugar

`provium.vm_fixture` and `provium.lab_fixture` are reserved keys that resolve
to the same callable surface as the `:vm_fixture(...)` / `:lab_fixture(...)`
methods. Both call shapes work:

```lua
local vm  = provium:vm_fixture("base")     -- method form
local vm2 = provium.vm_fixture("base")     -- function form (same effect)
```

Use whichever reads better in context. The function form is convenient inside
a `wait_until` predicate or a higher-order helper that captures the function
rather than the lab.

## Binary helpers (`pack` / `unpack`)

`provium:pack(fmt, ...)` and `provium:unpack(fmt, s)` are pass-throughs to Lua
5.4's `string.pack` / `string.unpack`. They exist on the `provium` global so
test code that uses binary protocols doesn't have to reach into `string.*`:

```lua
local frame = provium:pack(">I4 I4 s2", id, seq, payload)
local id, seq, payload = provium:unpack(">I4 I4 s2", frame)
```

The full Lua 5.4 format-string grammar applies. See the
[Lua reference manual on string.pack](https://www.lua.org/manual/5.4/manual.html#6.4.2)
for the format specifiers.

## See also

- [Lab reference](~provium/reference/lab): every method `provium` carries
  through inheritance.
- [VM reference](~provium/reference/vm): what `provium:vm(...)` returns.
- [test framework reference](~provium/reference/test-framework): `test()`,
  `t`, `wait_until`, `todo`.
