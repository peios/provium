---
title: Streams and Tails
type: how-to
description:
  The three stream types, when to use next, read_until, expect, or drain, how
  EOF works, and how snapshots interact with live streams.
related:
  - provium/reference/streams
  - provium/reference/console
  - provium/writing-tests/files-and-handles
  - provium/writing-tests/running-commands
---

Provium has three stream userdata types, Tail, Capture, and ConsoleStream,
that share a common surface: `next` / `read_until` / `expect` / `drain` /
`close` / `eof` / `creation_site`. This page is the practical guide.

The exhaustive method reference is on [Streams](~provium/reference/streams).

## What returns what

| Source                                          | Type          | Use for                                   |
| ----------------------------------------------- | ------------- | ----------------------------------------- |
| `vm:tail_file(path, opts?)`                     | Tail          | Following a file as it grows.             |
| `vm:fd_stream(fd_or_file)`                      | Tail          | Following an open file handle.            |
| `file:tail_stream()`                            | Tail          | Following a file from its current cursor. |
| `proc:stdout_stream()` / `proc:stderr_stream()` | Tail          | Following an async process's output.      |
| `bridge:capture()`                              | Capture       | Sniffing every packet on a bridge.        |
| `nic:capture()`                                 | Capture       | Sniffing one VM's TAP.                    |
| `console:read()`                                | ConsoleStream | Reading the guest's serial console.       |

All three types support the same operations. The differences are in the
underlying transport and the per-frame shape.

## Operations

Four reading operations cover almost every test. Their exact semantics, frame
shapes per type, default timeouts, error strings, and the pending-bytes
buffer, are in the
[Streams reference](~provium/reference/streams#common-methods); this section
shows how each is used.

One shared property matters for correctness: bytes past an
`expect`/`read_until` match are kept and replayed on the next call, so a
sequence of reads never silently loses data.

### `:next(timeout?)`, pull the next chunk

Returns the next chunk of bytes as a Lua string, or `nil` at EOF / timeout.

```lua
local stream = vm:tail_file("/var/log/messages")
vm:run("logger 'event'"):assert_ok()
local frame = stream:next("5s")
print(frame)  -- "Jan  1 00:00:00 v: event\n"
```

### `:read_until(pattern, timeout?)`, read until a substring

```lua
local line = stream:read_until("\n", "5s")
```

Pulls frames until `pattern` (a Lua string of bytes) appears. Returns the
prefix up to AND including the matched bytes. Errors with the pattern in the
message on timeout.

### Expecting a match

```lua
stream:expect("ready", "30s")
-- next() / read_until() will see anything past "ready"
```

Like `read_until`, but discards the matched prefix. Returns nothing. Use this
when you want the assertion semantics, "this stream produced X", without
caring about the bytes themselves.

### `:drain(timeout?)`, collect everything available

```lua
local chunks = stream:drain("2s")
-- chunks is a Lua array of strings
local body = table.concat(chunks)
```

Read frames until the stream goes quiet, hits EOF, or the timeout lapses.
Useful for "what did the stream produce in this window?" Note its default
timeout is deliberately short (0.5 s and 10 s for the other ops).

### Housekeeping: `:close()`, `:eof()`, `:creation_site()`

`stream:close()` drops the underlying transport (idempotent; a closed stream
reads as EOF). `stream:eof()` tells you the stream is finished and fully
consumed. `stream:creation_site()` reports where the stream was opened, you'll
mostly meet it in snapshot-refusal errors. Exact semantics for all three are
in the [Streams reference](~provium/reference/streams#common-methods).

## Choosing between next, read_until, expect, drain

| Scenario                              | Use                                   |
| ------------------------------------- | ------------------------------------- |
| "Tell me when X happens."             | `:expect("X", timeout)`               |
| "What's the next line?"               | `:read_until("\n", timeout)`          |
| "Pull bytes until I have enough."     | Loop on `:next(timeout)`              |
| "Snapshot everything in this window." | `:drain(timeout)`                     |
| "Has the stream finished?"            | `:eof()` after `:next()` returns nil. |

`:expect` is the workhorse for log-watching tests, it has tight, unambiguous
error messages on timeout, and it doesn't dump bytes you don't want into your
handler.

`:read_until` is the workhorse when you need the matched bytes (parsing
structured log lines, checking that the prefix matches an expected shape).

`:next` is most useful in loops where you want to inspect each frame before
deciding what to do.

`:drain` is most useful at the end of a test to confirm "nothing weird snuck
through" or to capture a quiet window for offline analysis.

## EOF semantics

A closed stream is semantically EOF, `:next` returns `nil`, `:read_until` and
`:expect` error. Idiomatic loops:

```lua
while true do
    local frame = stream:next("100ms")
    if not frame then break end       -- EOF or timeout
    process(frame)
end
```

For Tail specifically, the closed-stream-returns-nil behavior mirrors Capture
and ConsoleStream so the `while s:next() do … end` idiom works across all
three types.

## Tail-specific: starting position

```lua
vm:tail_file("/var/log/messages")                    -- start at end (default)
vm:tail_file("/var/log/messages", {start = "beginning"})  -- replay from byte 0
vm:tail_file("/var/log/messages", {start = -512})    -- last 512 bytes then follow
```

`"end"` is the most common, only bytes appended after the call are streamed.
`"beginning"` is useful for tests that need to assert on the whole file.
Negative integers mean "N bytes before EOF". The full `start` value table
(exact offsets, clamping, float handling) is in the
[Streams reference](~provium/reference/streams#tail-only-behavior).

## Capture: pcap bytes

`bridge:capture()` and `nic:capture()` produce raw pcap bytes (the standard
pcap-savefile format, not pcap-ng). Concatenate the chunks and pipe into
`tshark`, `tcpdump -r -`, or a parser library:

```lua
local cap = lan:capture()
a:run("ping -c 5 10.0.0.2")
local pcap = table.concat(cap:drain("3s"))
cap:close()

-- Analyse inside a guest without touching the host disk.
local r = a:run("tcpdump -r /dev/stdin -n", {stdin = pcap})
print(r.stdout)
```

A live capture blocks `vm:snapshot()` on purpose, no half-captured pcaps.
Close the capture before snapshotting. (The mechanism is described in the
[Streams reference](~provium/reference/streams#capture-only-notes).)

## ConsoleStream: bytes from the chardev

```lua
local console = vm:console()
local stream  = console:read()

stream:expect("login:", "30s")
console:write("root\n")
stream:expect("Password:", "5s")
console:write("toor\n")
stream:expect("# ", "5s")
```

ConsoleStream reads raw bytes from QEMU's console chardev, typically the boot
log, sign-in prompt, and anything the guest has written to `/dev/ttyS0` since
the last read. A VM reset or shutdown reads as console EOF, not an error.
Transport details (socket, timeouts, error mapping) are in the
[Streams reference](~provium/reference/streams#consolestream-only-notes).

## Process streams

`proc:stdout_stream()` and `proc:stderr_stream()` open Tail streams subscribed
to captured output:

```lua
local proc = vm:run_async("server")
local out  = proc:stdout_stream()
out:expect("listening on :8080", "10s")
-- now hit the server
```

The stream's `creation_site` and `kind`/`detail` carry the process handle id,
so snapshot diagnostics can name "proc_stdout_stream(handle=42)" when
something refuses a snapshot.

## File streams

`file:tail_stream()` opens a Tail rooted at the file's current cursor:

```lua
local h = vm:open_file("/var/log/messages", {read=true})
h:seek(0, "end")  -- start at current EOF
local stream = h:tail_stream()
-- stream subscribes from EOF onwards, just like vm:tail_file with start="end"
```

`vm:fd_stream(fd_or_file)` is the lower-level form, accepts either an integer
fd (from `file:fd()`) or the File userdata directly:

```lua
local h = vm:open_file("/dev/console", {read=true})
local stream = vm:fd_stream(h)
```

## Snapshots and live streams

Snapshots refuse to run while a stream is live. The error names the stream:

```text
provium: vm:snapshot() refused — file `tests/x.test.lua` has live streams:
  - tail_file("/var/log/messages") at tests/x.test.lua:42
  - proc_stdout_stream(handle=7) at tests/x.test.lua:55
Close the streams before snapshotting (or remove the snapshot).
```

Two ways out:

1. Close the streams explicitly before `vm:snapshot()`.
2. Move the snapshot earlier in the test, before the streams are opened.

The pre-condition is enforced for both single-VM (`vm:snapshot()`) and lab
(`provium:snapshot()`) snapshots. It also blocks
`provium.reset_between_tests = true` files at chunk-load time when file-scope
streams are open.

## Common patterns

### "Wait for a log line"

```lua
local stream = vm:tail_file("/var/log/messages")
vm:run("trigger-something"):assert_ok()
stream:expect("trigger landed", "5s")
```

### "Race a process boot against its readiness signal"

```lua
local proc = vm:run_async("server")
local out  = proc:stdout_stream()
out:expect("ready", "10s")
-- safe to hit the server now
```

### "Capture pcap during a specific operation"

```lua
local cap = lan:capture()
a:run("ping -c 3 10.0.0.2"):assert_ok()
local pcap = table.concat(cap:drain("2s"))
cap:close()
```

### "Drive an interactive prompt over the console"

```lua
local console = vm:console()
local stream  = console:read()
stream:expect("login:", "30s")
console:write("root\n")
stream:expect("# ", "10s")
```

### "Confirm nothing weird snuck through"

```lua
local cap = lan:capture()
vm:run("…benign workload…"):assert_ok()
local frames = cap:drain("1s")
cap:close()
local pcap = table.concat(frames)
local r = vm:run("tcpdump -r /dev/stdin -n", {stdin = pcap})
t:assert(not r.stdout:find("malformed"))
```

## See also

- [Streams reference](~provium/reference/streams): every method, EOF
  semantics, type-specific notes.


- [Console reference](~provium/reference/console): `console:read` and
  `console:expect`.
- [VM reference](~provium/reference/vm): `vm:tail_file`, `vm:fd_stream`.
- [File handle reference](~provium/reference/file-handle): `file:tail_stream`.
