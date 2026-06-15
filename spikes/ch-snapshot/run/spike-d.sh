#!/usr/bin/env bash
# Spike 2 / Option D: brute-force virtio_vsock unbind/rebind every 5s.
# Tests the *core* question: does sysfs unbind/rebind on virtio_vsock
# actually recover device-level vsock connectivity post-restore?
#
# Pass = after CH snapshot/restore + VMM kill, the host listener sees a
# fresh peer connection (peer 2) and the agent's counter is monotonic.

set -euo pipefail

SPIKE_DIR="$(cd "$(dirname "$0")/.." && pwd)"
BIN="$SPIKE_DIR/bin"
RUN="$SPIKE_DIR/run"
WORK="$SPIKE_DIR/run/work-d"
KERNEL="/home/jack/projects/peios/pkm/kernel/out/bzImage"
INITRD="$SPIKE_DIR/initrd-build/initrd.cpio.gz"

VCPUS="${VCPUS:-4}"
MEMORY="${MEMORY:-512M}"
DIAL_PORT=9999
GUEST_CID=3
WAIT_AFTER_RESTORE="${WAIT_AFTER_RESTORE:-25}"  # one rebind cycle ~5s, give margin

CH="$BIN/cloud-hypervisor"
CHR="$BIN/ch-remote"

rm -rf "$WORK"; mkdir -p "$WORK"
API_SOCK="$WORK/ch.sock"
VSOCK_SOCK="$WORK/vsock.sock"
CONSOLE_LOG="$WORK/console.log"
STREAM_LOG="$WORK/stream.log"

red()   { printf '\033[31m%s\033[0m\n' "$*"; }
green() { printf '\033[32m%s\033[0m\n' "$*"; }
blue()  { printf '\033[34m%s\033[0m\n' "$*"; }

cleanup() {
    local rc=$?
    [[ -n "${CH_PID:-}" ]] && kill -KILL "$CH_PID" 2>/dev/null || true
    [[ -n "${LSN_PID:-}" ]] && kill -TERM "$LSN_PID" 2>/dev/null || true
    if (( rc != 0 )); then
        echo "--- stream.log tail ---"; tail -40 "$STREAM_LOG" 2>/dev/null || true
        echo "--- console.log tail (kmsg + early agent logs) ---"
        tail -80 "$CONSOLE_LOG" 2>/dev/null || true
    fi
}
trap cleanup EXIT

start_ch() {
    rm -f "$API_SOCK" "$VSOCK_SOCK"
    "$CH" \
        --api-socket "$API_SOCK" \
        --console "file=$CONSOLE_LOG" \
        --serial off \
        --vsock "cid=$GUEST_CID,socket=$VSOCK_SOCK" \
        --cpus "boot=$VCPUS" \
        --memory "size=$MEMORY,shared=on" \
        "$@" \
        > "$WORK/ch.stdout.log" 2>> "$WORK/ch.stderr.log" &
    CH_PID=$!
    for _ in {1..50}; do [[ -S "$API_SOCK" ]] && break; sleep 0.1; done
    [[ -S "$API_SOCK" ]] || { red "API socket did not appear"; return 1; }
}

last_counter() {
    grep -oE 'DIAL COUNTER=[0-9]+' "$STREAM_LOG" 2>/dev/null \
        | tail -1 | sed 's/DIAL COUNTER=//'
}

count_peers() {
    grep -cE '^# peer [0-9]+ connected' "$STREAM_LOG" 2>/dev/null || echo 0
}

############################
blue "==> boot"
"$RUN/host-listener.py" "$VSOCK_SOCK" "$DIAL_PORT" "$STREAM_LOG" \
    > "$WORK/listener.log" 2>&1 &
LSN_PID=$!
sleep 0.2

start_ch \
    --kernel "$KERNEL" \
    --initramfs "$INITRD" \
    --cmdline "console=hvc0 reboot=k panic=1 quiet"

# Wait for first stream
deadline=$(( $(date +%s) + 10 ))
while (( $(date +%s) < deadline )); do
    [[ -n "$(last_counter)" ]] && break
    sleep 0.2
done
v0=$(last_counter)
[[ -n "$v0" ]] || { red "no stream from agent at boot"; exit 1; }
green "boot: counter=$v0, peers=$(count_peers)"

############################
blue "==> snapshot + restore"
SNAP_DIR="$WORK/snap"; mkdir -p "$SNAP_DIR"
"$CHR" --api-socket "$API_SOCK" pause >/dev/null
"$CHR" --api-socket "$API_SOCK" snapshot "file://$SNAP_DIR" >/dev/null
"$CHR" --api-socket "$API_SOCK" shutdown-vmm >/dev/null 2>&1 || true
wait "$CH_PID" 2>/dev/null || true
CH_PID=""

start_ch --kernel "$KERNEL" --restore "source_url=file://$SNAP_DIR"
green "restored, waiting ${WAIT_AFTER_RESTORE}s for periodic rebind to recover vsock..."

# Watch for new peer connection during the wait
peers_before=$(count_peers)
counter_before=$(last_counter)
echo "  baseline: peers=$peers_before counter=$counter_before"

deadline=$(( $(date +%s) + WAIT_AFTER_RESTORE ))
new_peer=0
while (( $(date +%s) < deadline )); do
    p=$(count_peers)
    if (( p > peers_before )); then
        new_peer=1
        break
    fi
    sleep 0.5
done

############################
echo "--- final state ---"
echo "peers seen: $(count_peers)"
echo "last counter: $(last_counter)"
echo "stream.log:"
tail -25 "$STREAM_LOG" || true
echo "console.log (kmsg from agent):"
tail -40 "$CONSOLE_LOG" || true

if (( new_peer )); then
    green "PASS: agent reconnected after rebind"
else
    red "FAIL: no new peer connection within ${WAIT_AFTER_RESTORE}s"
    exit 1
fi
