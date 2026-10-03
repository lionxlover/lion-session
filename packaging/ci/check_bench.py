#!/usr/bin/env python3
"""lion-bench regression gate (spec 02 §7): fail on >10% regression.

Usage: check_bench.py <current.json> [baseline.json]
current.json: one JSON object per line (cargo bench -- --json output).
baseline.json: same shape, checked in at packaging/ci/bench-baseline.json.
Without a baseline, this run becomes the recorded baseline (write to a
temp path and commit it).
"""
import json
import sys

REGRESSION = 1.10


def load(path):
    metrics = {}
    with open(path) as f:
        for line in f:
            line = line.strip()
            if not line.startswith("{"):
                continue
            obj = json.loads(line)
            metrics[obj["metric"]] = float(obj["value"])
    return metrics


def main():
    current = load(sys.argv[1])
    baseline_path = sys.argv[2] if len(sys.argv) > 2 else None
    if not baseline_path:
        print("no baseline; recording current as baseline")
        print(json.dumps(current, indent=2))
        return 0
    baseline = load(baseline_path)
    failed = []
    for metric, value in sorted(current.items()):
        base = baseline.get(metric)
        if base is None:
            continue
        # lower-is-better for all current metrics (us/ns/kb)
        if value > base * REGRESSION:
            failed.append((metric, base, value))
    for metric, base, value in failed:
        print(f"REGRESSION {metric}: {base} -> {value} (+"
              f"{(value / base - 1) * 100:.1f}%)")
    if failed:
        return 1
    print(f"bench OK: {len(current)} metrics within {int((REGRESSION - 1) * 100)}%")
    return 0


if __name__ == "__main__":
    sys.exit(main())
