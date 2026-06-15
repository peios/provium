#!/usr/bin/env python3
"""Host-side listener for CH hybrid-vsock guest-initiated connections.

CH proxies guest dial(CID=2, port=N) to a unix socket at
"<vsock_base>_<port>". This script listens there and prints every line
the guest sends, prefixed with a timestamp. Used by the spike harness
to verify whether guest->host streams survive snapshot/restore.
"""
import os
import socket
import sys
import threading
import time

BASE = sys.argv[1]            # e.g. /path/to/vsock.sock
PORT = int(sys.argv[2])       # e.g. 9999
OUT  = sys.argv[3]            # output file path

LISTEN_PATH = f"{BASE}_{PORT}"
try:
    os.unlink(LISTEN_PATH)
except FileNotFoundError:
    pass

srv = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
srv.bind(LISTEN_PATH)
srv.listen(8)
srv.settimeout(0.5)

out = open(OUT, "a", buffering=1)
out.write(f"# host-listener started, listening on {LISTEN_PATH}\n")

def handle(c, peer_id):
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
            ts = time.strftime("%H:%M:%S")
            out.write(f"[{ts} peer={peer_id}] {line.decode(errors='replace')}\n")
    c.close()
    out.write(f"# peer {peer_id} disconnected\n")

peer_id = 0
try:
    while True:
        try:
            c, _ = srv.accept()
        except socket.timeout:
            continue
        peer_id += 1
        out.write(f"# peer {peer_id} connected\n")
        threading.Thread(target=handle, args=(c, peer_id), daemon=True).start()
except KeyboardInterrupt:
    pass
finally:
    srv.close()
    try: os.unlink(LISTEN_PATH)
    except FileNotFoundError: pass
