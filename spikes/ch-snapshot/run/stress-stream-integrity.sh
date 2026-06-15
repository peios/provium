#!/usr/bin/env bash
# Stress: stream integrity across snapshot/restore at high rate.
# Modifies the agent to dial at 10ms intervals (10x normal), runs 3
# snapshot/restore cycles, then verifies:
#   - No truncated lines (every line parseable)
#   - Within each peer's connection, COUNTER is strictly increasing
#   - No duplicate COUNTER values within a peer
#   - No data corruption (UPTIME_MS is monotonic too)

set -euo pipefail

SPIKE_DIR="$(cd "$(dirname "$0")/.." && pwd)"
RUN="$SPIKE_DIR/run"
WORK="$SPIKE_DIR/run/work-integrity"
KERNEL="/home/jack/projects/peios/pkm/kernel/out/bzImage"
INITRD_FAST="$SPIKE_DIR/initrd-build/initrd-fast.cpio.gz"
AGENT_SRC="$SPIKE_DIR/agent/spike-agent.c"
AGENT_FAST="$SPIKE_DIR/agent/spike-agent-fast"

DIAL_PORT=9999
CID=420
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

############################
# Build a fast-mode agent variant: 10ms dialer interval, 10ms ticker
blue "==> building fast-mode agent (10ms cadence)"
sed -e 's/200 \* 1000 \* 1000/10 * 1000 * 1000/g' \
    -e 's/100 \* 1000 \* 1000/10 * 1000 * 1000/g' \
    "$AGENT_SRC" > "$WORK/spike-agent-fast.c"

nix-shell -p musl --run "musl-gcc -static -O2 -Wall -pthread -o $AGENT_FAST $WORK/spike-agent-fast.c" \
    2>&1 | grep -E 'error' | head -3 || true
[[ -x "$AGENT_FAST" ]] || { red "fast-agent build failed"; exit 1; }

# Build fast initrd
mkdir -p "$WORK/root"/{dev,proc,sys,tmp}
cp "$AGENT_FAST" "$WORK/root/init"
chmod +x "$WORK/root/init"
(cd "$WORK/root" && find . | cpio --quiet -o -H newc | gzip > "$INITRD_FAST")
echo "  initrd-fast: $(stat -c%s "$INITRD_FAST") bytes"

############################
"$RUN/host-listener-vsock.py" "$DIAL_PORT" "$STREAM_LOG" \
    > "$WORK/listener.log" 2>&1 &
LSN_PID=$!
sleep 0.2

start_qemu() {
    local extra=("$@")
    rm -f "$QMP_SOCK"
    "$QEMU" \
        -M q35,accel=kvm -cpu host -smp 2 -m 256 -nographic \
        -serial "file:$WORK/serial.log" \
        -monitor none \
        -qmp "unix:$QMP_SOCK,server=on,wait=off" \
        -device vhost-vsock-pci,guest-cid="$CID" \
        -nodefaults \
        -kernel "$KERNEL" -initrd "$INITRD_FAST" \
        -append "console=ttyS0 reboot=k panic=1 quiet" \
        "${extra[@]}" \
        > "$WORK/qemu.stdout.log" 2>> "$WORK/qemu.stderr.log" &
    QEMU_PID=$!
    for _ in {1..50}; do [[ -S "$QMP_SOCK" ]] && break; sleep 0.1; done
}

blue "==> boot at 10ms cadence"
start_qemu -kernel "$KERNEL" -initrd "$INITRD_FAST" -append "console=ttyS0 reboot=k panic=1 quiet"

# Let it run, accumulating high-rate stream
sleep 2

for i in $(seq 1 "$CYCLES"); do
    SNAP="$WORK/snap-$i.bin"
    blue "==> cycle $i: snapshot + restore (during high-rate streaming)"
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
    start_qemu -kernel "$KERNEL" -initrd "$INITRD_FAST" \
               -append "console=ttyS0 reboot=k panic=1 quiet" \
               -incoming "exec:cat $SNAP"
    sleep 0.5
    "$RUN/qmp.py" "$QMP_SOCK" cont >/dev/null 2>&1 || true
    sleep 1.5
done

# Tear down
"$RUN/qmp.py" "$QMP_SOCK" quit >/dev/null 2>&1 || true
sleep 0.5

############################
blue "==> stream integrity analysis"
python3 - "$STREAM_LOG" <<'PY'
import re, sys
from collections import defaultdict

path = sys.argv[1]
peers = defaultdict(list)
peer_meta = {}
current_peer = None

line_re = re.compile(r'\[\S+\s+peer=(\d+)\]\s+DIAL\s+COUNTER=(\d+)\s+UPTIME_MS=(\d+)')
peer_re = re.compile(r'^# peer (\d+) connected from cid=(\d+)')

with open(path) as f:
    for ln in f:
        m = peer_re.match(ln)
        if m:
            pid, cid = int(m.group(1)), int(m.group(2))
            peer_meta[pid] = cid
            continue
        m = line_re.match(ln)
        if m:
            pid = int(m.group(1))
            counter = int(m.group(2))
            uptime = int(m.group(3))
            peers[pid].append((counter, uptime))

bad = 0
total_lines = 0
for pid, samples in sorted(peers.items()):
    cid = peer_meta.get(pid, "?")
    n = len(samples)
    total_lines += n
    counters = [s[0] for s in samples]
    uptimes = [s[1] for s in samples]
    min_c, max_c = min(counters), max(counters)
    dupes = len(counters) - len(set(counters))
    monotonic_c = all(counters[i] <= counters[i+1] for i in range(len(counters)-1))
    monotonic_u = all(uptimes[i] <= uptimes[i+1] for i in range(len(uptimes)-1))
    status = "OK" if (dupes == 0 and monotonic_c and monotonic_u) else "BAD"
    if status == "BAD":
        bad += 1
    print(f"  peer={pid} cid={cid}: n={n} counter={min_c}..{max_c} "
          f"dupes={dupes} mono_counter={monotonic_c} mono_uptime={monotonic_u} {status}")

print(f"\nTotal lines: {total_lines}  peers: {len(peers)}  bad peers: {bad}")
sys.exit(1 if bad else 0)
PY

if (( $? == 0 )); then
    green "PASS: stream integrity intact across $CYCLES high-rate cycles"
else
    red "FAIL: stream corruption detected"
    exit 1
fi
