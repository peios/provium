#!/usr/bin/env bash
# Stress: SIGKILL QEMU during snapshot, then immediately respawn with same CID.
# Tests:
#   - Does the host kernel (vhost-vsock) cleanly release the CID?
#   - Does an in-progress migrate leave a corrupt file we'd accidentally
#     try to use later?
#   - Can a fresh VM with the same CID boot immediately?

set -euo pipefail

SPIKE_DIR="$(cd "$(dirname "$0")/.." && pwd)"
RUN="$SPIKE_DIR/run"
WORK="$SPIKE_DIR/run/work-sigkill"
KERNEL="/home/jack/projects/peios/pkm/kernel/out/bzImage"
INITRD="$SPIKE_DIR/initrd-build/initrd.cpio.gz"

DIAL_PORT=9999
CID=400

QEMU=qemu-system-x86_64

rm -rf "$WORK"; mkdir -p "$WORK"
STREAM_LOG="$WORK/stream.log"

red()   { printf '\033[31m%s\033[0m\n' "$*"; }
green() { printf '\033[32m%s\033[0m\n' "$*"; }
blue()  { printf '\033[34m%s\033[0m\n' "$*"; }

LSN_PID=""
QEMU_PID=""
cleanup() {
    [[ -n "${LSN_PID:-}" ]] && kill -TERM "$LSN_PID" 2>/dev/null || true
    [[ -n "${QEMU_PID:-}" ]] && kill -KILL "$QEMU_PID" 2>/dev/null || true
}
trap cleanup EXIT

"$RUN/host-listener-vsock.py" "$DIAL_PORT" "$STREAM_LOG" \
    > "$WORK/listener.log" 2>&1 &
LSN_PID=$!
sleep 0.2

start_qemu() {
    local label=$1; shift
    local qmp="$WORK/qmp-$label.sock"
    rm -f "$qmp"
    "$QEMU" \
        -M q35,accel=kvm -cpu host -smp 1 -m 256 -nographic \
        -serial "file:$WORK/serial-$label.log" \
        -monitor none \
        -qmp "unix:$qmp,server=on,wait=off" \
        -device vhost-vsock-pci,guest-cid="$CID" \
        -nodefaults \
        -kernel "$KERNEL" -initrd "$INITRD" \
        -append "console=ttyS0 reboot=k panic=1 quiet" \
        "$@" \
        > "$WORK/qemu-$label.stdout.log" 2> "$WORK/qemu-$label.stderr.log" &
    QEMU_PID=$!
    for _ in {1..50}; do [[ -S "$qmp" ]] && break; sleep 0.1; done
    [[ -S "$qmp" ]] || { red "$label: QMP did not appear"; return 1; }
    echo "$qmp"
}

############################
blue "==> phase 1: boot, start migrate, SIGKILL mid-flight"
qmp1=$(start_qemu "first")
sleep 1
peers_before=$(grep -cE '^# peer.*connected' "$STREAM_LOG" 2>/dev/null || true)
peers_before=${peers_before:-0}
peers_before=$(echo "$peers_before" | tr -d '[:space:]')
echo "  pre-kill peers: $peers_before"

# Kick off a migrate that will take a moment (writing 100MB+ to disk)
"$RUN/qmp.py" "$qmp1" stop >/dev/null
"$RUN/qmp.py" "$qmp1" migrate "{\"uri\":\"exec:cat > $WORK/aborted.snap\"}" >/dev/null

# Don't wait — slam SIGKILL on QEMU within 100ms (mid-migration most likely)
sleep 0.1
kill -KILL "$QEMU_PID" 2>/dev/null || true
wait "$QEMU_PID" 2>/dev/null || true
QEMU_PID=""

echo "  aborted snap size: $(stat -c%s "$WORK/aborted.snap" 2>/dev/null || echo "missing") bytes"

############################
# Wait briefly for kernel to release the CID. Empirical from
# stress-cid-release-timing: ~330ms is enough; we use 500ms for margin.
sleep 0.5

blue "==> phase 2: respawn fresh VM with SAME cid=$CID after 500ms grace"
qmp2=$(start_qemu "second") || { red "respawn failed"; exit 1; }

# Wait for stream from the new VM
deadline=$(( $(date +%s) + 10 ))
new_peer=0
while (( $(date +%s) < deadline )); do
    p=$(grep -cE '^# peer.*connected' "$STREAM_LOG" 2>/dev/null || true)
    p=${p:-0}; p=$(echo "$p" | tr -d '[:space:]')
    if (( p > peers_before )); then new_peer=1; break; fi
    sleep 0.2
done

if (( new_peer )); then
    last_cid=$(grep '^# peer.*connected' "$STREAM_LOG" | tail -1 | grep -oE 'cid=[0-9]+')
    green "PASS: respawn worked. New peer arrived ($last_cid)."
    grep -E "cid=$CID" "$STREAM_LOG" | tail -3 | sed 's/^/    /'
else
    red "FAIL: no new peer after respawn -- CID likely stuck in host kernel"
    echo "qemu stderr:"; tail -5 "$WORK/qemu-second.stderr.log" || true
    exit 1
fi
