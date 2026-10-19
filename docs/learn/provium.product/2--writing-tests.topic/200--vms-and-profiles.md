---
title: VMs and Profiles
type: how-to
description:
  Create, boot, snapshot, restore, pause, and reset VMs; how profiles map
  names to kernel and initrd choices, and how boot opts split between create
  and boot.
related:
  - provium/reference/vm
  - provium/configuration/profiles
  - provium/reference/snapshot
  - provium/writing-tests/labs-and-scope
---

Every Provium test ultimately drives one or more guest VMs. This page covers
the lifecycle: creating a VM, picking its profile, controlling boot, taking
snapshots, and tearing down.

The exhaustive reference is on [VM](~provium/reference/vm).

## Creating a VM

```lua
local vm = provium:vm("name", "profile")
```

The first arg is the VM's name (per-scope unique). The second arg is a profile
name from `provium.toml`. The VM is in `Created` state until you call
`:boot()`.

**Two forms.** `provium:vm(name, profile)` _creates_ a VM in the current
scope. `provium:vm(name)` _looks one up_ by name, walking from the current
scope to the file root. Inside a `test()` body, the current scope is the
per-test scope; at file top-level, it's the file root. See
[labs and scope](~provium/writing-tests/labs-and-scope) for the full rules.

Optional third arg is a sizing table, the two keys the scheduler and QEMU need
before boot:

```lua
local vm = provium:vm("v", "peios", {memory = "2G", cpus = 4})
```

Everything else about a boot, kernel command line, determinism seeds, injected
files, is passed to `vm:boot(opts)` instead:

```lua
vm:boot({
    kernel_cmdline = "console=ttyS0 quiet earlyprintk=ttyS0",
    rng_seed = 0xdeadbeef,
    initial_time = 1700000000,
    files = {
        {path = "/etc/hostname", content = "v"},
        {path = "/root/.ssh/authorized_keys", content = pubkey},
    },
})
```

The split is deliberate: `memory` and `cpus` size the VM, and the scheduler
needs them when the VM is declared; the boot opts shape one particular boot
and merge per field into the VM's pending boot options.

## Booting

```lua
local vm = provium:vm("v", "peios"):boot()
```

`:boot()` returns `self` so you can chain. The VM is in `Booted` state on
return. A `vm_spawned` event fires as soon as the agent has handshaken;
consumers see it before `:boot()` returns.

For multi-VM tests, you can boot each individually:

```lua
local a = provium:vm("a", "peios"):boot()
local b = provium:vm("b", "peios"):boot()
```

Or batch-boot through the lab:

```lua
provium:vm("a", "peios")
provium:vm("b", "peios")
provium:boot()                -- boots both
```

The batch form is mainly useful when you've declared a topology in a fixture
builder and want to bring it up atomically.

## Profiles

A profile is a `[profiles.<name>]` block in `provium.toml`:

```toml
[profiles.peios]
kernel   = "/build/peios/bzImage"
initrd   = "/build/peios/initrd.cpio.gz"
cmdline  = "console=ttyS0 quiet"
guest_os = "peios"
```

Each profile names a (kernel, initrd, cmdline) tuple. A test picks which
profile to use by name:

```lua
provium:vm("a", "peios")           -- uses [profiles.peios]
provium:vm("a", "peios-debug")     -- uses [profiles.peios-debug]
```

You can have any number of profiles. Common patterns:

| Pattern                               | Profiles                            |
| ------------------------------------- | ----------------------------------- |
| Test against multiple kernel versions | `peios-stable`, `peios-mainline`    |
| Compare optimized and debug builds    | `peios`, `peios-debug`              |
| Test pre/post a feature flag          | `peios-prefeatx`, `peios-postfeatx` |

Multi-profile fixtures invalidate every cached fixture when any profile's
kernel or initrd identifier changes, see
[fixtures and dependencies](~provium/running-tests/fixtures-and-dependencies).

## Lifecycle methods

A VM's lifecycle: `:boot()` takes it from `Created` to `Booted`; `:pause()` /
`:resume()` toggle between `Booted` and `Paused`; `:reset()` warm-reboots
(still `Booted`); and `:shutdown()` or `:power_button()` end in `Shutdown`.
The full state machine and per-method semantics are in the
[VM reference](~provium/reference/vm#state-machine).

Operations against a VM in the wrong state error cleanly with a hint:
`VM not booted` on a `Created` VM, `VM is paused; use resume()` on a `Paused`
one, `VM is shutdown; create a new one` after shutdown.

You typically don't call `:shutdown()` explicitly. The harness's
resource-graph walker tears every VM down at the appropriate scope boundary:
test-scope VMs at the end of their `test()` body, file-scope VMs at file end.
(`reset_between_tests = true` snapshots and restores instead, see

[labs and scope](~provium/writing-tests/labs-and-scope).)

## Boot opts in detail

The opts you'll use most often are `memory` and `cpus` (sizing, at creation),
`kernel_cmdline` (per-VM `loglevel`, `nokaslr`, and so on, replaces the
profile's `cmdline`; at boot), and `files` (inject config before init runs; at
boot):

```lua
local vm = provium:vm("v", "peios", {memory = "1G", cpus = 2})
vm:boot({
    files = {
        {path = "/etc/test.conf", content = "key=value\n"},
    },
})
```

This is a subset, the full boot-opts table (types, defaults, `rng_seed`,
`initial_time`) is in the [VM reference](~provium/reference/vm#boot-opts).

## Querying VM state

The accessors you'll use most are `vm:state()` (returns `"created"`,
`"booted"`, `"paused"`, `"shutdown"`, or `"dead"`) and `vm:is_quiescent()`
(true when there are no in-flight ops, open files, or open streams). The full
accessor list, `name`, `profile`, `cid`, `open_file_count`,
`open_stream_count`, is in the
[VM reference](~provium/reference/vm#accessors).

`is_quiescent` and the open-count accessors are useful for snapshot
precondition asserts:

```lua
test("snapshot is taken at quiescence", function(t)
    local vm = provium:vm("v", "peios"):boot()
    vm:run("…")
    t:assert(vm:is_quiescent(), "VM must be quiescent before snapshot")
    local s = vm:snapshot()
    t:assert(s:size() > 0)
end)
```

## Snapshots

```lua
local s = vm:snapshot()              -- writes to a tempfile
local s = vm:snapshot("/tmp/x.snap") -- writes to that path
```

Returns a [Snapshot](~provium/reference/snapshot) userdata wrapping the path.
Use it to:

- Restore later in the same test: `vm:shutdown(); vm:restore(s)`.
- Inspect size: `s:size()`.
- Delete: `s:delete()` (idempotent).

The snapshot file is what fixture builders return. If the snapshot fails
because of an open stream, the error names the stream's creation site, close
streams before snapshotting:

```lua
test("snapshot needs no open streams", function(t)
    local vm = provium:vm("v", "peios"):boot()
    local stream = vm:tail_file("/var/log/messages")
    -- vm:snapshot() would error here; close first.
    stream:close()
    local s = vm:snapshot()
end)
```

## Restoring

Restore from a Snapshot userdata or a bare path string:

```lua
vm:shutdown()
vm:restore(s)            -- from snapshot userdata
vm:restore("/tmp/x.snap") -- from path
```

The VM moves through `Shutdown → Created → Booted` (the restored state is
already `Booted`). Restoring requires the VM to be in `Created` or `Shutdown`
first.

## Determinism patterns

For tests that depend on randomness or wall-clock time, fix both at boot:

```lua
local vm = provium:vm("v", "peios")
vm:boot({
    rng_seed = 0xdeadbeef,
    initial_time = 1700000000,
})
```

After boot, you can move time forward (or backward) with
`vm:clock():advance(N)`, see [Clock reference](~provium/reference/clock).

## Pausing for inspection

`vm:pause()` freezes the guest's vCPUs. Useful for:

- Time-sensitive tests where you need to read multiple bits of state without
  races.
- Snapshotting (the snapshot path will pause anyway, but explicit pause makes
  the test's intent clear).

```lua
vm:pause()
local before = vm:read_file("/proc/loadavg")
vm:resume()
```

## Reset and power-button

`vm:reset()` warm-reboots the guest, same VM, same RAM image initially, then
init re-runs. Stays in `Booted`, and does not return until the guest is
answering again, so the next line of the test can use the VM straight away. A
reset that returns is a guest that came back; one that does not is an error
rather than a VM that quietly stops responding.

`vm:power_button()` sends ACPI power-button. The guest's init handles it as a
graceful shutdown signal (typically: stop services, sync filesystems, kernel
halts). Returns in `Shutdown` after confirmation. If the guest does not finish
within 60 seconds after accepting the request, the call errors without forcing
it off; see the [VM reference](~provium/reference/vm#vm-power-button).

```lua
test("guest survives reset", function(t)
    local vm = provium:vm("v", "peios"):boot()
    vm:write_file("/tmp/before", "1")
    vm:reset()
    -- /tmp is tmpfs in the test profile; vm:reset() is a warm
    -- reboot, so /tmp is fresh. Check that the test's expectation
    -- matches what the profile actually does.
    local r = vm:run("test ! -f /tmp/before")
    r:assert_ok()
end)
```

## Multi-VM topologies

The two-VM pattern is the workhorse of networking tests:

```lua
local lan = provium:bridge("lan")
local a   = provium:vm("a", "peios"):boot()
local b   = provium:vm("b", "peios"):boot()
lan:attach({a, b})
a:run("ip addr add 10.0.0.1/24 dev eth0 && ip link set eth0 up")
b:run("ip addr add 10.0.0.2/24 dev eth0 && ip link set eth0 up")

test("a can reach b", function(t)
    a:run("ping -c 1 -W 1 10.0.0.2"):assert_ok()
end)
```

For larger topologies, use sub-labs to keep names organized, see
[labs and scope](~provium/writing-tests/labs-and-scope).

## See also

- [VM reference](~provium/reference/vm): every method, every option.
- [Snapshot reference](~provium/reference/snapshot): snapshot/lab-snapshot
  userdata.
- [provium.toml reference](~provium/configuration/provium-toml): profile
  configuration.
- [Bridges and impairments](~provium/writing-tests/bridges-and-impairments),
  wiring VMs together.
