#!/usr/bin/env python3
#!/usr/bin/env python3 -u
"""Event-driven version of spike-qemu.sh. No `sleep` for settling.
Only filesystem-readiness polling (waiting for QMP socket file to appear
after fork+exec) is allowed, since that's an OS-level fact, not a timer.

Demonstrates the production pattern: launch QEMU -> wait for QMP socket
existence -> connect QMP -> capabilities -> subscribe to events ->
issue commands and wait for the corresponding events.

Pass criteria identical to spike-qemu.sh: 5 snapshot/restore cycles,
counter monotonic across cycles.
"""
from __future__ import annotations

import json
import os
import shutil
import signal
import socket
import subprocess
import sys
import threading
from pathlib import Path

SPIKE_DIR = Path(__file__).resolve().parent.parent
RUN_DIR = SPIKE_DIR / "run"
WORK = RUN_DIR / "work-qemu-events"
KERNEL = Path("/home/jack/projects/peios/pkm/kernel/out/bzImage")
INITRD = SPIKE_DIR / "initrd-build" / "initrd.cpio.gz"

CYCLES = 5
VCPUS = 4
MEMORY = 512
DIAL_PORT = 9999
GUEST_CID = 13

QEMU = shutil.which("qemu-system-x86_64") or "qemu-system-x86_64"

try:
    AF_VSOCK = socket.AF_VSOCK
except AttributeError:
    AF_VSOCK = 40
VMADDR_CID_ANY = 0xFFFFFFFF


def red(s):   print(f"\033[31m{s}\033[0m")
def green(s): print(f"\033[32m{s}\033[0m")
def blue(s):  print(f"\033[34m{s}\033[0m")


# ---------- QMP client (persistent connection, event-aware) ----------

class QMP:
    def __init__(self, sock_path: Path):
        self.sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        # Wait for the QMP socket file to exist. This is a filesystem
        # readiness check; we use blocking-poll because there's no
        # inotify-style notification for "QEMU created its socket".
        # In production we'd block on a select() against an inotify fd
        # but for a 50ms wait it's not worth the complexity.
        while not sock_path.exists():
            os.sched_yield()
        self.sock.connect(str(sock_path))
        self.f = self.sock.makefile("rwb", buffering=0)

        # Greeting
        greeting = json.loads(self.f.readline())
        assert "QMP" in greeting, f"unexpected greeting: {greeting}"

        # Negotiate capabilities and subscribe to all events (default).
        self._send({"execute": "qmp_capabilities"})
        self._await_return()

        # Enable the migration `events` capability so MIGRATION events
        # are actually emitted. Off by default in QEMU; without it
        # status transitions are silent and you must poll query-migrate.
        self._send({
            "execute": "migrate-set-capabilities",
            "arguments": {
                "capabilities": [{"capability": "events", "state": True}]
            }
        })
        self._await_return()

        self._events: list[dict] = []
        self._lock = threading.Lock()
        self._cv = threading.Condition(self._lock)
        self._reader = threading.Thread(target=self._read_loop, daemon=True)
        self._reader.start()

    def _send(self, msg):
        self.f.write((json.dumps(msg) + "\n").encode())
        self.f.flush()

    def _await_return(self):
        # Used only during pre-event-loop bootstrap.
        while True:
            line = self.f.readline()
            if not line:
                raise EOFError("qmp closed during bootstrap")
            msg = json.loads(line)
            if "return" in msg or "error" in msg:
                if "error" in msg:
                    raise RuntimeError(f"QMP error: {msg['error']}")
                return msg["return"]

    def _read_loop(self):
        # Background reader: queues events, signals waiters on each event.
        try:
            while True:
                line = self.f.readline()
                if not line:
                    break
                msg = json.loads(line)
                with self._cv:
                    self._events.append(msg)
                    self._cv.notify_all()
        except Exception:
            with self._cv:
                self._events.append({"_closed": True})
                self._cv.notify_all()

    def cmd(self, command: str, **args):
        with self._cv:
            mark = len(self._events)
            self._send({"execute": command, "arguments": args} if args
                       else {"execute": command})
            while True:
                self._cv.wait_for(lambda: len(self._events) > mark)
                msg = self._events[mark]; mark += 1
                if "return" in msg:
                    return msg["return"]
                if "error" in msg:
                    raise RuntimeError(f"QMP error on {command}: {msg['error']}")
                # else it was an event — push past it

    def event_mark(self) -> int:
        """Return a sequence point. wait_event(..., since=mark) ignores
        events that arrived before this point."""
        with self._cv:
            return len(self._events)

    def wait_event(self, event_name: str, predicate=None, since: int = 0):
        """Wait for a specific QMP event arriving at or after `since`.
        Without a sync point, stale events from earlier operations on
        the same QMP connection can match unintentionally."""
        with self._cv:
            i = since
            while True:
                self._cv.wait_for(lambda: i < len(self._events))
                msg = self._events[i]; i += 1
                if msg.get("event") == event_name:
                    if predicate is None or predicate(msg):
                        return msg

    def close(self):
        try: self.sock.close()
        except Exception: pass


# ---------- Vsock listener with peer-accept events ----------

class VsockListener:
    """Listens on AF_VSOCK port; signals once a peer connects, and
    streams counter values into a shared dict. Strictly event-driven."""

    def __init__(self, port: int):
        self.port = port
        self.peers_lock = threading.Lock()
        self.peers_cv = threading.Condition(self.peers_lock)
        self.peers: dict[int, list[tuple[int, int]]] = {}  # peer_id -> [(counter, uptime), ...]
        self.peer_meta: dict[int, tuple[int, int]] = {}    # peer_id -> (cid, port)
        self.peer_seq = 0
        self.srv = socket.socket(AF_VSOCK, socket.SOCK_STREAM)
        self.srv.bind((VMADDR_CID_ANY, port))
        self.srv.listen(8)
        self._accept_thread = threading.Thread(target=self._accept_loop, daemon=True)
        self._accept_thread.start()

    def _accept_loop(self):
        while True:
            try:
                c, addr = self.srv.accept()
            except OSError:
                return
            with self.peers_cv:
                self.peer_seq += 1
                pid = self.peer_seq
                self.peer_meta[pid] = addr
                self.peers[pid] = []
                self.peers_cv.notify_all()
            threading.Thread(target=self._handle, args=(c, pid), daemon=True).start()

    def _handle(self, c, pid):
        buf = b""
        while True:
            try:
                chunk = c.recv(1024)
            except OSError:
                break
            if not chunk:
                break
            buf += chunk
            while b"\n" in buf:
                line, _, buf = buf.partition(b"\n")
                line = line.decode(errors="replace").strip()
                if line.startswith("DIAL COUNTER="):
                    try:
                        ctr = int(line.split("COUNTER=")[1].split()[0])
                        upt = int(line.split("UPTIME_MS=")[1])
                    except Exception:
                        continue
                    with self.peers_cv:
                        self.peers[pid].append((ctr, upt))
                        self.peers_cv.notify_all()
        c.close()

    def wait_for_new_peer(self, after_seq: int) -> int:
        """Block until a peer with id > after_seq appears; return its id."""
        with self.peers_cv:
            self.peers_cv.wait_for(lambda: self.peer_seq > after_seq)
            return self.peer_seq

    def wait_for_counter_ge(self, peer_id: int, min_value: int) -> int:
        """Block until peer's counter has a sample >= min_value."""
        with self.peers_cv:
            self.peers_cv.wait_for(
                lambda: peer_id in self.peers
                and self.peers[peer_id]
                and self.peers[peer_id][-1][0] >= min_value
            )
            return self.peers[peer_id][-1][0]

    def last_counter_overall(self) -> int | None:
        with self.peers_lock:
            best = None
            for pid in self.peers:
                if self.peers[pid]:
                    v = self.peers[pid][-1][0]
                    if best is None or v > best:
                        best = v
            return best


# ---------- Main flow ----------

def reset_workdir():
    if WORK.exists():
        shutil.rmtree(WORK)
    WORK.mkdir(parents=True)


def start_qemu(*extra) -> tuple[subprocess.Popen, Path]:
    qmp = WORK / "qmp.sock"
    if qmp.exists(): qmp.unlink()
    cmd = [
        QEMU,
        "-M", "q35,accel=kvm",
        "-cpu", "host",
        "-smp", str(VCPUS),
        "-m", str(MEMORY),
        "-nographic",
        "-serial", f"file:{WORK / 'serial.log'}",
        "-monitor", "none",
        "-qmp", f"unix:{qmp},server=on,wait=off",
        "-device", f"vhost-vsock-pci,guest-cid={GUEST_CID}",
        "-nodefaults",
        *extra,
    ]
    p = subprocess.Popen(cmd,
                         stdout=open(WORK / "qemu.stdout.log", "ab"),
                         stderr=open(WORK / "qemu.stderr.log", "ab"))
    return p, qmp


def main():
    reset_workdir()

    # Start vsock listener (synchronous: bind+listen completes before we proceed)
    blue("==> bringing up vsock listener")
    listener = VsockListener(DIAL_PORT)

    # ---- Boot ----
    blue(f"==> boot: vCPUs={VCPUS} mem={MEMORY}M cid={GUEST_CID} cycles={CYCLES}")
    qemu, qmp_path = start_qemu(
        "-kernel", str(KERNEL),
        "-initrd", str(INITRD),
        "-append", "console=ttyS0 reboot=k panic=1 quiet",
    )
    qmp = QMP(qmp_path)

    # Wait for the agent to dial in. No timer; block on accept event.
    pid = listener.wait_for_new_peer(after_seq=0)
    # Wait for at least one counter sample so we have a baseline.
    v = listener.wait_for_counter_ge(pid, 0)
    green(f"boot: peer={pid} counter={v}")
    v_prev = v

    # ---- Snapshot/restore cycles ----
    for i in range(1, CYCLES + 1):
        snap = WORK / f"snap-{i}.bin"
        blue(f"==> cycle {i}/{CYCLES}")

        # Pause + migrate to file. Block on MIGRATION event reaching status=completed.
        qmp.cmd("stop")
        mark = qmp.event_mark()
        qmp.cmd("migrate", uri=f"file:{snap}")
        ev = qmp.wait_event(
            "MIGRATION",
            predicate=lambda e: e.get("data", {}).get("status") in ("completed", "failed"),
            since=mark,
        )
        if ev["data"]["status"] != "completed":
            red(f"cycle {i}: migration failed -- {ev}")
            return 1
        if ev["data"]["status"] != "completed":
            red(f"cycle {i}: migration failed -- {ev}")
            return 1

        # Tear down. Block on the process exiting (signal-driven via wait()).
        qmp.cmd("quit")
        qemu.wait()
        qmp.close()

        if not snap.exists() or snap.stat().st_size == 0:
            red(f"cycle {i}: snapshot empty"); return 1

        # Bring up fresh QEMU with -incoming. The receiving side waits for
        # the migration stream; once the stream completes it raises a
        # RESUME event.
        before_seq = listener.peer_seq
        qemu, qmp_path = start_qemu(
            "-kernel", str(KERNEL),
            "-initrd", str(INITRD),
            "-append", "console=ttyS0 reboot=k panic=1 quiet",
            "-incoming", f"file:{snap}",
        )
        qmp = QMP(qmp_path)

        # On the destination, wait for the inbound migration to complete
        # before issuing cont. QEMU emits MIGRATION events on both ends
        # once the `events` capability is on (handled in QMP bootstrap).
        ev = qmp.wait_event(
            "MIGRATION",
            predicate=lambda e: e.get("data", {}).get("status") in ("completed", "failed"),
        )
        if ev["data"]["status"] != "completed":
            red(f"cycle {i}: incoming migration failed -- {ev}")
            return 1

        # Now safe to cont. RESUME is the "vCPUs running again" signal.
        mark2 = qmp.event_mark()
        qmp.cmd("cont")
        qmp.wait_event("RESUME", since=mark2)

        # Wait for the agent's reconnection (a new peer accept) — no sleep.
        new_pid = listener.wait_for_new_peer(after_seq=before_seq)
        # Wait for counter to advance past the previous value.
        v = listener.wait_for_counter_ge(new_pid, v_prev + 1)
        if v < v_prev:
            red(f"cycle {i}: counter went backward ({v_prev} -> {v})"); return 1
        green(f"cycle {i}: peer={new_pid} counter={v} (delta={v - v_prev})")
        v_prev = v

    # Cleanup
    qmp.cmd("quit")
    qemu.wait()
    qmp.close()

    green(f"PASS: {CYCLES} event-driven snapshot/restore cycles, no settling sleeps")
    return 0


if __name__ == "__main__":
    sys.exit(main())
