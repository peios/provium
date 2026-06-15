#!/usr/bin/env bash
# Stress: deliberately allocate the same CID to two simultaneous VMs.
# Provium's scheduler must never do this, but if it does (bug, race),
# we want a CLEAN failure on the second VM, not silent data corruption.

set -euo pipefail

SPIKE_DIR="$(cd "$(dirname "$0")/.." && pwd)"
WORK="$SPIKE_DIR/run/work-cidcoll"
KERNEL="/home/jack/projects/peios/pkm/kernel/out/bzImage"
INITRD="$SPIKE_DIR/initrd-build/initrd.cpio.gz"

CID=410

QEMU=qemu-system-x86_64

rm -rf "$WORK"; mkdir -p "$WORK"

red()   { printf '\033[31m%s\033[0m\n' "$*"; }
green() { printf '\033[32m%s\033[0m\n' "$*"; }
blue()  { printf '\033[34m%s\033[0m\n' "$*"; }

PIDS=()
cleanup() {
    for p in "${PIDS[@]:-}"; do kill -KILL "$p" 2>/dev/null || true; done
}
trap cleanup EXIT

start_qemu() {
    local label=$1
    "$QEMU" \
        -M q35,accel=kvm -cpu host -smp 1 -m 128 -nographic \
        -serial "file:$WORK/serial-$label.log" \
        -monitor none \
        -device vhost-vsock-pci,guest-cid="$CID" \
        -nodefaults \
        -kernel "$KERNEL" -initrd "$INITRD" \
        -append "console=ttyS0 reboot=k panic=1 quiet" \
        > "$WORK/qemu-$label.stdout.log" 2> "$WORK/qemu-$label.stderr.log" &
    PIDS+=($!)
}

blue "==> launching VM A with cid=$CID"
start_qemu A
sleep 0.5
echo "  VM A pid=${PIDS[0]} alive=$(kill -0 ${PIDS[0]} 2>/dev/null && echo yes || echo no)"

blue "==> launching VM B with same cid=$CID (should fail)"
start_qemu B
sleep 1.0

echo "  VM B pid=${PIDS[1]} alive=$(kill -0 ${PIDS[1]} 2>/dev/null && echo yes || echo no)"
err=$(grep -iE 'address already in use|EADDRINUSE|busy|cannot|fail' "$WORK/qemu-B.stderr.log" 2>/dev/null | head -3)

if kill -0 "${PIDS[1]}" 2>/dev/null; then
    red "FAIL: VM B is still running with same CID -- silent collision"
    exit 1
fi

if [[ -z "$err" ]]; then
    red "FAIL: VM B exited but with no recognisable error message"
    echo "stderr:"; cat "$WORK/qemu-B.stderr.log" || true
    exit 1
fi

green "PASS: VM B failed cleanly with: $err"

# Bonus: verify VM A is unaffected
if kill -0 "${PIDS[0]}" 2>/dev/null; then
    green "  VM A still running -- collision didn't cripple existing VM"
else
    red "  VM A died as side effect (would be bad)"
    tail -5 "$WORK/qemu-A.stderr.log"
    exit 1
fi
