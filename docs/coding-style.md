# Coding Conventions

Provium follows Panorama's PSC common rules and Rust, Python, shell, and
Bazel profiles. Use four spaces and a 100-column formatter target. Prose
uses US English and wraps at 78 columns. The canonical rules are in
`../learn/4--standards-and-conventions.shelf/psc.product` relative to the repository.

`tools/check` runs the pinned mechanical checks, Bazel tests, and strict
Clippy. `tools/check-docs.py` checks published documentation. Manual review
also covers names, units, resource ownership, error handling, and whether
long functions have more than one responsibility.

## Repository boundaries

- Keep wire types in `provium-protocol`, QEMU monitor communication in
  `provium-qmp`, guest operations in `provium-agent`, and host orchestration
  in `provium-host`.
- Lua method-registration tables can be longer than the soft function
  guideline when they describe one userdata interface. Extract substantive
  operations into named helpers when that makes the interface clearer.
- Wire field names, Lua methods, environment variables, and command-line
  flags are compatibility contracts. A spelling cleanup does not rename
  them without an explicit compatibility decision.
- Tests can use `unwrap` or `expect` to identify a failed setup or assertion.
  Runtime code returns expected operational failures to its caller.
- Vendored PPK sources and dependency patches retain upstream provenance.
  Apply house style to maintained integration code, not imported snapshots.
