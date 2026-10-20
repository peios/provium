---
title: Labs and Scope
type: how-to
description:
  Sub-labs for multi-VM topologies, resource claims against the pool,
  barriers, whole-lab snapshot and restore, and cached lab fixtures.
related:
  - provium/reference/lab
  - provium/reference/provium-module
  - provium/running-tests/pools-and-parallelism
  - provium/writing-tests/vms-and-profiles
---

A Lab is the unit of resource ownership in Provium. The `provium` global is
the root Lab; sub-labs let you carve out scoped subsets. This page covers the
patterns for using labs effectively.

The exhaustive method reference is on [Lab](~provium/reference/lab).

## Per-test scope

Each `test()` body runs in its own ephemeral sub-Lab. Resources you create
inside a test are local to that test; resources declared at file scope are
visible (through lookup fallthrough) and persist across tests.

```lua
-- File scope: persists across all tests
local shared = provium:vm("shared", "peios"):boot()

test("first", function(t)
    local local_vm = provium:vm("local", "peios"):boot()  -- test scope
    -- both `shared` and `local_vm` work here
end)
-- `local_vm` is shutdown silently here

test("second", function(t)
    local local_vm = provium:vm("local", "peios"):boot()  -- DIFFERENT VM
    -- The "local" name in test 1 and test 2 are independent — no collision.
    -- `shared` is still here, with whatever state test 1 left.
end)
```

The rules:

| Operation                                                | What happens in a test() body                                                                            |
| -------------------------------------------------------- | -------------------------------------------------------------------------------------------------------- |
| `provium:vm(name, profile)`                              | Creates in the test scope. Auto-shutdown at test end.                                                    |
| `provium:vm(name)`                                       | Looks up name: test scope first, then file root. Errors if not found.                                    |
| `provium.foo` (dot access)                               | Same lookup as above; returns `nil` on miss.                                                             |
| `provium:bridge(name, opts?)`                            | Same as VMs: create local, lookup walks up.                                                              |
| `provium:lab(name)`                                      | Creates a sub-lab in the test scope (auto-cleaned).                                                      |
| `provium:vm_fixture(name)` / `provium:lab_fixture(name)` | Always materialises at file root, regardless of where called. The fixture cache stays warm across tests. |

**Shadow detection.** Declaring a name at test scope when it already exists at
file scope is an error, not a silent shadow:

```lua
local shared = provium:vm("shared", "peios"):boot()

test("oops", function(t)
    -- This errors: "name already declared at parent scope (lab `provium`).
    -- Pick a different name, or use `provium:vm("shared")` to reuse it."
    local v = provium:vm("shared", "peios")
end)
```

The intent: if you wanted the file-scope VM, use the lookup form
(`provium:vm("shared")`). If you wanted a fresh independent VM, pick a
different name.

**Federation sub-labs are isolated.** A sub-lab created through
`provium:lab("dc1")` has no parent chain, `dc1.web` doesn't fall through to
find a sibling DC's "web". This is intentional; federation models distinct
sites.

**Things that don't walk parents** (each by design):

- `vm_names` / `bridge_names` / `members`, return only the local scope's
  contents.
- `boot` / `shutdown` / `pause` / `resume`, batch ops on the local scope's
  VMs.
- `snapshot` / `restore`, operate on the local scope plus its sub-labs
  (downward, not upward).
- `barrier`, a file-scope coordination primitive; test-scope barriers are
  isolated.
- `claim`, there is one claim per file wherever it is called from, so it
  neither walks parents nor needs to: a claim made inside a test is the file's
  claim.

## Per-test scope and `reset_between_tests`

These solve different problems and compose well together, they're not
alternatives.

|                               | Per-test scope (default, automatic)             | `reset_between_tests = true` (opt-in)                           |
| ----------------------------- | ----------------------------------------------- | --------------------------------------------------------------- |
| **What it isolates**          | New declarations made inside a `test()` body.   | Mutable state of file-scope resources.                          |
| **Mechanism**                 | Test-scope sub-Lab; auto-shutdown at test end.  | Snapshot after file setup; restore between tests.               |
| **Per-test cost**             | Booting the test-scope VMs you declared.        | Snapshot restore (cheaper than full boot, slower than nothing). |
| **What it doesn't help with** | File-scope VMs accumulating cruft across tests. | Name collisions across tests for ad-hoc VMs.                    |

The typical heavy test file uses both:

```lua
provium.reset_between_tests = true

-- File-scope setup: expensive cluster build, snapshot baseline.
local lan = provium:bridge("lan")
local web = provium:vm("web", "peios"):boot()
local db  = provium:vm("db",  "peios"):boot()
lan:attach({web, db})
web:run("ip addr add 10.0.0.1/24 dev eth0 && ip link set eth0 up")
db:run("ip addr add 10.0.0.2/24 dev eth0 && ip link set eth0 up")
db:run("psql -c 'CREATE TABLE t (...)'")  -- baseline schema

test("api writes propagate to db", function(t)
    -- web and db are restored to baseline at start of every test.
    web:run("curl -X POST http://localhost/items -d '...'")
    local r = db:run("psql -tA -c 'SELECT count(*) FROM t'")
    t:assert_eq(r.stdout:match("%d+"), "1")
end)

test("scratch VM joins the network", function(t)
    -- Throwaway VM, test-scope, auto-shutdown at test end.
    local probe = provium:vm("probe", "peios"):boot()
    lan:attach(probe)  -- bridge is file-scope, found via parent walk
    probe:run("ip addr add 10.0.0.3/24 dev eth0 && ip link set eth0 up")
    probe:run("curl -s http://10.0.0.1/health"):assert_ok()
end)
```

Without per-test scope, the second test couldn't introduce `probe` without
colliding (or having to pick a unique name per test). Without
`reset_between_tests`, the row the first test inserted would still be in the
database for every later test.

When to use which:

- **Per-test scope alone** is enough when each test is self-contained: declare
  what you need, do work, done. The smoke-test shape,
  `local vm = provium:vm("v", "peios"):boot(); vm:run(...)` repeated per test.
- **Add `reset_between_tests`** when you have a non-trivial setup at file
  scope (configured cluster, populated DB, attached topology) and tests mutate
  it. Pays for itself once a per-test setup-cost crosses the snapshot/restore
  time.
- **Skip both** by declaring `reset_between_tests = false` AND being careful
  with naming when you actually want state to accumulate across tests (for
  example progressive integration scenarios).

## Why sub-labs

Most simple tests use only the root lab, `provium:vm("a", "peios")`,
`provium:bridge("lan")`. Sub-labs are useful when:

- You're modeling multi-DC topologies (`provium:lab("dc1")`,
  `provium:lab("dc2")`).
- You want to snapshot or restore a coherent subset of resources.
- You want to use `lab_fixture` to cache an entire topology.

```lua
local dc1 = provium:lab("dc1")
dc1:vm("a", "peios")
dc1:vm("b", "peios")
dc1:bridge("lan"):attach({dc1.a, dc1.b})

local dc2 = provium:lab("dc2")
dc2:vm("c", "peios")
dc2:bridge("lan"):attach(dc2.c)
```

`dc1.a` is shorthand for `dc1:vm("a")`. The dot lookup tries vm → bridge →
sub-lab in order; missing names return `nil` (no error).

## Anonymous sub-labs

```lua
local sub = provium:lab()  -- name auto-generated: __lab_0, __lab_1, ...
```

Useful for one-shot subsets that don't need a stable name.

## Membership: include and remove

You can move resources between labs:

```lua
local v = provium:vm("v", "peios")
local sub = provium:lab("sub")
sub:include(v)            -- v is now in sub
provium:remove(v)         -- and gone from root
```

`lab:include` accepts a single VM, Bridge, or sub-Lab userdata, or an array of
those. It errors on duplicate names, reserved names, and shadow conflicts, the
exact error cases are in the
[Lab reference](~provium/reference/lab#lab-include-resource-or-list).

`lab:remove` is
[graph-state only](~provium/writing-tests/bridges-and-impairments#declared-and-realized),
the underlying VM, bridge, or sub-lab is NOT shut down. It's removed from the
lab's child list. Useful when you want to take ownership of a resource
somewhere else.

## Listing members

```lua
provium:vm_names()          -- {"v", "v2"}
provium:bridge_names()      -- {"lan"}
provium:sub_lab_names()     -- {"dc1", "dc2"}
provium:members()           -- [{kind="vm", name="v"}, {kind="bridge", name="lan"}, …]
```

`members()` is the unified accessor; the others are sliced by kind.

## Batch lifecycle

The lifecycle methods on a lab apply to every direct VM child (not recursive):

```lua
provium:boot()       -- boot every VM in the root lab
provium:shutdown()   -- shutdown every VM
provium:pause()
provium:resume()
```

Useful when a test sets up the topology declaratively and wants to bring it up
atomically:

```lua
local lan = provium:bridge("lan")
local a   = provium:vm("a", "peios")
local b   = provium:vm("b", "peios")
lan:attach({a, b})
provium:boot()                -- boots a and b together
```

For per-sub-lab control:

```lua
local dc1 = provium:lab("dc1")
dc1:vm("a", "peios"):boot()
dc1:vm("b", "peios"):boot()
-- dc1 boots independently of the root lab
```

## Resource claims

Each test file can make at most one claim against the dispatcher's resource
pool:

```lua
provium:claim({memory = "4G", cpus = 4})

test("…", function(t) end)
test("…", function(t) end)
```

The claim sits across the file's lifetime; it's released at file end. A second
`:claim` errors, `lab claim already held; one-shot per lab`. The accepted
field shapes and the no-pool behavior are in the
[Lab reference](~provium/reference/lab#resource-claims).

Why claim? Two reasons. The dispatcher won't oversubscribe, it tracks total
RAM and CPU budget across files and only takes a claim when it fits, so a file
that needs 4 VMs at 2 GiB each is not scheduled alongside other heavy files to
OOM the host. And a claim is the file's whole VM budget: every boot in the
file draws from it instead of reserving from the pool, so a claimed file never
waits for a VM slot mid-file. Files that reserve per boot can deadlock a run,
each holding one VM while it waits for another, and the pool refuses the boot
that would complete such a cycle; see
[pools and parallelism](~provium/running-tests/pools-and-parallelism#deadlock).

Claim the file's peak: the most VMs alive at once, each counted as its memory
plus 100 MiB of VMM overhead and its vCPUs. A boot past the claim fails at
once rather than waiting.

```lua
-- Never more than 3 VMs alive at once, 1 GiB and 2 vCPUs each:
provium:claim({memory = "3300M", cpus = 6})  -- 3 × (1G + 100M overhead); 3 × 2 vCPUs
```

## Barriers

`lab:barrier(name, count, timeout?)` is an N-arrival rendezvous: it blocks the
caller until `count` callers have hit the same `name`-keyed barrier, then
returns `true` for all of them. On timeout it returns `false`, it does not
raise. The default timeout, count lock-in, and round-reuse semantics are in
the [Lab reference](~provium/reference/lab#lab-barrier-name-count-timeout).

```lua
-- count = 1 is satisfied immediately: a labelled checkpoint.
provium:barrier("setup-done", 1)

-- An unmet count times out and returns false rather than raising.
local ok = provium:barrier("both-ready", 2, 0.5)
if not ok then
    error("second arriver never showed up")
end
```

A count above 1 needs a second concurrent host-side caller. Test files execute
on a single thread, and workers run guest processes (they can't call back into
the test's Lua), so today a `count > 1` barrier in an ordinary test file times
out. The multi-caller form is aimed at thread-mode workers
(`vm:spawn_worker({thread = true})`), which aren't supported yet.

For inter-VM coordination (for example between two guests), use a guest-side
primitive (file in a shared mount, fifo, network message). Barriers are
host-side only.

## Snapshot and restore

```lua
local snap = provium:snapshot()           -- to a tempdir, returns LabSnapshot
local snap = provium:snapshot("/tmp/x")   -- to that path, returns the path string
```

The `LabSnapshot` userdata's accessors are in the
[Snapshot reference](~provium/reference/snapshot). Restore with:

```lua
provium:restore(snap)             -- from LabSnapshot userdata
provium:restore("/tmp/x")         -- from path
```

The same precondition that blocks `vm:snapshot()` blocks `lab:snapshot()`,
open streams cause the snapshot to refuse with the offending stream's creation
site.

## Lab fixtures

A lab fixture builds a multi-VM topology once, caches the snapshot, and
restores it per test:

```lua
-- tests/fixtures/cluster.fixture.lua
local lan = provium:bridge("lan")
local a = provium:vm("a", "peios"):boot()
local b = provium:vm("b", "peios"):boot()
lan:attach({a, b})
a:run("ip addr add 10.0.0.1/24 dev eth0 && ip link set eth0 up")
b:run("ip addr add 10.0.0.2/24 dev eth0 && ip link set eth0 up")
return provium:snapshot()
```

```lua
-- tests/uses-cluster.test.lua
test("cluster has both VMs reachable", function(t)
    local cluster = provium:lab_fixture("fixtures/cluster")
    cluster.a:run("ping -c 1 -W 1 10.0.0.2"):assert_ok()
end)
```

`provium:lab_fixture(path)` returns a Lab userdata wrapping a fresh sub-lab
with the fixture restored. For single-VM fixtures, use
`provium:vm_fixture(path)`, same cache mechanism, returns a VM directly. What
goes into the cache key (and therefore what triggers a rebuild) is covered in
[fixtures and dependencies](~provium/running-tests/fixtures-and-dependencies).

## Common patterns

### Multi-DC topology

```lua
local function dc(name, vms)
    local d = provium:lab(name)
    for _, n in ipairs(vms) do d:vm(n, "peios") end
    d:bridge("lan"):attach(d:vm_names())
    return d
end

local dc1 = dc("dc1", {"a", "b"})
local dc2 = dc("dc2", {"c", "d"})
provium:boot()  -- boot every VM
```

### File-level resource claim

```lua
provium:claim({memory = "8G", cpus = 6})

test("3-VM cluster", function(t)
    -- The dispatcher won't run another file alongside this one
    -- if the pool can't afford 8G + 6 CPUs.
end)
```

### Cached cluster fixture

```lua
-- fixture builder
return provium:snapshot()  -- after building the topology

-- test usage
local cluster = provium:lab_fixture("fixtures/big-cluster")
local a = cluster.a  -- look up by name from the restored sub-lab
```

### Reset-between-tests with a sub-lab snapshot

`provium.reset_between_tests = true` snapshots the root lab. To reset only a
subset:

```lua
local sub = provium:lab("workspace")
local v = sub:vm("v", "peios"):boot()
v:write_file("/tmp/data", "")
local snap = sub:snapshot()

test("a", function(t)
    v:write_file("/tmp/data", "1")
end)

test("b", function(t)
    sub:restore(snap)  -- explicit restore
    t:assert_eq(v:read_file("/tmp/data"), "")  -- back to baseline
end)
```

This pattern is more verbose than `provium.reset_between_tests = true` but
works when only part of the lab needs resetting.

## Auto-close ordering

When a test scope ends (per-test or per-file), the harness's resource graph
walker closes resources in reverse-dependency order:

1. Streams (Tails, Captures, ConsoleStreams).
2. Processes (`vm:run_async`, `worker:run_async`).
3. Files (`vm:open_file`).
4. Workers.
5. Bridges.
6. VMs.

This is implemented as a Lua-side registry that every `register_resource` call
appends to. Resources are closed in priority order, then within a priority by
registration order.

You don't usually need to call `:close` yourself, the walker fires it
automatically. Calling explicitly is fine (the methods are idempotent) and
useful when the resource's lifetime is bounded by a clear point in the test.

Keeping a Lua reference to a test-scope resource does not extend its lifetime.
For example, a stream opened inside a test on a file-scope VM is closed when
that test ends; the VM remains available to later tests. Cleanup continues
through the remaining resources if an individual close fails.

## See also

- [Lab reference](~provium/reference/lab): every method.
- [provium global](~provium/reference/provium-module): file-scope
  configuration globals.
- [VM reference](~provium/reference/vm),
  [Bridge reference](~provium/reference/bridge), what lives in a lab.
- [Pool and parallelism](~provium/running-tests/pools-and-parallelism): how
  the dispatcher uses claims.
