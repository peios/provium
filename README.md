# Provium

Provium runs Lua tests against virtual machines. The host runner controls
QEMU, while a guest agent provides commands, files, processes, and other
operations inside the test system.

Build the host tools and static guest archives on x86-64 Linux:

```sh
./bazel build //:artifacts
```

Run a configured test project from the repository:

```sh
./bazel run //:run -- --config /path/to/provium.toml /path/to/tests
```

The launcher selects the matching agent overlay. A real VM run also needs
QEMU, KVM, guest images, and the host access described in the documentation.

- [Building and checking](docs/building.md)
- [Coding conventions](docs/coding-style.md)
- [Learn source](docs/learn/provium.product)
- [Command manuals](docs/man)

`./bazel build //:packages` creates development PPK blobs and manifests.
A successful build does not establish guest qualification or release
readiness. Native Peios builds use the separate, recorded toolchain path
in the build guide.

Provium is licensed under either the Apache License 2.0 or the MIT License.
See [LICENSE-APACHE](LICENSE-APACHE) and [LICENSE-MIT](LICENSE-MIT).
