"""Compare compatible microbenchmark results; fail above a chosen regression limit."""
import argparse
import json


def compare(before, after, threshold):
    for key in ("platform", "machine", "cpu", "environment", "core", "compiler"):
        if before["metadata"][key] != after["metadata"][key]:
            raise ValueError(f"Incompatible environment: {key}")
    if before["schema"] != after["schema"] or before["kernels"].keys() != after["kernels"].keys():
        raise ValueError("Schema or kernel selection differs")
    failures = []
    for kernel, modes in before["kernels"].items():
        for mode in ("interpreter", "bytecode"):
            old, new = modes[mode], after["kernels"][kernel][mode]
            if old["scale"] != new["scale"] or old["retired"] != new["retired"]:
                raise ValueError(f"{kernel}/{mode}: workload differs")
            delta = new["per_unit"] / old["per_unit"] - 1
            print(f"{kernel:20s} {mode:12s} {delta:+.1%}")
            if delta > threshold:
                failures.append(f"{kernel}/{mode}")
    return failures


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("before")
    parser.add_argument("after")
    parser.add_argument("--threshold", type=float, default=0.10)
    args = parser.parse_args()
    if args.threshold < 0:
        parser.error("threshold must be nonnegative")
    with open(args.before) as f:
        before = json.load(f)
    with open(args.after) as f:
        after = json.load(f)
    failures = compare(before, after, args.threshold)
    if failures:
        raise SystemExit("Regressions: " + ", ".join(failures))


if __name__ == "__main__":
    main()
