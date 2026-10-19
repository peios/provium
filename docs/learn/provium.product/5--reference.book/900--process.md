---
title: Process
description:
  A Process userdata is a handle to an async-spawned guest process. Use it to
  wait, kill, signal, write stdin, stream stdout/stderr, and query exit
  status.
related:
  - provium/writing-tests/running-commands
  - provium/reference/vm
  - provium/reference/streams
  - provium/reference/worker
---

A Process is what `vm:run_async(...)` (or `worker:run_async(...)`) returns: a
handle to a guest process that the agent has launched but is not waiting on.
You control the lifetime, the agent does not auto-kill it.

## Constructing

| Source                         | Returns                                |
| ------------------------------ | -------------------------------------- |
| `vm:run_async(cmd, opts?)`     | New process under the VM's main agent. |
| `worker:run_async(cmd, opts?)` | New process under a worker namespace.  |

Opts mirror `vm:run`'s shape (`args`, `env`, `env_clear`, `cwd`), but the
`timeout` / `timeout_ms` keys are rejected with a pointer to use
`proc:wait(timeout)` instead. `stdin` works (initial bytes go through
`RunAsyncArgs`); subsequent input goes through `proc:stdin_write`.

## Methods

### `proc:wait(timeout?)`

Wait for the process to exit. Returns a
[RunResult](~provium/reference/vm#runresult) userdata.

`timeout` accepts:

- `nil`, wait forever.
- A number, seconds.
- A string, `"5s"` / `"500ms"` / `"5m"` / `"2h"`.

Passing `0` is rejected with a pointer to use `proc:status()` for non-blocking
polling, a literal-zero timeout would otherwise SIGKILL the process
immediately because the agent's `wait_with_timeout(0)` sees the deadline
already past.

When the timeout fires, the agent SIGKILLs the whole process group the process
leads, so anything it forked or backgrounded goes with it. Without that, a
surviving descendant would hold the captured stdout and stderr pipes open and
`:wait()` would block past its timeout waiting for them to close.

After `:wait()` returns, the agent-side slot is gone. The Process is
"consumed"; subsequent ops still work, but `:close()` short-circuits without
re-killing.

### `proc:kill(sig?)`

Send a signal. `sig` accepts:

- `nil`, defaults to `SIGTERM`.
- An integer, that signal number.
- A string, friendly name. Recognized: `term`/`sigterm`/`15`,
  `kill`/`sigkill`/`9`, `int`/`sigint`/`2`, `hup`/`sighup`/`1`,
  `quit`/`sigquit`/`3`, `stop`/`sigstop`, `cont`/`sigcont`,
  `usr1`/`sigusr1`/`10`, `usr2`/`sigusr2`/`12`, `alrm`/`sigalrm`/`14`,
  `pipe`/`sigpipe`/`13`, `chld`/`sigchld`/`17`, `winch`/`sigwinch`. Comparison
  is case-insensitive.

Unknown name → `unknown signal name \`X\``.

The signal reaches the process you spawned only, not the process group it
leads: a non-fatal signal is meant for the process you named. If the command
is a shell that backgrounded its work, that work survives, use
`proc:wait(timeout)`, which SIGKILLs the group, to take the tree down.

### `proc:signal(sig)`

Same as `:kill(sig)`. Read better when the signal is non-fatal
(`proc:signal("usr1")`).

### `proc:pid()`

Returns the kernel PID of the process inside the guest. Fetched live through
the agent's GetPid op.

### `proc:handle()`

Returns the opaque agent-side handle id (an integer). Distinct from `pid()`,
the handle stays stable across forks; the PID can change if the process
re-execs.

### `proc:status()`

Non-blocking poll. Returns the agent's view of the process's status without
waiting. Useful for "is it still running" without blocking.

### `proc:stdin_write(data)`

Write `data` (Lua string) to the process's stdin. Returns whatever the agent
reports (typically the byte count).

### `proc:close_stdin()`

Close the stdin pipe. After this, subsequent `:stdin_write` errors.

### `proc:stdout_stream()` / `proc:stderr_stream()`

Open a [Tail](~provium/reference/streams) stream subscribed to captured stdout
/ stderr. The agent polls the captured-output buffer and emits frames. Useful
when the process produces a lot of output you want to consume incrementally.

The returned Tail's `StreamMeta` is pre-filled with
`kind="proc_stdout_stream"` (or `"proc_stderr_stream"`) and the process handle
id, so snapshot diagnostics show meaningful detail.

Not available for a process a
[worker](~provium/reference/worker#worker-run-async-cmd-opts) spawned: its
output is captured in the worker, and the worker channel cannot carry a
stream. `proc:wait` returns it.

### `proc:close()`

Auto-close hook. If `:wait()` already happened, this is a no-op. Otherwise,
sends SIGTERM to the process, then waits up to 2 seconds for exit. That wait
is a `proc:wait` timeout, so a process still alive after 2 seconds takes its
whole group with it when the agent escalates to SIGKILL.

## Example: tail logs while the process runs

```lua
test("server emits ready then handles request", function(t)
    local vm = provium:vm("v", "peios"):boot()
    local proc = vm:run_async("python3", {args = {"server.py"}})
    local out  = proc:stdout_stream()

    -- Wait for the server to log "ready".
    out:expect("ready", "5s")

    -- Now hit it.
    local r = vm:run("curl -s http://localhost:8080/")
    r:assert_ok()
    t:assert(r.stdout:find("hello"))

    -- Tear down. Inside the test scope this happens automatically;
    -- here we just demonstrate it works explicitly too.
    proc:kill("term")
    local exit = proc:wait("2s")
    t:assert(exit.signal == 15 or exit.exit_code == 0)
end)
```

## See also

- [VM](~provium/reference/vm): `vm:run_async` returns a Process.
- [Worker](~provium/reference/worker): `worker:run_async` returns a Process.
- [Streams](~provium/reference/streams): what `proc:stdout_stream` /
  `proc:stderr_stream` return.
- [Running commands](~provium/writing-tests/running-commands): patterns for
  sync and async, stdin, env.
