# Building and Checking Provium

Use pekit to build Provium's host programs, its guest agent and the agent's
archives, and to run their tests. Pekit builds each crate on its own, so a
change rebuilds only the crates it reaches. The supported development host
is x86-64 Linux.

## Before you start

You need pekit and Git. Everything else is pinned and fetched by pekit on
the first build: the Bootlin C toolchains, Rust 1.98.1 with Clippy and the
musl standard library, Python 3.14, busybox, and the crates `Cargo.lock`
selects. Lua is compiled from the sources vendored in its crate. The first
build needs network access; later builds find what they need in pekit's
store, and every step runs without network access in a sandbox that holds
only its declared inputs.

The build files use the gcslib libraries named in `workspace.gcsl` and the
build files. `pekit lock` pins their commits in `pekit.lock`.

## Build

```sh
pekit build provium provium_coverage agent agent_overlay agent_initrd
```

| Target | Output |
|---|---|
| `provium` | The host command, which runs Lua tests against virtual machines |
| `provium_coverage` | The coverage report tool, which consumes Provium's events |
| `agent` | The guest agent, a static, position-independent musl program |
| `agent_overlay` | A deterministic newc archive with the agent at `sbin/provium-agent` |
| `agent_initrd` | A deterministic newc archive with the agent as `init` |

Pekit prints where each output is, under `pekit-out/<target>/`. The host
programs are built for glibc. The agent is built for musl, linked with full
RELRO and immediate binding, and has no ELF interpreter, so it never
depends on the host's dynamic loader. `build/agent-archive.py` writes the
archives with zero timestamps and root ownership.

Run a configured test project with the built command, and name the
matching overlay in `PROVIUM_OVERLAY`:

```sh
scripts/build-overlay.sh
provium=$(pekit build provium | sed -n 's/^  //p')
PROVIUM_OVERLAY="$PWD/dist/agent-overlay.cpio.gz" \
    "$provium" --config /path/to/provium.toml /path/to/tests
```

Pekit prints each output's path indented under its target. The compatibility
scripts `scripts/build-overlay.sh` and `scripts/build-initrd.sh` build an
archive with pekit and copy it to `dist/`, or to the path given with `-o`.

## Test

```sh
pekit test
```

| Test | Check |
|---|---|
| `protocol`, `qmp`, `agent`, `host`, `coverage` | Each member's unit tests, its programs' tests, and its integration tests |
| `doctests` | Each library's documentation tests |
| `agent_archive` | The archive writer's output is reproducible and extracts as newc, with busybox's `cpio` |
| `clippy` | Clippy's checks of every target of every member, with every warning denied |

Integration tests that run a program are told where it is, as Cargo tells
them in `CARGO_BIN_EXE_<name>`, and pekit places the program there when
they run. The normal tests use disposable files and mock services. The
`kvm_lifecycle` tests boot a real virtual machine with KVM, so pekit builds
them and does not run them. Ignored or environment-skipped tests do not
count as guest qualification. Do not start `provium-agent` as process 1 or
point a local-backend test at important host files as part of ordinary
validation.

A test whose inputs did not change is found in the store and not run
again.

For the full contributor check, install the pinned PSC tools into a
directory outside the repository, then run the shared entry point:

```sh
python3 tools/prepare-style-tools.py "$HOME/.cache/provium-style"
export PEIOS_STYLE_BIN="$HOME/.cache/provium-style/bin"
tools/check
```

`tools/check` checks owned source formatting and lint with rustfmt, Ruff,
ShellCheck, shfmt, and Prettier, then runs `pekit test`. The style checks
use the host's Rust 1.98.1 formatter through rustup. The tool versions and
hashes are recorded in `tools/style-tools.lock.json`. GitHub's workflow
runs the style checks alone; builds and tests run with pekit.

## How the build is planned

`build.gcsl` plans the Cargo workspace with the gcslib rust library's
`workspace`, once for glibc and once for musl: Cargo works out each
member's build, unit by unit, and pekit compiles each unit as a step.
Planning reads the manifests, `Cargo.lock`, and which files exist, never
the sources, so editing a source plans nothing again. Keep Rust dependency
versions in the Cargo manifests and `Cargo.lock`, and update the lockfile
deliberately with Cargo.

## Not built with pekit yet

Packages, debug information, and builds with the native Peios toolchain,
which hardens programs with control-flow enforcement technology (CET), are
not yet built with pekit. The package definitions, their validation, and
the native build settings from the Bazel build are kept outside this
repository until they are. Pekit does not build Cargo examples, so the
real-QEMU smoke run builds its runner with Cargo.

## Documentation and VM checks

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
