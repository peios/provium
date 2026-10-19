---
title: Running a First Test
type: how-to
description:
  Configure a guest profile and run a small test against a Peios image.
related:
  - provium/getting-started/project-structure
  - provium/configuration/provium-toml
  - provium/writing-tests/vms-and-profiles
  - provium/running-tests/the-cli
---

Run a small guest test to check that your profile, agent, and host setup work
together. This procedure uses installed Provium tools. Contributor build
instructions belong in the repository's `docs/building.md`.

## Before you start

- Install `provium` with its matching `agent-overlay.cpio.gz`.
- Prepare a compatible Peios kernel and initrd with a working `/init`. The
  guest kernel needs vsock support.
- Provide QEMU and access to `/dev/kvm` and `/dev/vhost-vsock` on the host.
- Provide `ip`, `tc`, and `nft`, and the host network privileges required by
  Provium's preflight check.
- Use disposable guest state. The commands in a test operate on its guest.

Provium looks for the overlay beside an installed binary at
`../share/provium/agent-overlay.cpio.gz`. Set `PROVIUM_OVERLAY` or a profile's
`agent_overlay_path` when the file is elsewhere. Keep the host runner and
agent from the same build.

## Configure a profile

1. Create a project directory and a `tests` subdirectory.
2. Add `provium.toml` with the following content. Replace the kernel and
   initrd paths with the artifacts prepared for your test system.

   ```toml
   [provium]
   roots = ["tests"]

   [profiles.peios]
   kernel = "/path/to/bzImage"
   initrd = "/path/to/initrd.cpio.gz"
   cmdline = "console=ttyS0 quiet"
   guest_os = "peios"
   ```

3. From the project directory, check discovery and preflight:

   ```sh
   provium tests/ list
   ```

The `roots` setting supplies Lua helper and fixture search paths. The
command's positional paths choose which tests to discover. With no paths, the
command searches the current directory.

By default, Provium adds its agent overlay to the profile's initrd and chains
to the image's `/init`. A profile that already contains the matching agent can
set `inject_agent = false`. See
[profile configuration](~provium/configuration/provium-toml).

## Write and run the test

1. Create `tests/smoke.test.lua`:

   ```lua
   test("guest file round trip", function(t)
       local vm = provium:vm("smoke", "peios"):boot()
       vm:write_file("/tmp/provium-smoke", "hello")
       t:assert_eq(vm:read_file("/tmp/provium-smoke"), "hello")
   end)
   ```

2. Run the file from the project directory:

   ```sh
   provium tests/smoke.test.lua
   ```

## Check the result

The run should report one passed test and exit with status 0. The VM belongs
to the test's scope and is closed when that scope ends.

A failed assertion or file timeout produces status 1. A configuration,
preflight, or infrastructure error produces status 2. Resolve a preflight
failure before relying on a VM result; skipping the check does not provide
missing host capabilities.

To confirm assertion failures are visible, change the expected string to
`"different"` and run the file again. It should fail with status 1. Restore
`"hello"` before keeping the test.

For repeated runs, use `provium tests/ --watch`. Use `--json` when another
tool needs structured file results. `provium(1)` and the
[command reference](~provium/reference/cli) describe the remaining options.
