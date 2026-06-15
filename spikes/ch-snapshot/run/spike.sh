#!/usr/bin/env bash
# Spike 2: cloud-hypervisor snapshot/restore round-trip on 4 vCPUs.
#
# Pass criteria:
#   - VM boots cleanly with vsock + 4 vCPUs
#   - Counter is monotonic across N snapshot/restore cycles
#   - No kernel oops (checked via console log) and no SMP-related crash
#
# Failure mode: if anything below trips, dump logs and exit non-zero.

set -euo pipefail

SPIKE_DIR="$(cd "$(dirname "$0")/.." && pwd)"
BIN="$SPIKE_DIR/bin"
RUN="$SPIKE_DIR/run"
WORK="$SPIKE_DIR/run/work"
KERNEL="/home/jack/projects/peios/pkm/kernel/out/bzImage"
INITRD="$SPIKE_DIR/initrd-build/initrd.cpio.gz"

CYCLES="${CYCLES:-5}"
VCPUS="${VCPUS:-4}"
MEMORY="${MEMORY:-512M}"
VSOCK_PORT=1234
GUEST_CID=3

CH="$BIN/cloud-hypervisor"
CHR="$BIN/ch-remote"

rm -rf "$WORK"
mkdir -p "$WORK"

API_SOCK="$WORK/ch.sock"
VSOCK_SOCK="$WORK/vsock.sock"
CONSOLE_LOG="$WORK/console.log"

red()   { printf '\033[31m%s\033[0m\n' "$*"; }
green() { printf '\033[32m%s\033[0m\n' "$*"; }
blue()  { printf '\033[34m%s\033[0m\n' "$*"; }

cleanup() {
    local rc=$?
    if [[ -n "${CH_PID:-}" ]] && kill -0 "$CH_PID" 2>/dev/null; then
        if (( rc != 0 )); then
            echo "--- failure: leaving CH PID $CH_PID alive briefly for diagnostics ---"
            sleep 0.5
        fi
        kill -TERM "$CH_PID" 2>/dev/null || true
        sleep 0.2
        kill -KILL "$CH_PID" 2>/dev/null || true
    fi
    if (( rc != 0 )); then
        echo "--- ch.stderr.log tail ---"
        tail -40 "$WORK/ch.stderr.log" 2>/dev/null || true
        echo "--- console.log tail ---"
        tail -60 "$CONSOLE_LOG" 2>/dev/null || true
    fi
}
trap cleanup EXIT

start_ch() {
    local extra=("$@")
    rm -f "$API_SOCK" "$VSOCK_SOCK"
    "$CH" \
        --api-socket "$API_SOCK" \
        --console "file=$CONSOLE_LOG" \
        --serial off \
        --vsock "cid=$GUEST_CID,socket=$VSOCK_SOCK" \
        --cpus "boot=$VCPUS" \
        --memory "size=$MEMORY,shared=on" \
        "${extra[@]}" \
        > "$WORK/ch.stdout.log" 2> "$WORK/ch.stderr.log" &
    CH_PID=$!

    # Wait for API socket to appear
    for _ in {1..50}; do
        [[ -S "$API_SOCK" ]] && break
        sleep 0.1
    done
    if [[ ! -S "$API_SOCK" ]]; then
        red "API socket did not appear"
        return 1
    fi
}

query_counter() {
    "$RUN/vsock-query.py" "$VSOCK_SOCK" "$VSOCK_PORT" 30.0
}

check_oops() {
    if grep -E -q 'Kernel panic|Oops|BUG:|stack guard page was hit|RIP:' "$CONSOLE_LOG" 2>/dev/null; then
        red "kernel oops/panic detected in console log"
        grep -E -B2 -A8 'Kernel panic|Oops|BUG:|RIP:' "$CONSOLE_LOG" || true
        return 1
    fi
}

############################
# Boot phase
############################
blue "==> boot: vCPUs=$VCPUS memory=$MEMORY cycles=$CYCLES"
start_ch \
    --kernel "$KERNEL" \
    --initramfs "$INITRD" \
    --cmdline "console=hvc0 reboot=k panic=1 quiet"

# First counter read (boot)
v0_line=$(query_counter)
green "boot: $v0_line"
v_prev=$(echo "$v0_line" | sed -n 's/.*COUNTER=\([0-9][0-9]*\).*/\1/p')
[[ -n "$v_prev" ]] || { red "could not parse counter from: $v0_line"; exit 1; }

############################
# Snapshot/restore cycles
############################
for i in $(seq 1 "$CYCLES"); do
    SNAP_DIR="$WORK/snap-$i"
    mkdir -p "$SNAP_DIR"

    blue "==> cycle $i/$CYCLES"

    # Pause and snapshot
    "$CHR" --api-socket "$API_SOCK" pause >/dev/null
    "$CHR" --api-socket "$API_SOCK" snapshot "file://$SNAP_DIR" >/dev/null

    # Tear down current VMM
    "$CHR" --api-socket "$API_SOCK" shutdown-vmm >/dev/null 2>&1 || true
    wait "$CH_PID" 2>/dev/null || true
    CH_PID=""

    # Restore from snapshot. CH v51 CLI parser still demands --kernel here,
    # though the snapshot's config.json has the kernel path baked in; the
    # flag is satisfied syntactically and the snapshot config wins at runtime.
    # CH v51 auto-resumes the restored VM, so no explicit resume call.
    start_ch \
        --kernel "$KERNEL" \
        --restore "source_url=file://$SNAP_DIR"

    echo "  vm.info after restore:"
    "$CHR" --api-socket "$API_SOCK" info 2>&1 | sed 's/^/    /' | head -20 || true
    echo "  vsock socket:"
    ls -la "$VSOCK_SOCK" 2>&1 | sed 's/^/    /'

    # Re-query counter
    v1_line=$(query_counter)
    v_now=$(echo "$v1_line" | sed -n 's/.*COUNTER=\([0-9][0-9]*\).*/\1/p')
    [[ -n "$v_now" ]] || { red "cycle $i: could not parse counter"; cat "$CONSOLE_LOG"; exit 1; }

    if (( v_now < v_prev )); then
        red "cycle $i: counter went backward ($v_prev -> $v_now)"
        exit 1
    fi
    green "cycle $i: $v1_line  (delta=$(( v_now - v_prev )))"
    v_prev=$v_now

    check_oops
done

############################
# Summary
############################
green "PASS: $CYCLES snapshot/restore cycles, vCPUs=$VCPUS, no oops detected"
echo "Logs: $CONSOLE_LOG  $WORK/ch.stderr.log"
