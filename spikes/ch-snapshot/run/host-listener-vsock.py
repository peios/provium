#!/usr/bin/env python3
"""Host-side AF_VSOCK listener (for QEMU vhost-vsock, where the host has
CID 2 in the kernel and processes can bind real AF_VSOCK sockets).

Unlike the CH hybrid-vsock listener (which binds a unix socket at
<base>_<port>), this binds a real AF_VSOCK socket on (CID_ANY, port).
"""
import socket
import sys
import threading
import time

PORT = int(sys.argv[1])
OUT  = sys.argv[2]

# AF_VSOCK = 40 on Linux
try:
    AF_VSOCK = socket.AF_VSOCK
except AttributeError:
    AF_VSOCK = 40

# CID constants
VMADDR_CID_ANY = 0xFFFFFFFF
VMADDR_CID_HOST = 2

srv = socket.socket(AF_VSOCK, socket.SOCK_STREAM)
srv.bind((VMADDR_CID_ANY, PORT))
srv.listen(8)
srv.settimeout(0.5)

out = open(OUT, "a", buffering=1)
out.write(f"# host-listener-vsock started, listening on AF_VSOCK port {PORT}\n")

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
            c, addr = srv.accept()
        except socket.timeout:
            continue
        peer_id += 1
        out.write(f"# peer {peer_id} connected from cid={addr[0]} port={addr[1]}\n")
        threading.Thread(target=handle, args=(c, peer_id), daemon=True).start()
except KeyboardInterrupt:
    pass
finally:
    srv.close()
