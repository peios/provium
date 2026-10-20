---
title: Files and Handles
type: how-to
description:
  Guest-side file I/O, read_file and write_file and open_file, mode tables,
  seek and tell, tail streams, stat, listdir, mkdir, unlink, and rename.
related:
  - provium/reference/file-handle
  - provium/reference/vm
  - provium/writing-tests/streams-and-tails
  - provium/reference/streams
---

Provium gives you two layers for guest-side file I/O. Most tests use the
high-level `vm:read_file` / `vm:write_file` / `vm:stat` calls; tests that need
cursor control, partial reads, or POSIX semantics drop to `vm:open_file` and
the [File](~provium/reference/file-handle) userdata.

## High-level: `read_file` / `write_file` / `stat`

These are one-shot operations. Each is a single round-trip to the agent.

```lua
vm:write_file("/etc/test.conf", "key=value\n")
local body = vm:read_file("/etc/test.conf")
local meta = vm:stat("/etc/test.conf")
```

### `vm:read_file(path)`

Returns the entire file as a Lua string. Errors on agent-side `read` failure
(ENOENT, EACCES, and so on).

```lua
local content = vm:read_file("/etc/hostname")
```

### `vm:write_file(path, data)`

Replaces the file's contents. Creates if absent. The mode bits follow agent
defaults, use `vm:open_file` if you need explicit `perm`.

```lua
vm:write_file("/tmp/data", "binary\0bytes")
```

### `vm:push_file(host_path, guest_path, opts?)`

Read a file on the host and write its bytes to `guest_path` in the guest.
Equivalent to reading the host file in Lua and passing the bytes to
`vm:write_file`, but with one important extra: inside a fixture, the host file
is folded into the fixture's cache key automatically, so rebuilding the host
artifact (for example a binary you're testing) invalidates the snapshot.

```lua
vm:push_file("../peios-uapi/target/release/peios-uapi", "/usr/bin/peios-uapi")
```

Relative `host_path` is resolved against the fixture (or helper) file's
directory, not the cwd at invocation time. Pass `{auto_dep = false}` to skip
the auto-fold for a single call (for example a large test corpus you don't
want included in the key):

```lua
vm:push_file("../big-corpus.tar", "/data.tar", {auto_dep = false})
```

The auto-fold relies on static scanning of the call site, so non-literal host
paths (variables, concatenation) are NOT tracked. Use
[`lab:depends_on_file`](~provium/reference/lab#lab-depends-on-file-host-path)
with a literal string to declare them explicitly. See
[Fixtures and dependencies, External host-file deps](~provium/running-tests/fixtures-and-dependencies#external-host-file-deps)
for the full model.

Mode bits follow `write_file`'s agent defaults, `vm:run("chmod +x …")` after
the push if you need executable bits.

### `vm:stat(path)`

Returns a table:

```lua
local m = vm:stat("/etc/test.conf")
print(m.size)        -- 10
print(m.perm)        -- POSIX mode bits
print(m.entry_type)  -- "file", "directory", "symlink", …
```

The full field table (including the complete `entry_type` value list) is in
the [VM reference](~provium/reference/vm#vm-stat-path). Two fields carry the
modification time: use `mtime_ns` for exact comparisons; use `mtime` (float
seconds) when "around what o'clock" is enough.

`perm` is in the POSIX range. Lua 5.4 doesn't accept `0o…` literals, use
decimal or hex (`0x180` for 0o600). `4095` is `0o7777`, the upper bound.

### `vm:listdir(path)`

Returns an array of `{name, entry_type}` tables:

```lua
for _, e in ipairs(vm:listdir("/etc")) do
    print(e.name, e.entry_type)
end
```

### `vm:mkdir(path, opts?)`

Create a directory:

```lua
vm:mkdir("/tmp/d")
vm:mkdir("/tmp/d/e/f", {parents = true})
vm:mkdir("/tmp/private", {perm = 0x1c0})  -- 0o700
```

### `vm:unlink(path)`

Remove a file or empty directory. Errors on non-empty directory (use
`vm:run("rm -rf …")` for that).

### `vm:rename(from, to)`

Atomic rename within the guest filesystem.

## Low-level: `vm:open_file`

Returns a [File](~provium/reference/file-handle) userdata that you can read,
write, seek, and close.

```lua
local h = vm:open_file("/tmp/data", {read=true, write=true, create=true, truncate=true})
h:write("hello")
h:seek(0)
local s = h:read(5)
h:close()
```

### Mode table

At least one of `read`, `write`, `append` must be true; an empty mode table
errors at open time. The flags you'll combine most often are `read` / `write`,
`create` (create if absent), `truncate`, and `perm` (POSIX mode for
newly-created files):

```lua
-- Write-only, create, truncate, mode 0o600.
local h = vm:open_file("/etc/secret", {write=true, create=true, truncate=true, perm=0x180})
```

The full mode-table reference (including `append` and `exclusive` / `O_EXCL`)
is on [vm:open_file](~provium/reference/vm#vm-open-file-path-mode-table).

### Reading

```lua
local h = vm:open_file("/etc/hostname", {read=true})

local first = h:read(64)         -- up to 64 bytes
local rest  = h:read_all()       -- drain to EOF
h:close()
```

`h:read(n)` returns up to `n` bytes; at EOF it returns the empty string `""`,
never an error, so a read loop terminates on `chunk == ""`. `h:read_all()`
drains from the cursor to EOF in one call. See
[EOF semantics](~provium/reference/file-handle#eof-semantics) in the File
reference.

### Writing

```lua
local h = vm:open_file("/tmp/data", {write=true, create=true, truncate=true})
local n = h:write("hello world")
-- n is 11 (bytes actually written, may be less than #data on partial write)
h:close()
```

### Seek and tell

```lua
local h = vm:open_file("/tmp/x", {read=true})
h:seek(10, "set")             -- absolute offset 10
h:seek(5, "cur")              -- relative +5 from current
h:seek(-1, "end")             -- 1 byte before EOF
h:tell()                      -- current offset
```

`tell()` reports the authoritative agent-side position, not a host-side cache,
the [File reference](~provium/reference/file-handle#file-tell) explains how.

### Closing

```lua
h:close()
h:close()  -- second close is fine; idempotent
h:read(1)  -- raises "file is closed"
```

The harness's resource walker auto-closes files at scope end through the
`_provium_close_test_scope` hook, so you can usually omit explicit closes.
Closing manually is good practice when the file's lifetime is bounded by a
clear point in the test.

### Raw fd

```lua
local fd = h:fd()              -- u64 handle id
vm:ioctl(fd, MY_IOCTL_NUM)
```

`fd()` returns `0` on a closed file, otherwise the handle's u64 value. Useful
for `vm:ioctl(fd, …)` and `vm:syscall(…)` invocations that take a file
descriptor.

## Tailing files

Two ways to tail:

### `vm:tail_file(path, opts?)`

Subscribe to bytes appended to a file. Returns a
[Tail](~provium/reference/streams).

```lua
local stream = vm:tail_file("/var/log/messages")
vm:run("logger 'hello'"):assert_ok()
local line = stream:read_until("\n", "5s")
t:assert(line:find("hello"))
```

`opts.start` controls the starting position, `"end"` (the default: only bytes
appended after the call), `"beginning"` (replay from byte 0, then continue
tailing), or a byte offset. Negative-offset and float handling are in the
[VM reference](~provium/reference/vm#vm-tail-file-path-opts).

### `file:tail_stream()`

Open a tail rooted at the file handle's current cursor:

```lua
local h = vm:open_file("/var/log/messages", {read=true})
h:seek(0, "end")
local stream = h:tail_stream()
-- stream now subscribes from current EOF onwards
```

Useful when you've already seek-d to a known position.

### `vm:fd_stream(fd_or_file)`

Open a tail against an existing file handle (by fd integer or by the File
userdata directly):

```lua
local h = vm:open_file("/dev/console", {read=true})
local stream = vm:fd_stream(h)
```

## Common patterns

### Writing then reading back

```lua
test("config round-trips", function(t)
    local body = "key1=value1\nkey2=value2\n"
    vm:write_file("/etc/test.conf", body)
    t:assert_eq(vm:read_file("/etc/test.conf"), body)
    t:assert_eq(vm:stat("/etc/test.conf").size, #body)
end)
```

### Asserting permission bits

Lua 5.4 has no `0o…` literal. Use decimal or hex:

```lua
test("private file is 0600", function(t)
    local h = vm:open_file("/etc/secret", {write=true, create=true, perm=0x180})  -- 0o600
    h:close()
    t:assert_eq(vm:stat("/etc/secret").perm, 0x180)
end)
```

### Tailing a log while the test acts

```lua
test("logger writes are persisted", function(t)
    local stream = vm:tail_file("/var/log/messages")
    vm:run("logger 'event 1'"):assert_ok()
    vm:run("logger 'event 2'"):assert_ok()
    stream:expect("event 1", "5s")
    stream:expect("event 2", "5s")
end)
```

### Listing then filtering

```lua
test("/tmp has the expected files", function(t)
    vm:write_file("/tmp/a", "")
    vm:write_file("/tmp/b", "")
    local names = {}
    for _, e in ipairs(vm:listdir("/tmp")) do
        if e.entry_type == "file" then table.insert(names, e.name) end
    end
    table.sort(names)
    t:assert(names[1] == "a" and names[2] == "b")
end)
```

## Files inside a worker

A [worker](~provium/reference/worker) is a separate guest process with its own
file table, and there is no worker-scoped File: `worker:open_file` is rejected
by the agent, because a descriptor opened in the worker could not be reached
by `file:read` and friends, which act through the parent agent.

Decide which of two things you actually need:

- **The file must be opened under the worker's credentials**, a permission
  test, say. Open it from inside the worker with `worker:syscall(openat, …)`,
  and read, write and close it with further syscalls, so the descriptor lives
  and dies in the worker process. The
  [Worker reference](~provium/reference/worker#worker-open-file-path-mode-table-rejected)
  has a complete example.
- **The test needs to see or seed the file.** Use the VM handle:
  `vm:open_file`, `vm:read_file`, `vm:write_file`. The worker's identity is
  not involved, and the File auto-registers with the test scope as usual.

## Batch I/O for low latency

For many small ops back-to-back, batch them in one round trip:

```lua
local results = vm:batch(function(b)
    b:write_file("/tmp/a", "1")
    b:write_file("/tmp/b", "2")
    b:write_file("/tmp/c", "3")
    b:read_file("/tmp/a")
    b:stat("/tmp/b")
end)
```

Each entry in `results` is `{ok=value}` or `{err=msg}`. A failure on one op
doesn't short-circuit the rest. See
[VM batch](~provium/reference/vm#vm-batch-fn) for the per-op return shape.

## See also

- [VM reference](~provium/reference/vm): `read_file`, `write_file`, `stat`,
  `mkdir`, `listdir`, `unlink`, `rename`, `open_file`, `tail_file`,
  `fd_stream`.
- [File handle reference](~provium/reference/file-handle): every method on the
  File userdata.
- [Streams and tails](~provium/writing-tests/streams-and-tails): patterns for
  tail streams.
