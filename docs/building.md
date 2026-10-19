# Building and Checking Provium

The supported development host is x86-64 Linux. Bazel owns the build graph,
tests, guest archives, and PPK packaging. Cargo remains available for Rust
editor support and Clippy; CI checks both against the committed lockfile.

## Development prerequisites

Provide Python 3.11 or later, a C toolchain, binutils, cpio, gzip, curl, and a JDK
supported by Bazel 8.4.2. The wrapper downloads and verifies the pinned
Bazel binary. Bazel downloads the pinned Rust 1.98.1 toolchains and locked
crate dependencies. Lua is compiled from its declared vendored sources.

The style check also needs Rust 1.98.1 with rustfmt and Clippy, Node.js, and
the tools pinned by `tools/style-tools.lock.json`. Prepare those tools
outside the source tree:

```sh
python3 tools/prepare-style-tools.py /tmp/provium-style
export PEIOS_STYLE_BIN=/tmp/provium-style/bin
```

## Build and test

```sh
./bazel build //:artifacts
./bazel test //...
tools/check
```

`//:provium` builds the host runner, `//:provium-coverage` builds the event
consumer, and `//:agent-static` builds the musl-linked static guest agent.
`//:agent-overlay` places the agent at `sbin/provium-agent` in a deterministic
cpio archive. `//:agent-initrd` places it at `init`. These guest targets
select their own toolchain; changing the host build does not accidentally
make the guest agent depend on the host's dynamic loader.

`//:artifacts` includes both host binaries and both archives. Find their
paths with `./bazel cquery TARGET --output=files`. Run the CLI through the
launcher to select the matching overlay automatically:

```sh
./bazel run //:run -- --config /path/to/provium.toml /path/to/tests
```

The launcher preserves the caller's working directory. The compatibility
scripts `scripts/build-overlay.sh` and `scripts/build-initrd.sh` call Bazel
and copy an archive to `dist/` or the path supplied with `-o`.

The normal tests use disposable files and mock services. KVM lifecycle
tests are manual targets. Ignored or environment-skipped tests do not count
as guest qualification. Do not start `provium-agent` as PID 1 or point a
local-backend test at important host files as part of ordinary validation.

## Build packages

```sh
./bazel build //:packages
```

`bazel-bin/packages.store` contains content-addressed blobs and manifests
for four development packages:

- `dev.peios.provium`: host commands, guest archives, manuals, and notices.
- `dev.peios.provium-debuginfo`: matching split debug data.
- `dev.peios.provium-debugsource`: declared source inputs for debugging.
- `dev.peios.provium-source`: the repository source needed to rebuild.

The development version is `0.1.0-1`. Packaging validates ELF layout,
checks matching debug identities, collects notices from the declared
crate closure, and verifies blob hashes. The source package includes the
Bazel and Cargo manifests and lockfiles. It still needs the pinned tools
and external inputs; it is not an offline dependency bundle.

The default host build targets GNU/Linux. It is not a native Peios release.
Package provenance records the userspace ABI and qualification limits.
Native packages also remain unsigned development artifacts until the
separate release and guest gates are met.

## Native Peios toolchains

`--config=peios` selects the installed Peios Rust compiler and GNU and musl
standard libraries. Set `PEIOS_RUSTC` if the compiler is not `/usr/bin/rustc`.
The compiler must be at least version 1.98.1 and supply the Peios hardening
support. Run this configuration inside the intended Peios userspace with
its C toolchain and libraries.

Acquire dependencies in a connected development environment:

```sh
tools/offline-build acquire /absolute/path/provium-inputs
```

Then place the same source tree and inputs in an isolated native build
environment and use a fresh writable work directory:

```sh
tools/offline-build build /absolute/path/provium-inputs /absolute/path/work
tools/offline-build test /absolute/path/provium-inputs /absolute/path/work
```

The offline consumer refuses a different module lock and disables
repository downloads. Record the source revision or tree hash, toolchain
identity, input snapshot, logs, and output hashes for qualification.
Native package checks require host CET properties and packed relocations.
The static musl guest has documented startup and relocation exceptions in
its build record; do not infer runtime shadow-stack activation from ELF
properties.

## Documentation and VM checks

Use the pinned Trail revision from `.github/workflows/check.yml`:

```sh
TRAIL=/path/to/trail python3 tools/check-docs.py
```

This checks Learn lint, strict rendering, links, and manual rendering. It
does not run documentation examples against a guest.

A real-QEMU smoke run needs an explicitly supplied compatible kernel:

```sh
KERNEL=/path/to/bzImage scripts/smoke-real-qemu.sh
```

Run it only in a prepared test host with KVM and vsock access. Retain its
logs separately from ordinary unit-test and build evidence. It checks the
standalone agent environment; full Peios boot, injected-agent startup,
networking, and fixture behavior need their own image-specific tests.
