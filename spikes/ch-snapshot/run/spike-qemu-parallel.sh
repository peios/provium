#!/usr/bin/env bash
# Test parallel QEMU + vsock: 4 VMs simultaneously, each with a unique CID.
# Verifies the CID-allocation story works under parallelism.

set -euo pipefail

SPIKE_DIR="$(cd "$(dirname "$0")/.." && pwd)"
RUN="$SPIKE_DIR/run"
WORK="$SPIKE_DIR/run/work-parallel"
KERNEL="/home/jack/projects/peios/pkm/kernel/out/bzImage"
INITRD="$SPIKE_DIR/initrd-build/initrd.cpio.gz"

NUM_VMS="${NUM_VMS:-4}"
BASE_CID="${BASE_CID:-100}"
DIAL_PORT=9999
DURATION="${DURATION:-10}"  # seconds to run

QEMU=qemu-system-x86_64

rm -rf "$WORK"; mkdir -p "$WORK"
STREAM_LOG="$WORK/stream.log"

red()   { printf '\033[31m%s\033[0m\n' "$*"; }
green() { printf '\033[32m%s\033[0m\n' "$*"; }
blue()  { printf '\033[34m%s\033[0m\n' "$*"; }

QEMU_PIDS=()
LSN_PID=""

cleanup() {
    [[ -n "${LSN_PID:-}" ]] && kill -TERM "$LSN_PID" 2>/dev/null || true
    for p in "${QEMU_PIDS[@]:-}"; do
        kill -KILL "$p" 2>/dev/null || true
    done
}
trap cleanup EXIT

# One listener handles all peers (multiplexed by source CID/port).
"$RUN/host-listener-vsock.py" "$DIAL_PORT" "$STREAM_LOG" \
    > "$WORK/listener.log" 2>&1 &
LSN_PID=$!
sleep 0.2

blue "==> launching $NUM_VMS VMs simultaneously, CIDs $BASE_CID..$((BASE_CID + NUM_VMS - 1))"
for i in $(seq 0 $((NUM_VMS - 1))); do
    cid=$(( BASE_CID + i ))
    serial_log="$WORK/serial-$i.log"
    qemu_err="$WORK/qemu-$i.stderr.log"
    "$QEMU" \
        -M q35,accel=kvm \
        -cpu host \
        -smp 2 \
        -m 256 \
        -nographic \
        -serial "file:$serial_log" \
        -monitor none \
        -device vhost-vsock-pci,guest-cid="$cid" \
        -nodefaults \
        -kernel "$KERNEL" \
        -initrd "$INITRD" \
        -append "console=ttyS0 reboot=k panic=1 quiet" \
        > "$WORK/qemu-$i.stdout.log" 2> "$qemu_err" &
    QEMU_PIDS+=($!)
done
echo "  PIDs: ${QEMU_PIDS[*]}"

blue "==> running for ${DURATION}s, then summarising"
sleep "$DURATION"

# Tear down
for p in "${QEMU_PIDS[@]}"; do
    kill -TERM "$p" 2>/dev/null || true
done
sleep 0.5
for p in "${QEMU_PIDS[@]}"; do
    kill -KILL "$p" 2>/dev/null || true
done

# Summarise
echo
blue "==> per-VM summary"
all_ok=1
for i in $(seq 0 $((NUM_VMS - 1))); do
    cid=$(( BASE_CID + i ))
    # Count lines from this CID
    lines=$(grep -c "cid=$cid " "$STREAM_LOG" 2>/dev/null || echo 0)
    last=$(grep "cid=$cid " "$STREAM_LOG" 2>/dev/null | tail -1 || echo "(none)")
    err=$(grep -E 'error|failed|EBUSY' "$WORK/qemu-$i.stderr.log" 2>/dev/null | head -3 || true)
    if (( lines > 0 )); then
        green "  cid=$cid: $lines stream lines, last connect: $last"
    else
        red "  cid=$cid: NO STREAM"
        all_ok=0
    fi
    if [[ -n "$err" ]]; then
        red "    qemu errors: $err"
        all_ok=0
    fi
done

if (( all_ok )); then
    green "PASS: all $NUM_VMS VMs got vsock streams; no CID conflicts"
else
    red "FAIL: see above"
    exit 1
fi
