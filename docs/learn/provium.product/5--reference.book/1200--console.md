---
title: Console
description:
  The Console userdata, read the boot log, stream the chardev, expect a
  substring, write input, and how it interacts with snapshots.
related:
  - provium/reference/streams
  - provium/reference/vm
  - provium/writing-tests/streams-and-tails
---

A Console wraps the guest's serial console, the host writes to and reads from
QEMU's chardev bound to the guest's serial port (`/dev/ttyS0`). Provium
appends `console=ttyS0` to the kernel command line if no serial console is
present, so kernel output always lands here.

## Constructing

| Source         | Returns                                                                                                                        |
| -------------- | ------------------------------------------------------------------------------------------------------------------------------ |
| `vm:console()` | The guest's console. Always succeeds; underlying chardev access is checked at the first `:read` / `:read_log` / `:write` call. |

## Methods

### `console:read()`

Returns a [ConsoleStream](~provium/reference/streams) backed by a `UnixStream`
to the QEMU chardev socket. Use the stream's `:next` / `:read_until` /
`:expect` / `:drain` to consume console output incrementally.

The stream is registered with the VM's resource graph, so an open
`console:read()` blocks `vm:snapshot()` from succeeding silently, the snapshot
precondition reports "live console stream" with creation site.

Errors with `console:read: VMM does not expose a console socket` if the VMM
backend doesn't surface a chardev path (rare; the LocalAgent backend does
not).

### `console:read_log()`

Returns the entire captured console log to date as a Lua string.
Snapshot-style; not a stream. Useful for one-shot inspections:

```lua
local log = vm:console():read_log()
t:assert(log:find("Welcome to Peios"))
```

### `console:expect(pattern, timeout?)`

Poll the captured log every 50 ms until `pattern` (a Lua string) appears or
`timeout` lapses. Default timeout: 30 seconds. Returns the matched substring
on hit; raises with the pattern in the message on timeout.

This is the simple form for "wait for the kernel to log X." For long-running
streams or when you also want to read past-the-match bytes, use
`console:read():expect(...)` instead.

### `console:write(data, opts?)`

Write `data` (Lua string) to the chardev. Opts:

| Key       | Type                                          | Effect                                          |
| --------- | --------------------------------------------- | ----------------------------------------------- |
| `timeout` | number (seconds) or string `"500ms"` / `"5s"` | Bound the blocking write. `0` means no timeout. |

The legacy `timeout_ms` integer key is **rejected** with a clear error
(`opts.timeout_ms is not supported (use timeout = "500ms" or timeout = 0.5)`).

Use this to feed input to interactive console programs, or to pre-populate a
getty sign-in.

### `console:close()`

No-op in v1. The chardev itself is owned by the VMM; closing the Console
userdata is a Lua-level concept only.

## Example: drive a getty sign-in

```lua
test("login as root via console", function(t)
    provium.timeout = "60s"
    local vm = provium:vm("v", "peios"):boot()
    local console = vm:console()
    local stream  = console:read()
    stream:expect("login:", "30s")
    console:write("root\n")
    stream:expect("Password:", "5s")
    console:write("toor\n")
    stream:expect("# ", "5s")
end)
```

## See also

- [VM](~provium/reference/vm): `vm:console()`.
- [Streams](~provium/reference/streams): what `console:read()` returns.
