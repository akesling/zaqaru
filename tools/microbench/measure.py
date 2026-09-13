# /// script
# requires-python = ">=3.11"
# dependencies = []
# ///
"""Compare Linux native, Wasm interpreter, and Wasm bytecode execution.

Interleave S/2S and execution modes; subtract fixed costs. Keep every raw
sample. Build fresh modules in a unique directory for every invocation.
"""
import argparse
import hashlib
import json
import os
import platform
import re
import subprocess
import tempfile
import time
from pathlib import Path

REPO = Path(__file__).resolve().parents[2]
SCALES = {
    "mixed": 200_000, "nops": 1_000_000, "regmov": 1_000_000,
    "regadd": 1_000_000, "loads": 1_000_000, "stores": 1_000_000,
    "alu": 7_000_000, "memory_sequential": 2, "memory_random": 8_000_000,
    "calls": 150, "branches": 1_500_000, "string": 2_500,
    "float": 1_000_000, "syscalls": 40_000, "alloc": 200_000,
}
MODES = ("native", "interpreter", "bytecode")


def metadata():
    def git(*args):
        return subprocess.check_output(["git", "-C", str(REPO), *args], text=True).strip()
    cpu = platform.processor()
    if Path("/proc/cpuinfo").exists():
        cpu = next((line.split(":", 1)[1].strip() for line in Path("/proc/cpuinfo").read_text().splitlines()
                    if line.startswith("model name")), cpu)
    return {"commit": git("rev-parse", "HEAD"), "dirty": bool(git("status", "--porcelain")),
            "platform": platform.platform(), "machine": platform.machine(),
            "python": platform.python_version(), "cpu": cpu,
            "timestamp": time.time(), "environment": os.environ.get("ZAQARU_BENCH_ENV", "host")}


def parse_sample(stdout, stderr, name, elapsed, mode):
    answers = [line for line in stdout.splitlines() if line.startswith(name + " ")]
    if len(answers) != 1:
        raise ValueError(f"{mode}/{name}: missing or ambiguous checksum")
    row = {"total": elapsed, "answer": answers[0]}
    if mode != "native":
        match = re.search(r"(\d+) instructions in ", stderr)
        if not match:
            raise ValueError(f"{mode}/{name}: missing instruction count")
        row["retired"] = int(match[1])
        match = re.search(r"of module in ([\d.]+)s", stderr)
        if match:
            row["compile"] = float(match[1])
    return row


def summarize(low, high, scale):
    seconds = min(r["total"] for r in high) - min(r["total"] for r in low)
    if seconds <= 0:
        raise ValueError("Nonpositive timing difference; increase scale or repeats")
    result = {"scale": scale, "seconds": seconds, "per_unit": seconds / scale,
              "samples": {"low": low, "high": high}}
    if "retired" in low[0]:
        counts = [{r["retired"] for r in rows} for rows in (low, high)]
        if any(len(c) != 1 for c in counts):
            raise ValueError("Instruction counts changed between repetitions")
        result["retired"] = high[0]["retired"] - low[0]["retired"]
    return result


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--kernels", nargs="+", choices=SCALES, default=list(SCALES))
    parser.add_argument("--repeats", type=int, default=5)
    parser.add_argument("--core", type=int, default=None)
    parser.add_argument("--scale-factor", type=float, default=1.0)
    parser.add_argument("--output", type=Path, default=Path("/tmp/microbench/results.json"))
    args = parser.parse_args()
    if args.repeats < 2 or args.scale_factor <= 0:
        parser.error("repeats must be >= 2 and scale-factor must be positive")
    if platform.system() != "Linux" or platform.machine() not in ("x86_64", "AMD64"):
        parser.error("requires x86-64 Linux; use tools/microbench/linux.sh on macOS")
    allowed = sorted(os.sched_getaffinity(0))
    core = args.core if args.core is not None else allowed[0]
    if core not in allowed:
        parser.error(f"core {core} is unavailable; allowed: {allowed}")
    binary = REPO / "target/release/zaqaru"
    subprocess.run(["cargo", "build", "--locked", "--release", "-p", "zaqaru"], cwd=REPO, check=True)
    info = metadata()
    info.update({"core": core, "repeats": args.repeats,
                 "binary_sha256": hashlib.sha256(binary.read_bytes()).hexdigest(),
                 "rustc": subprocess.check_output(["rustc", "--version"], text=True).strip(),
                 "compiler": subprocess.check_output(["gcc", "--version"], text=True).splitlines()[0]})
    results = {"schema": 1, "metadata": info, "kernels": {}}
    with tempfile.TemporaryDirectory(prefix="zaqaru-bench-") as tmp:
        work = Path(tmp)
        root = work / "root"
        root.mkdir()
        subprocess.run(["gcc", "-O2", "-static", "-o", str(root / "init"),
                        str(REPO / "tools/microbench/bench.c"), "-lm"], check=True)
        modules = {}

        def bake(name, scale):
            key = (name, scale)
            if key not in modules:
                path = work / f"{name}.{scale}.wasm"
                subprocess.run([str(binary), "bake", str(root), "-o", str(path),
                                "--", "/init", name, str(scale)], check=True, capture_output=True)
                modules[key] = path
            return modules[key]

        def run(mode, name, scale):
            command = ([str(root / "init"), name, str(scale)] if mode == "native" else
                       [str(binary), "run", str(bake(name, scale)), "--seed", "1"] +
                       (["--no-bytecode"] if mode == "interpreter" else []))
            start = time.perf_counter()
            done = subprocess.run(["taskset", "-c", str(core), *command],
                                  capture_output=True, text=True, check=True, timeout=600)
            return parse_sample(done.stdout, done.stderr, name, time.perf_counter() - start, mode)

        results["fixed"] = {mode: [run(mode, "noop", 0) for _ in range(args.repeats)] for mode in MODES}
        for name in args.kernels:
            scale = max(1, int(SCALES[name] * args.scale_factor))
            native_scale = scale
            while run("native", name, native_scale)["total"] < 0.3:
                native_scale *= 2
                if native_scale > 1 << 40:
                    raise ValueError(f"{name}: native calibration failed")
            expected = {s: run("native", name, s)["answer"] for s in (scale, scale * 2)}
            # Baking must happen outside timing, including the first sample.
            for s in expected:
                bake(name, s)
            samples = {mode: [[], []] for mode in MODES}
            for repeat in range(args.repeats):
                modes = MODES[repeat % 3:] + MODES[:repeat % 3]
                for factor in (1, 2):
                    for mode in modes:
                        at = (native_scale if mode == "native" else scale) * factor
                        row = run(mode, name, at)
                        samples[mode][factor - 1].append(row)
                        if mode != "native" and row["answer"] != expected[at]:
                            raise ValueError(f"{mode}/{name}/{at}: checksum mismatch")
            rows = {mode: summarize(*samples[mode], native_scale if mode == "native" else scale)
                    for mode in MODES}
            for mode in MODES:
                for group in samples[mode]:
                    if len({r["answer"] for r in group}) != 1:
                        raise ValueError(f"{mode}/{name}: unstable checksum")
            if rows["interpreter"]["retired"] != rows["bytecode"]["retired"]:
                raise ValueError(f"{name}: engine retirement counts differ")
            results["kernels"][name] = rows
            gain = rows["interpreter"]["per_unit"] / rows["bytecode"]["per_unit"]
            print(f"{name:20s} bytecode {rows['bytecode']['per_unit'] * 1e9:10.1f} ns/unit; "
                  f"{gain:.2f}x interpreter", flush=True)
    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_text(json.dumps(results, indent=2) + "\n")
    print(f"Written to {args.output}")


if __name__ == "__main__":
    main()
