---
title: Protocol Version
description:
  PROTOCOL_VERSION pins the host-agent wire shape and the event stream, what
  it pins, when it bumps, and how the pin tests catch silent breakage.
related:
  - provium/reference/events
  - provium/running-tests/events-and-coverage
  - provium/reference/cli
---

Provium has one wire-version constant: `PROTOCOL_VERSION`, `1`.

It pins:

- The host ↔ agent protocol, every op, every result variant, every error
  shape.


- The observability event stream: every `Event` variant and every payload
  field.
- The Hello / HelloOk / HelloErr handshake and the `OpenMode` field set.

## When it bumps

`PROTOCOL_VERSION` increments any time:

- A new wire variant is added (op or event).
- A field is added to an existing payload struct.
- A new `OpenMode` flag, `ExecResult` variant, or `OpResult` shape lands.
- The `EventFrame` envelope changes.

It is **not bumped** for changes that don't reach the wire, internal struct
renames, host-side refactors, agent-side optimization, additional Lua bindings
that map onto existing wire ops.

## What it does NOT pin

- The Lua API surface (the userdata methods `vm`, `bridge`, and so on). That
  surface evolves freely; tests that use it are rebuilt against the same
  Provium version that runs them.
- Internal types like `Vm`, `Lab`, `Bridge`. Those are language-level objects,
  not wire-level contracts.
- The CLI flag set. New flags can be added without bumping the protocol;
  removing or renaming a flag is a separate compatibility concern documented
  in [the CLI reference](~provium/reference/cli).

## How the pin is enforced

The conformance suite locks every wire-facing struct against an explicit byte
expectation. Examples of what it locks:

- `Hello.protocol_version` is a u32 encoded as `1`.
- `HelloOk` field tags (`agent_version`, `guest_os`, `agent_features`, …).
- `OpResult.outcome` is the `"ok"` / `"err"` discriminator key, with `value`
  for the payload.
- `SyscallResult` field set: `ret`, `errno`, optional `out_bufs` (omitted when
  empty).
- `OpenMode`'s six flags, `read`, `write`, `create`, `truncate`, `append`,
  `exclusive`, each serialize as their own named field (it is a struct of
  booleans, not a packed bitfield).

Any refactor that changes a tag name, reorders an enum, or shifts a bit fails
the pin test. The fix is to bump `PROTOCOL_VERSION` and update the pin to
match the new shape, explicit, not silent.

## How to add a new op or event

1. Add the new variant / payload type to the protocol crate.
2. Add a serialization round-trip test alongside it.
3. Bump `PROTOCOL_VERSION`.
4. Update the conformance pin to lock the new shape.
5. (Op only) Wire the agent-side handler.
6. (Op only) Wire the host-side dispatcher and a Lua binding.

Skipping step 4 produces a protocol change without explicit pinning, exactly
the regression the pin tests are designed to catch.

## Backwards compatibility

Provium does not commit to backwards compatibility on the host-agent wire. The
host and agent are versioned together, they ship in lock-step out of the same
workspace. The Hello handshake exchanges versions and `HelloErr`s the
connection if they don't match, so a stale agent against a fresh host fails
fast at connection time, not silently mid-op.

For the event stream, consumers (`provium-coverage`, dashboards) are versioned
against `PROTOCOL_VERSION`. A consumer built against version N reading a
stream from version N+1 will fail to deserialize the new fields; the typical
recovery is to rebuild the consumer.

## See also

- [Events](~provium/reference/events): what the event stream carries.
- [CLI](~provium/reference/cli): flags that produce events.
