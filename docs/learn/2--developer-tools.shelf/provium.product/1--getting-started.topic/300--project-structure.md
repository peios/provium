---
title: Project Structure
type: concept
description:
  How configuration, tests, helpers, fixtures, and caches fit together.
related:
  - provium/getting-started/quick-start
  - provium/configuration/provium-toml
  - provium/running-tests/fixtures-and-dependencies
---

A Provium project groups configuration and Lua tests. Keep its source files in
version control and provide the kernel and initrd named by its profiles.
Generated fixture snapshots belong in the cache, outside the source tree.

## Source layout

A project can use this layout:

```text
project/
    provium.toml
    tests/
        smoke.test.lua
        networking/
            connection.test.lua
        helpers/
            assertions.lua
        booted-base.fixture.lua
```

`provium.toml` defines profiles and runner settings. The command reads it from
the current directory unless `--config` selects another file. Profiles name
the kernel, initrd, and boot parameters used by `provium:vm(...)`.

A `.test.lua` file registers tests. Run `provium tests/` to discover those
files under `tests`. Without a positional path, discovery starts in the
current directory. `[provium].roots` is a separate setting: it determines
where helpers and fixture files are found.

## Tests and shared helpers

Each test file gets a fresh Lua state. Tests within that file run in order;
they do not run conwith each other. The dispatcher can run several files at
once when its resource budget allows.

With `roots = ["tests"]`, load `tests/helpers/assertions.lua` using:

```lua
local assertions = require("helpers.assertions")
```

A helper has an ordinary `.lua` filename. It is not a test or fixture. Changes
to a helper used by a fixture contribute to that fixture's cache identity.

## Fixtures and caches

A `.fixture.lua` file builds and returns a VM or lab snapshot. A test asks for
it by name using `provium:vm_fixture(...)` or `provium:lab_fixture(...)`. The
runner searches the configured roots, computes the fixture's identity, and
either restores a cached snapshot or builds it.

The default fixture cache is `~/.cache/provium/fixtures`. Set `cache_dir` and
`cache_max_size` under `[provium]` to choose another location or size budget.
Cache entries are generated state; keep the source fixture and its inputs as
the authority. See
[fixtures and dependencies](~provium/running-tests/fixtures-and-dependencies)
for invalidation and cache-management commands.

The rerun list is separate from the fixture cache. `--rerun-failed` selects
the previously failing files from those discovered by the current command.
`PROVIUM_RERUN_STATE` overrides the rerun-state path.

## Files outside the project

Profiles can refer to kernels and initrds outside the source tree. Record
which artifacts a test run used when the result is evidence for a change. A
passing result against one image does not qualify another image.

The host needs `qemu-system-x86_64` for VM execution and `ip`, `tc`, and `nft`
for network setup. Packet capture also needs `tcpdump`. These host tools are
separate from commands installed inside a guest.

Saved event streams and fixture caches can be large. Choose explicit output
paths for retained evidence and keep disposable output out of source control.
