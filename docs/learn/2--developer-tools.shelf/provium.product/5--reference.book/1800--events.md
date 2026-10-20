---
title: Events
description:
  Every event variant and field in Provium's msgpack event stream, file
  lifecycle, test outcomes, VM spawns, pool state, claims, and fixture builds.
related:
  - provium/running-tests/events-and-coverage
  - provium/reference/protocol-version
  - provium/reference/cli
---

Provium's host-side scheduler and runners emit observability events that drive
the human-readable summary, the `--json` output, `provium-coverage`, and any
other consumer that wants to follow what the harness is doing.

The transport is **length-prefixed msgpack frames** (the same framing as the
wire protocol). Get them through `--save-events <path>`, `--events-stdout`, or
`--events-socket <path>`.

## Frame shape

Each frame is an `EventFrame`:

```text
{
  "ts":      <i64>,    // ns since Unix epoch, host clock at emission
  "kind":    "<name>", // discriminator (snake_case)
  "payload": { … }     // variant-specific
}
```

The `Event` enum is flattened into the envelope so `kind` and `payload` appear
at the top level of each frame.

`PROTOCOL_VERSION` (`1`) is bumped on any wire-shape change. Adding a new
variant or field is a strict version bump; removing or renaming one is
breaking. See [protocol version](~provium/reference/protocol-version) for the
pinning policy.

## File lifecycle

### `file_discovered`

Emitted once per `*.test.lua` file before any dispatching begins.

| Field            | Type                      | Description                                                                                        |
| ---------------- | ------------------------- | -------------------------------------------------------------------------------------------------- |
| `path`           | string                    | Test-root-relative path.                                                                           |
| `fixture_refs`   | array of strings          | Fixtures referenced by `vm_fixture(...)` / `lab_fixture(...)` (path with `.fixture.lua` stripped). |
| `declared_claim` | optional `ResourceAmount` | `provium:claim(...)` declared at file scope, if any.                                               |

### `file_dispatched`

A test file was picked up by a runner thread.

| Field         | Type             | Description                                                      |
| ------------- | ---------------- | ---------------------------------------------------------------- |
| `path`        | string           | File path.                                                       |
| `reservation` | `ResourceAmount` | Resources reserved at dispatch (sum of runner overhead + claim). |

### `file_blocked`

A test file is waiting on the resource pool or PSI pressure.

| Field         | Type             | Description                            |
| ------------- | ---------------- | -------------------------------------- |
| `path`        | string           | File path.                             |
| `waiting_for` | `ResourceAmount` | What the file is waiting on.           |
| `reason`      | string           | `pool_full`, `psi_pressure`, and so on |

### `file_completed`

A test file finished, successfully, with failures, or terminated by timeout /
panic.

| Field         | Type   | Description                                 |
| ------------- | ------ | ------------------------------------------- |
| `path`        | string | File path.                                  |
| `status`      | enum   | `passed`, `failed`, `timed_out`, `crashed`. |
| `duration_ns` | u64    | Wall-clock duration.                        |

`crashed` means the runner panicked or hit a test-infrastructure failure;
remaining tests in the file are marked `failed-due-to-poisoned-state`.

## Test lifecycle

### `test_started`

A `test()` block within a file started.

| Field  | Type      | Description                                       |
| ------ | --------- | ------------------------------------------------- |
| `path` | string    | Containing file's path.                           |
| `name` | string    | `test()` block name.                              |
| `meta` | `MetaMap` | Per-test metadata. Always present; empty if none. |

### `test_skipped`

The test was filtered or marked Skipped (declarative `meta.skip`, inline
`t:skip()`, file-scope `todo()`, or tag/slow filter).

| Field    | Type      | Description                                                   |
| -------- | --------- | ------------------------------------------------------------- |
| `path`   | string    | Containing file's path.                                       |
| `name`   | string    | Test name.                                                    |
| `reason` | string    | Filter expression, `t:skip()` argument, or `todo()` argument. |
| `meta`   | `MetaMap` | Per-test metadata.                                            |

### `test_passed`

| Field         | Type      | Description               |
| ------------- | --------- | ------------------------- |
| `path`        | string    | File path.                |
| `name`        | string    | Test name.                |
| `duration_ns` | u64       | Test wall-clock duration. |
| `meta`        | `MetaMap` | Per-test metadata.        |

### `test_failed`

| Field             | Type      | Description                                                 |
| ----------------- | --------- | ----------------------------------------------------------- |
| `path`            | string    | File path.                                                  |
| `name`            | string    | Test name.                                                  |
| `duration_ns`     | u64       | Wall-clock up to failure.                                   |
| `reason`          | string    | Assertion message, exception text, or panic payload.        |
| `console_excerpt` | string    | Last 4 KiB from each booted VM's console log. Can be empty. |
| `meta`            | `MetaMap` | Per-test metadata.                                          |

## VM lifecycle

### `vm_spawned`

| Field          | Type   | Description                |
| -------------- | ------ | -------------------------- |
| `file`         | string | File that owns this VM.    |
| `vm_name`      | string | Name as given to `lab:vm`. |
| `profile`      | string | Profile used to boot.      |
| `memory_bytes` | u64    | Memory cap.                |
| `cid`          | u32    | Assigned vsock CID.        |

### `vm_shutdown`

| Field         | Type   | Description              |
| ------------- | ------ | ------------------------ |
| `file`        | string | File that owned this VM. |
| `vm_name`     | string | VM name.                 |
| `duration_ns` | u64    | VM uptime.               |

## Pool and claims

### `pool_state`

Periodic snapshot of resource-pool usage. Default cadence: 1 Hz.

| Field       | Type             | Description |
| ----------- | ---------------- | ----------- |
| `used`      | `ResourceAmount` | in use.     |
| `available` | `ResourceAmount` | free.       |

### `claim_acquired`

A file successfully claimed resources through `provium:claim(...)`.

| Field    | Type             | Description                     |
| -------- | ---------------- | ------------------------------- |
| `path`   | string           | File path the claim belongs to. |
| `amount` | `ResourceAmount` | Amount claimed.                 |

### `claim_released`

A file released its claim. Pairs with `claim_acquired` on the same `path`.

| Field    | Type             | Description      |
| -------- | ---------------- | ---------------- |
| `path`   | string           | File path.       |
| `amount` | `ResourceAmount` | Amount released. |

## Fixtures

### `fixture_build_started`

| Field  | Type   | Description                                           |
| ------ | ------ | ----------------------------------------------------- |
| `path` | string | Fixture path (test-root-relative, no `.fixture.lua`). |

### `fixture_build_done`

| Field            | Type   | Description                             |
| ---------------- | ------ | --------------------------------------- |
| `path`           | string | Fixture path.                           |
| `duration_ns`    | u64    | Build duration.                         |
| `snapshot_bytes` | u64    | Cached snapshot size, post-compression. |

### `fixture_build_waiting`

A second file is waiting on the build lock another file holds.

| Field          | Type   | Description                   |
| -------------- | ------ | ----------------------------- |
| `path`         | string | Fixture path being waited on. |
| `held_by_file` | string | File holding the build lock.  |

### `fixture_cache_hit`

A fixture was resumed from its cached snapshot, no rebuild required.

| Field  | Type   | Description   |
| ------ | ------ | ------------- |
| `path` | string | Fixture path. |

## Shared types

### `ResourceAmount`

```text
{
  "memory_bytes": <u64>,
  "cpus":         <u32>
}
```

Used in pool / claim / reservation events.

### `MetaMap` and `MetaValue`

`MetaMap` is `BTreeMap<String, MetaValue>`. `MetaValue` is a tagged-by-type
union over native msgpack types:

| Variant | msgpack mapping           |
| ------- | ------------------------- |
| `Null`  | unit / nil                |
| `Bool`  | bool                      |
| `Int`   | i64                       |
| `Float` | f64                       |
| `Str`   | str                       |
| `Bytes` | bin (distinct from str)   |
| `Array` | array of MetaValue        |
| `Map`   | map of String → MetaValue |

The deserializer dispatches on the input type rather than relying on
declaration-order fallthrough, so valid-UTF-8 byte sequences are not
mis-classified as strings.

## Event guarantees

- `file_discovered` for every discovered file, before any dispatching.
- For each file: at most one `file_dispatched`, optional `file_blocked`(s)
  before it, exactly one `file_completed` after.
- For each test: exactly one `test_started`, then exactly one of `test_passed`
  / `test_failed` / `test_skipped`.
- `claim_acquired` and `claim_released` are paired per file.
- `fixture_build_started` and `fixture_build_done` are paired per build. A
  `fixture_build_waiting` might precede the `_done` if the file queued behind
  a peer.
- `fixture_cache_hit` fires AFTER successful restore, never on a corrupt entry
  that turns into a rebuild.

## Consuming the stream

The three output channels, `--save-events <path>` (file), `--events-stdout`
(pipe), and `--events-socket <path>` (multi-consumer Unix socket), and the
patterns for choosing between them are covered in
[events and coverage](~provium/running-tests/events-and-coverage#output-channels).

## See also

- [CLI](~provium/reference/cli): flags that produce / consume events.
- [Protocol version](~provium/reference/protocol-version): wire-shape pinning
  policy.
- [Events and coverage](~provium/running-tests/events-and-coverage): patterns
  for using the event stream.
