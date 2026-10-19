"""Check newc compatibility and deterministic guest archive contents."""

import gzip
import importlib.util
import os
import stat
import subprocess
import tempfile
import unittest
from pathlib import Path

spec = importlib.util.spec_from_file_location(
    "agent_archive", Path(__file__).with_name("agent-archive.py")
)
assert spec is not None and spec.loader is not None
builder = importlib.util.module_from_spec(spec)
spec.loader.exec_module(builder)


class ArchiveTests(unittest.TestCase):
    def test_archive_is_reproducible_and_extractable(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            agent = root / "agent"
            payload = b"\x7fELF" + bytes(range(256))
            agent.write_bytes(payload)
            for location in ("init", "sbin/provium-agent"):
                with self.subTest(location=location):
                    first, second = root / "first.gz", root / "second.gz"
                    builder.archive(agent, first, location)
                    os.utime(agent, (123, 456))
                    builder.archive(agent, second, location)
                    self.assertEqual(first.read_bytes(), second.read_bytes())
                    output = root / location.replace("/", "-")
                    output.mkdir()
                    subprocess.run(
                        [
                            "cpio",
                            "--extract",
                            "--make-directories",
                            "--quiet",
                            "--no-absolute-filenames",
                        ],
                        input=gzip.decompress(first.read_bytes()),
                        cwd=output,
                        check=True,
                    )
                    installed = output / location
                    self.assertEqual(installed.read_bytes(), payload)
                    self.assertEqual(stat.S_IMODE(installed.stat().st_mode), 0o755)
                    self.assertEqual(stat.S_IMODE((output / "tmp").stat().st_mode), 0o1777)
                    files = sorted(
                        str(p.relative_to(output)) for p in output.rglob("*") if p.is_file()
                    )
                    self.assertEqual(files, [location])


if __name__ == "__main__":
    unittest.main()
