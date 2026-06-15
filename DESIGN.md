# Provium — Design Document

Working design for the provium rewrite. Captures decisions and
rationale; refine collaboratively.

---

## What provium is

Provium is the **one-size-fits-all userspace test harness** for Peios.
Anything testable from userspace — syscalls, services, daemons,
federation, networking, package install, recovery — runs through
provium. Anything internal to the kernel (LSM hooks not exposed to
userspace, scheduler internals) belongs in kunit, not provium.

Provium's **architecture and primitives are OS-agnostic**, but each
agent binary is necessarily OS-specific (init contract, syscall
numbers, native equivalents of Layer 1 ops). Peios test logic lives
in Lua helper libraries layered on top of provium's agnostic
primitives.

In practice this means: the wire protocol, scheduler, resource
model, Lua API, and host code are not Peios-specific. The agent is —
each guest OS needs its own port (Peios is the default; a Linux port
or others are possible but require deciding the per-OS launch story
and Layer 1 implementations). Profiles bind a kernel + initrd +
agent binary as a unit, so each profile is implicitly tied to one
guest OS.

## What it isn't

- A unit test framework — language-native frameworks (cargo test, go
  test) handle in-process unit tests
- A kernel test framework — that's kunit's job
- A CI orchestration system — provium produces results; CI integrates
- A benchmarking suite — measurement helpers can be built on top, not
  built in

## Why rewrite

The current provium is functional but has accumulated complexity:
hand-rolled binary wire protocol with duplicated parsing per handler,
1200+ line Lua bindings file mixing concerns, a `200ms` sleep
synchronizing snapshot quiesce that produces intermittent failures, a
parallelism cap at `-p 4` that wastes capacity, no spec-coverage
story, no networking primitives. The shape is roughly right but the
implementation has aged.

The rewrite preserves the user-visible primitives (Lua tests, vsock
wire, fixture-resumed VMs) and replaces the messy internals with a
schema-driven wire protocol, a Rust agent, QEMU as the VMM (after
spiking confirmed cloud-hypervisor's snapshot/restore is fundamentally
broken for our workflow), a real resource scheduler, and meaningful
networking primitives.

---

## Architecture

```
                       Host
                       ────
provium  (single Rust process)
│
├── thread: scheduler          (resource pool, file dispatch)
├── thread: file_A runner      (mlua state, runs *.test.lua)
├── thread: file_B runner      (mlua state)
├── thread: file_C runner      (mlua state)
│
├── child: QEMU    (VM owned by file_A)
├── child: QEMU    (VM owned by file_A)
├── child: QEMU    (VM owned by file_B)
└── ...
```

**Host:** Single Rust process. The scheduler runs in one thread; each
dispatched test file runs in its own OS thread with its own mlua Lua
state. VMM processes (QEMU) are spawned as children of
provium directly. Each thread tracks its own VM children for cleanup
on completion.

**Agent:** Rust, statically linked against musl. Runs as PID 2 inside
the guest after the wrapper PID 1 forks it. Listens on vsock,
processes one connectionless request per accept().

**VMM:** QEMU by default. Pluggable via a small Rust trait so other
VMMs (cloud-hypervisor, Firecracker, KVM-direct) can be added later
if specific needs arise. QEMU was chosen after a thorough VMM spike:
cloud-hypervisor's snapshot/restore + VMM-teardown workflow wedges
all emulated I/O (vsock, serial, virtio-console) in ways no userspace
recovery can fix; QEMU's `migrate file:` + `-incoming "file:"` is
mature and survived every realistic stress scenario.

**Wire protocol:** Shared Rust crate (`provium-protocol`) with
serde-derived op types; msgpack on the wire (`rmp-serde`). Both host
and agent depend on the crate, so layout disagreement is structurally
impossible. Adding an op = add struct + dispatch arm + Lua wrapper.

**Test language:** Lua. Mature, embeddable, fast, expressive. Test
authors write Lua against provium's Lua API.

---

## Components

### Host: startup pre-flight

Before scanning tests or initialising the scheduler, provium runs a
pre-flight check for systemic prerequisites. Any failure exits
immediately with an actionable error.

| Check | Why | Failure message |
|---|---|---|
| `CAP_NET_ADMIN` (or root) | Required for bridge / TAP / tc operations | "run with sudo, or `setcap cap_net_admin,cap_net_raw=eip $(which provium)`" |
| `/dev/kvm` exists and accessible | QEMU needs KVM | "load the kvm module or grant access to /dev/kvm" |
| `/dev/vhost-vsock` exists | Required for vsock between host and VMs | "modprobe vhost_vsock" |
| iproute2 binaries on `PATH` | Bridge management uses tc/ip | "install iproute2 (`apt install iproute2` or equivalent)" |

This means systemic failures are caught before a test ever runs.
Per-test bridge / network errors (TAP exhaustion, tc rule rejection)
are handled at the call site as the offending op's error.

### Host: scheduler

Owns the resource pool. Spawns file runner threads. Routes results
to the report via channels. See **Scheduler** section for details.

### Host: file runner

One OS thread per dispatched test file. Sets up its own mlua state,
loads the test file, runs each `test()` block sequentially, sends
per-test results to the scheduler over a channel.

Each runner thread tracks the QEMU child PIDs it has
spawned. On completion (success, failure, or panic), the runner
kills its tracked children to release VM resources.

Every runner thread's entry point is wrapped in `catch_unwind`. A
Rust panic in test code or its bindings becomes a test failure for
the *currently-running* test in that file. Because `mlua::Lua` is
`!UnwindSafe` and the state may be corrupted post-panic,
**provium then drops the entire file**: the file runner marks all
remaining tests in the file as `failed-due-to-poisoned-state`,
tears down its lab and Lua state, releases pool resources, and
exits the thread. Other file-runner threads continue unaffected.

This is the conservative choice — losing potentially-passing tests
in the panic case is much better than mysterious failures in
subsequent tests with a corrupted Lua state. In practice panics
are rare (most failures are clean Lua errors via `t:fail` /
`t:assert_*`); the few that occur are likely from mlua bindings
or our own code where corruption is plausible.

Genuine memory unsafety (segfault from our own code) takes down the
whole provium process — accepted because we're disciplined about
`unsafe` and the only obvious surface is the vsock kernel interface
and a small amount of TUN/bridge syscall code via netlink.

### Host: Lua bindings (mlua)

Exposes the provium API to Lua. Translates Lua calls into wire
messages. Uses generated Rust types from the schema for type-safe
op construction.

### Agent (guest)

Rust musl-static binary. Roles:

- Bind a vsock listener at startup. Readiness is signalled by the
  Hello/HelloOk handshake — the host's boot loop polls with `ping()`
  (a Hello round-trip) until the agent's listener is accepting and
  the version handshake succeeds. There is no separate `READY`
  message — the handshake doubles as the ready signal.
- Two operating modes:
  - **Short ops** (exec, open_file, syscall, ioctl, etc.):
    connectionless — each op opens a fresh vsock connection, sends
    one framed request, reads one framed response, closes. Agent
    runs `accept()` → handle → close → `accept()`.
  - **Streams** (file tail, console read, stdout subscribe, packet
    capture): hold the connection open for the stream's lifetime,
    pushing frames over time. Agent spawns a per-stream handler
    thread; host's Stream Lua object owns the connection and frees
    it on `:close()`.
- Stateful only between operations: job table for backgrounded
  processes, worker registry for spawned sub-agents
- All per-request state is on the handler stack and gone when the
  handler returns

So the agent is **connectionless for short ops, long-running for
streams**. The snapshot model accommodates this explicitly — see
**Wire Protocol → Snapshot precondition: no open streams**.

### Agent: per-OS ports

Layer 1 ops (open/read/write/exec/etc.) are portable in *concept* —
each port implements them in OS-native ways. Layer 0 ops (raw
syscall, ioctl) pass through. The default and only v1 port is Peios.

A port to another OS is bounded but non-trivial work: it has to
decide the agent's launch contract (Peios uses a wrapper-PID-1 that
forks the agent; Linux would need an initramfs `init=` story or a
systemd unit; etc.) and implement Layer 1 against the host OS's
syscalls. Not "straightforward" in the trivial sense — but the wire
protocol, schema, and host code don't change.

### VMM abstraction

```rust
// Factory: spawns + restores VMs. Lifecycle ops (pause, resume,
// snapshot, shutdown, set_link, detach_disk) live on the
// VmInstance the factory returns — they're stateful per-VM and
// don't belong on the Vmm-as-a-service surface.
trait Vmm: Send + Sync {
    fn launch(&self, name: &str, profile: &Profile, opts: BootOpts)
        -> Result<VmRunning, VmmError>;
    fn restore(&self, name: &str, profile: &Profile, opts: BootOpts,
               snapshot_path: &Path)
        -> Result<VmRunning, VmmError>;
}

// Returned by launch/restore. Wraps the per-VM resources +
// (type-erased) backend handle.
struct VmRunning {
    cid: u32,                         // vsock CID
    summary: BootSummary,             // memory_bytes, cpus, guest_os
    instance: VmInstance,             // lifecycle handle
    client: AgentClient,              // wire to in-VM agent
    console_log: Option<PathBuf>,
    console_socket: Option<PathBuf>,
}

// VmInstance methods (selected): pause/resume/snapshot/shutdown/
// reset/power_button/set_link/detach_disk. Drop is best-effort
// shutdown so a panicking test doesn't leak a QEMU child.
```

QEMU is impl 1. The trait stays pluggable in code so a future
backend (cloud-hypervisor if upstream fixes its snapshot bugs,
Firecracker, or a custom KVM-direct VMM) can implement it. The
profile config is honest about being QEMU-shaped today rather than
carrying speculative VMM abstraction.

### Profiles

A profile is a named bundle of "what to boot" — kernel image, initrd
image, default cmdline, agent port. Profiles are defined in
`provium.toml` and selected by name when creating a VM:

```lua
local vm = provium:vm("dc1", "peios-server")
                              -- profile name --^
```

```toml
# provium.toml — a static profile (paths already on disk)
[profiles.peios]
kernel       = "/path/to/peios/test-kernel"
initrd       = "/path/to/peios-test-initrd"
cmdline      = "console=hvc0 quiet"
guest_os     = "peios"                       # optional; defaults "peios"

# A static profile that reads its cmdline from an image builder's file
[profiles.peios-server]
kernel       = "../peiso/out/root/boot/vmlinuz"
initrd       = "../peiso/out/initrd.img"
cmdline_file = "../peiso/out/root/boot/cmdline"
cmdline      = "role=server"                 # appended after the file

# A dynamic profile that builds its own artifacts first (see below)
[profiles.peios-full]
build        = "peiso build manifests/peios-full.toml --out {out}"
kernel       = "{out}/root/boot/vmlinuz"
initrd       = "{out}/initrd.img"
cmdline_file = "{out}/root/boot/cmdline"
```

#### Schema

| Field | Required | Description |
|---|---|---|
| `kernel` | yes | Path to a Linux bzImage (or PVH ELF) for direct boot via QEMU's `-kernel`. |
| `initrd` | yes | Path to initramfs. Provium overlays its agent + wrapper at boot — your initrd is not modified. |
| `cmdline` | one of `cmdline`/`cmdline_file` | Inline kernel command line. Boot opts can override via `kernel_cmdline`. |
| `cmdline_file` | one of `cmdline`/`cmdline_file` | Path to a file whose contents are the **base** cmdline (e.g. an image builder's generated `cmdline`). Whitespace, including newlines, collapses to single spaces; the inline `cmdline` (if any) is appended after, so inline tokens win for last-wins kernel params (`init=`, `loglevel=`). Read at boot; resolved relative to cwd. |
| `guest_os` | no, default `"peios"` | Selects which embedded agent port to inject. Only `"peios"` exists in v1. |
| `inject_agent` | no, default `true` | When true, concatenate the agent-overlay cpio onto `initrd` and append `rdinit=/sbin/provium-agent`. Set false if the initrd already contains the agent. |
| `agent_overlay_path` | no | Override the agent-overlay cpio location. |
| `build` | no | Shell command (run with `sh -c`) that produces this profile's artifacts, executed **once before any VM boots**. See *Dynamic profiles*. |
| `build_out` | no | Override the `{out}` directory. Default: `$XDG_CACHE_HOME/provium/builds/<profile>/`. |

#### Dynamic profiles (`build` + `{out}`)

A profile may declare a `build` command that produces its own boot
artifacts, so the suite always tests a freshly-built image instead of
whatever stale files happen to be on disk. This is the same anti-drift
principle as `cmdline_file`, applied to the whole image.

The literal token `{out}` — in the `build` command **and** in the path
fields — expands to the profile's build-output directory (`build_out`,
or the per-profile default). Because the build's `--out` and the
`kernel`/`initrd`/`cmdline_file` it reads come from the *same* token,
they cannot drift: one source of truth, expanded once at config load.

Semantics:

- **Runs once, before any VM boots.** For `provium` (the test runner),
  every profile with a `build` is built up front — tests pick profiles
  at runtime from Lua, so provium can't statically know which a run will
  use. For `console`/`repl`, only the named profile is built.
- **No staleness tracking.** provium runs the command every invocation
  and trusts the builder to be cheap when nothing changed; making
  rebuilds incremental is the **builder's** job, not provium's. Skip the
  hook with `--no-build` when you know the artifacts are current.
- **A non-zero exit aborts the run.** provium never falls through to
  booting stale outputs after a failed build.
- **provium never wipes `{out}`.** The build command owns its output
  directory (so it can keep its own caches there).

Two commands round this out:

- `provium prepare [PROFILE]` — run build commands without booting. With
  no `PROFILE`, builds every profile that declares one. Skips the
  network/KVM preflight, so images can be built on a machine without the
  VM-test prerequisites.
- `provium --no-build` — skip all build hooks for this run.

The build command runs from provium's cwd with inherited stdio and
environment; reference inputs (manifests, etc.) relative to the suite so
the suite stays portable — there is deliberately no `build_cwd`.

#### Three layers of configuration

- **Profile** (immutable, named in `provium.toml`): kernel, initrd,
  cmdline, guest_os — the identity of what's booting
- **VM opts** (passed to `provium:vm(...)`): memory, cpus — VM
  resource identity, declared at creation, claimed at boot
- **Boot opts** (passed to `vm:boot(...)`): files, rng_seed,
  initial_time, kernel_cmdline override — boot-specific runtime
  configuration

#### Why the schema is QEMU-shaped

QEMU is the only VMM in v1. The Rust `VMM` trait stays pluggable in
code (so a future backend can be added by implementing the trait),
but the **profile schema is honest about what's there now**. We don't
carry speculative "VMM-agnostic" config without a real second backend
to validate the abstraction against.

If/when a second VMM is added, the schema grows — likely via
backend-tagged sections (`[profiles.peios.firecracker]`) — designed
against the real second case rather than against hypotheticals.

#### Future fields (when needed)

Not currently included; document here so the path is clear:

- `console` — output channel selection (hvc0 / serial / tty)
- `boot_protocol` — explicit PVH/legacy override
- `secure_boot`, UEFI fields — when test scenarios require them
- Backend-tagged sections — when a second VMM lands

### Workspace structure

Provium is a Cargo workspace shipping multiple binaries:

```
provium/                  # core: scheduler, file runners, agent IPC,
                          #       Lua bindings, event emission
provium-protocol/         # shared crate: op types + event types
                          #       (depended on by every other binary)
provium-agent/            # the guest-side agent binary
provium-coverage/         # consumer: reads event stream, groups by
                          #       metadata key (default "spec"),
                          #       produces coverage report
```

Future siblings as needed (`provium-junit` for CI XML,
`provium-html-report`, etc.). All ship together as one release
artifact set.

The architectural commitment is: **provium core is fully agnostic**.
It knows about VMs, scheduling, and runner-affecting metadata
(`slow`, `tags`, `timeout`). Anything else — coverage, fancy
reports, CI integration — is a sibling consumer reading the event
stream. First-party consumers use exactly the same API as any
third-party consumer would.

---

## Resource model

Resources are independent objects. `provium` itself is a Lab (via
metatable indirection); sub-labs are also Labs. All resources are
tracked by the global lab (provium) plus any sub-labs they were added
to.

| Resource | Owner | Notes |
|---|---|---|
| Lab | provium / parent lab | Snapshottable, includes other resources, can include sub-labs (recursive) |
| VM | provium / lab | Snapshottable, pausable, owns NICs/disks/console |
| Process | VM | First-class; `vm:run` (sync) returns RunResult, `vm:run_async` returns Process |
| File | VM | Open-file semantics: read/write/seek/close |
| Stream | proc/file/fd/nic/link/console | Tap on a source; first-class lifecycle |
| Worker | VM | Sub-agent (process or thread), same guest API as VM |
| Clock | VM | Per-VM guest time control |
| NIC | VM (created by `bridge:attach`) | Hypervisor-side: counters, capture, link state |
| Bridge | provium / lab | L2/L3, partition, latency, drop, capture, optional uplink |
| Disk | VM | Hypervisor-side: sector I/O, fault-injection, attach/detach |
| Console | VM | Hypervisor-side bidirectional serial |
| Snapshot | VM / Lab | Transient handle; persisted via `.fixture.lua` files |

### Agnosticism principle

Provium primitives are either:

1. **Hypervisor-level** — operate on a VM from outside, no guest
   cooperation required (VM lifecycle, NIC counters, disk sectors,
   console, snapshot, pause/resume)
2. **OS-agnostic guest-level** — work via universal userspace
   concepts (process, file, fd, syscall, ioctl)

Anything that bakes in OS-specific configuration (cgroups, /etc files,
sysctl, registry, peinit ioctls) lives in **per-OS Lua helper
libraries**, never in provium core.

Layer 0 ops (raw `syscall`/`ioctl`) are OS-specific by content but
agnostic at the wire level. Layer 1 ops (`open`, `read`, `exec`) take
abstract inputs the agent translates to native flags.

---

## API surface

### Top-level

```lua
-- provium IS a lab (via __index fall-through to a hidden global Lab)
-- All lab methods (vm, bridge, lab, boot, snapshot, ...) work on provium

-- Factory functions (dot, no self)
provium.vm_fixture(path)             -- resume single-VM fixture
provium.lab_fixture(path)            -- resume lab fixture

-- Binary helpers
provium.pack(fmt, ...) -> bytes
provium.unpack(fmt, bytes) -> ...

-- Test framework
test(name, [meta,] fn)               -- fn receives a test context: function(t) ... end
todo(reason?)                        -- declarative skip at file scope

-- Polling helper (global, not an assertion)
wait_until(predicate, opts?) -> any
-- opts: {timeout=10, interval=0.1, desc="condition"}
-- calls predicate repeatedly until it returns truthy
-- returns predicate's return value; raises on timeout
-- predicate errors propagate immediately (no retry)
```

### Test context (`t`)

The function passed to `test()` receives a single argument `t` — a
test context object carrying assertions, logging, and metadata
access. This avoids shadowing Lua's built-in `assert` and makes the
test-framework dependency explicit.

```lua
test("name", function(t)
  local r = vm:run("ls")
  t:assert(r:ok())
  t:assert_eq(r.exit_code, 0)
  t:assert_contains(r.stdout, "etc")
end)
```

API on `t`:

```lua
-- Assertions (raise → test failure with rich context)
t:assert(cond, msg?)
t:assert_eq(a, b, msg?)
t:assert_neq(a, b, msg?)
t:assert_contains(haystack, needle, msg?)
t:assert_raises(fn, msg?)            -- expect fn to raise
t:fail(msg)                          -- explicit failure
t:skip(msg?)                         -- skip from inside a running test

-- Logging
t:log(msg)                           -- attach a log entry to this test's result event

-- Metadata access (read-only fields, plain dot form)
t.name                               -- "this test's name"
t.meta                               -- {spec=..., tags=..., ...} from test() call
```

Method calls use colon (`t:assert(...)`) so the framework can
deliver `self`. Field reads stay dot (`t.name`). Calling
`t.assert(cond)` with a dot would pass `cond` as `self` and an
empty `cond` slot — the assertion silently always passes.

Helpers that need to assert receive `t` explicitly:

```lua
function peios.check_role(t, vm, expected)
  local r = vm:run("peiosctl get-role")
  t:assert(r:ok())
  t:assert_eq(r.stdout:trim(), expected)
end

test("...", function(t)
  peios.check_role(t, lab.dc1, "domain_controller")
end)
```

Lua's built-in `assert` is preserved (not shadowed). Library code
that uses it gets standard error-raising behaviour; test code uses
`t:assert` for the test-failure variant.

### Lab (and provium, since provium is a lab)

```lua
-- Constructors (create and include in this lab)
lab:vm(name, profile, vm_opts?) -> VM        -- 2+ args: create
lab:bridge(name, opts?) -> Bridge            -- 2+ args: create
lab:lab() -> Lab                             -- sublab, included as member

-- Membership
lab:include(resource_or_list)
lab:remove(resource)
lab:members() -> [resources]

-- Accessors (look up existing)
lab:vm(name) -> VM                           -- 1 arg: lookup; errors if not found
lab:bridge(name) -> Bridge                   -- 1 arg: lookup; errors if not found
lab.<name>                                   -- shorthand: lab.dc1. Reserved keys (cannot be used as VM/bridge/sub-lab names if you want dot-access): "vm_fixture", "lab_fixture", "pack", "unpack". Use lab:vm(name)/lab:bridge(name) for those.

-- Lifecycle (recurse through sublabs)
lab:boot()                           -- atomic batch boot of all member VMs
                                     -- (one resource acquisition; idiomatic
                                     -- pattern to avoid hold-and-wait)
lab:shutdown()
lab:pause() / lab:resume()           -- parallel across VMs; see Lab snapshot model
lab:snapshot() -> LabSnapshot        -- captures all VMs + bridge state
lab:restore(snap)                    -- restores VMs, bridges, attachments, impairments

-- Resource declaration / scheduling hint
lab:claim({memory, cpus})            -- block until reservable; see Scheduler

-- Coordination
lab:barrier(name, count)
```

**Constructor vs accessor disambiguation.** `lab:vm(...)` and
`lab:bridge(...)` overload on arity:

- **1 arg** (just a name) → accessor; looks up an existing resource;
  errors if not found
- **2+ args** (name + profile / opts) → constructor; creates and
  registers a new resource; errors if the name is already in use

For VM the disambiguation is reinforced by `profile` being required
to create. For Bridge there are no required-beyond-name args, so
"create with all defaults" still uses the 2-arg form with an explicit
opts table: `lab:bridge("lan", {})`. In practice this is rare —
bridges are configured via `attach()` / `route()` after creation
rather than via opts, so the bare 1-arg form is the lookup case
the test author actually wants.

`lab.<name>` shorthand always works for lookup of named resources
and is the idiomatic accessor in test code.

### VM

VMs are constructed via `lab:vm(name, profile, opts?)` (or
`provium:vm(...)` since provium is a lab). The `opts` table holds
**VM resource identity** — memory and CPU count — declared at
creation, claimed at boot, immutable for the VM's lifetime.

```lua
provium:vm(name, profile, vm_opts?) -> VM
-- vm_opts: {memory="2G", cpus=4}
```

```lua
-- Lifecycle
vm:boot(boot_opts?) -> VM            -- returns self for chaining
                                     -- boot_opts: {files, rng_seed,
                                     --              initial_time, kernel_cmdline}
vm:shutdown()
vm:pause() / vm:resume()
vm:reset() / vm:power_button()
vm:snapshot() -> Snapshot
vm:restore(snapshot)

-- File ops (Layer 1, portable)
vm:open_file(path, mode) -> File     -- mode: {read, write, create,
                                     --        truncate, append, exclusive}
vm:read_file(path) -> bytes
vm:write_file(path, data)
vm:listdir(path) -> [{name, entry_type}]
vm:mkdir(path, parents?)
vm:unlink(path)
vm:rename(from, to)
vm:stat(path) -> {size, mtime, mtime_ns, perm, entry_type}     -- mtime in seconds (float); mtime_ns full nanosecond precision; perm POSIX mode bits

-- Process ops (Layer 1)
vm:run(cmd, opts?) -> RunResult                      -- sync; see RunResult below
vm:run_async(cmd, opts?) -> Process                  -- async; equivalent to run minus the wait

-- Raw ops (Layer 0)
vm:syscall(nr, args?, bufs?, ptrs?) -> {result, out_bufs}
vm:ioctl(fd, cmd, data?, ptrs?) -> {result, out_data, out_bufs}

-- Streams
vm:tail_file(path, opts?) -> Stream                  -- opts: {start = "end" (default) | "beginning" | <byte_offset>}
vm:fd_stream(fd) -> Stream

-- Resource accessors
vm:nic(name) -> NIC
vm:disk(id) -> Disk
vm:console() -> Console
vm:clock() -> Clock

-- Resource constructors
vm:attach_disk(spec) -> Disk
vm:spawn_worker(opts?) -> Worker     -- opts: {thread=true}
```

### Why creation and boot are separate

`provium:vm(...)` allocates a VM object in the resource graph and
records its profile + opts. It claims **no host resources** and does
not spawn any process. `vm:boot()` is what actually acquires
resources from the pool, spawns QEMU, loads kernel +
initrd, and waits for the agent.

The split exists because configuration must happen *before* boot:

```lua
local dc1 = provium:vm("dc1", "peios")
local dc2 = provium:vm("dc2", "peios")
local lan = provium:bridge("lan")
lan:attach({dc1, dc2})                -- network topology before boot
provium:boot()                        -- atomic batch boot of both VMs
```

Without the split, networking attachments would be hot-attach
(messy, the guest sees a NIC appear mid-life) and `provium:boot()`
couldn't do atomic batch boot — each VM would acquire resources
independently with hold-and-wait risk.

For single-VM tests where this distinction is just extra typing,
`:boot()` returns self so it chains:

```lua
local vm = provium:vm("test", "peios"):boot()
local vm = provium:vm("test", "peios", {memory="2G"}):boot()
```

### VM state machine

```
Created --boot()--> Booted/Running --pause()--> Paused
                          |                         |
                          |                         resume()
                          |<------------------------+
                          |
                          shutdown() / power_button() / kill on file end
                          |
                          v
                      Shutdown (terminal)
```

Operations are state-checked. Calling an op in the wrong state
raises a clean Lua error naming the current state and the
expected transition.

| From state | Op | Result |
|---|---|---|
| Created | `boot()` | → Booted |
| Created | any guest op (run, syscall, ...) | error: "VM not booted" |
| Booted | `boot()` | error: "VM already booted" |
| Booted | `pause()` | → Paused |
| Booted | `shutdown()` / `power_button()` / `reset()` | → Shutdown / restart |
| Booted | guest ops | normal |
| Paused | `resume()` | → Booted |
| Paused | `boot()` / guest ops | error: "VM is paused; use resume()" |
| Paused | `snapshot()` | normal (already paused) |
| Shutdown | `boot()` | error: "VM is shutdown; create a new one" |

Snapshot/restore work on Booted or Paused states; the operation
internally pauses if needed and restores afterward.

### Bridge

```lua
-- Membership
bridge:attach(vm or [vms])           -- L2: VMs become broadcast-domain members
bridge:route(bridge or [bridges])    -- L3: other bridges routed through this (v2; v1 records graph state only)
bridge:detach(vm)                    -- triggers link-down on guest

-- Per-attachment impairments
bridge:isolate(vm) / bridge:unisolate(vm)

-- Pair partitions (symmetric or directional)
bridge:partition(a, b)               -- symmetric: block a↔b
bridge:partition({from=a, to=b})     -- directional: block a→b only
bridge:unpartition(a, b)
bridge:unpartition({from=a, to=b})

-- Whole-bridge impairments
bridge:add_latency(ms)               -- whole bridge
bridge:add_latency({from=a, to=b, ms=N})        -- directional
bridge:drop_rate(p)                  -- whole bridge
bridge:drop_rate({from=a, to=b, p=N})           -- directional
bridge:bandwidth_limit(bps)          -- whole bridge
bridge:bandwidth_limit({from=a, to=b, bps=N})   -- directional (v2; v1 raises NotSupported — needs class-based htb)
-- v1 limitation: combining bandwidth_limit(bps>0) with add_latency or
-- drop_rate on the SAME bridge is graph-state only. Plain `tc tbf`
-- can't compose with `tc netem` as a peer root qdisc; the combined
-- shape needs class-based htb (deferred to v2). When this combo is
-- set, the in-memory state still records both knobs (so snapshot /
-- restore preserve them) but only the netem (latency+drop) qdisc
-- lands on the host. Lab authors needing combined shaping should
-- pick one axis or wait for v2.
bridge:partition_all() / bridge:restore_all()   -- cut/heal whole bridge. restore_all only undoes partition_all; explicit pair partitions and isolation survive (use bridge:reset for impairments+partitions, or bridge:unisolate(vm) for isolation)
bridge:reset()                       -- clear all impairments + partitions

-- Uplink (NAT to host network)
bridge:enable_uplink() / bridge:disable_uplink()

-- Observation
bridge:capture() -> Stream
```

All impairment ops accept either a primitive arg (number, etc.) for
whole-bridge effect, or a table with `from`/`to` keys for directional
effect targeting the relevant TAP filter chain. The directional form
enables realistic asymmetric failure testing (one-way drops, lopsided
latency) which is what most distributed-systems bugs actually
manifest under. Symmetric remains the common-case shorthand.

**v1 directional simplification:** when multiple `(from=vm, to=*)`
rules are set for the same source VM, the per-tap netem qdisc
applies the worst (max) latency and max drop across them — true
per-pair shaping needs class-based htb + filter and is v2.
Bandwidth_limit's directional form likewise raises NotSupported in
v1.

### Process

```lua
proc:wait(timeout?) -> RunResult
proc:kill(signal?)                   -- defaults to "term" when arg omitted; abstract names: "term"/"kill"/"stop"/"cont"/"hup"/"int"/"quit"/"usr1"/"usr2"/"alrm"/"pipe"/"chld"/"winch"; numeric also accepted
proc:signal(sig)                     -- like proc:kill but the signal arg is required
proc:pid() -> int
proc:status() -> "running" | "exited"
proc:stdout_stream() -> Stream
proc:stderr_stream() -> Stream
proc:stdin_write(data)
proc:close_stdin()
proc:close()
```

### RunResult

Returned by `vm:run(...)` and `proc:wait(...)`.

```lua
result.exit_code     -> int          -- normal exit code; -1 if killed by signal, -2 if timed out
result.stdout        -> string
result.stderr        -> string
result.status        -> "exited" | "signalled" | "timed_out"
result.signal        -> int | nil    -- POSIX signal number when killed by signal, else nil
result.timed_out     -> bool         -- true when the agent killed the process for timeout
result:ok()          -> bool         -- shortcut: exit_code == 0 and status == "exited"
result:assert_ok()                   -- raise if not ok(), with stdout/stderr in message
```

`:ok()` and `:assert_ok()` exist so tests don't have to write
`assert_eq(r.exit_code, 0)` everywhere; `if r:ok() then ...` and
`vm:run("foo"):assert_ok()` are the idiomatic patterns.

### File

```lua
file:read(n) -> bytes
file:read_all() -> bytes
file:write(data) -> n_written
file:seek(off, whence?)              -- whence: "set"/"cur"/"end"
file:tell() -> int
file:close()
file:fd() -> int                     -- raw fd for syscall/ioctl
file:tail_stream() -> Stream
```

### Stream

```lua
stream:next(timeout?) -> frame       -- nil on eof
stream:read_until(pattern, timeout?) -> match
stream:expect(pattern, timeout?)     -- assertion variant; raises on timeout
stream:drain(timeout?) -> [frames]
stream:close()
stream:eof() -> bool
```

### NIC, Disk, Console, Snapshot, Clock, Worker

```lua
-- NIC (hypervisor-side)
nic:counters() -> {rx_bytes, tx_bytes, rx_packets, tx_packets, errors}   -- all zero before the bridge is realised on the host (no /sys/class/net entry yet); call after lab:boot()
nic:capture() -> Stream
nic:disconnect() / nic:reconnect()

-- Disk (hypervisor-side)
disk:read_sectors(off, n) -> bytes
disk:write_sectors(off, data)
disk:size() -> bytes
disk:fault_inject(mode)              -- "eio_read"/"eio_write"/"slow"/...
disk:clear_faults()
disk:detach()

-- Console
console:read() -> Stream
console:read_log() -> string         -- legacy non-streaming snapshot of the console log; tests that need streaming should use :read()
console:write(data, opts?)           -- send keystrokes; opts: {timeout=duration} bounds the kernel-side write. The legacy `timeout_ms` key is rejected — use `timeout = "500ms"` or `timeout = 0.5`.
console:expect(pattern, timeout?)
console:close()

-- Snapshot (transient handle)
snap:delete()
snap:size() -> bytes

-- Clock (per-VM guest time)
clock:get() -> seconds (float)
clock:set(t)                         -- absolute time, seconds since epoch
clock:sleep(duration)                -- duration: seconds (number) or "500ms"/"5s"/...
clock:advance(duration)              -- advance guest clock by duration

-- Worker (sub-agent, same VM API for guest ops)
worker:run() / :run_async() / :open_file() / :syscall() / ...
worker:kill(sig?)                    -- sig: int | string ("SIGTERM" etc.). Default SIGTERM; bad-type errors loudly.
worker:join() -> exit_status         -- terminal: invalidates the worker handle. exit_status is the worst integer exit code across the worker's reaped async children (0 if none ran or all exited cleanly)
worker:handle() -> int               -- raw handle id; useful for diagnostics/logging
worker:close()                       -- auto-close hook called by the scope walker; SIGTERM + join. Idempotent.
```

**v1 worker isolation (open-file scoping):** `worker:open_file`
allocates the file handle in the parent VM's table rather than a
dedicated per-worker file table. Subsequent `file:read` /
`:write` / `:close` ops therefore work transparently — the same
handle is routable from outside the worker too. Dedicated
per-worker file ops are deferred to a later slice; the worker
boundary in v1 is a bookkeeping namespace for `worker:kill` /
`worker:join`, not a hard isolation barrier.

### Naming conventions

- **Constructors** (create resources): include resource name in
  method (`vm`, `bridge`, `lab`, `open_file`, `spawn_worker`,
  `attach_disk`). Exception: `run`/`run_async` for processes —
  `run` predates the convention and is too natural to rename.
- **Accessors** (retrieve existing): use resource name as method
  (`vm:nic("eth0")`, `lab:bridge("lan")`, `lab.dc1`)
- **Actions** (do work, return data): natural verbs (`run`, `read`,
  `wait`, `kill`, `partition`, `expect`)

### Time and timeouts

Every API that takes a time duration (timeouts, sleeps, latencies,
clock control) accepts:

- **Numbers** — seconds (float for sub-second). `5` = 5s, `0.5` = 500ms
- **Strings** with unit suffix — parsed at the API boundary:
  `"500ms"`, `"5s"`, `"5m"`, `"2h"`

Internally everything is normalised to seconds (float). The string
form is for readability when the unit isn't obvious from context.

```lua
stream:expect("ready", 5)            -- 5 seconds
stream:expect("ready", "500ms")
clock:sleep(0.1)                     -- 100ms
wait_until(fn, {timeout="30s"})
proc:wait("10m")
```

### Error model

- **Infrastructure failures raise** — vsock disconnect, schema
  mismatch, VMM crash. Tests can't recover; fail loud.
- **Operation results return** — process exit codes, syscall errno,
  file read 0 bytes. Tests assert on results explicitly.

### Cleanup

- Every resource has idempotent `:close()`
- All resources auto-close at test end (provium-as-lab tracks them)
- Streams **must** be closed before snapshot (enforced by checking
  open connection count at snapshot time)

---

## Test organisation

### File suffixes drive discovery

```
*.test.lua       discovered + run automatically
*.fixture.lua    discovered as fixture builders, referenced by path
*.lua            ignored by discovery; only loaded via require()
```

Provium walks configured test roots recursively. Filenames declare
their role; directory structure is at the user's discretion.

### Fixture references are paths

```
tests/kacs/fixtures/kacs_warm.fixture.lua
                    ↓
provium.vm_fixture("kacs/fixtures/kacs_warm")
```

Path relative to test root, with `.fixture.lua` stripped. Forward
slashes (filesystem-style, distinct from `require`'s dot syntax).
Filesystem enforces uniqueness — no name-collision logic in provium.

### Helpers are plain Lua

`require("helpers.kacs")` loads `helpers/kacs.lua` from any directory
on `package.path`. Provium adds the test root to `package.path` at
startup. Standard Lua module system; nothing provium-specific.

### Config

```toml
# provium.toml
[provium]
roots = ["tests"]                    # scan for *.test.lua and *.fixture.lua
                                     # also added to package.path

[profiles.peios]
kernel  = "/path/to/peios/test-kernel"
initrd  = "/path/to/peios-test-initrd"
cmdline = "console=hvc0 quiet"

# Additional profiles as needed (peios-server, peios-router, ...)
```

See **Profiles** under Components for the full profile model.

### Test-level metadata

```lua
test("name", { spec = "PSD-KACS §4.2.1.1", tags = {"federation"} }, function()
  -- ...
end)
```

Provium stores arbitrary metadata and attaches it to test-result
events. **Provium core itself only interprets keys that affect
runner behaviour** — everything else is opaque, passed through to
event consumers.

Runner-interpreted (in core):
- `slow = true` — skipped by the runner unless `--include-slow`
- `tags = {...}` — filterable via `--tag` / `--no-tag`
- `timeout = N` — per-test timeout override

Consumed externally (not interpreted by core):
- `spec = "..."` — read by `provium-coverage`, a sibling tool
  (see Workspace structure / Observability)
- Anything else — stored on events; user-written consumers can read
  whatever they want

This keeps provium core fully OS- and project-agnostic. Even
"spec coverage" is just a worked example of an external consumer.

### Within-file behavior

Tests within a file run **sequentially**. They share VMs (the file's
lab is one set of resources). State reset between tests is via
snapshot/restore on a live VM (~50ms), not fixture re-resume from
cold cache (~200ms).

By default, tests inherit whatever state the previous test left
behind — provium does no automatic reset. For most files that's
fine (tests are independent observations) or the test cleans up
after itself.

### Auto-reset between tests

Files where every test should start from the same baseline state
opt in via a per-file flag:

```lua
provium.reset_between_tests = true

-- Top-level setup runs once
local vm = provium:vm("test", "peios"):boot()
vm:run("setup_environment.sh")
vm:write_file("/etc/test/config", "...")

-- Snapshot is taken here, after top-level code returns

test("a", function(t)
  vm:run("destructive_op")             -- mutates state
end)
-- Auto-restore before "b"

test("b", function(t)
  -- vm back in post-setup state, NOT post-"a"
end)
```

Mechanics:

1. The file's chunk executes; top-level code runs once
2. `test(name, fn)` calls *register* their fn (don't run inline)
3. After the chunk returns, if `reset_between_tests = true`:
   - Take `provium:snapshot()` — captures all VMs, bridges, sub-labs
4. For each registered test: run it; restore from the snapshot
   afterward (before the next test)

Same precondition as any snapshot: no open streams at the moment
the auto-snapshot is taken. File-scope streams must be opened
after the snapshot (i.e., inside test bodies) or the file errors
with the usual snapshot diagnostic.

What it doesn't do:
- Doesn't reset host-side Lua state — variables, helpers, and
  references at file scope persist across tests. Usually what you
  want.
- Doesn't run hooks. For custom per-test setup beyond "restore to
  baseline," do it in the test body.
- Doesn't fit files with multiple distinct baselines (some tests
  from "warm," others from "warm+user"). Those use explicit
  `provium.vm_fixture(...)` per-test.

The `provium.reset_between_tests` flag is **per-file naturally**:
each file runner thread has its own mlua state, so the flag is
scoped to that state — setting it in one file has no effect on any
other file's runs.

#### Mutually exclusive with file-scope streams

A file cannot use both `reset_between_tests = true` AND open
file-scope streams (streams created at file scope outside any
`test()` block). The auto-snapshot taken after top-level code
returns has the standard "no open streams" precondition; a
file-scope stream open at that moment fails the snapshot.

Provium catches this at file-load time with a clear error:

```
foo.test.lua: cannot use reset_between_tests with file-scope streams
  - audit_subscribe at foo.test.lua:7 opens a stream at file scope
  - reset_between_tests would auto-snapshot after this stream is open
  Either move the stream into a test() block, or set reset_between_tests = false.
```

Pick one pattern per file. Tests that need both shape would split
into separate files.

### Spec annotation

`spec = "..."` is just opaque metadata to provium core — stored and
attached to `test_passed` / `test_failed` events, never interpreted.
The coverage report is produced by `provium-coverage`, a separate
sibling tool that reads the event stream (see Workspace structure).

This means:

- Provium core has zero knowledge of "spec" semantics
- The coverage tool runs against the same events as any other
  consumer; it has no privileged access
- A run can be replayed from saved events:
  `provium --save-events run.msgpack`,
  `provium-coverage --from run.msgpack`
- Coverage groups by any metadata key, defaulting to `spec`:
  `provium-coverage --by spec` (default), `--by ticket`, `--by feature`
- Free-form string values; the consumer groups by exact match with
  no format imposed

---

## Fixtures

### Fixture file returns a snapshot

```lua
-- kacs_warm.fixture.lua
local vm = provium:vm("kacs", "peios")
vm:boot()
vm:console():expect("kacs initialised")
return vm:snapshot()
```

```lua
-- ad_domain.fixture.lua
local dc1 = provium:vm("dc1", "peios-server")
local dc2 = provium:vm("dc2", "peios-server")
local lan = provium:bridge("lan")
lan:attach({dc1, dc2})
provium:boot()
peios.set_role(dc1, "domain_controller")
peios.set_role(dc2, "replica")
return provium:snapshot()
```

The framework runs the file, captures the returned `Snapshot` or
`LabSnapshot`, persists it, tears down build-phase resources.

Files **must** return a snapshot. Returning anything else is a
fixture-build error.

### Lazy build with file-locking

Fixtures build **on first reference**, not in an upfront phase. When
`vm_fixture("path")` is called:

1. Compute cache key = hash of (builder file + transitively-required
   helpers + provium version + kernel/agent identifier + external
   host-file deps declared via `vm:push_file` / `lab:depends_on_file`)
2. If cache hit, resume the cached snapshot
3. If miss, acquire build lock for this fixture path
4. With lock held, run the builder, cache the snapshot, release lock
5. If lock was already held by another file, wait until released,
   then re-check cache (will hit)

Cache invalidation: any input hash change → key differs → miss →
rebuild.

#### Eviction policy

- **Default cache directory**: `~/.cache/provium/fixtures/`,
  configurable via `provium.toml`: `cache_dir = "..."`
- **Default size cap**: 20 GB, configurable via
  `provium.toml`: `cache_max_size = "20G"` (number-or-string per the
  time/size convention)
- **Eviction order**: LRU on access time (most-recently-resumed =
  freshest, last to be evicted)
- **When GC runs**: at provium startup, before scanning tests.
  Lists cache entries by access time, evicts oldest until total
  size is under the cap. Cheap (filesystem stat only).
- **Manual override**: `provium fixture clean` wipes the entire
  cache regardless of policy

#### Provium version churn

The cache key includes the provium binary version. During provium
development, every rebuild of provium itself invalidates the entire
fixture cache. This is correct (a new provium might emit different
events, embed a different agent, etc.) but means dev-loop iteration
on provium core has a fixture-rebuild cost. For test-author
iteration (only test files change), the cache stays warm.

### Dependency tracking

Fixtures can reference other fixtures:

```lua
-- kacs_with_user.fixture.lua
local vm = provium.vm_fixture("kacs/fixtures/kacs_warm")
peios.create_user(vm, "test")
return vm:snapshot()
```

The cache key for `kacs_with_user` includes the cache key of
`kacs/fixtures/kacs_warm`. Rebuilding the parent invalidates all
derivatives.

**Dependency ordering is positional.** The cache key folds dep
keys in source order — reordering the `vm_fixture(...)` /
`lab_fixture(...)` calls in a fixture file invalidates the cache
even if the *set* of dependencies is unchanged. This is a
conservative choice: order-insensitive hashing would need a
canonicalisation pass (sort by key) and the over-invalidation
under reordering is cheap (rebuild on next reference).

**Multi-profile invalidation.** The cache key folds in EVERY
profile's `kernel` + `initrd` identifier (path + size + mtime),
not just the first. A kernel swap on any profile invalidates
every fixture so a guest never restores against a stale kernel.
The over-invalidation when only one profile's kernel changes is
the safe failure mode (R9 sched-m4).

**External host-file deps.** Fixtures regularly pull host-side
artifacts into the guest — a binary under test, a config
template. The cache key folds in every host file declared by
`vm:push_file("host", "guest", opts?)` (auto-tracked unless the
call site contains the literal `auto_dep = false`) and every
file declared explicitly with `lab:depends_on_file("host")`
(no-op at runtime; exists purely as a static-scan marker).
Folded as path + mtime + size, same shape as kernel/initrd.

Detection is by static scan of fixture and require'd-helper
sources for literal string arguments. Non-literal paths
(variables, concatenation) are NOT detected — declare them
with a sibling `lab:depends_on_file("…")` literal call, or fall
back to `provium fixture rebuild`. Relative paths are resolved
against the directory of the source file containing the call,
not the runtime cwd.

### No `save_as_fixture` API

Fixtures are produced exclusively by `*.fixture.lua` files. There is
no API to promote a transient snapshot to a fixture during a test.
Snapshot objects are transient handles for within-test rollback only.

---

## Wire protocol

### Shared Rust crate, msgpack on the wire

Both host and agent are Rust binaries. The "schema" is a shared
`provium-protocol` crate containing every op as a `serde`-derived
struct pair (request + response). Both binaries depend on the same
crate; agent and host can never disagree about layout because there
is one definition.

```rust
// provium-protocol::wire — actual current shapes (R9 D1).
// Argument structs are flat; result structs use the OpResult
// envelope so the success-side shape is decoupled from the
// OS-error path.

pub struct ExecArgs {
    pub cmd: String,
    pub args: Vec<String>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    #[serde(default)]
    pub env_clear: bool,
    pub cwd: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub stdin: Vec<u8>,
    #[serde(default)]
    pub timeout_ms: Option<u64>,
}

// ExecResult is OpResult-shaped: Ok(ExecOk) | Err(OsError).
pub struct ExecOk {
    pub status: ExitStatus,         // Exited(i32) | Signalled(i32) | TimedOut
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

pub struct OpenFileArgs {
    pub path: String,
    pub mode: OpenMode,
    pub create_perm: Option<u32>,   // POSIX mode bits
}

pub struct OpenMode {
    pub read: bool,
    pub write: bool,
    pub create: bool,
    pub truncate: bool,
    pub append: bool,
    pub exclusive: bool,
}

// Result envelopes for OS-fallible ops:
// OpResult<T> serializes as {"outcome": "ok"|"err", "value": ...}
pub enum OpResult<T> { Ok(T), Err(OsError) }
```

Wire format: msgpack frames via `rmp-serde`. The host envelope is
`HostMessage` (one variant per op); the agent envelope is
`AgentMessage` (one variant per result + every error envelope).
Each variant serializes as `{"kind": "<snake_case_tag>",
"payload": ...}` so consumers can match on a single string field.

Adding an op:

1. Add request/result structs to `provium-protocol::wire`
2. Add `HostMessage` request variant + `AgentMessage` result variant
3. Add the handler match arm in `provium-agent::connection`
4. Add a thin Lua-binding wrapper in the host
5. Bump `PROTOCOL_VERSION` (and update the
   `tests/conformance_protocol_pin.rs` byte-shape expectations
   to match the new wire) if the change is wire-visible

Three edits per op, no codegen tooling, no DSL, no risk of agent and
host disagreeing about the layout.

### Protocol version handshake

The first op on every connection is implicitly a `Hello` exchange
carrying a `protocol_version: u32` constant baked into
`provium-protocol`. The agent rejects any connection whose version
doesn't match its own with a clean error:

```
agent: protocol version mismatch (host: 7, agent: 6)
```

Defends against version-skew bugs that the cache key check should
prevent in theory but might miss in practice (cross-machine
fixture loads, forced cache hits, version-check bugs).

`protocol_version` is bumped whenever any struct in
`provium-protocol` changes shape — it's just the `provium-protocol`
crate's major version, ensuring agent and host always agree on what
they think the wire looks like.

### Two layers

- **Layer 0** — raw: `syscall(nr, args, bufs, ptrs)`, `ioctl(fd, cmd,
  data, ptrs)`. OS-specific by content; agnostic at the wire level.
  Each agent port supports the syscall numbers of its OS.
- **Layer 1** — portable userspace: `open_file`, `read`, `write`,
  `close`, `seek`, `run`, `run_async`, `wait`, `kill`, `gettime`,
  `settime`. Inputs are abstract (mode flags, signal names, whence
  enums); each agent port translates to native.

Layer 1 includes a small set of "common composite" ops (`exec`,
`read_file`, `write_file`) where round-trip cost matters and atomic
semantics make sense.

### Connectionless for short ops

Short ops (`exec`, `open_file`, `read`, `syscall`, `ioctl`, etc.)
open a fresh vsock connection, send one framed request, read one
framed response, and close. Agent runs `accept()` → handle → close →
`accept()` for these.

### Streams use a long-running connection

Streams (subscriptions, file tails, console reads, packet captures)
need to push frames over time, so they hold the connection open.
Connection lifetime = stream lifetime. The agent's `accept()` loop
spawns a handler thread per stream; the host's `Stream` Lua object
owns the connection and frees it on `:close()`.

This means the agent is **not uniformly connectionless** — when any
stream is open, there is an active connection between host and
agent. The snapshot model accommodates this explicitly.

### Snapshot precondition: no open streams

The host enforces "no open vsock connections to this VM" before any
snapshot operation. The model has three pieces:

**1. Resource graph tracking.** Every Stream has an owner — either
a test (created inside a `test()` block) or the file (created at
file scope, outside any test).

At creation, provium walks the Lua call stack to find the
**first frame whose source path is under the test root** (i.e.,
matches a `*.test.lua` or `*.fixture.lua` file, not a helper
under `helpers/`). That frame's source location is captured as
the stream's creation site. This way, helpers like
`peios.tail_audit(vm)` that internally call `vm:tail_file()` get
attributed to the test that called the helper, not the helper
itself — matching pytest's `tb_filter` pattern.

If no frame matches (e.g., stream created from a Rust callback or
deeply nested helper outside test source), the immediate caller's
frame is used as fallback.

**2. Snapshot fails on open streams.** `vm:snapshot()` and
`lab:snapshot()` walk every member's open-stream set first. Any open
stream — regardless of owner, including streams attached to workers,
console streams, and bridge captures — fails the call with named
offenders:

```
vm:snapshot() failed: 1 stream still open
  - tail_file("/log") created at peinit/services.test.lua:42 (test "foo")
  Close streams before snapshotting.
```

This is a *raise*, not an auto-close. Snapshotting silently while
streams are mid-flight loses state subtly; forcing the test to be
explicit is correct.

**3. Auto-close at end of scope.** Streams owned by a test are
auto-closed when the `test()` block returns; streams owned by the
file are auto-closed at file end. This catches the common case
(forgot `:close()` at the very end) without papering over mid-test
mistakes.

### Auto-close ordering

When a scope ends (test or file), the resource graph walks in
reverse-dependency order:

1. Streams (depend on procs/files/fds; close first)
2. Processes (reap before associated VMs)
3. Files (open-file handles)
4. Workers (separate vsock; like a sub-VM)
5. VMs and bridges only at file end (not at test end — they live
   for the file's lifetime)

Auto-close runs before any inter-test snapshot/restore, so the
common pattern (test-1 leaves a stream open, test-2 starts) sees a
clean state automatically.

### Cross-test stream sharing (file scope)

A stream created at file scope (outside any `test()` block) is owned
by the file and survives across tests. Useful for continuous capture:

```lua
local audit = vm:audit_subscribe()        -- file-scope, survives test boundaries

test("a", function() audit:expect("LOGIN")  end)
test("b", function() audit:expect("LOGOUT") end)
```

File-scope streams still fail snapshots if open. Tests that need
both file-scope streams and snapshots should snapshot at file start
before opening the streams.

### What "quiescent" actually means here

With no streams open and no in-flight short ops (the Lua side
finishes a request before issuing the next), the agent is in
`accept()` and its persistent state (job table, worker registry) is
stable. The host then:

1. Confirms no open connections (resource graph check)
2. Issues `pause` to the VMM (synchronous)
3. Snapshots
4. Resumes (or discards, depending on caller intent)

No sleep, no race, no quiesce ACK. The `200ms` sleep that produces
intermittent failures in the current provium is replaced by protocol
design + explicit tracking.

### What this still doesn't capture

The model captures the agent's protocol state and the guest's CPU
+ memory state. It does **not** automatically capture:

- In-flight kernel work the guest started before the last op
  completed (writeback, fsync, async I/O). Tests that depend on
  durability after restore should issue an explicit sync via
  `vm:run("sync")` before snapshotting.
- Bridge state (in-flight packets, netem queue, partition rules).
  Bridges are host-side; their state is captured separately by the
  Lab snapshot machinery (see Lab snapshot).

### Streams via long-running response

Each operation can stream multiple response frames over its
connection before closing:

```
host  → SUBSCRIBE(audit_log)
agent → event frame
agent → event frame
agent → event frame
... (until host closes connection)
```

Push semantics within an op's lifetime. Stream lifecycle =
host-controlled connection lifetime. Snapshot precondition: no open
connections. Tests close streams before snapshot; framework verifies.

---

## Lab snapshot model

A Lab contains VMs, bridges, and (recursively) sub-labs. QEMU
snapshots are per-VM only; bridges are host-side; "atomic across
members" needs a precise definition.

### What atomic means here

Strong atomicity (all VMs frozen at the same wall-clock instant) is
impossible — QEMU's pause is per-VM via QMP over a unix socket, and
each VMM has its own QMP socket. Even parallel `stop` calls from N
host threads each take their own round-trip. Drift between first and
last pause is small in practice but **not strongly atomic** and not
bounded to a specific number we can promise.

What we provide instead is **useful atomicity**: the captured state
represents a consistent *quiescent* point of the system. Combined
with the precondition that the lab is logically quiescent at call
time (no open streams, agents in `accept()`, tests have received
all responses they were waiting on), the result is consistent for
any test that doesn't depend on sub-millisecond cross-VM timing.

Tests requiring sub-millisecond cross-VM timing relationships are
out of scope for the snapshot model; they should use other
synchronization (barriers, explicit coordination) and not expect
snapshot/restore to preserve them.

### `lab:snapshot()` operation

```
1. Walk member tree; confirm no open streams (existing precondition)
2. Spawn N threads (one per VM) coordinated by a startup barrier
3. All threads fire `pause` near-simultaneously
4. Wait for all pauses to complete
5. Capture bridge state to a metadata file
   (attachments, impairments, uplink config, names)
6. In parallel, snapshot each VM to its own file
7. Resume all VMs in parallel (or discard, if for fixture build)
```

### `lab:restore()` operation

```
1. Read metadata file: bridge configs, VM list, attachments, impairments
2. Create bridges with their captured config (no impairments yet)
3. Restore each VM in parallel from its snapshot file
4. Re-attach VM TAPs to bridges
5. Apply impairments
6. Resume all VMs in parallel
```

### What the snapshot captures

- **Per-VM state**: memory, CPU registers, virtual device state,
  attached disk state — everything QEMU's per-VM
  snapshot captures
- **Per-bridge state**: attachments, impairments (latency, drop
  rate, bandwidth limit, partition rules), uplink config, names

### What the snapshot does NOT capture

- **In-flight kernel work** the guest started before the last op
  completed (writeback, fsync, async I/O). Same caveat as single-VM
  snapshot. Tests requiring durability across snapshot/restore
  should `vm:run("sync")` first.
- **In-flight network packets** in netem queues at snapshot time.
  Dropped, not preserved. Recipients would see retransmits if the
  protocol does so. For partition-recovery testing, the partition
  *rule* is captured (link is still down on restore); whatever was
  queued is gone.
- **Pause-drift sub-millisecond timing** between VMs (see "What
  atomic means here" above). Tests depending on cross-VM timing
  within tight windows shouldn't expect snapshot/restore to preserve
  them.
- **Wall-clock continuity.** Each guest restores with its clock at
  the time of snapshot; if host wall-clock has moved on, guests
  start "in the past" until NTP, `clock:set()`, or `clock:advance()`
  catches them up.

### Out of scope

- **Strong (sub-millisecond) atomicity** across VMs. Would require
  custom paravirt coordination beyond QEMU's API.
- **In-flight packet capture/replay.** Snapshotting netem queue
  state is technically possible but brittle and rarely useful.

---

## Scheduler

### Runtime is dynamic; static analysis (if added) is pure optimisation

The scheduler's foundation is **dynamic synchronous resource
acquisition** with blocking. Every resource-using call (`boot()`,
`claim()`, `attach_disk()`) acquires from a global pool. If the
request fits, take it. If not, the call blocks until resources free.

A file with no annotation behaves correctly through the dynamic path
alone. Static analysis (when added in v2 as an optimisation) only
informs dispatch ordering; it never changes runtime behaviour or
becomes required.

### Pool

```
Pool = { memory, cpus }
```

Memory and CPU are both hard-gated. vsock CIDs are u32 — effectively
unlimited, not tracked as a pool resource. Allocation: provium
maintains a single atomic counter starting at 100 (skips well-known
0/1/2 plus buffer); `vm:boot()` reserves the next value and passes
it to QEMU at start. CIDs are never recycled — at u32
scale we would not exhaust the space in practice. Single-source
allocation means collisions are structurally impossible. The counter
is in-process and resets on provium restart; it's not persisted.

Other resources (KVM fds, TAP devices, file descriptors) are
provisioned high enough at startup that they don't bottleneck. See
the startup pre-flight check for systemic prerequisites.

Default pool budget:
- Memory: 80% of host RAM (leaves headroom for OS / external load)
- CPUs: host cores (1× — strict, no oversubscription by default)

CPU strictness: total declared vCPUs across all dispatched files
must not exceed `host_cores × overcommit_factor`. Default overcommit
factor is **1.0**. Override via `--cpu-overcommit 1.5` (or 2.0, etc.)
to allow oversubscription if the test workload tolerates scheduling
jitter.

Why 1× by default: KVM guests doing busy-loops or polling saturate
their assigned vCPUs at 100%; multiple oversubscribed VMs cause CFS
to thrash, introducing scheduling latency that flakes timeout-based
tests. Kubernetes hit this; nextest warns about it. Conservative
default trades capacity for predictability — and most tests have
idle guests anyway, so 1× rarely bottlenecks.

### SCHED_BATCH for VMM processes

After spawning a QEMU child, provium calls
`sched_setscheduler(pid, SCHED_BATCH, 0)` on it. This is Linux's
batch scheduling class: same priority as SCHED_OTHER, but the
kernel reduces preempt frequency for these processes (throughput
over latency). VM workloads are batch by nature; this aligns the
kernel scheduler with that reality.

Result: lower scheduling overhead even at 1× CPU, smoother behavior
if a user opts into 1.5× or 2× via the flag.

`nice` is *not* applied — it would deprioritize VMs against other
host work, which isn't what we want. We want VMs to get full CPU
when they need it; SCHED_BATCH just changes *how often* the kernel
context-switches them.

Linux-only. Provium's host platform is Linux (KVM requires it
anyway), so no portability concern.

`SCHED_BATCH` lowers scheduling latency-sensitivity, which doesn't
require `CAP_SYS_NICE`. If the syscall fails for any reason
(unusual permission setup, custom security policies), provium logs
the failure and continues without setting the policy — same
behaviour as if SCHED_BATCH were unavailable. No hard dependency.

### Acquisition

```python
def pool.acquire(amount):
    with lock:
        while not amount.fits_in(pool.available):
            cond.wait()
        pool.available -= amount

def pool.release(amount):
    with lock:
        pool.available += amount
        cond.notify_all()
```

`boot()`, `claim()`, etc. call `pool.acquire(...)`. VM shutdown,
file-runner exit call `pool.release(...)`.

### Per-file overhead

Each file runner reserves a small fixed amount (default 50 MB) on
startup for runner + bookkeeping. Released on completion. Bounds how
many files can run simultaneously even with zero VMs.

### Per-VM overhead

Each `boot()` reserves declared VM memory + ~100 MB VMM overhead.
QEMU enforces per-VM memory caps internally. Overhead
released on VM shutdown.

### `claim()`

```lua
provium:claim({memory="4G", cpus=8})
```

Synchronous: blocks until 4 GB + 8 CPUs are reservable, then takes
them as an atomic file-level reservation. Subsequent `boot()` calls
within the file consume from this reservation without further
blocking (assuming they fit).

If a `boot()` would exceed the claim, the excess goes through the
pool with the same wait-if-insufficient semantics. Claim is a
**guaranteed minimum, not a hard cap.**

This means `claim()` only fully prevents hold-and-wait when the
file's peak is ≤ the claimed amount. If a file claims 4 GB and a
later `boot()` requests 6 GB, the excess 2 GB is acquired
incrementally from the pool while the claim is still held — same
hold-and-wait risk as un-claimed sequential boots. The per-file
timeout is the backstop in that case.

For full atomicity, claim the *peak* you'll use, not a lower bound:

```lua
-- Predictable peak: claim it all up front
provium:claim({memory="6G"})         -- covers everything below
local vm1 = provium:vm("a", ...):boot()  -- 4G, draws from claim
local vm2 = provium:vm("b", ...):boot()  -- 2G, draws from claim
```

`claim()` is the escape hatch for files that:
- Have weird/dynamic resource patterns
- Want to avoid hold-and-wait deadlock by atomically reserving peak
- Use significant host-side Lua memory beyond their VMs

Most files don't need it.

**One-shot per file.** A second `claim()` call within the same file
errors with "claim already taken" — claims are atomic file-level
reservations, not adjustable mid-run.

### Deadlock — the only real concern

Hold-and-wait can occur if a file does sequential `boot()` calls and
multiple files do this concurrently:

```lua
test("bad", function()
    local vm1 = provium:vm("a", ...):boot()  -- holds 2G
    local vm2 = provium:vm("b", ...):boot()  -- waits for 2G; holds vm1's 2G
end)
```

Mitigations:

1. **Idiomatic batch boot.** `provium:boot()` boots all declared VMs
   atomically — one acquisition, no hold-and-wait. Documentation and
   examples make this the obvious pattern.
2. **`claim()` at file top** when peak is predictable.
3. **Per-file timeout** (default 5min, configurable) as backstop. If
   a file's hold persists past the timeout, kill it, mark as timeout
   failure. Other files continue. The pool releases on kill.

Idiomatic test patterns avoid deadlock by construction; the timeout
catches the rare bad cases.

### No cgroups

QEMU hard-caps per-VM memory. Inference being wrong about
a VM's actual use causes guest-side OOM (test fails cleanly), not
host-side OOM. The only way a misbehaving file can affect others is
by pulling huge amounts of data into host-side Lua, exceeding the
file's claim.

This is bounded by `claim()` correctness (test author's contract)
and is rare in practice. Cgroup defense-in-depth doesn't justify the
implementation cost or cgroups v1/v2 portability burden.

### Adaptive host pressure

Background thread reads `/proc/pressure/memory` (PSI — Pressure
Stall Information) every ~1s. PSI reports the percentage of time
processes were stalled waiting on memory; it's purpose-built for
this, doesn't lag the way `MemAvailable` does, and doesn't oscillate
under page-cache churn or KSM activity. Bazel originally used
MemAvailable polling for similar adaptive scheduling and reverted
because of those exact problems.

Heuristic: if `some avg10 > 10%` (10% of recent time stalled on
memory), pause new dispatches. Already-running files keep running
with their reservations. Resume dispatching when the metric drops.

CPU pressure (`/proc/pressure/cpu`) is monitored on the same
cadence and OR'd into the throttle flag — either signal pauses
new dispatches. Memory remains the primary signal in practice;
the CPU axis catches workloads where CPU contention shows up
before memory does (heavily-oversubscribed pools).

Linux-only. On non-Linux hosts (development on macOS via remote VM,
etc.) we fall back to no adaptive throttling — the configured pool
budget is the limit.

PSI requires `CONFIG_PSI` in the host kernel (default-on in mainline
since 4.20, but not always set in custom/embedded distros). Provium
checks for PSI availability at startup; if absent, falls back to no
adaptive throttling and logs a notice.

### Failure mode catalogue

| Failure | Detection | Response |
|---|---|---|
| `boot()` request > host budget | Computed at acquisition | Fail test (impossible to satisfy) |
| `claim()` > host budget | Computed at acquisition | Fail file at claim time |
| `boot()` blocks waiting | Normal flow | Wait, resume when freed |
| File deadlock (hold-and-wait) | Per-file timeout | Kill file, mark timeout |
| VM exceeds declared memory | QEMU cap | Guest OOM, test fails, others continue |
| Test pulls excessive Lua memory | Host OOM (rare) | Author's bug; fix `claim()` |
| Rust panic in file runner thread | `catch_unwind` at thread entry | Currently-running test fails; remaining tests in same file marked `failed-due-to-poisoned-state` (mlua state can't be safely reused); file runner tears down + exits; other file threads continue |
| Provium itself crashes (segfault, panic-abort, signal) | OS / parent | All VM children die; scheduler exits non-zero with partial results |
| Agent crashes / hangs in guest (kernel panic, agent panic, killed) | vsock disconnect or op timeout | In-flight op raises clean error; VM marked dead; subsequent ops on it fail immediately; console-stream content captured for diagnostic |
| Snapshot/protocol version mismatch (cache key collision; stale snapshot) | Verified at restore (snapshot metadata vs current versions) | Treat as cache miss; rebuild |
| Disk OOS during snapshot | ENOSPC on write | Atomic write via tmp+rename; ENOSPC deletes tmp, raises clean error; no partial snapshot in cache |
| vsock CID collision | Cannot occur — single-source allocation | n/a |
| Networking systemic (CAP_NET_ADMIN missing, kvm/vhost-vsock module absent) | Pre-flight check at startup | Provium exits immediately with actionable error; never enters test phase |
| Bridge op fails per-test (TAP exhaustion, tc rule rejection) | tc/iproute2 errors propagate | Bridge op raises in test; just that test fails |
| Fixture rebuild storm (parallel files block on shared lock) | `fixture_build_waiting` events | Consumers (CLI / TUI) surface the wait with the holding file; tests still serialise on the lock |
| Stream / file / process leaks (un-closed at test end) | Auto-close walks resource graph at scope end | Reverse-dependency cleanup at test end (test-scope) and file end (file-scope); no fd accumulation across long runs |
| QEMU QMP hangs (control plane wedge) | Per-call timeout on QMP commands | Treat VM as dead; mark in-flight ops failed; surface diagnostic; subsequent ops on this VM fail immediately |
| Snapshot file corruption discovered at restore (distinct from version mismatch) | Restore reads metadata header / decompresses; checksum or parse error | Treat as cache miss; rebuild fixture; surface warning |
| Inbound migration stuck in `inmigrate` (corrupt snapshot, kernel/initrd mismatch the restored guest expects, QMP wedge) | Restore polls `query-status` with a 60s deadline; status never leaves `inmigrate` | Tear down QEMU, surface error with console tail; caller treats as cache miss and rebuilds |
| Fixture build memory spike (build VMs unaccounted in pool) | None at v1 — fixture builds bypass pool reservation | Same-fixture concurrent builds serialise via file lock; different fixtures may double-book actual memory. Mitigation: keep fixture VMs small; future work threads the pool through to `build_fixture` |

### What's explicitly NOT in v1

- **Static cost analysis as primary mechanism** (rejected; runtime is
  the foundation, static is a future optimisation layer)
- **Cgroup enforcement** (QEMU caps suffice)
- **Suspend/resume for opportunistic packing** (snapshot-to-disk cost
  exceeds expected benefit)
- **Mid-run resource claiming** (atomic at acquisition only)
- **Cycle/deadlock detection** (per-file timeout suffices)
- **Speculative dispatch** (atomic only)
- **Per-test reservation** (file is the unit; tests within share)
- **Pre-built fixture phase** (lazy with file-locking)

---

## Performance

### Order of optimisation work

1. **Sparse + compressed fixture snapshots** — standalone, clear
   win, no kernel/host config required
2. **KSM** — QEMU's `-object memory-backend-ram,...,merge=on` enables
   `MADV_MERGEABLE`; combined with host-side ksmd tuning, lets common
   pages across VMs be deduplicated
3. **Snapshot cache on tmpfs** — RAM-backed fixture cache (fits
   naturally with sparse snapshots)
4. **Stripped kernel + initramfs** — minimal Peios test kernel for
   fast cold boot
5. **Op batching** — multiple ops per wire round-trip via explicit
   `vm:batch(fn)` block
6. **Selective re-run / `--since`** — file-dependency-based test
   selection for dev loop

### Sparse + compressed snapshots (priority 1)

Most VM memory is zero pages. Snapshot writer:

- Sparse files: zero pages elided (filesystem holes)
- zstd compression on non-zero pages

Realistic savings: 1 GB naive snapshot → ~80–150 MB on disk, ~5x
faster cold load.

Implementation: post-process QEMU's `migrate file:` output with
`dd conv=sparse` + `zstd`. (QEMU writes a linear migration stream;
sparseness comes from filesystem-level hole detection.)

Differential snapshots (delta from baseline fixture) are deferred to
v2.

### KSM (priority 2)

Multiple QEMU VMs running similar Peios kernels share
most pages. KSM merges them transparently. Realistic savings: 20–40%
real memory reduction.

Implementation:

- Use QEMU's `-object memory-backend-ram,id=mem,size=...,merge=on
  -machine memory-backend=mem` so the backend calls `madvise(MADV_MERGEABLE)`
  on its mapping
- Tune `ksmd` aggressively at provium startup:
  ```
  /sys/kernel/mm/ksm/run = 1
  /sys/kernel/mm/ksm/pages_to_scan = 1000
  /sys/kernel/mm/ksm/sleep_millisecs = 20
  ```
- Flag `--no-ksm` to disable for users who don't want the CPU
  overhead (sets `mergeable=off`)

### What's deferred

- Snapshot cache on tmpfs, stripped kernel, selective re-run —
  cover with separate work items as they become bottlenecks.
- **Op batching** — landed in v1 as `vm:batch(fn)`. Collects ops
  inside the callback and ships one round-trip; results come back
  as `{ok=...}` / `{err=...}` per op preserving call order.
- **Pre-warmed fixture pool** — keep idle pre-resumed VMs of common
  fixtures, hand out instantly. Skipped for now; revisit if
  fixture-resume cost is the dominant test-runtime bottleneck.

---

## CLI

### Default action: run tests

```
provium                              # run all *.test.lua under roots
provium tests/kacs/                  # path filter
provium tests/kacs/tokens/creation.test.lua
provium --filter creation
provium --tag federation
provium --no-tag slow
provium --include-slow
provium --rerun-failed
provium --fail-fast
provium --mem 8G                     # override memory pool budget
provium --cpu-overcommit 1.5         # allow 1.5× CPU oversubscription (default 1.0)
provium --coverage                   # convenience: pipe events to provium-coverage
provium --save-events run.msgpack    # persist event stream for later replay
provium --events-socket /tmp/p.sock  # multiplex events to live consumers
provium --events-stdout              # emit raw msgpack frames to stdout (mutually exclusive with --json)
provium --watch                      # rerun on file change
provium --since <ref-file>           # only run files newer than the reference's mtime
provium --filter <substr>            # restrict discovery to paths matching substr
provium --rerun-failed               # only re-run paths from prior run's failure state file
provium --timeout 10m                # per-file watchdog; accepts seconds or duration string ("30s"/"10m"/"2h"/"500ms")
provium --json                       # structured output
provium -v / -q                      # verbosity
```

### Subcommands

```
provium fixture list                 # cached fixtures + size + hash
provium fixture build [path]         # explicit build
provium fixture rebuild [path]       # force rebuild
provium fixture clean                # wipe cache
provium fixture stale                # list invalidated by builder change

provium repl <profile>               # boot VM, drop into Lua REPL
provium repl --fixture <path>        # resume a fixture in REPL

provium console <profile>            # boot VM, attach terminal to its serial console (qemu-direct)
provium console <profile> --agent    # ...and inject the vsock agent overlay too
provium console <profile> --print-command   # dry run: print the assembled QEMU argv
provium console <profile> -- <qemu-args>     # forward extra args verbatim to QEMU

provium list                         # show discovered tests
provium list --fixtures              # show discovered fixtures
```

The `console` subcommand is the qemu-direct escape hatch: it wires
QEMU's serial port + monitor onto the controlling terminal
(`-serial mon:stdio`, `-display none`) with no QMP control plane, so
you interact with the guest exactly as you would a hand-rolled
`qemu-system-x86_64 -kernel … -initrd …`. `Ctrl-A X` quits, `Ctrl-A C`
toggles the monitor. Unlike `repl`, the agent overlay is opt-in
(`--agent`) — a bare boot uses the profile's initrd as-is.

### Sibling tools (separate binaries, same release)

```
provium-coverage [--from <events>] [--by <key>]
                                     # coverage report; default --by spec
                                     # reads stdin if no --from
```

(Future: `provium-junit`, `provium-html-report`, etc. — all consume
the event stream the same way.)

### Output

- Default: human-readable, colour, one line per test, expanded blocks
  on failure
- `--json`: machine consumption (CI integration)
- `-v`: shows passes + timing
- `-q`: hides passes

Failure reporting bundles: test name, `spec` annotation, Lua stack
trace, captured console output, stream contents at failure point.

### REPL

Productivity feature. Boot a VM (or resume a fixture), drop into a
Lua prompt against it.

```
$ provium repl peios
provium> local vm = provium:vm("test", "peios")
provium> vm:boot()
provium> vm:run("ls /etc")
{exit_code=0, stdout="...", stderr=""}
```

```
$ provium repl --fixture kacs/fixtures/kacs_warm
(VM resumed in <1s)
provium>
```

Most test development is "set up state, try a thing, check what
happened." REPL collapses that loop.

### Intentionally not in CLI

- `init`/scaffold — project structure is so simple it doesn't need
  scaffolding
- Test creation helpers — `touch foo.test.lua` and start writing
- CI-specific modes — `--json` + exit codes are sufficient

---

## Observability and event stream

Provium's scheduler and runners emit a structured event stream
describing everything that happens during a run. The stream is the
**stable public API** for any consumer that needs to reason about
test progress or scheduler decisions:

- The plain-text and `--json` CLI outputs (built into core)
- `provium-coverage` (sibling tool — see Workspace structure)
- Future first-party tools (`provium-junit`, `provium-html-report`,
  the deferred TUI) — same API as any third-party consumer
- Custom user-written consumers

### Event categories

`file_dispatched` fires AFTER pool acquisition completes (i.e.
the file is now actually running). A file that blocks on the
pool will emit `file_blocked` first, then `file_dispatched`
once the wait clears.

```
file_discovered       { path, fixture_refs, declared_claim }
file_dispatched       { path, reservation }
file_blocked          { path, waiting_for: {memory_bytes, cpus}, reason }   -- reason ∈ {"pool_full","psi_pressure"}
file_completed        { path, status, duration_ns }
test_started          { path, name, meta }            -- `meta.spec` is a conventional key
test_passed           { path, name, duration_ns, meta }
test_failed           { path, name, reason, console_excerpt, duration_ns, meta }
test_skipped          { path, name, reason, meta }    -- todo() / t:skip() / tag/slow/meta.skip filter
vm_spawned            { file, vm_name, profile, memory_bytes, cid }
vm_shutdown           { file, vm_name, duration_ns }
pool_state            { used, available }            -- periodic snapshot
claim_acquired        { path, amount }                 -- pair via `path`
claim_released        { path, amount }
fixture_build_started { path }
fixture_build_done    { path, duration_ns, snapshot_bytes }
fixture_build_waiting { path, held_by_file }         -- another file holds the build lock
fixture_cache_hit     { path }
```

Format: msgpack frames. Each frame has `{kind, ts, payload}`. The
schema is **versioned and stable** — it's the public API consumers
depend on.

The `rmp-serde` version is pinned in the workspace `Cargo.toml`
(both producers and consumers depend on the same version). Saved
event files (`--save-events`) replay correctly with any consumer
built against the same `provium-protocol` + `rmp-serde` major.
Cross-major migration would need a small upgrade tool; not v1.

Emission targets (selected via flag, can be combined):

- **stdout** (default) — for piping to a single consumer
- **`--save-events <file>`** — persist for later replay or analysis
- **`--events-socket <path>`** — unix socket for live multiplexing
  to multiple connected consumers

### Why first-class events

**Separation of concerns.** The scheduler/runners produce events; the
presentation layer consumes them. Either can change without breaking
the other.

**Replay.** Save the event stream from a run, replay through any
consumer later. Useful for analysing scheduler decisions, regressions,
flaky tests.

**Multiple simultaneous consumers.** A single run can feed plain-text
output to the terminal, JSON to a CI artifact, and (later) the TUI —
all reading the same stream.

### Default consumer behaviour

- TTY stdout, no flags: plain-text human output (the existing
  pass/fail line format)
- `--json`: JSON-lines output suitable for CI / pipelines
- `--quiet` / `-q`: failures only
- `--tui`: the deferred TUI consumer (when implemented)

### TUI (deferred)

A ratatui-based terminal UI that visualises the event stream:
- Pool state with memory/CPU usage bars
- Running files with their reservations and current test
- Queued files with blocking reasons
- Recent results with pass/fail and durations
- Drill-in panes for a specific file (its VMs, console output,
  active streams) and for failures (assertion, console excerpt,
  Lua stack)

Architecturally additive: the TUI consumes the same events the CLI
already produces. Schedule for v1.5 or later; the v1 commitment is
the event stream itself.

---

## Resolved decisions

(Items previously open; settled during design refinement.)

### MADV_MERGEABLE for KSM

QEMU supports it via a memory backend with `merge=on`:
`-object memory-backend-ram,id=mem,size=...,merge=on -machine memory-backend=mem`.
The backend calls `madvise(MADV_MERGEABLE)` on its mapping. No
upstream patch, no wrapper-side madvise needed.

### Wire protocol — shared Rust crate, no codegen

Both host and agent are Rust. The "schema" is a shared
`provium-protocol` crate containing all op types as `serde`-derived
structs. Both binaries depend on the crate; the wire is msgpack
(`rmp-serde`). Adding an op = add a struct + a handler in the agent
+ a thin wrapper in the host's Lua bindings. No DSL, no codegen
tooling, no cross-language translation step.

### Per-file timeout

Default `"5m"`. Override at three layers, most-specific wins (all
accept the seconds-or-string convention):

- File-level: `provium.timeout = "5m"` (or `300`) at file top
- Test-level: `test("name", { timeout = "30s" }, fn)`
- CLI: `--timeout 10m` (suite-wide ceiling)

Tests that genuinely need longer document it explicitly.

### REPL line editing

rustyline for line editing: history file, bracket matching, basic
identifier completion. mlua introspection drives method-name
completion on `vm:`/`lab:`/etc. where reasonable. Syntax highlighting
deferred to v1.5+.

### Spec citation grouping

v1 uses exact-string matching for coverage rollup. Hierarchical
grouping (`§4.2.1.1` rolling up to `§4.2.1`) is nicer but requires
a parsing convention. Defer until real-world citation patterns are
visible; revisit in v1.5.

### Panic strategy

`panic = "unwind"` in both `[profile.dev]` and `[profile.release]`
of the workspace `Cargo.toml`. `catch_unwind` only works under
unwind; with `panic = "abort"` (common in release for binary size),
the thread-level panic isolation collapses. Slight binary-size cost
is worth the isolation.

### QEMU stays out-of-process

QEMU runs as a child process; provium controls it via QMP over a
unix socket (the `-qmp unix:<path>,server=on,wait=off` flag). On
every QMP connection, capabilities are negotiated, then
`migrate-set-capabilities` is sent with `events=true` so MIGRATION
state transitions are emitted as events (off by default, otherwise
you must poll `query-migrate`). Long-lived QMP connections must
track an event sequence mark before each command — `wait_event`
without a sync point will match stale events from prior operations
(see the spike's `provium-protocol` reference for the pattern).

Snapshots use `migrate file:<path>`; restores use
`-incoming "file:<path>"` on a fresh QEMU process. The exec-pipe
form (`exec:cat > file`) was tested in the spike and has a flush
race that produces truncated snapshots; do not use it.

Linking QEMU in-process is not an option (huge C codebase, no Rust
API). The QMP-over-unix-socket model is the production-tested
control plane; provium uses it directly.

---

## Pre-implementation spikes

All spikes are complete. Outcomes:

### Spike 1: mlua + per-thread states — passed

The thread-per-file architecture assumes each file-runner thread
owns an independent `mlua::Lua` state with no cross-thread sharing.

Confirmed in `provium/spikes/mlua-threads/`: 4 parallel mlua states
in 4 OS threads, each computing distinct values, no crosstalk.
A thread whose Lua script calls `error()` returns an `mlua::Error`
result without panicking; sibling threads are completely unaffected
(only Rust-side panics need `catch_unwind`). The mlua docs'
recommendation holds in practice.

### Spike 2: VMM choice — QEMU, not cloud-hypervisor

The fixture model and lab snapshot model both depend on
snapshot/restore being stable for our use case (custom kernel +
initramfs, vsock, multi-vCPU, pause → snapshot-to-disk → restore in
a fresh process).

**Cloud-hypervisor is fundamentally unsuitable.** CH's design
assumes live migration between two simultaneously-running VMMs;
the kill-and-restart workflow we need destroys host-side device
backend state. Restored guest state believes its vsock connections,
virtqueues, and FIFOs are still live; nothing is. ALL emulated I/O
wedges: vsock both directions, serial UART, virtio-console (kernel
printk path). No userspace recovery is possible because every
channel for a recovery signal is also wedged. Confirmed open
upstream issues: cloud-hypervisor #7263, #7759 — no fix in flight.

**QEMU passes everything.** Full validation in
`provium/spikes/ch-snapshot/RESULT.md`. Summary:

| Test | Result |
|---|---|
| Snapshot/restore preserves vsock (5 cycles) | counter monotonic |
| 4 parallel distinct-CID VMs | no conflicts |
| 40 VMs / 8 concurrent / CID reuse 5× per slot | zero EBUSY |
| Fixture fan-out: 1 snapshot → 8 parallel restores | all independent |
| CID release after SIGKILL mid-migration | ~330 ms |
| Same-CID collision: clean error, sibling unaffected | passes |
| Stream integrity at 10 ms cadence | 0 dupes, all monotonic |
| Bidirectional vsock across snapshot/restore | both intact |
| Event-driven QMP control with no settling sleeps | passes |

The spike also produced the QMP-control gotchas listed in the "QEMU
stays out-of-process" decision above (events capability, sequence
marks, `file:` URI). Those are mandatory; the spike showed the
failure modes when they're missing.

---

## Migration from current provium

Out of scope for this design doc. Worth a separate plan once the
target shape is settled. Rough shape:

- Rust agent + QEMU VMM as the new bones
- Tests migrate file-by-file (rename `*.lua` → `*.test.lua`,
  fixtures move to `*.fixture.lua` returning snapshots)
- API surface largely compatible at the call-site level (renames:
  `provium.create` → `provium.vm`, `vm:exec` → `vm:run`, etc.)
- Old binary protocol replaced with schema-driven; old wire-level
  hand-rolling deleted

---

## What this document is not

A spec. The schema, the VMM trait, the wire format details, the Lua
binding generation — those need their own documents when
implementation begins. This is the design intent and rationale; the
spec is what comes after refinement here.
