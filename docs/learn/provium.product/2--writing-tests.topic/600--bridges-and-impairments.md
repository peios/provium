---
title: Bridges and Impairments
type: how-to
description:
  Wire VMs into bridges, partition them, inject latency, drop rate, and
  bandwidth limits, isolate VMs, capture pcap, and reach the outside world
  through uplink.
related:
  - provium/reference/bridge
  - provium/reference/nic
  - provium/reference/streams
  - provium/writing-tests/labs-and-scope
---

Provium's networking is real Linux bridges with TAP attachments. You declare a
topology, the harness realizes it as the VMs boot, and you can mutate it
(partition, impair, capture) at runtime.

The exhaustive method reference is on [Bridge](~provium/reference/bridge) and
[Nic](~provium/reference/nic).

> [!NOTE]
>
> Provium wires layer 2 only. A bridge gives each attached VM a NIC on
> a shared segment; IP addressing, routing, and name resolution inside the
> guests are the guest image's business. The examples on this page assign
> addresses with ordinary `ip addr` commands run in the guests.

## Declared and realized

Provium tracks resources in two layers: the resource graph records what a test
has declared (bridges, attachments, partitions, impairments), and the harness
separately realizes that graph on the host, bridge interfaces, TAPs, `tc`
qdiscs, nft rules, QMP calls, once the backing pieces exist, typically when an
attached VM boots. A call marked **graph-state only** updates the declared
graph without touching the host: it's recorded and visible to inspection
methods, but on its own it doesn't change anything real.

## Wiring up

```lua
local lan = provium:bridge("lan")
local a   = provium:vm("a", "peios"):boot()
local b   = provium:vm("b", "peios"):boot()
lan:attach({a, b})  -- atomic; either all attach or none do

a:run("ip addr add 10.0.0.1/24 dev eth0 && ip link set eth0 up")
b:run("ip addr add 10.0.0.2/24 dev eth0 && ip link set eth0 up")

-- a can reach b.
a:run("ping -c 1 -W 1 10.0.0.2"):assert_ok()
```

`bridge:attach` takes a single VM or an array (validated atomically, a bad
element fails before anything is recorded). The bare-string form and its
graph-state-only caveat are in the
[Bridge reference](~provium/reference/bridge#membership).

The host-side bridge interface and per-VM TAPs come up the first time any
attached VM boots.

## Multi-bridge topologies

```lua
local mgmt = provium:bridge("mgmt")
local data = provium:bridge("data")
local a    = provium:vm("a", "peios"):boot()
local b    = provium:vm("b", "peios"):boot()

mgmt:attach({a, b})    -- both VMs on mgmt
data:attach({a, b})    -- both VMs on data too

-- a:nic("mgmt") and a:nic("data") return separate Nic handles.
local mgmt_nic = a:nic("mgmt")
local data_nic = a:nic("data")
```

Inside the guest, each NIC shows up as a separate interface. The mapping from
bridge name to guest-side interface name (eth0, eth1, …) is determined by
attachment order sorted by bridge name. For portable tests, prefer
`vm:nic("mgmt")` (by bridge name) over `vm:nic("eth0")` (by guest-name index).

## Partitions

A partition is a network-layer drop between two specific VMs. Two flavors:

### Symmetric

```lua
lan:partition(a, b)        -- A↔B traffic dropped both ways
lan:unpartition(a, b)      -- restore
```

Symmetric partitions are graph-state, they install drop rules at boot through
nft and lift cleanly.

### Directional

```lua
lan:partition({from = a, to = b})    -- only A→B dropped; B→A still flows
lan:unpartition({from = a, to = b})
```

Directional partitions install per-TAP nft rules and require **both endpoints
to be already attached and booted**, otherwise they error with a "call
bridge:attach first" pointer. The (more lenient) `unpartition` semantics are
in the [Bridge reference](~provium/reference/bridge#partitions).

### Whole-bridge

```lua
lan:partition_all()         -- every pair partitioned
lan:restore_all()           -- every partition lifted
```

### Inspect

```lua
if lan:is_partitioned(a, b) then
    -- A↔B is currently partitioned (symmetric or A→B directional)
end
```

## Impairments

Three knobs: latency, drop rate, bandwidth limit. Each accepts either a scalar
(whole-bridge) or a directional table.

### Latency

```lua
lan:add_latency(50)                                -- 50 ms one-way to every flow
lan:add_latency({from = a, to = b, ms = 50})       -- 50 ms recorded for A→B
```

Implemented as netem qdiscs. A directional impairment is recorded per
`(from, to)` pair but realized per **source**: each VM's outbound pairs
collapse into one qdisc chain on its own TAP, taking the worst case across its
pairs, so in practice the `{from = a, to = b}` latency above delays everything
leaving `a`, not only its traffic to `b`. The endpoint attachment check
applies (same as directional partitions).

### Drop rate

```lua
lan:drop_rate(10)                                  -- ~10 % loss, both ways
lan:drop_rate({from = a, to = b, p = 10})          -- recorded for A→B
```

### Bandwidth limit

```lua
lan:bandwidth_limit(1024 * 1024)                       -- 1 Mbit/s, both ways
lan:bandwidth_limit({from = a, to = b, bps = 500000})  -- 500 kbit/s leaving A
```

The number is **bits per second**, not bytes, matches `tc rate Nbit`. Two
realization caveats matter when you design a test: the directional form shapes
every packet leaving the source TAP (not only traffic to the named `to`), and
whole-bridge bandwidth is not enforced while whole-bridge latency/drop is also
set, for combined shaping use the directional form on each source. The full
realization details (TBF and HTB, `max(bps)` collapse across pairs) are in the
[Bridge reference](~provium/reference/bridge#bridge-bandwidth-limit-bps-or-table).

Combine directional bandwidth with directional latency / drop on the same
source for a complete profile:

```lua
lan:bandwidth_limit({from = a, to = b, bps = 1_000_000})
lan:add_latency({from = a, to = b, ms = 25})
lan:drop_rate({from = a, to = b, p = 1})
```

### Reset

```lua
lan:reset()    -- tear down every netem/tbf qdisc, clear every partition
```

`reset` is a clean way to go back to "default" without enumerating every
impairment you applied. Isolation, uplink, and L3 routes survive a `reset`,
they're topology, not impairments.

### Inspect

```lua
lan:latency_ms()        -- current whole-bridge latency
lan:drop_rate_pct()     -- current whole-bridge drop rate
lan:bandwidth_bps()     -- current whole-bridge bandwidth cap
```

These return the last applied whole-bridge value. They don't enumerate
per-direction impairments.

## Isolation

Isolation puts one VM behind a hairpin filter, it can't reach any other VM on
the bridge, but the bridge stays up:

```lua
lan:isolate(a)
local r = a:run("ping -c 1 -W 1 10.0.0.2")
-- r:ok() is false; a is isolated

lan:unisolate(a)
a:run("ping -c 1 -W 1 10.0.0.2"):assert_ok()
```

`lan:is_isolated(vm)` returns `true` if the VM is isolated.

## NICs

```lua
local nic = a:nic("lan")              -- by bridge name
local nic = a:nic("eth0")             -- by guest-name index
local nic = lan:nic(a)                -- equivalent
```

The Nic gives you per-NIC capabilities the bridge can't. The ones you'll use
most:

```lua
nic:counters()         -- per-NIC traffic counters, guest's perspective
nic:disconnect()       -- link-down via QMP set_link(false)
nic:reconnect()        -- link-up
```

`disconnect` / `reconnect` drive QMP `set_link` so the guest sees a real
link-down event. The full method list, the counter field table (and its
guest-perspective mapping), and the graph-state-only bare-string case are in
the [Nic reference](~provium/reference/nic).

## Packet capture

Two scopes:

### Bridge-wide capture

```lua
local cap = lan:capture()
a:run("ping -c 5 10.0.0.2")
local frames = cap:drain("2s")
local pcap = table.concat(frames)
-- pcap is now standard pcap-format bytes, parseable by tshark, etc.
```

`bridge:capture()` returns a [Capture](~provium/reference/streams) stream of
pcap-format bytes. It requires `tcpdump` on `PATH`, and a live capture blocks
`vm:snapshot()` (no half-captured pcap), capability requirements and the
mechanism are in the
[Bridge reference](~provium/reference/bridge#bridge-capture).

### Per-NIC capture

```lua
local nic = a:nic("lan")
local cap = nic:capture()    -- captures only A's TAP, not the whole bridge
```

Useful when multiple VMs are on the bridge and you only want one VM's
perspective.

`nic:capture()` errors if the VM hasn't been booted yet (the per-VM TAP
doesn't exist):

```text
nic:capture: vm `a` has no TAP on bridge `lan` (not booted?). Call lab:boot() / vm:boot() first.
```

## Uplink (NAT to the outside world)

```lua
lan:enable_uplink()
vm:run("curl -s https://example.com/"):assert_ok()
lan:disable_uplink()
```

`enable_uplink` installs an nft NAT masquerade rule between the bridge and the
host's default-route interface. Failures (for example no default-route
interface) error with the underlying detail.

## L3 routing (preview)

```lua
lan:route(other_bridge)
```

`bridge:route` records the routing intent in the graph but installs no nft
forward rules in v1, cross-bridge IP traffic does not actually flow yet, and
the first call per bridge prints a one-shot warning saying so.
`bridge:routes()` returns the recorded routes. Plan tests around the
limitation. See the
[Bridge reference](~provium/reference/bridge#l3-routing-preview).

## Common patterns

### Test split-brain recovery

```lua
test("application recovers after partition heals", function(t)
    local lan = provium:bridge("lan")
    local a, b = provium:vm("a", "peios"):boot(), provium:vm("b", "peios"):boot()
    lan:attach({a, b})
    a:run("ip addr add 10.0.0.1/24 dev eth0 && ip link set eth0 up")
    b:run("ip addr add 10.0.0.2/24 dev eth0 && ip link set eth0 up")

    -- Baseline: a can talk to b.
    a:run("ping -c 1 -W 1 10.0.0.2"):assert_ok()

    -- Partition.
    lan:partition(a, b)
    local r = a:run("ping -c 1 -W 1 10.0.0.2")
    t:assert(not r:ok())

    -- Heal.
    lan:unpartition(a, b)
    -- Wait for ARP / route to re-converge.
    wait_until(function()
        return a:run("ping -c 1 -W 1 10.0.0.2"):ok()
    end, {timeout = "10s", desc = "post-heal connectivity"})
end)
```

### Test latency-sensitive code

```lua
test("client retries on slow link", function(t)
    local lan = provium:bridge("lan")
    -- … attach VMs …
    lan:add_latency(500)  -- 500 ms each way ≈ 1s RTT
    local r = client:run("curl --max-time 0.5 http://10.0.0.2/")
    t:assert(not r:ok())  -- timeout fires

    lan:reset()
    r = client:run("curl --max-time 0.5 http://10.0.0.2/")
    r:assert_ok()
end)
```

### Test packet loss tolerance

```lua
test("client succeeds with 30% drop", function(t)
    lan:drop_rate(30)
    -- Apps should retry; some will succeed.
    local successes = 0
    for _ = 1, 20 do
        if client:run("curl --max-time 5 http://10.0.0.2/"):ok() then
            successes = successes + 1
        end
    end
    t:assert(successes >= 5)  -- pessimistic floor
end)
```

### Inspect packet flow with capture

```lua
test("DNS query produces UDP traffic", function(t)
    lan:enable_uplink()
    local cap = lan:capture()
    a:run("dig @8.8.8.8 example.com")
    local pcap = table.concat(cap:drain("3s"))
    cap:close()
    -- Pipe pcap through tshark or pyshark to assert UDP/53.
end)
```

## See also

- [Bridge reference](~provium/reference/bridge): every method on the Bridge
  userdata.
- [Nic reference](~provium/reference/nic): per-NIC handle.
- [Streams reference](~provium/reference/streams): what `bridge:capture()` and
  `nic:capture()` return.
