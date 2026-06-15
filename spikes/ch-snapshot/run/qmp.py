#!/usr/bin/env python3
"""Minimal QMP client over a unix socket. Reads negotiation, sends
qmp_capabilities, then runs the requested command and prints the reply.

Usage:
  qmp.py <sock> <command> [<arg-json>]

Examples:
  qmp.py /tmp/qmp.sock query-status
  qmp.py /tmp/qmp.sock stop
  qmp.py /tmp/qmp.sock migrate '{"uri":"exec:cat > /tmp/snap.bin"}'
"""
import json
import socket
import sys
import time

SOCK = sys.argv[1]
CMD = sys.argv[2]
ARGS = json.loads(sys.argv[3]) if len(sys.argv) > 3 else {}

s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
s.settimeout(10.0)
s.connect(SOCK)
f = s.makefile("rwb", buffering=0)

# Read greeting
greeting = f.readline()
# Negotiate caps
f.write(b'{"execute":"qmp_capabilities"}\n')
f.flush()
f.readline()

req = {"execute": CMD}
if ARGS:
    req["arguments"] = ARGS
f.write((json.dumps(req) + "\n").encode())
f.flush()

# Read responses; the command may emit events before the return
deadline = time.monotonic() + 8.0
while time.monotonic() < deadline:
    line = f.readline()
    if not line:
        break
    try:
        msg = json.loads(line)
    except json.JSONDecodeError:
        continue
    if "return" in msg or "error" in msg:
        print(json.dumps(msg))
        break
s.close()
