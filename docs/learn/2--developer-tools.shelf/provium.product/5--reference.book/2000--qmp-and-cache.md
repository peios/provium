---
title: Monitor Deadlines and Cache Publication
description:
  Deadline ownership for QEMU monitor requests and atomic cache writes.
---

The QEMU monitor protocol (QMP) carries host lifecycle requests. Connection
and command deadlines bound different operations; neither is a substitute for
the guest agent's boot timeout.

## Monitor connections

`Qmp::connect` and `Qmp::from_stream` use a 30-second handshake deadline.
Their timeout variants accept an explicit duration. The budget covers the
greeting and capability negotiation. `connect_timeout` also includes Unix
socket connection establishment. Individual reads do not restart it.

The VM startup path supplies its remaining monitor-startup budget. It does not
add the library's default handshake duration to that budget.

## Commands

A command's timeout covers waiting for the writer, sending the command, and
waiting for the matching reply. A timeout during an incomplete send closes the
connection: the remaining bytes cannot be treated as a separate command. A
response timeout after a complete send removes that request's pending reply
slot without closing an otherwise usable connection.

Unrepresentable timeout durations return an input error. Event waits use the
same checked deadline calculation. Monitor deadlines do not specify a maximum
runtime for guest commands; the guest operation's timeout applies there.

## Cache publication

Snapshot sparse rewrites and composed initrd cache entries use exclusively
created temporary files in the destination directory. A completed result
replaces the destination atomically. An error before publication removes the
temporary file and leaves the previous destination intact.

Concurrent cache writers publish complete entries. Atomic replacement is a
visibility guarantee, not a claim of persistence across a host power failure.
