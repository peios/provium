---
title: Bridge
description:
  The Bridge userdata, wire VMs together, partition, inject latency and drops,
  limit bandwidth, capture packets, isolate VMs, and NAT outbound traffic.
related:
  - provium/writing-tests/bridges-and-impairments
  - provium/reference/nic
  - provium/reference/lab
  - provium/reference/streams
---

A Bridge represents one Linux bridge with TAP-interface attachments. It is
created through `lab:bridge("name")` and lazily realized, the host-side bridge
and per-VM TAPs come up the first time any attached VM boots.

## Constructing

| Source                                 | Returns                                                                          |
| -------------------------------------- | -------------------------------------------------------------------------------- |
| `lab:bridge("name")`                   | New bridge.                                                                      |
| `lab:bridge("name", opts)`             | Same. Opts are reserved for future use.                                          |
| `lab:bridge("name")` (already created) | The same bridge.                                                                 |
| `lab.<name>`                           | Same lookup as `lab:bridge("name")` for declared bridges. Returns nil otherwise. |

A bridge is a graph object until a VM attached to it boots; only at that point
does the host-side bridge interface exist.

## Membership

### `bridge:attach(vm)` / `bridge:attach({vms…})`

Records that `vm` is on this bridge. When `vm` is booted the host installs a
TAP, attaches it to the bridge, and adds a `-netdev tap` flag to the QEMU
launch.

Accepts:

- A single VM userdata, `lan:attach(a)`.
- A bare string, `lan:attach("a")`. Graph-state only; no VM handle is wired,
  so future link-down through `bridge:detach(vm)` will not drive QMP.
- An array, `lan:attach({a, b, c})`. Validated atomically: a bad type at index
  N fails before any element is recorded.

### `bridge:detach(vm)`

Remove `vm` from the bridge. When `vm` is a VM userdata, also issues a QMP
`set_link(false)` against the matching netdev so the guest sees a link-down
event.

### `bridge:members()`

Returns an array of attached VM names.

## Partitions

Partitions are network-layer drops between specific pairs of VMs. Two forms:

### `bridge:partition(a, b)` (symmetric)

Drop both directions of A↔B traffic. Accepts VM userdata or bare strings.
Graph-state only, drops install when each VM is booted.

### `bridge:partition({from=A, to=B})` (directional)

Drop only A→B traffic. B→A continues to flow. Both endpoints must already be
attached and booted; otherwise it errors with a "call bridge:attach first"
pointer (the rule would otherwise install onto a non-existent TAP).

### `bridge:unpartition(a, b)` / `bridge:unpartition({from=A, to=B})`

Undo the matching partition. The directional unpartition does NOT require the
endpoints to still be attached, if you `:detach()` a VM and then
`:unpartition` it, the call is a clean no-op rather than an error (detach
already cleared the directional rule).

### `bridge:partition_all()`

Drop every pair of attached VMs. Equivalent to calling `bridge:partition` for
every pair.

### `bridge:restore_all()`

Undo every partition on the bridge.

### `bridge:is_partitioned(a, b)`

Returns `true` if A↔B is partitioned (either symmetric or directional from A
to B).

## Impairments

Impairments install `tc` qdiscs on the bridge or per-TAP. All three accept
either a scalar (whole-bridge) or a directional table.

### `bridge:add_latency(ms_or_table)`

Whole-bridge form: `bridge:add_latency(50)` adds 50 ms of one-way delay to
every packet through this bridge.

Directional form: `bridge:add_latency({from=A, to=B, ms=50})` records 50 ms
for the A→B pair. Realization is per **source**: each VM's outbound
`(from, *)` pairs collapse, worst case per axis across latency, drop, and
bandwidth, into one qdisc chain on that VM's TAP, so the delay in practice
applies to everything leaving `A`. Endpoint-attachment check applies (see
Partitions).

### `bridge:drop_rate(pct_or_table)`

Whole-bridge: `bridge:drop_rate(10)` drops ~10 % of packets through netem.

Directional: `bridge:drop_rate({from=A, to=B, p=10})`. Same per-source
realization as directional latency.

### `bridge:bandwidth_limit(bps_or_table)`

Whole-bridge: `bridge:bandwidth_limit(1_000_000)` caps the bridge to 1 Mbit/s
through `tc tbf`.

Directional: `bridge:bandwidth_limit({from=A, to=B, bps=500_000})` installs an
HTB-root + one rate-limited class on `A`'s source TAP, with a `netem` child
when latency/drop is also set for the same source. Bits per second, not bytes.
The `to` is graph-recorded but the realization shapes every packet leaving the
source TAP, HTB at the bridge layer can't select by destination MAC. Multiple
`(from=A, to=*)` pairs collapse to `max(bps)` on `A`'s TAP so no pair is
over-shaped.

Whole-bridge bandwidth does **not** combine with whole-bridge latency/drop on
the same bridge: when netem is also configured, the latency/drop shaping is
what's realized and the bandwidth cap is recorded in the graph but not
enforced (combined netem+tbf needs class-based HTB, out of scope for v1). For
combined shaping use the directional form on each source.

### `bridge:add_directional_latency({from=A, to=B, ms=N})`

Explicit directional form of `:add_latency`. Same effect as the table form
above.

### `bridge:directional_drop_rate({from=A, to=B, p=N})`

Explicit directional form of `:drop_rate`.

### `bridge:reset()`

Tear down every installed netem / tbf qdisc and clear every partition.
Isolation, uplink, and L3 routes are preserved, they're topology, not
impairments.

### Introspection

- `bridge:latency_ms()`, Current whole-bridge latency.
- `bridge:drop_rate_pct()`, Current whole-bridge drop rate.
- `bridge:bandwidth_bps()`, Current whole-bridge bandwidth cap.

## Isolation

Isolation puts a single VM behind a hairpin filter so it cannot reach any
other VM on the bridge (but the bridge itself remains alive).

### `bridge:isolate(vm)` / `bridge:unisolate(vm)`

Toggle isolation for one VM. Accepts a VM userdata or a bare string.

### `bridge:is_isolated(vm)`

Returns `true` if `vm` is isolated on this bridge.

## L3 routing (preview)

### `bridge:route(other_bridge_or_list)`

Records that traffic should be routed to `other_bridge`. **Graph-state only in
v1.** No nft forward rules are installed, the call records the intent but
cross-bridge IP traffic does not actually flow yet. The first call to
`:route()` per bridge prints a one-shot warning to stderr so test authors
notice at the call site rather than chasing dropped packets.

### `bridge:routes()`

Returns the array of bridge names this bridge is routed to.

## Uplink

Uplink installs an nft NAT masquerade rule between the bridge and the host's
default-route interface, so VMs on the bridge can reach the outside world.

### `bridge:enable_uplink()` / `bridge:disable_uplink()`

Enable / disable NAT for this bridge. Both can fail if the host doesn't have a
default-route interface or if nft commands fail; the error includes the
underlying detail.

## NICs and capture

### `bridge:nic(vm)`

Returns a [Nic](~provium/reference/nic) bound to the (bridge, vm) pair. When
`vm` is a VM userdata, the Nic carries the VM handle through so
`nic:disconnect()` / `nic:reconnect()` can drive QMP `set_link`. With a bare
string, the Nic is
[graph-state only](~provium/writing-tests/bridges-and-impairments#declared-and-realized).

### `bridge:capture()`

Spawn `tcpdump -i <bridge> -U -w -` and return a
[Capture](~provium/reference/streams) stream. Reads pcap bytes off the tcpdump
child's stdout. Requires `tcpdump` on `PATH` plus `CAP_NET_RAW` (in addition
to `CAP_NET_ADMIN`).

The capture holds a guard that pins the bridge's `active_captures` counter, so
`vm:snapshot()` can detect "stream live, snapshot would race" and refuse.

## Auto-close

### `bridge:close()`

Auto-close hook used by the resource walker. Tears down host-side networking,
TAPs, qdiscs, nft tables, the bridge itself. Idempotent.

## Accessors

- `bridge:name()`, Returns the bridge's name.

## See also

- [Nic](~provium/reference/nic): per-(bridge, vm) handle for link-state and
  per-NIC capture.
- [Streams](~provium/reference/streams): what `bridge:capture()` returns.
- [Lab](~provium/reference/lab): bridges live in a lab.
