---
title: Running Commands inside the Guest
type: how-to
description:
  Patterns for vm:run, shell form and direct exec, env, cwd, stdin, timeouts,
  plus vm:run_async process handles, signals, and workers.
related:
  - provium/reference/vm
  - provium/reference/process
  - provium/reference/worker
  - provium/writing-tests/streams-and-tails
---

Provium gives you several ways to run commands inside a guest. This page is
the practical guide; the canonical method reference is on
[VM](~provium/reference/vm).

## Sync exec: `vm:run`

The default. Returns a [RunResult](~provium/reference/vm#runresult) with
`exit_code`, `stdout`, `stderr`, `status`, `signal`, `timed_out`.

```lua
local r = vm:run("echo hello")
r:assert_ok()
t:assert_eq(r.stdout, "hello\n")
```

Two call shapes:

### Shell form: `vm:run(string)`

```lua
vm:run("echo hello | tr a-z A-Z")
vm:run("ls /etc/*.conf | head -3")
```

The string is run through `/bin/sh -c "<string>"` so shell metacharacters
work. Convenient for one-liners; quote-handling is the shell's problem.

### Direct exec: `vm:run(cmd, {args, ...})` or `vm:run(cmd, {arr})`

```lua
vm:run("echo", {args = {"hello"}})            -- canonical
vm:run("echo", {"hello"})                     -- legacy bare-array form
```

No shell. The first arg is the executable; positional args go in `args` (or as
a bare array when no opts keys are present). Use this when:

- The args contain shell metacharacters you don't want interpreted.
- You don't want a shell process in your tree (PID, signal handling, and so
  on).
- You're passing user-controlled data that you'd otherwise have to quote.

The detection between the two table forms is by recognized keys: the presence
of any of `env`, `env_clear`, `cwd`, `stdin`, `timeout`, `timeout_ms`, or
`args` selects the opts form; a table without any of them is treated as the
legacy direct-args array.

## Environment, cwd, stdin

```lua
vm:run("printenv FOO", {env = {FOO = "bar"}})
vm:run("ls", {cwd = "/etc"})
vm:run("cat", {stdin = "hello\n"})
```

Combine freely:

```lua
local r = vm:run("python3 -c 'import os, sys; print(os.environ[\"X\"], file=sys.stderr); print(sys.stdin.read())'", {
    env  = {X = "from-env"},
    stdin = "from-stdin\n",
})
t:assert_eq(r.stderr, "from-env\n")
t:assert_eq(r.stdout, "from-stdin\n")
```

`env_clear = true` makes the guest see ONLY the keys you supplied:

```lua
vm:run("env", {env = {PATH = "/usr/bin"}, env_clear = true})
```

Without `env_clear`, your `env` merges with the agent's environment.

## Timeouts

Two equivalent keys: `timeout_ms` (int, milliseconds) and `timeout` (number
seconds, or string with suffix).

```lua
vm:run("sleep 100", {timeout_ms = 500})        -- TimedOut
vm:run("sleep 100", {timeout = 0.5})           -- Same
vm:run("sleep 100", {timeout = "500ms"})       -- Same
vm:run("sleep 100", {timeout = "5s"})          -- 5s
vm:run("sleep 100", {timeout = "1m"})          -- 1 minute
```

When the timeout fires, the agent kills the command and returns a RunResult
with `status = "timed_out"`, `timed_out = true`, and `exit_code = -2`. Use
`r.timed_out` to disambiguate from a clean exit with code -2.

Each command runs in its own process group, and the timeout SIGKILLs that
whole group. This matters most for the shell form: `sh` commonly forks its
command rather than exec'ing it, so the work you asked for is a grandchild of
the agent. If it survived the kill it would keep the captured stdout and
stderr pipes open, and `vm:run` would stay blocked until it finished on its
own, long past the timeout you set.

```lua
local r = vm:run("flaky", {timeout = "2s"})
if r.timed_out then
    t:fail("flaky timed out")
elseif not r:ok() then
    t:fail("flaky failed: " .. r.stderr)
end
```

## RunResult fields and helpers

Most tests only need three things from a RunResult:

```lua
local r = vm:run("…")

r:assert_ok() -- raise if not ok; message includes status, stdout, stderr
r.stdout      -- captured stdout (bytes)
r:ok()        -- shorthand for exit_code == 0
```

When a command can fail in more than one way, check `r.status` (`"exited"` /
`"signalled"` / `"timed_out"`) rather than pattern-matching `exit_code`, the
sentinel exit codes and the `signal` field are documented in the
[RunResult reference](~provium/reference/vm#runresult).

## Async exec: `vm:run_async`

Returns a [Process](~provium/reference/process) userdata immediately. The
agent does NOT auto-kill; you control the lifetime.

```lua
local proc = vm:run_async("python3", {args = {"server.py"}})
-- … do other things …
proc:kill("term")
local r = proc:wait("2s")
```

The opts shape mirrors `vm:run`'s, **except** that passing `timeout` /
`timeout_ms` is rejected, use `proc:wait(timeout)` instead. (Silently honoring
`timeout` here would be a footgun: the agent doesn't auto-kill, so the timeout
would do nothing.)

### Stdin pipe

```lua
local proc = vm:run_async("cat")
proc:stdin_write("hello\n")
proc:stdin_write("world\n")
proc:close_stdin()
local r = proc:wait("5s")
t:assert_eq(r.stdout, "hello\nworld\n")
```

### Streaming stdout / stderr

```lua
local proc = vm:run_async("server")
local out  = proc:stdout_stream()
out:expect("listening on port 8080", "10s")
-- now exercise the server
```

See [streams and tails](~provium/writing-tests/streams-and-tails) for the full
stream API.

### Signals

```lua
proc:kill()                  -- defaults to SIGTERM
proc:kill(9)                 -- SIGKILL by number
proc:kill("kill")            -- by name
proc:kill("usr1")            -- SIGUSR1
proc:signal("usr2")          -- alias for kill(); reads better for non-fatal signals
```

Signals are accepted by friendly name (`term`, `kill`, `usr1`, …), with a
`sig` prefix (`sigterm`), or as a bare integer. The full recognized-name list
is in the [Process reference](~provium/reference/process#proc-kill-sig).

The signal goes to the process you spawned, not to the process group it leads,
`proc:signal("usr1")` reaches that one process, as you asked. So a shell that
backgrounded its work keeps that work running. To take the whole tree down,
give `proc:wait` a timeout: that path SIGKILLs the group.

### Inspecting the process

```lua
proc:pid()       -- live kernel PID inside the guest
proc:handle()    -- opaque agent-side handle id
proc:status()    -- non-blocking poll
```

`pid()` calls into the agent every time. `handle()` is a stable in-memory id
that never changes.

### Waiting

```lua
proc:wait()              -- wait forever
proc:wait("5s")          -- wait at most 5 seconds
proc:wait(0.5)           -- 500 ms
```

Don't pass `0`, the harness rejects it and points you at `proc:status()` for
non-blocking polling. The
[Process reference](~provium/reference/process#proc-wait-timeout) explains why
a literal-zero timeout is a footgun.

## Workers: parallel commands in the same VM

`vm:spawn_worker()` returns a [Worker](~provium/reference/worker), a sub-agent
connection. Useful for driving the same VM from multiple threads of control:

```lua
local w1 = vm:spawn_worker()
local w2 = vm:spawn_worker()

-- Two writers in parallel.
local p1 = w1:run_async("dd if=/dev/urandom of=/tmp/a bs=1M count=8")
local p2 = w2:run_async("dd if=/dev/urandom of=/tmp/b bs=1M count=8")

p1:wait("10s"):assert_ok()
p2:wait("10s"):assert_ok()
```

Workers expose the same surface as the VM (`run`, `run_async`, `open_file`,
`syscall`, `kill`, `join`, `close`). Files and processes allocated under a
worker live in the worker's namespace; cleanup is per-worker.

For coordination between workers' guest processes, use a guest-side primitive
(file on a shared mount, fifo, network message).
[`lab:barrier(name, count, timeout?)`](~provium/reference/lab#lab-barrier-name-count-timeout)
is a host-side rendezvous and can't be reached from inside a guest, see
[Labs and scope, Barriers](~provium/writing-tests/labs-and-scope#barriers).

## Common patterns

### "Did the fixture build correctly?"

```lua
test("fixture data is present", function(t)
    local vm = provium:vm_fixture("base")
    local r = vm:run("wc -c < /srv/corpus")
    r:assert_ok()
    t:assert(tonumber(r.stdout) > 0)
end)
```

### "Run a server, hit it, tear down"

```lua
test("nginx serves index", function(t)
    local vm = provium:vm("v", "peios"):boot()
    local proc = vm:run_async("nginx", {args = {"-g", "daemon off;"}})
    local out  = proc:stdout_stream()
    -- nginx writes nothing to stdout in this mode; use wait_until on
    -- the listening port.
    wait_until(function()
        return vm:run("ss -ltn | grep :80"):ok()
    end, {timeout = "10s", desc = "nginx listening on :80"})
    local r = vm:run("curl -s http://localhost/")
    r:assert_ok()
    t:assert(r.stdout:find("Welcome to nginx"))
end)
```

### "Compose multiple ops in one round trip"

```lua
local results = vm:batch(function(b)
    b:write_file("/tmp/a", "1")
    b:write_file("/tmp/b", "2")
    b:read_file("/tmp/a")
    b:stat("/tmp/missing")
end)
-- results[3].ok == "1"
-- results[4].err contains "No such file"
```

`vm:batch` is one wire round-trip for N ops. Useful when latency dominates
(many small ops back-to-back) or when you want to inspect the ordered outcome.

## See also

- [VM reference](~provium/reference/vm): every method.
- [Process reference](~provium/reference/process): async-process surface.
- [Worker reference](~provium/reference/worker): concurrent agent connections.
- [Streams and tails](~provium/writing-tests/streams-and-tails),
  `proc:stdout_stream` / `proc:stderr_stream`.
