#!/usr/bin/env bash
# Stress: 40 VMs total, 8 concurrent, with CID reuse from a small pool.
# Tests the actual concern from old provium — does the host's vhost-vsock
# release CIDs promptly enough that rapid spawn/destroy cycles don't
# wedge on EBUSY or interleave state from prior VMs into new ones.

set -euo pipefail

SPIKE_DIR="$(cd "$(dirname "$0")/.." && pwd)"
RUN="$SPIKE_DIR/run"
WORK="$SPIKE_DIR/run/work-churn"
KERNEL="/home/jack/projects/peios/pkm/kernel/out/bzImage"
INITRD="$SPIKE_DIR/initrd-build/initrd.cpio.gz"

TOTAL_VMS="${TOTAL_VMS:-40}"
CONCURRENCY="${CONCURRENCY:-8}"
CID_BASE="${CID_BASE:-200}"   # pool: CID_BASE..CID_BASE+CONCURRENCY-1
DIAL_PORT=9999
VM_LIFETIME="${VM_LIFETIME:-3}"  # seconds each VM runs

QEMU=qemu-system-x86_64

rm -rf "$WORK"; mkdir -p "$WORK"
STREAM_LOG="$WORK/stream.log"
EVENT_LOG="$WORK/events.log"

red()   { printf '\033[31m%s\033[0m\n' "$*"; }
green() { printf '\033[32m%s\033[0m\n' "$*"; }
blue()  { printf '\033[34m%s\033[0m\n' "$*"; }
log()   { echo "[$(date +%H:%M:%S.%3N)] $*" >> "$EVENT_LOG"; }

LSN_PID=""
declare -A SLOT_PID    # slot index -> qemu pid (running)
declare -A SLOT_VMID   # slot index -> vm id (1..N)
declare -A SLOT_CID    # slot index -> cid in use

cleanup() {
    [[ -n "${LSN_PID:-}" ]] && kill -TERM "$LSN_PID" 2>/dev/null || true
    for k in "${!SLOT_PID[@]}"; do
        kill -KILL "${SLOT_PID[$k]}" 2>/dev/null || true
    done
}
trap cleanup EXIT

# Listener (one for the whole run; demuxes by source CID)
"$RUN/host-listener-vsock.py" "$DIAL_PORT" "$STREAM_LOG" \
    > "$WORK/listener.log" 2>&1 &
LSN_PID=$!
sleep 0.2

# Spawn one VM in a given slot with a given CID and VM id.
spawn_vm() {
    local slot=$1
    local vmid=$2
    local cid=$3
    local serial="$WORK/serial-${vmid}.log"
    local stderr="$WORK/qemu-${vmid}.stderr.log"

    log "spawn vm=$vmid slot=$slot cid=$cid"
    "$QEMU" \
        -M q35,accel=kvm \
        -cpu host \
        -smp 1 \
        -m 128 \
        -nographic \
        -serial "file:$serial" \
        -monitor none \
        -device vhost-vsock-pci,guest-cid="$cid" \
        -nodefaults \
        -kernel "$KERNEL" \
        -initrd "$INITRD" \
        -append "console=ttyS0 reboot=k panic=1 quiet" \
        > /dev/null 2> "$stderr" &
    local pid=$!

    SLOT_PID[$slot]=$pid
    SLOT_VMID[$slot]=$vmid
    SLOT_CID[$slot]=$cid

    # Background timer that kills this VM after its lifetime
    (
        sleep "$VM_LIFETIME"
        kill -TERM "$pid" 2>/dev/null || true
        sleep 0.3
        kill -KILL "$pid" 2>/dev/null || true
    ) &
}

############################
blue "==> churn: total=$TOTAL_VMS concurrent=$CONCURRENCY cid pool $CID_BASE..$((CID_BASE + CONCURRENCY - 1))  lifetime=${VM_LIFETIME}s"

next_vmid=1

# Initial fill of all slots
for slot in $(seq 0 $((CONCURRENCY - 1))); do
    cid=$(( CID_BASE + slot ))
    spawn_vm "$slot" "$next_vmid" "$cid"
    ((next_vmid++))
done

# Replace VMs as they exit, until we've spawned TOTAL_VMS in total
while (( next_vmid <= TOTAL_VMS )); do
    progress=0
    for slot in $(seq 0 $((CONCURRENCY - 1))); do
        pid=${SLOT_PID[$slot]:-}
        if [[ -z "$pid" ]] || ! kill -0 "$pid" 2>/dev/null; then
            # Slot empty — replace
            cid=$(( CID_BASE + slot ))   # CID reused from pool
            spawn_vm "$slot" "$next_vmid" "$cid"
            ((next_vmid++))
            progress=1
            (( next_vmid > TOTAL_VMS )) && break
        fi
    done
    if (( progress == 0 )); then
        sleep 0.2
    fi
done

# Wait for the last batch to finish
blue "==> all $TOTAL_VMS VMs spawned; waiting for in-flight ones to exit"
for slot in $(seq 0 $((CONCURRENCY - 1))); do
    pid=${SLOT_PID[$slot]:-}
    if [[ -n "$pid" ]]; then
        wait "$pid" 2>/dev/null || true
    fi
done
sleep 0.5

############################
blue "==> per-VM summary"
ok=0
fail=0
ebusy=0
no_stream=0
for vmid in $(seq 1 "$TOTAL_VMS"); do
    serial="$WORK/serial-${vmid}.log"
    stderr="$WORK/qemu-${vmid}.stderr.log"
    # Find the CID this vmid had (recover from event log)
    cid=$(grep "spawn vm=$vmid " "$EVENT_LOG" | head -1 | sed -n 's/.*cid=\([0-9]*\).*/\1/p')
    [[ -z "$cid" ]] && cid="?"

    # Did we see a connection from that CID, with at least one counter line?
    n=$(grep -cE "DIAL COUNTER=.*" <(grep -A1 "cid=$cid " "$STREAM_LOG" 2>/dev/null) 2>/dev/null || echo 0)
    qerr=$(grep -E 'EBUSY|Address already in use|address already in use|busy' "$stderr" 2>/dev/null | head -1 || true)

    if [[ -n "$qerr" ]]; then
        ((ebusy++)); ((fail++))
        red "  vm=$vmid cid=$cid: QEMU EBUSY -- $qerr"
    elif (( n == 0 )); then
        ((no_stream++)); ((fail++))
        red "  vm=$vmid cid=$cid: no stream"
    else
        ((ok++))
    fi
done

echo
total=$(( ok + fail ))
if (( fail == 0 )); then
    green "PASS: $ok/$total VMs streamed cleanly. CID reuse stable."
else
    red "FAIL: ok=$ok fail=$fail (ebusy=$ebusy no_stream=$no_stream) of $total"
    echo "events log:"; tail -30 "$EVENT_LOG"
    exit 1
fi
