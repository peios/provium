#!/usr/bin/env bash
# QEMU vsock + snapshot/restore spike. Same agent and initrd as CH spikes.
#
# Pass criteria:
#   - VM boots with vsock (4 vCPUs), agent dials host, counter streams
#   - Snapshot via QMP migrate to file
#   - Kill QEMU; start fresh QEMU with -incoming from snapshot file
#   - Agent reconnects, counter advances (monotonic)
#   - 5+ cycles without crash

set -euo pipefail

SPIKE_DIR="$(cd "$(dirname "$0")/.." && pwd)"
RUN="$SPIKE_DIR/run"
WORK="$SPIKE_DIR/run/work-qemu"
KERNEL="/home/jack/projects/peios/pkm/kernel/out/bzImage"
INITRD="$SPIKE_DIR/initrd-build/initrd.cpio.gz"

CYCLES="${CYCLES:-5}"
VCPUS="${VCPUS:-4}"
MEMORY="${MEMORY:-512}"  # MB
DIAL_PORT=9999
GUEST_CID="${GUEST_CID:-13}"   # avoid clashes with anything else lingering

QEMU=qemu-system-x86_64

rm -rf "$WORK"; mkdir -p "$WORK"
QMP_SOCK="$WORK/qmp.sock"
SERIAL_LOG="$WORK/serial.log"
STREAM_LOG="$WORK/stream.log"

red()   { printf '\033[31m%s\033[0m\n' "$*"; }
green() { printf '\033[32m%s\033[0m\n' "$*"; }
blue()  { printf '\033[34m%s\033[0m\n' "$*"; }

cleanup() {
    local rc=$?
    [[ -n "${QEMU_PID:-}" ]] && kill -KILL "$QEMU_PID" 2>/dev/null || true
    [[ -n "${LSN_PID:-}" ]] && kill -TERM "$LSN_PID" 2>/dev/null || true
    if (( rc != 0 )); then
        echo "--- stream.log tail ---"; tail -30 "$STREAM_LOG" 2>/dev/null || true
        echo "--- serial.log tail ---"; tail -50 "$SERIAL_LOG" 2>/dev/null || true
    fi
}
trap cleanup EXIT

start_qemu() {
    local extra=("$@")
    rm -f "$QMP_SOCK"
    "$QEMU" \
        -M q35,accel=kvm \
        -cpu host \
        -smp "$VCPUS" \
        -m "$MEMORY" \
        -nographic \
        -serial "file:$SERIAL_LOG" \
        -monitor none \
        -qmp "unix:$QMP_SOCK,server=on,wait=off" \
        -device vhost-vsock-pci,guest-cid="$GUEST_CID" \
        -nodefaults \
        "${extra[@]}" \
        > "$WORK/qemu.stdout.log" 2> "$WORK/qemu.stderr.log" &
    QEMU_PID=$!
    for _ in {1..50}; do [[ -S "$QMP_SOCK" ]] && break; sleep 0.1; done
    [[ -S "$QMP_SOCK" ]] || { red "QMP socket did not appear"; return 1; }
}

start_listener() {
    "$RUN/host-listener-vsock.py" "$DIAL_PORT" "$STREAM_LOG" \
        > "$WORK/listener.log" 2>&1 &
    LSN_PID=$!
}

last_counter() {
    grep -oE 'DIAL COUNTER=[0-9]+' "$STREAM_LOG" 2>/dev/null \
        | tail -1 | sed 's/DIAL COUNTER=//'
}

count_peers() {
    grep -cE '^# peer [0-9]+ connected' "$STREAM_LOG" 2>/dev/null || echo 0
}

wait_for_counter_ge() {
    local min=$1
    local timeout="${2:-20}"
    local deadline=$(( $(date +%s) + timeout ))
    while (( $(date +%s) < deadline )); do
        local v
        v=$(last_counter)
        if [[ -n "$v" && "$v" -ge "$min" ]]; then
            echo "$v"; return 0
        fi
        sleep 0.2
    done
    return 1
}

############################
blue "==> boot: QEMU vCPUs=$VCPUS memory=${MEMORY}M cid=$GUEST_CID cycles=$CYCLES"
start_listener
sleep 0.2
start_qemu \
    -kernel "$KERNEL" \
    -initrd "$INITRD" \
    -append "console=ttyS0 reboot=k panic=1 quiet"

v=$(wait_for_counter_ge 0 15) || { red "no stream from agent at boot"; exit 1; }
green "boot: counter=$v peers=$(count_peers)"
v_prev=$v
peers_prev=$(count_peers)

############################
for i in $(seq 1 "$CYCLES"); do
    SNAP_FILE="$WORK/snap-$i.bin"
    blue "==> cycle $i/$CYCLES"

    # Pause + migrate (snapshot) to file
    "$RUN/qmp.py" "$QMP_SOCK" stop >/dev/null
    "$RUN/qmp.py" "$QMP_SOCK" migrate-set-capabilities '{"capabilities":[{"capability":"events","state":true}]}' >/dev/null
    "$RUN/qmp.py" "$QMP_SOCK" migrate "{\"uri\":\"exec:cat > $SNAP_FILE\"}" >/dev/null

    # Wait for migration to complete
    for _ in {1..50}; do
        status=$("$RUN/qmp.py" "$QMP_SOCK" query-migrate 2>/dev/null | python3 -c 'import sys,json;d=json.load(sys.stdin);print(d.get("return",{}).get("status",""))' || echo "")
        if [[ "$status" == "completed" ]]; then break; fi
        if [[ "$status" == "failed" ]]; then red "cycle $i: migration failed"; exit 1; fi
        sleep 0.1
    done

    # Tear down QEMU
    "$RUN/qmp.py" "$QMP_SOCK" quit >/dev/null 2>&1 || true
    wait "$QEMU_PID" 2>/dev/null || true
    QEMU_PID=""

    [[ -s "$SNAP_FILE" ]] || { red "cycle $i: snapshot file empty"; exit 1; }
    echo "  snap size: $(stat -c%s "$SNAP_FILE") bytes"

    # Restore: start fresh QEMU with -incoming from snapshot file
    start_qemu \
        -kernel "$KERNEL" \
        -initrd "$INITRD" \
        -append "console=ttyS0 reboot=k panic=1 quiet" \
        -incoming "exec:cat $SNAP_FILE"

    # When QEMU starts via -incoming, vCPU starts paused. We need to cont.
    sleep 0.5
    "$RUN/qmp.py" "$QMP_SOCK" cont >/dev/null

    # Wait for counter to advance past previous value
    target=$(( v_prev + 1 ))
    v=$(wait_for_counter_ge "$target" 20) || {
        red "cycle $i: counter did not advance past $v_prev (peers seen so far: $(count_peers))"
        exit 1
    }
    if (( v < v_prev )); then
        red "cycle $i: counter went backward ($v_prev -> $v)"; exit 1
    fi
    green "cycle $i: counter=$v (delta=$(( v - v_prev ))) peers=$(count_peers)"
    v_prev=$v
done

green "PASS: $CYCLES QEMU snapshot/restore cycles, vsock survived"
echo "Logs: $STREAM_LOG  $SERIAL_LOG  $WORK/qemu.stderr.log"
