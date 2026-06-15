#!/usr/bin/env bash
# Stress: bidirectional vsock + multiple ports across snapshot/restore.
# Agent already does:
#   - listens on port 1234 (host can connect IN)
#   - dials port 9999 (agent connects OUT)
# CH broke host->guest entirely. Test that QEMU does both, and both
# survive snapshot/restore.

set -euo pipefail

SPIKE_DIR="$(cd "$(dirname "$0")/.." && pwd)"
RUN="$SPIKE_DIR/run"
WORK="$SPIKE_DIR/run/work-bidir"
KERNEL="/home/jack/projects/peios/pkm/kernel/out/bzImage"
INITRD="$SPIKE_DIR/initrd-build/initrd.cpio.gz"

DIAL_PORT=9999
LISTEN_PORT=1234
CID=600
CYCLES=3

QEMU=qemu-system-x86_64

rm -rf "$WORK"; mkdir -p "$WORK"
QMP_SOCK="$WORK/qmp.sock"
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

start_qemu() {
    local extra=("$@")
    rm -f "$QMP_SOCK"
    "$QEMU" \
        -M q35,accel=kvm -cpu host -smp 2 -m 256 -nographic \
        -serial "file:$WORK/serial.log" -monitor none \
        -qmp "unix:$QMP_SOCK,server=on,wait=off" \
        -device vhost-vsock-pci,guest-cid="$CID" \
        -nodefaults \
        -kernel "$KERNEL" -initrd "$INITRD" \
        -append "console=ttyS0 reboot=k panic=1 quiet" \
        "${extra[@]}" \
        > "$WORK/qemu.stdout.log" 2>> "$WORK/qemu.stderr.log" &
    QEMU_PID=$!
    for _ in {1..50}; do [[ -S "$QMP_SOCK" ]] && break; sleep 0.1; done
    sleep 0.3   # extra time for QMP to be ready
}

# Connect to (guest CID, listen_port), read one line, return it.
host_to_guest_query() {
    python3 - "$CID" "$LISTEN_PORT" <<'PY'
import socket, sys, time
try:
    AF_VSOCK = socket.AF_VSOCK
except AttributeError:
    AF_VSOCK = 40
cid = int(sys.argv[1]); port = int(sys.argv[2])
s = socket.socket(AF_VSOCK, socket.SOCK_STREAM)
s.settimeout(3.0)
try:
    s.connect((cid, port))
    data = b""
    while True:
        try:
            chunk = s.recv(256)
        except socket.timeout:
            break
        if not chunk: break
        data += chunk
    print(data.decode(errors="replace").strip())
except Exception as e:
    print(f"ERR {e}", file=sys.stderr)
    sys.exit(1)
finally:
    s.close()
PY
}

############################
# Listener for guest->host stream
"$RUN/host-listener-vsock.py" "$DIAL_PORT" "$STREAM_LOG" \
    > "$WORK/listener.log" 2>&1 &
LSN_PID=$!
sleep 0.2

blue "==> boot"
start_qemu -kernel "$KERNEL" -initrd "$INITRD" \
           -append "console=ttyS0 reboot=k panic=1 quiet"
sleep 1.5

# Verify guest->host works
gtoh=$(grep -oE 'DIAL COUNTER=[0-9]+' "$STREAM_LOG" 2>/dev/null | tail -1 || true)
[[ -n "$gtoh" ]] || { red "boot: no guest->host stream"; exit 1; }
green "boot guest->host: $gtoh"

# Verify host->guest works
htog=$(host_to_guest_query)
echo "boot host->guest reply: $htog"
if [[ "$htog" =~ ^COUNTER= ]]; then
    green "boot host->guest: $htog"
else
    red "boot: host->guest connection failed"; exit 1
fi

############################
for i in $(seq 1 "$CYCLES"); do
    SNAP="$WORK/snap-$i.bin"
    blue "==> cycle $i: snapshot/restore (both directions in flight)"
    "$RUN/qmp.py" "$QMP_SOCK" stop >/dev/null
    "$RUN/qmp.py" "$QMP_SOCK" migrate "{\"uri\":\"exec:cat > $SNAP\"}" >/dev/null
    for _ in {1..50}; do
        s=$("$RUN/qmp.py" "$QMP_SOCK" query-migrate 2>/dev/null \
            | python3 -c 'import sys,json;d=json.load(sys.stdin);print(d.get("return",{}).get("status",""))' || echo "")
        [[ "$s" == "completed" ]] && break
        [[ "$s" == "failed" ]] && { red "migrate failed"; exit 1; }
        sleep 0.1
    done
    "$RUN/qmp.py" "$QMP_SOCK" quit >/dev/null 2>&1 || true
    wait "$QEMU_PID" 2>/dev/null || true
    QEMU_PID=""

    start_qemu -kernel "$KERNEL" -initrd "$INITRD" \
               -append "console=ttyS0 reboot=k panic=1 quiet" \
               -incoming "exec:cat $SNAP"
    sleep 0.3
    "$RUN/qmp.py" "$QMP_SOCK" cont >/dev/null 2>&1 || true
    sleep 1

    # Re-verify both directions
    gtoh=$(grep -oE 'DIAL COUNTER=[0-9]+' "$STREAM_LOG" 2>/dev/null | tail -1 || true)
    htog=$(host_to_guest_query 2>&1 || echo "ERR")

    if [[ -z "$gtoh" ]]; then
        red "cycle $i: guest->host stream lost"; exit 1
    fi
    if [[ ! "$htog" =~ ^COUNTER= ]]; then
        red "cycle $i: host->guest broke ($htog)"; exit 1
    fi
    green "cycle $i: g->h $gtoh   h->g $htog"
done

green "PASS: bidirectional vsock survived $CYCLES snapshot/restore cycles"
