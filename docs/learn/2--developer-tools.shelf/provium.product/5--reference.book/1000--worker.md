---
title: Worker
description:
  A Worker userdata gives a test concurrent agent connections to the same
  guest VM, so you can drive multiple ops in parallel without spinning up
  another VM.
related:
  - provium/reference/vm
  - provium/reference/process
  - provium/reference/file-handle
  - provium/writing-tests/running-commands
---

A Worker is what `vm:spawn_worker()` returns: a sub-agent connection to the
same VM. It exposes the same VM-style API for running commands, opening files,
and issuing syscalls, handles allocated under it live in the worker's own
namespace on the agent side.

A worker is a real, separate guest process: a re-exec'd copy of the agent that
the main agent relays ops to over a socket pair. Its syscalls run with its own
credentials, its own kernel token, privileges and process security block, so a
test can stand up two distinct security principals in one VM: a caller and the
target of a process check, an unprivileged caller against a privileged
operation, two peers on a socket. Handles a worker allocates live in its own
table; the main agent routes them, so from the test's side they look like any
other handle.

## Constructing

Workers also run as separate host processes when using the local-agent
backend. The `provium` executable handles the worker protocol itself, so this
backend does not need a separate installed guest-agent binary. Rust
applications embedding `LocalAgentVmm` must select a worker-capable executable
with `LocalAgentVmm::new().with_worker_executable(path)` if their own
executable does not implement that protocol. The path can name `provium` or
`provium-agent`.

| Source                              | Returns                                                           |
| ----------------------------------- | ----------------------------------------------------------------- |
| `vm:spawn_worker()`                 | New worker on the VM's main agent.                                |
| `vm:spawn_worker({thread = false})` | Same. (`{thread = true}` is rejected with a future-work pointer.) |

## Methods

The worker mirrors the VM's run / file / syscall surface. Where semantics
differ from the VM equivalent, it's noted explicitly.

### `worker:run(cmd, opts?)`

Synchronous exec. Same call shape as
[`vm:run`](~provium/reference/vm#vm-run-cmd-or-args-opts). Returns a
[RunResult](~provium/reference/vm#runresult).

### `worker:run_async(cmd, opts?)`

Async spawn. Returns a [Process](~provium/reference/process). Like
`vm:run_async`, the `timeout` opt is rejected, pass it to `proc:wait` instead.

The child is forked and exec'd _by the worker_, so it starts life with the
worker's credentials: whatever token the worker installed, whatever privileges
it adjusted, is what the new program runs under. That is what makes exec-time
behavior testable from a principal the test minted, a token surviving or being
relabelled at exec, a descriptor without `FD_CLOEXEC` crossing into the new
image, a child's identity after its parent impersonated. `proc:pid()` gives
the child's pid for the main agent to inspect from outside.

`proc:wait`, `proc:kill`, `proc:pid`, `proc:status`, and the stdin methods all
work; the main agent relays them to the worker, and the worker performs them,
so they run with the worker's credentials _at that moment_. A worker that is
impersonating another user, or has installed a lowered token, might find
`proc:kill` on its own child refused with EPERM; revert or restore before
reaping. `proc:stdout_stream()` and `proc:stderr_stream()` do not work, a
stream owns its connection for its lifetime and the worker channel is a
request/reply pipe, so read a worker-spawned child's output from the
`RunResult` that `proc:wait` returns.

The returned Process is auto-registered with the test's resource registry, so
the scope walker SIGTERMs and reaps it at scope end. Without this, a
worker-spawned child would leak past the test boundary. Joining the worker
first orphans any child still running: it is reparented to PID 1 and the
Process handle cannot reach it.

### `worker:open_file(path, mode_table)`, rejected

There is no worker-scoped File. The call errors with
`worker_open_file: not supported for a process-isolated worker`, and the
reason is structural: a worker is a separate guest process with its own file
table, so a descriptor opened there would be out of reach of `file:read`,
`file:write` and `file:close`, which act through the parent agent.

Open files from inside the worker with [`worker:syscall`](#worker-syscall-nr)
instead, so the descriptor is created, used and closed in the worker's own
process under the worker's credentials:

```lua
-- x86-64 syscall numbers: openat = 257, write = 1, close = 3.
local AT_FDCWD, O_WRONLY_CREAT_TRUNC = -100, 0x241
local r = w:syscall(257, {
    args = { AT_FDCWD, 0, O_WRONLY_CREAT_TRUNC, tonumber("644", 8) },
    bufs = { "/tmp/from-worker\0" },   -- NUL-terminated path
    ptrs = { 1 },                       -- buf 1 → arg slot 1
})
local fd = r.ret
w:syscall(1, { args = { fd, 0, 2 }, bufs = { "hi" }, ptrs = { 1 } })
w:syscall(3, fd)
```

If the file only needs to be read or written by the _test_, and the worker's
identity does not matter, `vm:open_file` on the VM is the simpler call.

### `worker:syscall(nr, …)`

Direct syscall. Same call shape as
[`vm:syscall`](~provium/reference/vm#vm-syscall-nr), both the integer-only and
table forms work. Returns the same `{ret, result, errno, out_bufs}` shape.

### `worker:kill(sig?)`

Broadcast a signal to every async process spawned under this worker. Argument
shape mirrors `proc:kill`: nil → SIGTERM, int → that signal, string → friendly
name.

A bad argument type (for example a Lua table) errors with the type-name in the
message rather than silently defaulting to SIGTERM. This catches test-code
typos.

### `worker:join()`

Wait for every in-flight worker child to exit. Returns whatever the agent
reports as the joined exit summary.

### `worker:handle()`

Returns the worker's opaque handle id.

### `worker:close()`

Auto-close hook. SIGTERMs every in-flight worker child, then joins. The scope
walker fires this at scope end.

## Concurrency pattern

Workers shine for tests that need two threads of control inside the same
guest:

```lua
test("two concurrent writers don't tear", function(t)
    local vm = provium:vm("v", "peios"):boot()
    local w1 = vm:spawn_worker()
    local w2 = vm:spawn_worker()

    -- Each worker writes its own block of data, in parallel.
    local p1 = w1:run_async("dd", {args = {"if=/dev/urandom", "of=/tmp/a", "bs=1M", "count=8"}})
    local p2 = w2:run_async("dd", {args = {"if=/dev/urandom", "of=/tmp/b", "bs=1M", "count=8"}})

    p1:wait("10s"):assert_ok()
    p2:wait("10s"):assert_ok()

    t:assert_eq(vm:stat("/tmp/a").size, 8 * 1024 * 1024)
    t:assert_eq(vm:stat("/tmp/b").size, 8 * 1024 * 1024)
end)
```

For coordination between workers' guest processes, use a guest-side primitive
(file, fifo, and so on).
[`lab:barrier(name, count, timeout?)`](~provium/reference/lab#lab-barrier-name-count-timeout)
is a host-side rendezvous, a worker's guest processes can't call it. See
[Labs and scope, Barriers](~provium/writing-tests/labs-and-scope#barriers) for
what barriers can and can't synchronize today.

## See also


- [VM](~provium/reference/vm): `vm:spawn_worker()` and the parent surface.
- [Process](~provium/reference/process),
  [File](~provium/reference/file-handle), what worker ops return.
