---
title: Building Images with Dynamic Profiles
type: how-to
description: Connect an existing image builder to a profile's build hook.
related:
  - provium/configuration/provium-toml
  - provium/configuration/profiles
  - provium/running-tests/fixtures-and-dependencies
---

Use a dynamic profile to run your image builder before tests boot a guest.
Provium invokes the builder and consumes its output. The project supplies
the builder; Provium does not compose an operating-system image itself.

## Before you start

- Provide a working image-build script and its declared inputs.
- Decide which output files contain the kernel, initrd, and command line.
- Make the script return a nonzero status if any required build step fails.

The example below assumes `build-image.sh OUTPUT` produces `bzImage`,
`initrd.cpio.gz`, and `cmdline` in the supplied directory. Adapt these names
to your builder's output contract.

## Connect the build hook

1. Add a profile to `provium.toml`:

   ```toml
   [profiles.peios]
   build = "./build-image.sh '{out}'"
   kernel = "{out}/bzImage"
   initrd = "{out}/initrd.cpio.gz"
   cmdline_file = "{out}/cmdline"
   ```

2. Build the profile without starting a VM:

   ```sh
   provium prepare peios
   ```

3. Confirm that the output directory contains the three expected files.
   Then run the tests on a host prepared for VM execution:

   ```sh
   provium tests/
   ```

The hook runs through `sh -c` with the invoking environment. An inline
profile uses Provium's working directory. A profile discovered through
`from_dir` uses its own directory. Keep build commands appropriate to that
working directory.

## Reading the command line

`cmdline_file` supplies the kernel command line at boot. Provium collapses
whitespace and appends any inline `cmdline` text. For example, add a debug
setting without copying the builder's complete command line:

```toml
cmdline_file = "{out}/cmdline"
cmdline = "loglevel=7"
```

The effect of repeated kernel parameters depends on the parameter. Do not
assume every parameter uses its last occurrence.

## Choose an output directory

`{out}` expands to the profile's build directory in both the command and
path fields. Set `build_out` for an explicit location:

```toml
build_out = "/tmp/provium-builds/peios"
```

Without that setting, Provium uses its configured build-cache base and
appends the profile name. See
[`build_out`](~provium/configuration/provium-toml#build-out) for the fallback
order. Provium creates the directory but does not empty it. The builder
owns its contents and decides which outputs can be reused.

## When the build runs

The test runner builds all profiles with hooks before scheduling files.
Lua chooses profiles at runtime, so discovery cannot determine which
profiles a file will use. Console and interactive sessions build their
selected profile.

`provium prepare` builds all dynamic profiles without booting. Supply a
profile name to prepare only that profile. Preparation skips VM preflight,
so it can run on a build host without KVM access.

`--no-build` skips automatic hooks for that invocation. Use it only when
the existing artifacts are the intended test inputs. It does not prevent
an explicit `prepare` command from building.

## Making a builder incremental

Provium invokes the hook on each run. Dependency tracking belongs to the
builder. Prefer a build system that records its inputs and preserves
unchanged output files rather than adding a second cache in the profile.

If a project-specific wrapper records completion, publish that record only
after all outputs are complete. An interrupted build must not leave a
record that identifies partial output as successful.

The fixture cache includes kernel and initrd identity information. Replacing
those files can invalidate fixtures even when their names stay the same.
See [fixtures and dependencies](~provium/running-tests/fixtures-and-dependencies).

## Check the result

A successful preparation exits with status 0. A build command that fails
stops the operation with status 2; Provium does not continue to boot an
older artifact after reporting a failed build.

For qualification, retain the build result and exact artifact identities
with the test log. A successful hook confirms its exit status, not that the
builder included every intended source change.
