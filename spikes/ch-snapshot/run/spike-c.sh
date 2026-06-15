#!/usr/bin/env bash
# Spike 2 / Option C: vsock recovered via serial-triggered driver rebind.
#
# Phases:
#   1. Serial sanity: PING -> PONG before snapshot.
#   2. Serial sanity: PING -> PONG after snapshot/restore (does serial itself
#      survive CH snapshot/restore?).
#   3. Full recovery cycle: snapshot, restore, send RESUMED on serial,
#      agent rebinds virtio_vsock, redials host, counter advances.
#
# Pass = phase 3 succeeds for N cycles, counter monotonic.

set -euo pipefail

SPIKE_DIR="$(cd "$(dirname "$0")/.." && pwd)"
BIN="$SPIKE_DIR/bin"
RUN="$SPIKE_DIR/run"
WORK="$SPIKE_DIR/run/work-c"
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
SERIAL_SOCK="$WORK/serial.sock"
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
        echo "--- stream.log tail ---"; tail -30 "$STREAM_LOG" 2>/dev/null || true
        echo "--- console.log tail ---"; tail -40 "$CONSOLE_LOG" 2>/dev/null || true
        echo "--- ch.stderr.log tail ---"; tail -20 "$WORK/ch.stderr.log" 2>/dev/null || true
    fi
}
trap cleanup EXIT

start_ch() {
    rm -f "$API_SOCK" "$VSOCK_SOCK" "$SERIAL_SOCK"
    "$CH" \
        --api-socket "$API_SOCK" \
        --console "file=$CONSOLE_LOG" \
        --serial "socket=$SERIAL_SOCK" \
        --vsock "cid=$GUEST_CID,socket=$VSOCK_SOCK" \
        --cpus "boot=$VCPUS" \
        --memory "size=$MEMORY,shared=on" \
        "$@" \
        > "$WORK/ch.stdout.log" 2>> "$WORK/ch.stderr.log" &
    CH_PID=$!
    for _ in {1..50}; do [[ -S "$API_SOCK" ]] && break; sleep 0.1; done
    [[ -S "$API_SOCK" ]] || { red "API socket did not appear"; return 1; }
    for _ in {1..50}; do [[ -S "$SERIAL_SOCK" ]] && break; sleep 0.1; done
    [[ -S "$SERIAL_SOCK" ]] || { red "serial socket did not appear"; return 1; }
}

start_listener() {
    "$RUN/host-listener.py" "$VSOCK_SOCK" "$DIAL_PORT" "$STREAM_LOG" \
        > "$WORK/listener.log" 2>&1 &
    LSN_PID=$!
}

# Send a command on the serial unix socket and read one reply line.
serial_cmd() {
    local cmd="$1"
    local timeout="${2:-5.0}"
    python3 - "$SERIAL_SOCK" "$cmd" "$timeout" <<'PY'
import socket, sys, time
sock_path, cmd, timeout = sys.argv[1], sys.argv[2], float(sys.argv[3])
s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
s.settimeout(timeout)
s.connect(sock_path)
s.sendall((cmd + "\n").encode())
deadline = time.monotonic() + timeout
buf = b""
while time.monotonic() < deadline and b"\n" not in buf:
    try:
        chunk = s.recv(256)
    except socket.timeout:
        break
    if not chunk:
        break
    buf += chunk
s.close()
print(buf.split(b"\n", 1)[0].decode(errors="replace").strip())
PY
}

last_counter() {
    grep -oE 'DIAL COUNTER=[0-9]+' "$STREAM_LOG" 2>/dev/null \
        | tail -1 | sed 's/DIAL COUNTER=//'
}

wait_for_counter_ge() {
    local min=$1
    local timeout="${2:-15}"
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
# Boot phase
############################
blue "==> boot: vCPUs=$VCPUS memory=$MEMORY cycles=$CYCLES"
start_listener
sleep 0.2
start_ch \
    --kernel "$KERNEL" \
    --initramfs "$INITRD" \
    --cmdline "console=hvc0 reboot=k panic=1 quiet"

# Wait for vsock stream and serial to be ready.
v=$(wait_for_counter_ge 0) || { red "no stream from agent at boot"; exit 1; }
green "boot: counter=$v (vsock stream up)"

############################
# Phase 1: serial PING pre-snapshot
############################
blue "==> phase 1: serial PING pre-snapshot"
reply=$(serial_cmd "PING" 3.0)
echo "  reply: $reply"
[[ "$reply" == "PONG" ]] || { red "phase 1 failed: expected PONG got '$reply'"; exit 1; }
green "phase 1: serial bidirectional working"

############################
# Phase 2: serial across one snapshot/restore
############################
blue "==> phase 2: serial across snapshot/restore"
SNAP_DIR="$WORK/snap-phase2"; mkdir -p "$SNAP_DIR"
"$CHR" --api-socket "$API_SOCK" pause >/dev/null
"$CHR" --api-socket "$API_SOCK" snapshot "file://$SNAP_DIR" >/dev/null
"$CHR" --api-socket "$API_SOCK" shutdown-vmm >/dev/null 2>&1 || true
wait "$CH_PID" 2>/dev/null || true
CH_PID=""
start_ch --kernel "$KERNEL" --restore "source_url=file://$SNAP_DIR"

# Diagnostic: try multiple PINGs spaced out. First-byte loss is plausible
# if the UART FIFO has stale state. Sustained failure means real break.
for delay in 0.2 1.0 2.0 5.0; do
    sleep "$delay"
    reply=$(serial_cmd "PING" 3.0)
    echo "  +${delay}s ping reply: '$reply'"
    if [[ "$reply" == "PONG" ]]; then
        green "phase 2: serial survives CH snapshot/restore (recovered after ${delay}s)"
        break
    fi
done
if [[ "$reply" != "PONG" ]]; then
    echo "--- post-restore vm.info ---"
    "$CHR" --api-socket "$API_SOCK" info 2>&1 | head -20 || true
    echo "--- post-restore console.log ---"
    cat "$CONSOLE_LOG" 2>/dev/null || true
    red "phase 2 failed: serial broken across snapshot/restore"
    red "consider QEMU"
    exit 1
fi

# Reset cycle counter baseline for phase 3.
v_prev=$(last_counter)
[[ -n "$v_prev" ]] || v_prev=0
echo "  counter baseline going into phase 3: $v_prev"

############################
# Phase 3: full recovery cycles
############################
blue "==> phase 3: $CYCLES full recovery cycles"
# At this point we already did one snapshot/restore in phase 2 but didn't
# rebind vsock. Trigger a rebind now and verify counter advances.
reply=$(serial_cmd "RESUMED" 5.0)
echo "  RESUMED reply: '$reply'"
v=$(wait_for_counter_ge $(( v_prev + 1 )) 15) || {
    red "phase 3 init: vsock did not recover after RESUMED"
    exit 1
}
green "phase 3 init: counter=$v after rebind (delta=$(( v - v_prev )))"
v_prev=$v

for i in $(seq 1 "$CYCLES"); do
    SNAP_DIR="$WORK/snap-$i"; mkdir -p "$SNAP_DIR"
    blue "==> cycle $i/$CYCLES"

    "$CHR" --api-socket "$API_SOCK" pause >/dev/null
    "$CHR" --api-socket "$API_SOCK" snapshot "file://$SNAP_DIR" >/dev/null
    "$CHR" --api-socket "$API_SOCK" shutdown-vmm >/dev/null 2>&1 || true
    wait "$CH_PID" 2>/dev/null || true
    CH_PID=""

    start_ch --kernel "$KERNEL" --restore "source_url=file://$SNAP_DIR"
    sleep 0.3

    reply=$(serial_cmd "RESUMED" 5.0)
    echo "  RESUMED reply: '$reply'"

    target=$(( v_prev + 1 ))
    v=$(wait_for_counter_ge "$target" 15) || {
        red "cycle $i: counter did not advance past $v_prev"
        exit 1
    }
    if (( v < v_prev )); then
        red "cycle $i: counter went backward ($v_prev -> $v)"
        exit 1
    fi
    green "cycle $i: counter=$v (delta=$(( v - v_prev )))"
    v_prev=$v
done

green "PASS: $CYCLES recovery cycles, vCPUs=$VCPUS, vsock recovered via serial-triggered rebind"
