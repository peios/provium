#!/usr/bin/env bash
# Diagnostic: how long does the host kernel hold a CID after abnormal QEMU
# termination (SIGKILL during migration vs SIGKILL while idle vs SIGTERM)?
# This is the actual operational concern for provium when a test panics.

set -euo pipefail

SPIKE_DIR="$(cd "$(dirname "$0")/.." && pwd)"
WORK="$SPIKE_DIR/run/work-cid-release"
KERNEL="/home/jack/projects/peios/pkm/kernel/out/bzImage"
INITRD="$SPIKE_DIR/initrd-build/initrd.cpio.gz"
QEMU=qemu-system-x86_64

rm -rf "$WORK"; mkdir -p "$WORK"

red()   { printf '\033[31m%s\033[0m\n' "$*"; }
green() { printf '\033[32m%s\033[0m\n' "$*"; }
blue()  { printf '\033[34m%s\033[0m\n' "$*"; }

start_qemu() {
    local label=$1 cid=$2
    local extra=("${@:3}")
    "$QEMU" \
        -M q35,accel=kvm -cpu host -smp 1 -m 128 -nographic \
        -serial null -monitor none \
        -qmp "unix:$WORK/qmp-$label.sock,server=on,wait=off" \
        -device vhost-vsock-pci,guest-cid="$cid" \
        -nodefaults \
        -kernel "$KERNEL" -initrd "$INITRD" \
        -append "console=ttyS0 reboot=k panic=1 quiet" \
        "${extra[@]}" \
        > "$WORK/$label.stdout.log" 2> "$WORK/$label.stderr.log" &
    echo $!
}

# Try to bind the CID until success or timeout. Returns time to success.
time_to_reuse() {
    local cid=$1
    local label=$2
    local start=$(date +%s.%3N)
    local deadline=$(( $(date +%s) + 30 ))
    while (( $(date +%s) < deadline )); do
        local pid=$(start_qemu "$label" "$cid")
        sleep 0.3
        if grep -q 'Address already in use' "$WORK/$label.stderr.log" 2>/dev/null; then
            kill -KILL "$pid" 2>/dev/null || true
            wait "$pid" 2>/dev/null || true
            sleep 0.2
            > "$WORK/$label.stderr.log"
            continue
        fi
        # Success
        local end=$(date +%s.%3N)
        kill -KILL "$pid" 2>/dev/null || true
        wait "$pid" 2>/dev/null || true
        echo "$start $end" | awk '{printf "%.2f\n", $2 - $1}'
        return 0
    done
    echo "TIMEOUT"
    return 1
}

############################
# Scenario A: SIGTERM (clean shutdown) — CID should be free immediately
blue "==> A: SIGTERM clean shutdown"
PID=$(start_qemu "A1" 500)
sleep 1
kill -TERM "$PID"
wait "$PID" 2>/dev/null || true
ttr=$(time_to_reuse 500 "A2")
echo "  CID 500 reusable after SIGTERM in: ${ttr}s"

############################
# Scenario B: SIGKILL while idle (no migration in progress)
blue "==> B: SIGKILL while idle"
PID=$(start_qemu "B1" 501)
sleep 1
kill -KILL "$PID"
wait "$PID" 2>/dev/null || true
ttr=$(time_to_reuse 501 "B2")
echo "  CID 501 reusable after SIGKILL (idle) in: ${ttr}s"

############################
# Scenario C: SIGKILL during migration
blue "==> C: SIGKILL during migration"
PID=$(start_qemu "C1" 502)
sleep 1
"$SPIKE_DIR/run/qmp.py" "$WORK/qmp-C1.sock" stop >/dev/null
"$SPIKE_DIR/run/qmp.py" "$WORK/qmp-C1.sock" migrate "{\"uri\":\"exec:cat > $WORK/c.snap\"}" >/dev/null
sleep 0.1
kill -KILL "$PID"
wait "$PID" 2>/dev/null || true
ttr=$(time_to_reuse 502 "C2")
echo "  CID 502 reusable after SIGKILL (mid-migration) in: ${ttr}s"

green "All scenarios eventually succeed; provium should pick fresh CIDs anyway"
