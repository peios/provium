---
title: Disk
description:
  The Disk userdata, read and write sectors directly, inject EIO and slow-I/O
  faults, and detach the disk from the live guest.
related:
  - provium/writing-tests/disks-and-fault-injection
  - provium/reference/vm
---

A Disk wraps one disk attached to a VM. It exposes block-level operations and
a fault-injection surface useful for exercising I/O error paths in the guest.

## Constructing

| Source                                    | Returns                                                 |
| ----------------------------------------- | ------------------------------------------------------- |
| `vm:disk("id")`                           | Handle for a disk the VM booted with. Errors if absent. |
| `vm:attach_disk({id=…, size=…, image=…})` | Hot-plugs into a running VM, see below.                 |

A disk the **guest** can see is declared before it boots: in the profile's
[`disks`](~provium/configuration/provium-toml#disks) when the image belongs to
the system under test, or in
[`vm:boot({disks = …})`](~provium/reference/vm#boot-disks) when it belongs to
one test. `vm:disk(id)` then returns a handle for it.

`vm:attach_disk` adds a disk to a machine that is **already running**. With an
`image`, on a booted or paused VM, the guest gets a real `virtio-blk-pci`
device; without one, or before the VM boots, it records the host-side
attachment alone, which is all sector access and fault injection address. The
one thing it cannot do is be present from the guest's first instruction, so a
device the guest must find at boot belongs in the forms above.

Managed QEMU VMs reserve **eight additional PCIe root ports** at launch for
runtime disks. These are separate from the ports for profile and boot disks. A
full pool returns a capacity error before adding another block node. Guest
removal is asynchronous: `disk:detach()` does not immediately free a port, and
a paused guest cannot complete removal until it resumes. A later attachment
checks QEMU's PCI inventory, removes the old block node for a device that has
disappeared, and reuses that port. If all ports are still occupied, wait for
removal and retry. Creation and cleanup errors are reported, not treated as
successful attachment. The pool is bounded per VM, not a limit on the total
number of attach/detach cycles.

`vm:attach_disk` opts:

| Field      | Type          | Default                | Description                                                                      |
| ---------- | ------------- | ---------------------- | -------------------------------------------------------------------------------- |
| `id`       | string        | `"attached-<vm_name>"` | Disk identifier within the VM.                                                   |
| `size`     | int (bytes)   | `4 GiB`                | Modeled disk size. Used for `:size()` when no image is attached.                 |
| `image`    | string (path) | none                   | Backing file. Sector ops require an image, and so does the guest-visible device. |
| `readonly` | bool          | `false`                | Attach the hot-plugged device read-only.                                         |

Disks are 512-byte sectors throughout. The `read_sectors` and `write_sectors`
ops express offsets and counts in sectors.

## Methods

### `disk:size()`

Returns the disk's size in bytes. When a backing image is attached, returns
the live `stat()` size of the image file (so a test that resized the
underlying file with `truncate` / `fallocate` reads honest output). Without an
image, returns the modeled size from `attach_disk`.

### `disk:read_sectors(offset, n)`

Read `n` sectors starting at sector `offset`. Returns the bytes as a Lua
string. Errors if:

- The disk is detached (`disk:read_sectors: disk is detached`).
- No backing image
  (`disk:read_sectors: no backing image — disk:with_image required`).
- An `eio_read` fault is active (`disk:read_sectors: EIO (fault_inject)`).
- The host-side `read` fails (passes the underlying error through).

Honors active faults:

- `eio_read`, short-circuits to EIO before any I/O.
- `slow`, sleeps 50 ms per call. After the sleep, re-checks for `eio_read` so
  a concurrent fault injection during the sleep still takes effect. After the
  actual I/O, checks one more time for the same reason.

### `disk:write_sectors(offset, data)`

Write `data` (a Lua string) starting at sector `offset`. The data does not
have to be a multiple of 512 bytes; it is written verbatim from the offset.
Returns nothing.

Errors and fault handling mirror `read_sectors`:

- Detached → error.
- No backing image → error.
- `eio_write` active before the I/O → EIO.
- `slow` → 50 ms sleep, with `eio_write` re-checks after the sleep and after
  the I/O.

### `disk:fault_inject(mode)`

Activate a fault. Valid modes: `"eio_read"`, `"eio_write"`, `"slow"`. Unknown
modes error with the valid list (`disk:fault_inject: unknown mode \`X\`
(valid: eio_read, eio_write, slow)`).

Multiple modes can be active simultaneously. When `slow` and the matching
`eio_*` are both active at call time, the EIO check runs first, the call
errors immediately, without the 50 ms delay. (The post-sleep re-checks only
matter for an EIO fault set after the call started.)

### `disk:clear_faults()`

Clear every active fault. Subsequent reads / writes succeed normally.

### `disk:power_cut()`

Discard every write the guest has not flushed. What is left is exactly what it
made durable.

Unlike the fault modes above, which change what `read_sectors` and
`write_sectors` do, and so act on the _test's_ view of the image, this one
acts on the guest's own I/O. It requires a disk booted with
[`mediated = true`](~provium/reference/vm#boot-disks); on any other disk it
errors naming the disk, rather than quietly succeeding and letting a test
conclude that data survived a crash which never happened.

The guest's page cache is untouched and still holds what it wrote, so read
back _after_ a reboot rather than before, otherwise the answer comes from the
guest's memory instead of from the disk.

```lua
local vm = provium:vm("v", "peios"):boot({
    disks = {{scratch = "512M", id = "state", mediated = true}},
})

-- … the guest writes, and flushes some of it …

vm:disk("state"):power_cut()  -- everything unflushed is gone
vm:reset()                    -- and the reboot clears the guest's caches
```

### Faults the guest can see

Everything reached through `disk:fault_inject` changes what `read_sectors` and
`write_sectors` do, the _test's_ view of the image. The verbs below are served
into the guest's own I/O, so the guest is what sees them fail.

They need a disk booted with
[`mediated = true`](~provium/reference/vm#boot-disks). On any other disk each
errors naming the disk and the missing `mediated`, rather than quietly doing
the weaker host-side thing: a test that watched its guest sail through an
error the guest never actually saw would report that as resilience.

| Method                           | Effect on the guest's I/O                                                          |
| -------------------------------- | ---------------------------------------------------------------------------------- |
| `disk:fail_reads()`              | Every read returns EIO.                                                            |
| `disk:fail_writes()`             | Every write returns EIO. Flushes still work.                                       |
| `disk:fail_after(n)`             | The nth command and every one after it returns EIO, reads, writes **and flushes**. |
| `disk:fail_range(sector, count)` | Any command whose bytes overlap `count` sectors from `sector` returns EIO.         |
| `disk:delay(ms)`                 | Every command sleeps `ms` before it is served.                                     |
| `disk:clear_policy()`            | Disarm all of the above.                                                           |
| `disk:fault_policy()`            | Read back what is armed, as a table.                                               |

Rules accumulate: `disk:fail_reads()` then `disk:delay(20)` gives a disk that
is both slow and failing reads, the way `bridge:add_latency` and
`bridge:drop_rate` compose. Only `disk:clear_policy()` removes them,
`disk:clear_faults()` clears the _host-side_ set and leaves these untouched.

### `fail_after` is the one that can fail an fsync

`fail_writes` refuses writes and leaves flushes working, which models a disk
rejecting new data. `fail_after` counts flushes too, so it is the rule that
can tell a guest its **durable** data is gone, the fault a write-ahead log
actually has to survive.

The counter resets every time a policy is armed, so `fail_after(1)` means the
first command after _that call_, not the first since boot. A booted guest has
already issued hundreds, and a counter running from the server's start could
never be aimed at the test's own workload. `n` must be at least 1.

### Ranges fail whole commands

A command overlapping the range fails entirely rather than partially: one NBD
reply carries one error, so half a read is not expressible. `count` must be at
least 1, an empty range overlaps nothing, so it would arm a fault that could
never fire, which is indistinguishable from a passing test.

`fail_range` never catches a flush, which carries no offset or length. Failing
a flush is `fail_after`'s job alone.

### `disk:active_faults()`

Returns a Lua array of the active fault mode names. Useful for tests that need
to assert the harness state.

### `disk:detach()`

Mark the disk as detached and issue a best-effort QMP `device_del` against the
parent VM. Subsequent `read_sectors` / `write_sectors` error. The QMP call is
best-effort: a disk that was never QMP-added (host-bookkeeping-only attach)
silently surfaces "Device not found" but the local detached flag is still set.

Disks declared in the profile or in `vm:boot({disks = …})` are addressable by
their disk ID. Detaching them requests removal of the corresponding guest
device. Removal is asynchronous and requires guest cooperation: the local
detached flag does not prove that the guest has finished removing it. A test
that depends on removal should wait for the guest device to disappear before
continuing.

### `disk:is_detached()` / `disk:id()`

Accessors. `:id()` returns the disk identifier as given to `attach_disk`.

## Example: full fault-injection pattern

```lua
test("guest sees EIO and recovers after clear", function(t)
    local img = "/tmp/test-image"
    -- Pre-create the backing file.
    local f = io.open(img, "w"); f:write(string.rep("\0", 512 * 1024)); f:close()

    local vm = provium:vm("v", "peios"):boot()
    local disk = vm:attach_disk({id = "vda", size = 512 * 1024, image = img})

    -- Confirm baseline read works.
    local body = disk:read_sectors(0, 1)
    t:assert_eq(#body, 512)

    -- Inject EIO.
    disk:fault_inject("eio_read")
    local ok, err = pcall(function() disk:read_sectors(0, 1) end)
    t:assert(not ok)
    t:assert(tostring(err):find("EIO"))

    -- Clear and confirm reads recover.
    disk:clear_faults()
    body = disk:read_sectors(0, 1)
    t:assert_eq(#body, 512)
end)
```

## See also

- [VM](~provium/reference/vm): disks come from `vm:attach_disk` / `vm:disk`.
- [Disks and fault injection](~provium/writing-tests/disks-and-fault-injection),
  patterns for exercising I/O error paths.
