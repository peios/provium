---
title: File
description:
  A File userdata wraps an open file descriptor inside the guest. Use it to
  read, write, seek, tell, fd-to-int, tail, and close. Mirrors POSIX
  semantics.
related:
  - provium/writing-tests/files-and-handles
  - provium/reference/vm
  - provium/reference/streams
  - provium/reference/worker
---

A File is an open guest-side file handle, returned by
`vm:open_file(path, mode)`. Once closed, all ops error with `file is closed`.
Idempotent: closing twice is safe.

## Constructing

| Source                     | Returns                       |
| -------------------------- | ----------------------------- |
| `vm:open_file(path, mode)` | New file handle in the guest. |

Mode table fields: `read`, `write`, `create`, `truncate`, `append`,
`exclusive`, `perm`. See
[VM open_file](~provium/reference/vm#vm-open-file-path-mode-table) for the
full mode-table reference.

At least one of `read`, `write`, or `append` must be true.

## Methods

### `file:read(n)`

Read up to `n` bytes from the current cursor. Returns the bytes as a Lua
string. Returns the empty string at EOF (POSIX `read` semantics, never errors
on EOF).

```lua
local f = vm:open_file("/etc/hostname", {read=true})
local s = f:read(64)
```

### `file:read_all()`

Drain the file from the current cursor to EOF. Returns the bytes as a Lua
string. Internally chunked at 64 KiB. At EOF, returns the empty string.

### `file:write(data)`

Write `data` (a Lua string of bytes) at the current cursor. Returns the number
of bytes actually written (which can be less than `#data` on a partial write,
POSIX semantics).

### `file:seek(offset, whence?)`

Reposition the cursor. `whence` defaults to `"set"`; valid values: `"set"`,
`"cur"`, `"end"`. Anything else errors with
`seek: whence must be set/cur/end, got \`X\``.

Returns the new absolute offset.

### `file:tell()`

Returns the current cursor position. This authoritatively re-reads from the
agent (a no-op `Seek(Cur, 0)`) rather than trusting the host-side cursor
cache, so `tell()` stays honest when a previous read or write returned
mid-flight.

### `file:close()`

Close the file. Idempotent: a second `:close()` does not error. After close,
every other op errors with `file is closed`.

### `file:fd()`

Returns the raw u64 handle id. Useful for `vm:ioctl(fd, …)`, `vm:syscall(…)`
(where the fd is one of the integer args), or `vm:fd_stream(fd)` to open a
streaming subscription against the same file.

After close, returns `0`.

### `file:tail_stream()`

Open a [Tail](~provium/reference/streams) stream rooted at the file's current
cursor. Streams new bytes appended after the current position. Useful for
"give me a stream that starts here" without re-opening the file.

Errors with

```text
file:tail_stream needs the source path; opened via wrap() not wrap_with_path()
```

if the File was constructed without a recorded path (which never happens
through `vm:open_file`, it always records the path).

## Example

```lua
test("file handle round-trip", function(t)
    local vm = provium:vm("v", "peios"):boot()
    local h = vm:open_file("/tmp/data", {write=true, create=true, truncate=true, perm=0x180})  -- 0o600
    local n = h:write("hello world")
    t:assert_eq(n, 11)
    h:close()

    local r = vm:open_file("/tmp/data", {read=true})
    r:seek(6, "set")
    t:assert_eq(r:read(5), "world")
    t:assert_eq(r:tell(), 11)
    r:close()
end)
```

## EOF semantics

Reading past EOF returns the empty string `""`, not nil and not an error.
Tests that loop "read until empty" should use:

```lua
while true do
    local chunk = h:read(4096)
    if chunk == "" then break end
    -- process chunk
end
```

## See also

- [VM](~provium/reference/vm): `vm:open_file`, `vm:read_file`,
  `vm:write_file`, `vm:fd_stream`.
- [Worker](~provium/reference/worker): why there is no `worker:open_file`, and
  the syscall form that opens a file inside a worker.
- [Streams](~provium/reference/streams): what `file:tail_stream()` returns.
- [Files and handles](~provium/writing-tests/files-and-handles): patterns for
  guest-side file I/O.
