---
title: Snapshot and LabSnapshot
description:
  Snapshot and LabSnapshot userdata wrap the on-disk artefacts from
  vm:snapshot() and lab:snapshot(), path, size, and delete.
related:
  - provium/reference/vm
  - provium/reference/lab
  - provium/running-tests/fixtures-and-dependencies
  - provium/writing-tests/vms-and-profiles
---

`vm:snapshot()` returns a **Snapshot** wrapping a single file.
`lab:snapshot()` returns a **LabSnapshot** wrapping a directory containing
per-VM snapshot files plus a `lab.json` index.

Both are thin handles over filesystem paths. They have no agent-side state,
once the snapshot file or directory is on disk, the userdata gives you
ergonomic access to it.

## Snapshot

### Constructing

| Source                          | Returns                           |
| ------------------------------- | --------------------------------- |
| `vm:snapshot()`                 | New snapshot at a fresh tempfile. |
| `vm:snapshot("/explicit/path")` | Snapshot at that exact path.      |

A fixture's chunk returns a Snapshot (single-VM fixture); the harness reads
the path, compresses + sparsifies the file, and installs it into the cache.

### Methods

### `snap:path()`

Returns the on-disk path as a Lua string.

### `snap:size()`

Returns the file size in bytes. Returns `0` rather than ENOENT if the file has
been moved or deleted (idempotent with `:delete()`).

### `snap:delete()`

Best-effort delete. Idempotent, calling on a path that's already gone is fine.
Returns nothing.

## LabSnapshot

### Constructing

| Source               | Returns                              |
| -------------------- | ------------------------------------ |
| `lab:snapshot()`     | New lab snapshot in a fresh tempdir. |
| `provium:snapshot()` | Same, on the root lab.               |

A lab fixture's chunk returns a LabSnapshot; the harness moves the directory
into the cache as `<key>.lab/`.

### Methods

### `lab_snap:path()`

Returns the on-disk directory path as a Lua string.

### `lab_snap:size()`

Returns the sum of file sizes directly under the directory (one level deep,
doesn't recurse). Useful for picking large fixtures out of the cache.

### `lab_snap:delete()`

Recursively deletes the directory. Idempotent, calling on a path that's
already gone is fine.

## Example: build a fixture

```lua
-- tests/fixtures/base.fixture.lua
local vm = provium:vm("base", "peios"):boot()
vm:run("seq 1000000 > /srv/corpus"):assert_ok()
return vm:snapshot()
```

```lua
-- tests/fixtures/cluster.fixture.lua
provium:bridge("lan")
local a = provium:vm("a", "peios"):boot()
local b = provium:vm("b", "peios"):boot()
provium.lan:attach({a, b})
a:run("ip addr add 10.0.0.1/24 dev eth0 && ip link set eth0 up")
b:run("ip addr add 10.0.0.2/24 dev eth0 && ip link set eth0 up")
return provium:snapshot()
```

```lua
-- tests/uses-base.test.lua
test("base fixture has the corpus pre-built", function(t)
    local vm = provium:vm_fixture("fixtures/base")
    vm:run("test -s /srv/corpus"):assert_ok()
end)

test("cluster fixture brings two VMs", function(t)
    local cluster = provium:lab_fixture("fixtures/cluster")
    cluster.a:run("ping -c 1 10.0.0.2"):assert_ok()
end)
```

## See also

- [VM](~provium/reference/vm): `vm:snapshot()`, `vm:restore()`.
- [Lab](~provium/reference/lab): `lab:snapshot()`, `lab:restore()`,
  `lab:vm_fixture()`, `lab:lab_fixture()`.
- [Fixtures and dependencies](~provium/running-tests/fixtures-and-dependencies),
  how snapshots become cached fixtures.
