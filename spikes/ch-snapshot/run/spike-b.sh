#!/usr/bin/env bash
# Spike 2 / Option B: guest-initiated vsock across CH snapshot/restore.
#
# Pass criteria:
#   - Guest agent dials host CID 2 port 9999, streams counter lines
#   - After snapshot+VMM kill+restore, agent reconnects and streams resume
#   - Counter is monotonic across cycles (state preserved)
#   - 5+ cycles without crash

set -euo pipefail

SPIKE_DIR="$(cd "$(dirname "$0")/.." && pwd)"
BIN="$SPIKE_DIR/bin"
RUN="$SPIKE_DIR/run"
WORK="$SPIKE_DIR/run/work-b"
KERNEL="/home/jack/projects/peios/pkm/kernel/out/bzImage"
INITRD="$SPIKE_DIR/initrd-build/initrd.cpio.gz"

CYCLES="${CYCLES:-5}"
VCPUS="${VCPUS:-4}"
MEMORY="${MEMORY:-512M}"
DIAL_PORT=9999
GUEST_CID=3

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
        echo "--- stream.log tail ---"
        tail -30 "$STREAM_LOG" 2>/dev/null || true
        echo "--- ch.stderr.log tail ---"
        tail -20 "$WORK/ch.stderr.log" 2>/dev/null || true
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

start_listener() {
    "$RUN/host-listener.py" "$VSOCK_SOCK" "$DIAL_PORT" "$STREAM_LOG" \
        > "$WORK/listener.log" 2>&1 &
    LSN_PID=$!
}

last_counter() {
    # extract last DIAL COUNTER=N value from stream.log; empty if none
    grep -oE 'DIAL COUNTER=[0-9]+' "$STREAM_LOG" 2>/dev/null \
        | tail -1 | sed 's/DIAL COUNTER=//'
}

wait_for_counter_ge() {
    local min=$1
    local deadline=$(( $(date +%s) + 30 ))
    while (( $(date +%s) < deadline )); do
        local v
        v=$(last_counter)
        if [[ -n "$v" && "$v" -ge "$min" ]]; then
            echo "$v"
            return 0
        fi
        sleep 0.2
    done
    return 1
}

############################
# Boot phase
############################
blue "==> boot: vCPUs=$VCPUS memory=$MEMORY cycles=$CYCLES"
start_listener
sleep 0.3
start_ch \
    --kernel "$KERNEL" \
    --initramfs "$INITRD" \
    --cmdline "console=hvc0 reboot=k panic=1 quiet"

# Wait for first stream line
v=$(wait_for_counter_ge 0) || { red "no stream from agent at boot"; exit 1; }
green "boot: counter=$v (initial stream up)"
v_prev=$v

############################
# Snapshot/restore cycles
############################
for i in $(seq 1 "$CYCLES"); do
    SNAP_DIR="$WORK/snap-$i"
    mkdir -p "$SNAP_DIR"
    blue "==> cycle $i/$CYCLES"

    # Pause + snapshot
    "$CHR" --api-socket "$API_SOCK" pause >/dev/null
    "$CHR" --api-socket "$API_SOCK" snapshot "file://$SNAP_DIR" >/dev/null

    # Tear down VMM (this also closes the listener's accepted connection)
    "$CHR" --api-socket "$API_SOCK" shutdown-vmm >/dev/null 2>&1 || true
    wait "$CH_PID" 2>/dev/null || true
    CH_PID=""

    # Listener is still alive; restart CH from snapshot
    start_ch \
        --kernel "$KERNEL" \
        --restore "source_url=file://$SNAP_DIR"

    # Wait for the agent to reconnect and counter to advance past v_prev
    target=$(( v_prev + 1 ))
    v=$(wait_for_counter_ge "$target") || {
        red "cycle $i: agent did not reconnect / counter did not advance past $v_prev"
        exit 1
    }
    if (( v < v_prev )); then
        red "cycle $i: counter went backward ($v_prev -> $v)"
        exit 1
    fi
    green "cycle $i: counter=$v (delta=$(( v - v_prev )))"
    v_prev=$v
done

green "PASS: $CYCLES snapshot/restore cycles, vCPUs=$VCPUS, guest-initiated vsock survived"
echo "Logs: $STREAM_LOG  $CONSOLE_LOG  $WORK/ch.stderr.log"
