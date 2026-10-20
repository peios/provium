---
title: Streams
description:
  The three stream types, Tail, Capture, and ConsoleStream, and the shared
  surface of next, read_until, expect, drain, close, eof, and creation_site.
related:
  - provium/writing-tests/streams-and-tails
  - provium/reference/console
  - provium/reference/process
  - provium/reference/file-handle
---

Provium has three concrete stream types. They expose a shared surface, so test
code can `:next` / `:read_until` / `:expect` / `:drain` / `:close` / `:eof` /
`:creation_site` against any of them without caring which kind it is.

| Type              | Returned by                                                                                    | Backing                                          |
| ----------------- | ---------------------------------------------------------------------------------------------- | ------------------------------------------------ |
| **Tail**          | `vm:tail_file`, `vm:fd_stream`, `file:tail_stream`, `proc:stdout_stream`, `proc:stderr_stream` | Frame-based agent stream over vsock.             |
| **Capture**       | `bridge:capture`, `nic:capture`                                                                | `tcpdump` child stdout, pcap bytes, not framed.  |
| **ConsoleStream** | `console:read` (returned by `vm:console():read()`)                                             | Unix socket connected to QEMU's console chardev. |

All three buffer "leftover bytes" past the last `expect`/`read_until` match. A
subsequent `:next` returns those leftover bytes as the next frame, so test
code never silently loses data.

## Common methods

### `:next(timeout?)`

Returns the next chunk of bytes as a Lua string, or `nil` at EOF / timeout.
The exact frame shape:

- **Tail**: one frame per agent-side write. Each frame is whatever bytes the
  guest had emitted since the previous frame.
- **Capture**: chunks of pcap bytes from tcpdump's pipe (variable size, up to
  64 KiB).
- **ConsoleStream**: chunks of console bytes (variable size, up to 64 KiB).

A timeout does not consume a partial Tail frame or mark the stream as EOF. The
next read resumes the same frame, including when the timeout arrived partway
through its length prefix or payload. This applies to file tails,
file-descriptor streams, and process stdout/stderr streams. Interrupted
transport reads are retried automatically.

The `timeout` argument accepts seconds (number) or a string with
`ms`/`s`/`m`/`h` suffix. Default: 10 seconds.

### `:read_until(pattern, timeout?)`

Pull frames until `pattern` (a Lua string) is found in the accumulated buffer.
Returns the prefix up to and including the matched bytes. Errors with a
pattern-naming message on timeout
(`read_until timed out waiting for \`X\``) or stream EOF (`stream EOF`).

Bytes past the matched suffix are kept as `pending` and replayed on the next
call.

Default timeout: 10 seconds.

### `:expect(pattern, timeout?)`

Like `:read_until` but discards the matched prefix. Returns nothing. Useful as
an assertion: "the stream produced this; I don't care about the bytes". Same
error shape as `read_until`.

Default timeout: 10 seconds.

### `:drain(timeout?)`

Collect every frame available and return them as a Lua array. Stops when the
next read would block past the timeout, when EOF is reached, or when no more
data is available.

Frame-per-element semantics, each iteration of the inner read produces one
Lua-string entry. Pending bytes from a prior `expect`/`read_until` come out as
the first entry.

Default timeout: 0.5 seconds (much shorter than `next`/`read_until`/`expect`
because drain's intent is "what's available right now, then return").

### `:close()`

Close the underlying transport. Subsequent `:next` returns `nil`; subsequent
`:read_until`/`:expect` error with `stream closed` / `console closed` /
`capture closed`. Idempotent.

Pending bytes are dropped on close so a `:next` after close can't drain stale
data.

### `:eof()`

Returns `true` once the stream's transport has reported EOF AND no `pending`
bytes remain. Pending bytes mean there's still data to deliver before EOF is
honest, so `eof()` returns `false` until they've been consumed.

### `:creation_site()`

Returns `{file=string, line=int}` for the test-author frame that opened the
stream, or `nil` if no `*.test.lua`/`*.fixture.lua` frame is on the stack at
creation. Useful for snapshot diagnostics, when `vm:snapshot()` refuses
because of a live stream, the error names the offending stream's creation site
so you can fix it.

## Tail-only behavior

`vm:tail_file(path, opts?)` accepts a `start` opt:

| Value                      | Effect                                                                                                                                                |
| -------------------------- | ----------------------------------------------------------------------------------------------------------------------------------------------------- |
| `"end"` (default)          | Stream only bytes appended after the call.                                                                                                            |
| `"beginning"` or `"start"` | Replay the whole file from byte 0, then continue tailing.                                                                                             |
| Non-negative integer       | Start streaming from that exact byte offset.                                                                                                          |
| Negative integer           | Start N bytes before EOF, Provium stats the file and resolves to an absolute offset. If N exceeds the current file size, the stream starts at byte 0. |
| Finite float               | Truncated toward zero, then treated as the integer cases above (negative floats count back from EOF).                                                 |

A closed Tail returns `nil` from `:next`, the closed stream is semantically
EOF. This matches Capture and ConsoleStream, so `while s:next() do … end`
loops terminate cleanly.

Tails carry per-frame metadata visible to scope-end diagnostics: `kind` (for
example `tail_file`, `fd_stream`, `file_tail_stream`, `proc_stdout_stream`),
`detail` (the path or fd), `creation_site`, and `test_name`.

## Capture-only notes

Capture spawns `tcpdump -i <iface> -U -w - -s 65535`. The output is
pcap-formatted bytes, the standard pcap file format with global header
followed by per-packet records. Feed it through `pcap-parser`, `tshark -r -`,
or `pyshark` to interpret.

The Capture holds a guard against the bridge's `active_captures` counter so a
`vm:snapshot()` while capture is live errors instead of silently producing a
half-captured pcap.

Drop on close prevents `:next` from returning stale tcpdump bytes after the
child has been killed.

## ConsoleStream-only notes

ConsoleStream connects through `UnixStream` to the VMM's console chardev path
(`vm:console_socket_path()`). Reads come back as raw bytes from the chardev,
typically the boot console + sign-in prompt + anything the guest has written
to `/dev/ttyS0` since the last read.

The connect-time read timeout is 50 ms. The `:next(timeout)` arg overrides it
for the duration of the call and the previous timeout is restored afterward,
so a `next("5s")` followed by a bare `next()` doesn't inherit the 5s deadline
forever.

Mid-stream `ConnectionReset` and `BrokenPipe` errors are mapped to "console
EOF", they happen when the QEMU chardev closes mid-read on a VM reset or
shutdown. Semantically EOF for a console stream, not a real I/O error.

## Example patterns

### Wait for a log line

```lua
local f = vm:open_file("/var/log/messages", {read=true})
local stream = f:tail_stream()
stream:expect("ready", "30s")
```

### Race a process against a timeout

```lua
local proc = vm:run_async("slow-server")
local out  = proc:stdout_stream()
out:expect("listening", "10s")
-- now hit it
```

### Drain a capture and pass through pcap-parser

```lua
local cap = bridge:capture()
vm:run("ping -c 5 10.0.0.2")
local frames = cap:drain("2s")
local pcap = table.concat(frames)
-- write pcap to a file or feed it to a parser
```

## See also

- [VM](~provium/reference/vm): `vm:tail_file`, `vm:fd_stream`.
- [Bridge](~provium/reference/bridge): `bridge:capture`.
- [Nic](~provium/reference/nic): `nic:capture`.
- [Console](~provium/reference/console): `console:read` returns a
  ConsoleStream.
- [Process](~provium/reference/process): `proc:stdout_stream` /
  `proc:stderr_stream` return Tails.
- [File](~provium/reference/file-handle): `file:tail_stream` returns a Tail.


- [Streams and tails](~provium/writing-tests/streams-and-tails): patterns and
  gotchas.
