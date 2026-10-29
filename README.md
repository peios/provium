# Provium

Provium runs Lua tests against virtual machines. The host runner controls
QEMU, while a guest agent provides commands, files, processes, and other
operations inside the test system.

Build the host tools and static guest archives on x86-64 Linux with pekit:

```sh
pekit build provium provium_coverage agent_overlay agent_initrd
```

Run a configured test project with the built command, and name the
matching agent overlay in `PROVIUM_OVERLAY`, as the
[build guide](docs/building.md) shows. A real VM run also needs QEMU, KVM,
guest images, and the host access described in the documentation.

- [Building and checking](docs/building.md)
- [Coding conventions](docs/coding-style.md)
- [Learn source](docs/learn/2--developer-tools.shelf/provium.product)
- [Command manuals](docs/man)

A successful build does not establish guest qualification or release
readiness. Packages and native Peios builds are not yet built with pekit.

Provium is licensed under either the Apache License 2.0 or the MIT License.
See [LICENSE-APACHE](LICENSE-APACHE) and [LICENSE-MIT](LICENSE-MIT).
