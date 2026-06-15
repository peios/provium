#!/usr/bin/env python3
"""Query the spike-agent over CH's hybrid-vsock unix socket.

Cloud-hypervisor exposes vsock as a unix socket; clients connect by sending
"CONNECT <port>\\n" and reading an "OK <peer_port>\\n" response, after which
the stream is the raw vsock stream.
"""
import socket
import sys
import time

SOCK_PATH = sys.argv[1]
PORT = int(sys.argv[2]) if len(sys.argv) > 2 else 1234
TIMEOUT = float(sys.argv[3]) if len(sys.argv) > 3 else 5.0

deadline = time.monotonic() + TIMEOUT
last_err = None
while time.monotonic() < deadline:
    try:
        s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        s.settimeout(0.5)
        s.connect(SOCK_PATH)
        s.sendall(f"CONNECT {PORT}\n".encode())
        buf = b""
        while b"\n" not in buf:
            chunk = s.recv(64)
            if not chunk:
                break
            buf += chunk
        head, _, rest = buf.partition(b"\n")
        if not head.startswith(b"OK "):
            raise RuntimeError(f"expected OK, got {head!r}")
        # Read the rest until close
        data = rest
        while True:
            try:
                chunk = s.recv(256)
            except socket.timeout:
                break
            if not chunk:
                break
            data += chunk
        s.close()
        line = data.decode(errors="replace").strip()
        print(line)
        sys.exit(0)
    except (FileNotFoundError, ConnectionRefusedError, OSError, RuntimeError) as e:
        last_err = e
        time.sleep(0.1)

print(f"vsock-query: gave up after {TIMEOUT}s, last error: {last_err}", file=sys.stderr)
sys.exit(1)
