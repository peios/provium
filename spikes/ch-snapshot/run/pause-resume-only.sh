#!/usr/bin/env bash
# Diagnostic: does pause/resume in-place (no VMM kill) preserve vsock?
# This isolates whether the bug is in pause/resume or in restore.

set -euo pipefail

SPIKE_DIR="$(cd "$(dirname "$0")/.." && pwd)"
BIN="$SPIKE_DIR/bin"
RUN="$SPIKE_DIR/run"
WORK="$SPIKE_DIR/run/work-pause"
KERNEL="/home/jack/projects/peios/pkm/kernel/out/bzImage"
INITRD="$SPIKE_DIR/initrd-build/initrd.cpio.gz"

CH="$BIN/cloud-hypervisor"
CHR="$BIN/ch-remote"

rm -rf "$WORK"; mkdir -p "$WORK"
API_SOCK="$WORK/ch.sock"
VSOCK_SOCK="$WORK/vsock.sock"
CONSOLE_LOG="$WORK/console.log"

cleanup() {
    [[ -n "${CH_PID:-}" ]] && kill -KILL "$CH_PID" 2>/dev/null || true
}
trap cleanup EXIT

"$CH" \
    --api-socket "$API_SOCK" \
    --console "file=$CONSOLE_LOG" \
    --serial off \
    --vsock "cid=3,socket=$VSOCK_SOCK" \
    --cpus "boot=4" \
    --memory "size=512M,shared=on" \
    --kernel "$KERNEL" \
    --initramfs "$INITRD" \
    --cmdline "console=hvc0 reboot=k panic=1 quiet" \
    > "$WORK/ch.stdout.log" 2> "$WORK/ch.stderr.log" &
CH_PID=$!

for _ in {1..50}; do [[ -S "$API_SOCK" ]] && break; sleep 0.1; done

echo "boot: $("$RUN/vsock-query.py" "$VSOCK_SOCK" 1234 5.0)"

for i in 1 2 3; do
    "$CHR" --api-socket "$API_SOCK" pause >/dev/null
    sleep 0.5
    "$CHR" --api-socket "$API_SOCK" resume >/dev/null
    sleep 0.2
    echo "after pause/resume $i: $("$RUN/vsock-query.py" "$VSOCK_SOCK" 1234 10.0)"
done

echo "PASS: pause/resume in-place preserves vsock"
