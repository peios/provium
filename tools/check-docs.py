#!/usr/bin/env python3
"""Lint and render Provium Learn pages and manuals without retaining output."""

import os
import shutil
import subprocess
import tempfile
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent


def main() -> None:
    trail = os.environ.get("TRAIL", "trail")
    with tempfile.TemporaryDirectory(prefix="provium-docs-") as work:
        source = Path(work) / "source"
        shutil.copytree(
            ROOT / "docs/learn", source, ignore=shutil.ignore_patterns("dist", ".trail-cache")
        )
        subprocess.run([trail, "lint", str(source)], check=True)
        subprocess.run(
            [
                trail,
                "build",
                str(source),
                "--strict",
                "--no-cache",
                "--out",
                str(Path(work) / "learn"),
            ],
            check=True,
        )
        for page in sorted((ROOT / "docs/man").glob("*")):
            result = subprocess.run(
                ["groff", "-man", "-Tutf8", str(page)],
                check=True,
                stdout=subprocess.DEVNULL,
                stderr=subprocess.PIPE,
                text=True,
            )
            if result.stderr:
                raise RuntimeError(f"{page.name}: {result.stderr}")


if __name__ == "__main__":
    main()
