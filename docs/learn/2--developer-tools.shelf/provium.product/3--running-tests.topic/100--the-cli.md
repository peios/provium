---
title: Running Tests with the CLI
type: how-to
description:
  Patterns for the provium binary, discovery, filtering, the watch loop,
  --rerun-failed, --since, --tag, --include-slow, JSON output, and exit codes
  for CI.
related:
  - provium/reference/cli
  - provium/running-tests/events-and-coverage
  - provium/running-tests/fixtures-and-dependencies
  - provium/running-tests/pools-and-parallelism
---

The `provium` binary is what you run. This page covers the day-to-day
patterns. The full flag inventory is on
[the CLI reference](~provium/reference/cli).

## The basic invocation

```text
provium tests/
```

Walks `tests/` for `*.test.lua` files and runs each. Default output:

```text
PASS tests/smoke.test.lua (3 passed, 0 failed, 0 skipped)
FAIL tests/networking/partition.test.lua (2 passed, 1 failed, 0 skipped)
    FAIL split-brain
        client A could not reach client B after partition

2 file(s); 5 passed, 1 failed, 0 skipped; 12.34s
```

A single file works too:

```text
provium tests/networking/partition.test.lua
```

If you omit paths entirely, the current directory is scanned recursively.

## Selecting what runs

### `--filter <substring>`

Match against the test-root-relative path:

```text
provium tests/ --filter networking
provium tests/ --filter partition.test
```

### `--rerun-failed`

After every run with at least one failure, Provium writes the canonical paths
to `~/.cache/provium/rerun.json`. `--rerun-failed` intersects discovered files
against this list:

```text
provium tests/ --rerun-failed
```

If no prior failure state exists, `--rerun-failed` exits cleanly with a notice
(running the FULL suite is almost never what the user wanted).

A clean run leaves the failed-state file alone, so the failed set persists
until you fix the failures.

### `--since <reference-file>`

Run only files whose mtime is newer than the reference file:

```text
provium tests/ --since main.lua          # files edited after main.lua
provium tests/ --since .last-checkpoint  # custom marker
```

Useful for "what's changed since I last ran".

### Per-test filters

Tag and slow filtering happen inside each file (the test body's `meta` is
what's matched):

```text
provium tests/ --tag smoke
provium tests/ --tag smoke --tag fast    # OR: any tag matches
provium tests/ --no-tag flaky            # exclude tests tagged flaky
provium tests/ --include-slow            # include meta.slow tests
```

`--no-tag` wins over `--tag` (intersect semantics).

For filtering on arbitrary meta fields (not only `tags`), use
`--tag-meta KEY=VALUE`:

```text
provium tests/ --tag-meta subsystems=peinit
provium tests/ --tag-meta subsystems=peinit --tag-meta subsystems=loregd   # OR within KEY
provium tests/ --tag-meta subsystems=peinit --tag-meta area=boot            # AND across KEYs
provium tests/ --no-tag-meta flaky=true
```

This is the recommended pattern when test files are organized by user-visible
scenario rather than by code subsystem, annotate tests with
`subsystems = {"peinit", "loregd"}` (or any other axis you care about) and let
CI compute "what to run" from the changeset:

```bash
# CI: PR touched the peinit subsystem
provium tests/ --tag-meta subsystems=peinit
```

See [meta tags](~provium/reference/meta-tags) for the convention.

Skipped tests still appear in outcomes, you'll see `SKIP <name>` in `-v` mode,
with the filter expression as the reason.

## Output modes

### Default (compact)

```text
FAIL tests/x.test.lua (2 passed, 1 failed, 0 skipped)
    FAIL the failing case
        message lines
```

### Verbose (`-v`)

Every test status, not only failures:

```text
PASS tests/x.test.lua (3 passed, 0 failed, 0 skipped)
    PASS first
    PASS second
    PASS third
```

### Quiet (`-q`)

Only failures show. Files with all passing / skipped are silent. The summary
line still prints.

### JSON (`--json`)

Line-delimited JSON, one object per file:

```json
{
  "path": "tests/x.test.lua",
  "timeout": "in_time",
  "passed": true,
  "chunk_error": null,
  "tests": [{ "name": "…", "status": "passed", "message": null, "log": [] }]
}
```

Useful for piping into another tool. Mutually exclusive with
`--events-stdout`.

### Msgpack events (`--events-stdout`)

Length-prefixed msgpack frames over stdout. The human-readable / JSON renderer
redirects to stderr automatically. Useful with `provium-coverage`:

```text
provium tests/ --events-stdout | provium-coverage
```

See [events and coverage](~provium/running-tests/events-and-coverage) for the
full event-stream story.

## Watch mode

```text
provium tests/ --watch
```

Polls every 500 ms for stat-changed files; re-runs when one changes. Use
Ctrl-C to exit. The empty-set case is fine, the watcher rescans every tick, so
dropping a `*.test.lua` into the watched root after launch triggers discovery.

`--rerun-failed` is automatically dropped under `--watch` so file-edit
detection works as expected (otherwise each tick would re-run only the
original failed set forever).

## Per-file timeouts

```text
provium tests/ --timeout 300            # 5 min (default)
provium tests/ --timeout 30s
provium tests/ --timeout 10m
provium tests/ --timeout 500ms
provium tests/ --timeout 0              # disable
```

Bare integers are seconds. Suffixes accepted: `ms`, `s`, `m`, `h`. `0`
disables the timeout entirely.

The timeout is a budget for the file's own work. Time the file spends queued
in the resource pool, at its `provium:claim`, or at a `vm:boot()` when it has
no claim, is not counted; see
[pools and parallelism](~provium/running-tests/pools-and-parallelism#file-timeouts-and-queueing).

When a file's timeout fires, the harness records it as `timed_out`, marks the
file as failed for exit-code purposes, and emits a `file_completed` event with
`status = "timed_out"`.

## Pool and CPU controls

By default Provium uses 80 % of host RAM and the full host CPU count for the
resource pool. Override:

```text
provium tests/ --mem 16G                 # explicit pool memory
provium tests/ --cpus 8                  # explicit pool vCPUs
provium tests/ --cpu-overcommit 1.5      # multiplier on --cpus
```

See [pools and parallelism](~provium/running-tests/pools-and-parallelism) for
how the pool, claims, and dispatcher interact, including when overcommit is
safe. Flag defaults and the clamp range are in the
[CLI reference](~provium/reference/cli#vmm-and-resources).

## Dev-mode flags

### `--no-preflight`

Skip the `/dev/kvm` / iproute2 / nft / qemu / `CAP_NET_ADMIN` checks. Use in
containers or CI environments where you know the environment is fine but the
checks would fail.

### `--no-ksm`

Skip Kernel Same-page Merging tuning at startup. Default tunes
`/sys/kernel/mm/ksm/*` per the design's pool-density goals; pass this on
shared dev hosts where you don't want global tuning.

### `--vmm local`

Use the in-process LocalAgent backend instead of QEMU. Useful when KVM isn't
available (CI, dev-on-laptop). The local backend does not actually boot a
kernel, it gives the host bindings something to dispatch against, but tests
that rely on guest-side behavior (running commands, file I/O) won't work.

```text
provium tests/ --vmm local
```

## Subcommands

When a subcommand is given, the test-runner mode is suppressed.

### `provium repl <profile>`

Boot a VM and drop into an interactive Lua REPL:

```text
provium repl peios                  # cold-boot the peios profile
provium repl peios --name dev       # custom VM name
provium repl --fixture base         # resume from a fixture
```

Useful for poking at a guest interactively. The full Provium API is available,
`vm:run`, `vm:read_file`, `vm:tail_file`, and so on

### `provium fixture <op>`

Manage the fixture cache:

```text
provium fixture list                       # show every cached entry
provium fixture build path/to/fixture      # force-build
provium fixture rebuild path/to/fixture    # evict and rebuild
provium fixture clean                      # wipe the cache
provium fixture stale                      # list fixtures whose source doesn't match any cache entry
```

See
[fixtures and dependencies](~provium/running-tests/fixtures-and-dependencies)
for when each is useful.

### `provium prepare [profile]`

Run dynamic profiles' `build` commands without booting anything, one profile
by name, or every profile that declares a `build`. Pre-warm images with
`provium prepare && provium tests/ --no-build`; see
[dynamic profiles](~provium/configuration/dynamic-profiles).

### `provium list`

List discovered tests / fixtures without running anything:

```text
provium list                 # tests
provium list --fixtures      # fixtures
```

### `provium lsp-setup [dir]`

Write Lua Language Server stubs and a `.luarc.json` into a test directory so
the harness globals resolve in your editor, see
[the CLI reference](~provium/reference/cli#provium-lsp-setup-dir-force).

## Exit codes

For CI, three values: `0` means every file passed (or skipped), `1` means at
least one file did not finish cleanly, `2` means an internal error (config,
pre-flight, dispatcher). The exit code is deliberately clamped to those three,
the precise triggers and the clamping rationale are in the
[CLI reference](~provium/reference/cli#exit-codes).

## CI patterns

### "Run everything, fail on any failure"

```text
#!/bin/sh
provium tests/
```

Exit code is your test result. Capture stdout for the summary, stderr for the
human-readable lines (when `--json` is set).

### "Run with structured output"

```text
provium tests/ --json | tee results.jsonl
```

Each line is a complete `{"path":…,"tests":[…]}` object. Easy to pipe into a
custom dashboard or per-test reporter.

### "Save events for post-hoc analysis"

```text
provium tests/ --save-events events.msgpack
provium-coverage --from events.msgpack > coverage.html
```

The msgpack file is portable; you can analyze it on a different host.

### "Fail fast"

```text
provium tests/ --fail-fast
```

Stops after the first failed file. Useful for tight inner loops where you want
to see the first failure quickly.

### "Slow CI and fast CI"

```text
# Fast: skip slow tests by default.
provium tests/

# Slow / nightly: run everything.
provium tests/ --include-slow
```

`meta.slow = true` tests are skipped by default. Tag them in the test bodies,
and your dev loop stays fast while nightly catches the slow stuff.

### "Parallel CI sharding"

There's no built-in shard splitter, but `provium` accepts any number of path
arguments, so split the file list in your CI script and pass each shard its
own files:

```text
# $SHARDS = total shard count, $INDEX = this shard (0-based).
git ls-files 'tests/**/*.test.lua' | awk "NR % $SHARDS == $INDEX" | xargs provium
```

Each shard runs a disjoint subset of files. Don't try to build a shard with
`--filter`, it's a single substring match against the relative path, not a
pattern list.

## See also

- [CLI reference](~provium/reference/cli): every flag, every env var.
- [Events and coverage](~provium/running-tests/events-and-coverage),
  `--save-events`, `--events-stdout`, `--coverage`.
- [Pools and parallelism](~provium/running-tests/pools-and-parallelism): how
  the resource pool works.
