#!/usr/bin/env python3
"""Check Provium-owned sources with the pinned PSC formatters and linters."""

import os
import shutil
import subprocess
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent


def run(command: list[str]) -> None:
    subprocess.run(command, cwd=ROOT, check=True)


def tool(name: str, version: str) -> str:
    directory = os.environ.get("PEIOS_STYLE_BIN")
    executable = str(Path(directory) / name) if directory else shutil.which(name)
    if executable is None:
        raise SystemExit(f"missing {name}; run tools/prepare-style-tools.py OUTSIDE_REPO")
    lines = subprocess.check_output([executable, "--version"], text=True).splitlines()
    if version not in [line.strip() for line in lines]:
        raise SystemExit(f"unexpected {name} version: {lines}")
    return executable


def main() -> None:
    python = [
        str(p.relative_to(ROOT))
        for folder in ("tools", "tests", "build")
        for p in sorted((ROOT / folder).glob("*.py"))
    ]
    ruff = tool("ruff", "ruff 0.17.0")
    run([ruff, "check", "--no-cache", "--config", "tools/ruff.toml", *python])
    run([ruff, "format", "--check", "--no-cache", "--config", "tools/ruff.toml", *python])
    shell = ["tools/check"] + [
        str(p.relative_to(ROOT)) for p in sorted((ROOT / "scripts").glob("*.sh"))
    ]
    run([tool("shellcheck", "version: 0.11.0"), *shell])
    run([tool("shfmt", "v3.14.1"), "-d", "-i", "4", *shell])
    data = [
        str(p.relative_to(ROOT))
        for folder in (".github",)
        for p in sorted((ROOT / folder).rglob("*"))
        if p.suffix in {".json", ".yml"}
    ]
    if data:
        run([tool("prettier", "3.9.10"), "--config", "tools/prettier.json", "--check", *data])
    run(["cargo", "+1.98.1", "fmt", "--all", "--", "--check"])


if __name__ == "__main__":
    main()
