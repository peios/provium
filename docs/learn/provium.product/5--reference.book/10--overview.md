---
title: Scope and Architecture
description: The boundaries and compatibility assumptions of Provium 0.1.0.
---

This manual describes Provium 0.1.0. It covers the Lua bindings, host resource
model, guest transport, event stream, and QEMU monitor connection. The command
manuals are `provium(1)` and `provium-coverage(1)`.

## Components

The host runner discovers Lua files and assigns resource budgets. A file
runner owns a Lua state and a root lab. Tests create child scopes within that
lab. The QEMU backend starts guests and controls their lifecycle; the guest
agent performs the operations sent over vsock.

The local backend uses host processes and files for runner development. Its
results do not establish guest-kernel behavior. Profiles for real VMs select
the guest artifacts explicitly.

## Compatibility

The host and guest share `provium-protocol`. Its version is checked during the
initial handshake. Use an agent from the same build as the host. The
[protocol version](~provium/reference/protocol-version) article records the
wire-version rules.

Lua method names, command flags, and wire field names retain their literal
spelling. They are interface contracts, even where a spelling differs from
ordinary documentation prose.

## Failure boundaries

A failed test is distinct from a configuration or infrastructure error.
Resource cleanup follows scope ownership. Per-test timeouts can end the file's
lab, so timeout handling is not a promise that other tests in the same file
can continue using their resources. See
[the test framework](~provium/reference/test-framework).

Build, unit-test, and package checks do not establish that a particular kernel
and initrd boot correctly. Guest results need the exact image identity and the
test profile used to produce them.
