# Shared conformance helpers

This bundle exports generic syscall, MessagePack, and SHA-1 helpers used by
component suites. It contains no standalone test files. The central runner
loads these helpers from the Provium revision in its suite lock.

`suite.json` preserves the import origin and maps each source file into the
run layout. `LICENSE` preserves the original MIT notice.

Component-specific helpers remain with their components. Run the existing
Provium unit and conformance checks when changing generic machinery, then
run the affected component suites against supplied images and tools.
