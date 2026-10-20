---
title: Profiles
type: how-to
description:
  Patterns for multiple profiles, kernel-version testing, debug builds,
  kernel-only VMs, one directory per profile, feature-flag gating, and what
  happens to the fixture cache when profiles change.
related:
  - provium/configuration/provium-toml
  - provium/configuration/dynamic-profiles
  - provium/writing-tests/vms-and-profiles
  - provium/running-tests/fixtures-and-dependencies
---

A profile names what a VM boots, a kernel, an initramfs, a command line, a
guest OS, in `provium.toml`. Tests pick a profile by name when they create a
VM. This page covers the practical patterns.

The configuration field reference is on
[provium.toml](~provium/configuration/provium-toml). A profile can also
_build_ its own artifacts before booting rather than pointing at files already
on disk, see [Dynamic profiles](~provium/configuration/dynamic-profiles).

## When you need more than one profile

For most projects, one profile is enough, `peios`, pointing at the latest
build. You'd add more when:

- You're testing across kernel versions (`peios-stable`, `peios-mainline`).
- You want a debug build available for diagnosing failures (`peios-debug`).
- You're gating tests on a feature flag in the kernel (`peios-prefeatx`,
  `peios-postfeatx`).
- You're testing a port to a new architecture (`peios-arm64`).

## Declaring multiple profiles

```toml
[profiles.peios]
kernel   = "/build/peios/bzImage"
initrd   = "/build/peios/initrd.cpio.gz"
cmdline  = "console=ttyS0 quiet"

[profiles.peios-debug]
kernel   = "/build/peios-debug/bzImage"
initrd   = "/build/peios-debug/initrd.cpio.gz"
cmdline  = "console=ttyS0 debug loglevel=7 nokaslr"

[profiles.peios-stable]
kernel   = "/build/peios-stable/bzImage"
initrd   = "/build/peios-stable/initrd.cpio.gz"
cmdline  = "console=ttyS0 quiet"
```

Tests pick:

```lua
provium:vm("v",        "peios")
provium:vm("debug-v",  "peios-debug")
provium:vm("stable-v", "peios-stable")
```

A test can mix profiles in the same file, useful for compatibility testing:

```lua
test("stable client can talk to mainline server", function(t)
    local lan    = provium:bridge("lan")
    local server = provium:vm("server", "peios"):boot()
    local client = provium:vm("client", "peios-stable"):boot()
    lan:attach({server, client})
    server:run("ip addr add 10.0.0.1/24 dev eth0 && ip link set eth0 up")
    client:run("ip addr add 10.0.0.2/24 dev eth0 && ip link set eth0 up")

    -- Run a real protocol against the mismatched pair.
    server:run_async("python3", {args = {"-m", "http.server", "8080"}})
    wait_until(function()
        return server:run("ss -ltn | grep :8080"):ok()
    end, {timeout = "5s"})
    client:run("curl -s http://10.0.0.1:8080/"):assert_ok()
end)
```

## One directory per profile

Profiles do not have to be written in `provium.toml` at all.
`[profiles] from_dir` reads them from a directory, one subdirectory each:

```toml
[provium]
roots = ["tests"]

[profiles]
from_dir = "profiles"
```

```text
profiles/
  kernel-only/
    profile.provium.toml     # the profile
    image.toml               # whatever else it needs, beside it
  full-system/
    profile.provium.toml
    image.toml
```

Everything a discovered profile names is relative to its own directory, and
its `build` runs there. So a profile directory is self-contained: it can be
moved, copied or renamed without rewriting what is inside it, and adding one
is `mkdir` rather than an edit to a central list.

Prefer this when profiles carry files of their own, an image spec, a package
manifest, a vendored initrd, and especially when the repository's purpose is
holding many of them. Prefer inline blocks when a profile is three lines
pointing at paths that already exist.

## Profile choice patterns

### Default and debug

The most common split: a fast `peios` build for the default loop, and a
`peios-debug` build with `loglevel=7`, `nokaslr`, and (probably)
`KASAN`/`UBSAN` enabled. Use the debug profile when reproducing a kernel bug:

```lua
test("debug build reveals the leak", {tags = {"slow", "debug"}}, function(t)
    local vm = provium:vm("v", "peios-debug"):boot()
    vm:run("…workload that leaks…")
    local r = vm:run("dmesg | grep -i 'BUG:'")
    -- KASAN / UBSAN reports show up here in the debug build.
end)
```

### Cross-version compatibility

Two profiles, one per release line. Ship a small handful of cross-version
tests:

```lua
test("rolling upgrade: stable client + mainline server", function(t)
    -- … as above …
end)

test("rolling upgrade: mainline client + stable server", function(t)
    -- … inverse …
end)
```

Tag these `cross-version` and gate through `--no-tag cross-version` in the dev
loop. CI runs them on a nightly cadence with `--include-slow`.

### Booting only a kernel

A profile that names no `initrd` boots the agent overlay alone: the agent is
PID 1 and supplies the guest userspace. Paired with a `root` composed to hold
the kernel, that gives a smaller test environment. Results still depend on
the agent and harness as well as the kernel.

```toml
# profiles/kernel-only/profile.provium.toml
build   = "./build.sh {out}"
root    = "{out}/root"
cmdline = "console=hvc0 quiet panic=1"
```

The project supplies `build.sh`. Keep dependency tracking in that builder;
Provium runs its hook on
[each invocation](~provium/configuration/dynamic-profiles#making-a-builder-incremental).

No `kernel` either: Provium finds it inside the composed root. Tests then
drive the kernel directly, through the agent's syscall interface rather than
through any command:

```lua
test("stratafs refuses a duplicate stratum", function(t)
    local vm = provium:vm("v", "kernel-only"):boot()
    -- … mount(2) via the agent, assert on errno …
end)
```

### Feature-flag gating

Two profiles built from the same source with one feature toggled:

```toml
[profiles.peios-prefeatx]
kernel  = "/build/peios-prefeatx/bzImage"   # FEATX disabled
# …

[profiles.peios-postfeatx]
kernel  = "/build/peios-postfeatx/bzImage"  # FEATX enabled
# …
```

Tests that exercise FEATX-specific behavior pick `peios-postfeatx`; regression
tests run against both.

```lua
test("featx changes API behaviour", function(t)
    local before = provium:vm("v1", "peios-prefeatx"):boot()
    local after  = provium:vm("v2", "peios-postfeatx"):boot()

    local r1 = before:run("…")
    local r2 = after:run("…")
    t:assert_neq(r1.stdout, r2.stdout)
end)
```

## Per-VM cmdline overrides

The profile's `cmdline` is the default; `kernel_cmdline` in `vm:boot(opts)`
overrides it for one VM:

```lua
local vm = provium:vm("v", "peios")
vm:boot({kernel_cmdline = "console=ttyS0 quiet maxcpus=1 isolcpus=0"})
```

This is useful for one-off testing of cmdline-sensitive features. For cmdline
configurations you use repeatedly, declare a dedicated profile.

To add a parameter rather than restate the whole line, `kernel_cmdline_append`
keeps the profile's tokens and puts yours after them:

```lua
vm:boot({kernel_cmdline_append = "maxcpus=1"})
```

Where an appended token repeats one the profile already sets, the kernel takes
the last occurrence, so the appended value wins.

## Default profile selection

One subcommand takes an optional profile arg and falls back to "the first
profile in `provium.toml` sorted by name" when you omit it:

| Subcommand                                   | Behavior                                |
| -------------------------------------------- | --------------------------------------- |
| `provium repl --fixture <path>` (no profile) | Boots the first profile by sorted name. |

That is a boot-time choice and nothing more. It does not narrow the fixture
cache key: the test runner, `provium fixture build`,
`provium fixture rebuild`, `provium fixture stale` and
`provium repl --fixture` all fold every profile's kernel and initrd into the
key, so they address the same cache entry however many profiles you have
configured. Pre-warming in CI with `provium fixture build` warms the entry the
runner then reads.

To make the REPL default predictable, name your most common profile so it
sorts first alphabetically. `aaa-default` is ugly but it works; `peios` is
fine when it's your only profile.

## What happens to the cache when profiles change

The fixture cache key folds in the kernel and initrd identifiers of **every
profile** in `provium.toml`, sorted by name for determinism (the full key
model is on
[fixtures and dependencies](~provium/running-tests/fixtures-and-dependencies#what-ends-up-in-the-cache-key)).
Practical consequences for profile changes specifically:

| Action                             | Cache effect                                                               |
| ---------------------------------- | -------------------------------------------------------------------------- |
| Edit a kernel image (any profile)  | Every fixture invalidates.                                                 |
| Edit an initrd image (any profile) | Every fixture invalidates.                                                 |
| Add a new profile                  | Every fixture invalidates (the new profile's kernel/initrd are folded in). |
| Remove a profile                   | Every fixture invalidates.                                                 |
| Rename a profile                   | Usually invalidates everything (the fold order follows profile names).     |
| Edit `cmdline` on a profile        | Cache is unaffected (cmdline isn't in the key).                            |
| Change `cache_dir`                 | Cache is unaffected, the new dir is empty.                                 |

This is deliberately conservative. A new profile means a new kernel could
behave differently, so the existing fixtures might be subtly stale. Rather
than guess, the harness rebuilds.

If you need to add a profile without invalidating the cache, you can't. Either
accept the rebuild or use a sibling `provium.toml` with
`--config alt-config.toml`.

## Multi-arch profiles (preview)

In v1, the QemuVmm backend invokes `qemu-system-x86_64` exclusively, there's
no per-profile arch field yet. To run on aarch64, you'd have to swap the
binary at the harness level (not exposed).

When multi-arch lands, the profile is the natural place for an
`arch = "x86_64"` / `arch = "aarch64"` field.

## See also

- [provium.toml reference](~provium/configuration/provium-toml): every field.
- [VMs and profiles](~provium/writing-tests/vms-and-profiles),
  `provium:vm(name, profile, opts?)`.
- [Fixtures and dependencies](~provium/running-tests/fixtures-and-dependencies),
  what triggers a fixture rebuild.
