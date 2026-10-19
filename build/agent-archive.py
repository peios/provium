#!/usr/bin/env python3
"""Write deterministic Linux newc initramfs archives without host cpio state."""

import gzip
import stat
import sys
from pathlib import Path
from typing import BinaryIO


def entry(output: BinaryIO, name: str, mode: int, data: bytes, inode: int) -> None:
    """Write one root-owned newc entry, aligned to the format's four-byte boundary."""
    encoded = name.encode("ascii") + b"\0"
    fields = (
        inode,
        mode,
        0,
        0,
        2 if stat.S_ISDIR(mode) else 1,
        0,
        len(data),
        0,
        0,
        0,
        0,
        len(encoded),
        0,
    )
    header = b"070701" + b"".join(f"{field:08X}".encode("ascii") for field in fields)
    output.write(header + encoded)
    output.write(b"\0" * (-(len(header) + len(encoded)) % 4))
    output.write(data)
    output.write(b"\0" * (-len(data) % 4))


def archive(agent: Path, destination: Path, location: str) -> None:
    """Install only the agent and its mountpoint directories, with zero timestamps."""
    if location not in {"sbin/provium-agent", "init"}:
        raise ValueError("unsupported agent location")
    payload = agent.read_bytes()
    if not payload.startswith(b"\x7fELF"):
        raise ValueError("the agent must be an ELF executable")
    with destination.open("wb") as raw:
        with gzip.GzipFile(filename="", mode="wb", fileobj=raw, mtime=0) as output:
            directories = [".", "dev", "proc", "sbin", "sys", "tmp"]
            for inode, name in enumerate(directories, start=1):
                mode = 0o1777 if name == "tmp" else 0o755
                entry(output, name, stat.S_IFDIR | mode, b"", inode)
            entry(output, location, stat.S_IFREG | 0o755, payload, len(directories) + 1)
            entry(output, "TRAILER!!!", 0, b"", 0)


if __name__ == "__main__":
    archive(Path(sys.argv[1]), Path(sys.argv[2]), sys.argv[3])
