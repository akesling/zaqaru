"""Exercise cache decisions with a small build fixture, without Docker."""

import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import unittest


class DemoCache(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.repo = Path(self.tmp.name)
        for name in ("web", "crates/cpu/src", "demo/hello-django", "tools/microbench"):
            (self.repo / name).mkdir(parents=True)
        shutil.copy(Path(__file__).with_name("demo_build.py"), self.repo / "web/demo_build.py")
        for name in ("Cargo.toml", "Cargo.lock", "tools/microbench/Dockerfile",
                     "web/preboot.mjs", "web/check-demo.mjs", "web/snapshot.js",
                     "crates/cpu/src/lib.rs", "demo/hello-django/Dockerfile"):
            (self.repo / name).write_text("fixture")
        (self.repo / "web/demo-build.sh").write_text('''set -eu
repo=$(cd "$(dirname "$0")/.." && pwd)
echo build >> "$repo/count"
if [ "${FAIL_BUILD:-0}" = 1 ]; then exit 1; fi
echo module > "$1/hello-django.wasm"
echo snapshot > "$1/hello-django.snapshot"
echo decoder > "$repo/web/brotli.wasm"
''')

    def run_build(self, *args, fail=False):
        return subprocess.run([sys.executable, str(self.repo / "web/demo_build.py"), *args],
                              env={**os.environ, "FAIL_BUILD": "1" if fail else "0"},
                              capture_output=True, text=True)

    def count(self):
        return len((self.repo / "count").read_text().splitlines())

    def test_reuse_and_force(self):
        self.assertEqual(self.run_build().returncode, 0)
        # A cache hit must not invoke even a builder configured to fail.
        result = self.run_build(fail=True)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("Using cached validated demo", result.stdout)
        self.assertEqual(self.count(), 1)
        self.assertEqual(self.run_build("--rebuild").returncode, 0)
        self.assertEqual(self.count(), 2)

    def test_changed_input_and_corrupt_or_missing_artifact(self):
        self.assertEqual(self.run_build().returncode, 0)
        source = self.repo / "crates/cpu/src/lib.rs"
        before = source.stat()
        source.write_text("changed")
        os.utime(source, ns=(before.st_atime_ns, before.st_mtime_ns))
        self.assertEqual(self.run_build().returncode, 0)
        self.assertEqual(self.count(), 2)
        (self.repo / "web/demo/hello-django.snapshot").write_text("corrupt")
        self.assertEqual(self.run_build().returncode, 0)
        self.assertEqual(self.count(), 3)
        (self.repo / "web/brotli.wasm").unlink()
        self.assertEqual(self.run_build().returncode, 0)
        self.assertEqual(self.count(), 4)

    def test_failed_rebuild_cannot_be_reused(self):
        self.assertEqual(self.run_build().returncode, 0)
        self.assertNotEqual(self.run_build("--rebuild", fail=True).returncode, 0)
        self.assertFalse((self.repo / "web/demo/build.json").exists())
        self.assertEqual(self.run_build().returncode, 0)
        self.assertEqual(self.count(), 3)


if __name__ == "__main__":
    unittest.main()
