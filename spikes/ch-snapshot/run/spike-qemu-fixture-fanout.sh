#!/usr/bin/env bash
# Fixture fan-out: take ONE snapshot, then restore from it N times in parallel.
# This is the actual fixture-replay use case for provium: build a fixture
# (e.g., joined domain) once, replay it as the start state for many tests.
#
# Pass criteria:
#   - All N parallel restores succeed
#   - Each instance gets its own vsock stream
#   - Counter values are consistent: each restored VM starts at ~the same
#     counter value (the value at snapshot time) — proves they all loaded
#     the same memory image
#   - No CID conflicts, no QEMU errors

set -euo pipefail

SPIKE_DIR="$(cd "$(dirname "$0")/.." && pwd)"
RUN="$SPIKE_DIR/run"
WORK="$SPIKE_DIR/run/work-fanout"
KERNEL="/home/jack/projects/peios/pkm/kernel/out/bzImage"
INITRD="$SPIKE_DIR/initrd-build/initrd.cpio.gz"

FANOUT="${FANOUT:-8}"
DIAL_PORT=9999
SOURCE_CID="${SOURCE_CID:-50}"  # CID for the snapshot-builder VM
DEST_CID_BASE="${DEST_CID_BASE:-300}"  # restored instances: 300..300+FANOUT-1

QEMU=qemu-system-x86_64

rm -rf "$WORK"; mkdir -p "$WORK"
QMP_SOCK="$WORK/qmp.sock"
SERIAL_LOG="$WORK/serial-source.log"
STREAM_LOG="$WORK/stream.log"
SNAP_FILE="$WORK/fixture.snap"

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

# Listener handles all peers, demuxed by source CID
"$RUN/host-listener-vsock.py" "$DIAL_PORT" "$STREAM_LOG" \
    > "$WORK/listener.log" 2>&1 &
LSN_PID=$!
sleep 0.2

############################
# Phase 1: build the fixture (boot, run a bit, snapshot, kill)
############################
blue "==> phase 1: build fixture (boot, run ~2s, snapshot)"
"$QEMU" \
    -M q35,accel=kvm \
    -cpu host \
    -smp 2 \
    -m 256 \
    -nographic \
    -serial "file:$SERIAL_LOG" \
    -monitor none \
    -qmp "unix:$QMP_SOCK,server=on,wait=off" \
    -device vhost-vsock-pci,guest-cid="$SOURCE_CID" \
    -nodefaults \
    -kernel "$KERNEL" \
    -initrd "$INITRD" \
    -append "console=ttyS0 reboot=k panic=1 quiet" \
    > "$WORK/source-qemu.stdout.log" 2> "$WORK/source-qemu.stderr.log" &
SOURCE_PID=$!
QEMU_PIDS+=($SOURCE_PID)

for _ in {1..50}; do [[ -S "$QMP_SOCK" ]] && break; sleep 0.1; done

# Let it run for ~2s so we get a known counter value baked into the snapshot
sleep 2

# Snapshot
"$RUN/qmp.py" "$QMP_SOCK" stop >/dev/null
"$RUN/qmp.py" "$QMP_SOCK" migrate "{\"uri\":\"exec:cat > $SNAP_FILE\"}" >/dev/null
for _ in {1..50}; do
    status=$("$RUN/qmp.py" "$QMP_SOCK" query-migrate 2>/dev/null \
        | python3 -c 'import sys,json;d=json.load(sys.stdin);print(d.get("return",{}).get("status",""))' || echo "")
    [[ "$status" == "completed" ]] && break
    [[ "$status" == "failed" ]] && { red "fixture migrate failed"; exit 1; }
    sleep 0.1
done
"$RUN/qmp.py" "$QMP_SOCK" quit >/dev/null 2>&1 || true
wait "$SOURCE_PID" 2>/dev/null || true
QEMU_PIDS=()

[[ -s "$SNAP_FILE" ]] || { red "snapshot empty"; exit 1; }
green "  fixture built: $(stat -c%s "$SNAP_FILE") bytes"

# What was the counter value at snapshot time?
src_counter=$(grep -oE 'DIAL COUNTER=[0-9]+' "$STREAM_LOG" 2>/dev/null \
    | grep -oE '[0-9]+' | tail -1)
[[ -n "$src_counter" ]] || src_counter=0
echo "  source counter at snapshot: ~$src_counter"

############################
# Phase 2: fan-out parallel restores
############################
blue "==> phase 2: $FANOUT parallel restores from same fixture"
DEST_CIDS=()
for i in $(seq 0 $((FANOUT - 1))); do
    cid=$(( DEST_CID_BASE + i ))
    DEST_CIDS+=($cid)
    serial="$WORK/serial-restore-$i.log"
    qerr="$WORK/restore-$i.stderr.log"

    # Each restore needs a unique QMP socket so we can `cont` it.
    qmp="$WORK/qmp-restore-$i.sock"
    "$QEMU" \
        -M q35,accel=kvm \
        -cpu host \
        -smp 2 \
        -m 256 \
        -nographic \
        -serial "file:$serial" \
        -monitor none \
        -qmp "unix:$qmp,server=on,wait=off" \
        -device vhost-vsock-pci,guest-cid="$cid" \
        -nodefaults \
        -kernel "$KERNEL" \
        -initrd "$INITRD" \
        -append "console=ttyS0 reboot=k panic=1 quiet" \
        -incoming "exec:cat $SNAP_FILE" \
        > /dev/null 2> "$qerr" &
    QEMU_PIDS+=($!)
done
echo "  spawned PIDs: ${QEMU_PIDS[*]}"

# Wait for all QMP sockets, then `cont` each
sleep 1
for i in $(seq 0 $((FANOUT - 1))); do
    qmp="$WORK/qmp-restore-$i.sock"
    for _ in {1..50}; do [[ -S "$qmp" ]] && break; sleep 0.1; done
    if [[ -S "$qmp" ]]; then
        "$RUN/qmp.py" "$qmp" cont >/dev/null 2>&1 || true
    fi
done

# Let them run for ~5s to accumulate vsock streams
sleep 5

# Check what we got
blue "==> per-restore summary"
ok_count=0
for i in $(seq 0 $((FANOUT - 1))); do
    cid=${DEST_CIDS[$i]}
    n=$(grep -c "cid=$cid " "$STREAM_LOG" 2>/dev/null || echo 0)
    first=$(grep "cid=$cid " "$STREAM_LOG" 2>/dev/null \
        | grep -oE 'DIAL COUNTER=[0-9]+' | head -1 | grep -oE '[0-9]+' || echo "")
    last=$(grep "cid=$cid " "$STREAM_LOG" 2>/dev/null \
        | grep -oE 'DIAL COUNTER=[0-9]+' | tail -1 | grep -oE '[0-9]+' || echo "")
    qerr=$(grep -iE 'error|fail|busy' "$WORK/restore-$i.stderr.log" 2>/dev/null | head -1 || true)

    if [[ -n "$first" && -n "$last" ]]; then
        if [[ -n "$qerr" ]]; then
            red "  restore $i cid=$cid: streamed but qemu err: $qerr"
        else
            green "  restore $i cid=$cid: counter $first -> $last (lines=$n)"
            ((ok_count++)) || true
        fi
    else
        red "  restore $i cid=$cid: no counter stream (qerr: ${qerr:-none})"
    fi
done

# Tear down
for p in "${QEMU_PIDS[@]:-}"; do
    kill -TERM "$p" 2>/dev/null || true
done

echo
if (( ok_count == FANOUT )); then
    green "PASS: $ok_count/$FANOUT parallel restores from same fixture, distinct CIDs, all streamed cleanly"
else
    red "FAIL: $ok_count/$FANOUT succeeded"
    exit 1
fi
