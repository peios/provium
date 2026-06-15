# Spike 2 — VMM choice for provium

## Verdict

**Use QEMU, not cloud-hypervisor.** CH's snapshot/restore + VMM-teardown
workflow breaks all emulated I/O devices (vsock, virtio-console, serial)
in ways that no userspace recovery can fix. QEMU's snapshot/restore is
mature and survives every realistic stress scenario.

## Test matrix

| # | Test | CH | QEMU |
|---|---|---|---|
| 1 | Boot 4-vCPU + vsock | ✅ | ✅ |
| 2 | Pause/resume in-place | ✅ | ✅ |
| 3 | Snapshot → kill VMM → restore: vsock host→guest | ❌ wedged | ✅ |
| 4 | Snapshot → kill VMM → restore: vsock guest→host | ❌ wedged | ✅ |
| 5 | Snapshot → kill VMM → restore: serial UART | ❌ wedged | n/a |
| 6 | Snapshot → kill VMM → restore: virtio-console (printk) | ❌ wedged | n/a |
| 7 | Driver unbind/rebind recovery | ❌ no working channel to trigger | n/a |
| 8 | 5 sequential snapshot/restore cycles | n/a | ✅ counter monotonic |
| 9 | 4 parallel distinct-CID VMs | n/a | ✅ no conflicts |
| 10 | 40 VMs, 8 concurrent, CID reuse 5× per CID | n/a | ✅ zero EBUSY/failures |
| 11 | Fixture fan-out: 1 snapshot → 8 parallel restores | n/a | ✅ all stream independently |
| 12 | CID release after SIGTERM | n/a | ✅ ~330 ms |
| 13 | CID release after SIGKILL (idle) | n/a | ✅ ~330 ms |
| 14 | CID release after SIGKILL mid-migration | n/a | ✅ ~330 ms |
| 15 | Same-CID collision: error handling | n/a | ✅ clean "Address already in use", sibling unaffected |
| 16 | Stream integrity at 10 ms cadence, 3 cycles | n/a | ✅ 550 lines, 0 dupes, all monotonic |
| 17 | Bidirectional (host→guest + guest→host) survives restore | ❌ | ✅ both intact across 3 cycles |

## Why CH fails

CH's snapshot/restore was designed for live migration between two
running VMMs. The "save-now-replay-later" workflow we need (kill VMM,
restart from snapshot) leaves all host-side device backend state
destroyed. Reconstructed CH instances start with empty proxies but
restored guest state believes connections, queues, and FIFOs are still
live. The result: every emulated device wedges, including the kernel's
own printk → console path. No userspace recovery is possible because
no working communication channel survives.

Confirmed open upstream issues:
- [#7263](https://github.com/cloud-hypervisor/cloud-hypervisor/issues/7263)
  host→guest vsock broken after restore
- [#7759](https://github.com/cloud-hypervisor/cloud-hypervisor/issues/7759)
  guest→host vsock broken after restore

These are symptoms of the same root cause and have no fix in flight.

## Why QEMU works

QEMU's `migrate to file` + `-incoming "exec:cat <file>"` is the live
migration code path that's been hammered on in production for over a
decade. Device state (including vhost-vsock CID, virtqueue indices,
in-flight buffer state) is fully captured in the migration stream and
faithfully restored. Cross-process restore works correctly because the
state stream is self-contained.

## Operational notes for provium

1. **CID allocation:** use a monotonic counter (`next_cid++`), never
   reuse within a single provium run. CID space is u32, so this is
   effectively unbounded for any realistic session length. Reuse-
   immediately-after-kill needs a ~500 ms grace window for the host
   kernel's vhost-vsock to release; counter approach avoids this.

2. **Boot speed:** QEMU with `-M q35,accel=kvm`, no defaults, kernel +
   initrd direct boot, 4 vCPUs, 256 MB → boots in ~150 ms (agent dialer
   connected at UPTIME_MS=1 in the integrity test). Slower than CH's
   ~75 ms but well within tolerable for fixture replay use cases.

3. **Snapshot file size:** ~107-114 MB for a 256 MB / 512 MB VM. Reasonable.

4. **Fixture fan-out:** the same snapshot file can be read by N parallel
   QEMU `-incoming "exec:cat ..."` invocations simultaneously. Each
   gets its own QEMU process, own CID, own vsock connections. No
   coordination needed.

5. **Bidirectional vsock works:** both `host_dial_guest(cid, port)` and
   `agent_dial_host(CID_HOST=2, port)` are usable. CH's failure of
   host→guest does not apply.

## Artifacts

```
spikes/ch-snapshot/
├── agent/                  # spike agent: vsock dialer + listener + counter
│   ├── spike-agent.c       # source
│   └── spike-agent         # built (musl-static)
├── initrd-build/           # initrd assembly + build artifacts
├── bin/                    # cloud-hypervisor v50/v51 binaries (kept for reference)
├── run/
│   ├── host-listener-vsock.py     # AF_VSOCK listener (used by QEMU spikes)
│   ├── host-listener.py           # CH hybrid-vsock listener (legacy)
│   ├── vsock-query.py             # CH host→guest probe (legacy)
│   ├── qmp.py                     # minimal QMP client
│   ├── spike-qemu.sh              # baseline 5-cycle snapshot/restore
│   ├── spike-qemu-parallel.sh     # 4 parallel distinct-CID VMs
│   ├── spike-qemu-churn.sh        # 40 VMs / 8 concurrent / CID reuse
│   ├── spike-qemu-fixture-fanout.sh  # 1 snapshot → N parallel restores
│   ├── stress-cid-collision.sh    # same-CID collision behaviour
│   ├── stress-cid-release-timing.sh  # how long CID stays held after kill
│   ├── stress-stream-integrity.sh # high-rate cadence integrity check
│   ├── stress-bidirectional.sh    # host→guest + guest→host together
│   └── (legacy CH spikes A-D)
└── RESULT.md (this file)
```
