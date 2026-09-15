"""Cache the complete validated demo against local inputs and artifact hashes."""

import argparse
import fcntl
import hashlib
import json
from pathlib import Path
import subprocess
import sys


def digest(path):
    value = hashlib.sha256()
    with path.open("rb") as source:
        for chunk in iter(lambda: source.read(1024 * 1024), b""):
            value.update(chunk)
    return value.hexdigest()


def inputs(repo):
    paths = {repo / name for name in (
        "Cargo.toml", "Cargo.lock", "tools/microbench/Dockerfile",
        "web/demo-build.sh", "web/demo_build.py", "web/preboot.mjs",
        "web/check-demo.mjs",
    )}
    for directory in ("crates", "demo/hello-django"):
        paths.update(path for path in (repo / directory).rglob("*")
                     if path.is_file() and "target" not in path.parts)
    paths.update((repo / "web").glob("*.js"))
    return {str(path.relative_to(repo)): digest(path) for path in sorted(paths)}


def artifacts(repo, out):
    return {"module": out / "hello-django.wasm",
            "snapshot": out / "hello-django.snapshot",
            "decoder": repo / "web/brotli.wasm"}


def current(repo, out, fingerprint):
    try:
        record = json.loads((out / "build.json").read_text())
        return (record["inputs"] == fingerprint and record["artifacts"] ==
                {name: digest(path) for name, path in artifacts(repo, out).items()})
    except (OSError, ValueError, KeyError, TypeError):
        return False


def main():
    repo = Path(__file__).resolve().parent.parent
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("out", nargs="?", type=Path, default=repo / "web/demo")
    parser.add_argument("--rebuild", action="store_true", help="force a build even when cached")
    args = parser.parse_args()
    out = args.out.resolve()
    out.mkdir(parents=True, exist_ok=True)
    with (out / ".build.lock").open("w") as lock:
        print("demo-progress: Checking cached demo artifacts", flush=True)
        fcntl.flock(lock, fcntl.LOCK_EX)
        fingerprint = inputs(repo)
        if not args.rebuild and current(repo, out, fingerprint):
            print("Using cached validated demo; skipping build, boot and compression.", flush=True)
            return 0
        # Failed or interrupted builds must never leave a valid cache marker.
        (out / "build.json").unlink(missing_ok=True)
        result = subprocess.run(["sh", str(repo / "web/demo-build.sh"), str(out)])
        if result.returncode:
            return 1
        if inputs(repo) != fingerprint:
            print("Build inputs changed during the build; rerun to create a valid cache.", file=sys.stderr)
            return 1
        record = {"inputs": fingerprint,
                  "artifacts": {name: digest(path) for name, path in artifacts(repo, out).items()}}
        temporary = out / "build.json.tmp"
        temporary.write_text(json.dumps(record, indent=2) + "\n")
        temporary.replace(out / "build.json")
        print("Validated demo cached, including its compressed snapshot.", flush=True)
        return 0


if __name__ == "__main__":
    sys.exit(main())
