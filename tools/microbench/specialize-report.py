"""Record enough context to reproduce the manual specialization experiment."""
import csv
import hashlib
import json
import platform
import statistics
import subprocess
import sys
from pathlib import Path

root = Path("benchmark-results")
rows = list(csv.DictReader((root / "specialize.csv").open()))
names = ("arithmetic", "checked_memory", "flags_widths", "multiply_shift")
results = {}
for name in names:
    samples = [row for row in rows if row["fixture"] == name]
    assert len(samples) == 5, f"Incomplete measurements for {name}"
    ratios = [float(s["baseline_seconds"]) / float(s["specialized_seconds"]) for s in samples]
    results[name] = {"median_speedup": statistics.median(ratios), "samples": samples}

def command(*args):
    return subprocess.check_output(args, text=True).strip()

modules = ["specialized.wasm", "specialize-reference.wasm", "../target/wasm32-unknown-unknown/release/examples/specialize.wasm"]
report = {
    "scope": "Four single-trace fixtures; NOT general OCI or end-to-end performance",
    "baseline": "Actual faf6988 CPU/x87 sources, same fixture and state checks",
    "commit": command("git", "rev-parse", "HEAD"),
    "dirty": bool(command("git", "status", "--porcelain")),
    "platform": platform.platform(),
    "machine": platform.machine(),
    "core": 0,
    "rustc": command("rustc", "--version"),
    "weval": "0.5.0",
    "iterations": int(sys.argv[1]),
    "specialization_seconds_rounded": int(sys.argv[2]),
    "validation_cases": 560,
    "module_sha256": {name: hashlib.sha256((root / name).read_bytes()).hexdigest() for name in modules},
    "results": results,
}
(root / "specialize.json").write_text(json.dumps(report, indent=2) + "\n")
for name, result in results.items():
    print(f"{name}: {result['median_speedup']:.3f}x median paired speedup")
