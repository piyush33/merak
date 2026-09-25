#!/usr/bin/env python3
"""Run `merak diff` over the recent commits of a real repository and summarise
latency and what was derived.

    cargo build --release
    scripts/bench.py ~/src/immich --root server -n 60 [--out runs/]

Use a full clone: with a partial (`--filter=blob:none`) clone, git fetches
missing blobs lazily and the timings measure the network (`git backfill` fixes it).
"""

import argparse
import collections
import json
import pathlib
import statistics
import subprocess
import sys
import time

MERAK = pathlib.Path(__file__).resolve().parent.parent / "target" / "release" / "merak"


def git(repo, *args):
    return subprocess.run(["git", "-C", repo, *args], capture_output=True, text=True, check=True).stdout


def verdict(ops):
    if not ops:
        return "no change"
    if any(o["kind"] == "PURE_REFACTOR" for o in ops):
        return "pure refactor"
    behaviour = [o for o in ops if o["layer"] != "structural"]
    if not behaviour:
        return "structural only"
    if all(o["kind"] == "UNCLASSIFIED_CHANGE" for o in behaviour):
        return "unclassified only"
    return "typed behaviour"


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("repo")
    ap.add_argument("--root", default="", help="subdirectory to analyze")
    ap.add_argument("-n", type=int, default=50, help="number of commits")
    ap.add_argument("--out", help="directory for per-commit JSON transitions")
    args = ap.parse_args()

    path = [args.root] if args.root else []
    commits = git(args.repo, "log", "--no-merges", "--format=%h", f"-{args.n}", "--", *path).split()
    out = pathlib.Path(args.out) if args.out else None
    if out:
        out.mkdir(parents=True, exist_ok=True)

    times, kinds, verdicts, failures = [], collections.Counter(), collections.Counter(), []
    for h in commits:
        cmd = [str(MERAK), "diff", f"{h}~1..{h}", "--repo", args.repo, "--json"]
        if args.root:
            cmd += ["--root", args.root]
        t = time.perf_counter()
        r = subprocess.run(cmd, capture_output=True, text=True)
        times.append(time.perf_counter() - t)
        if r.returncode != 0:
            failures.append((h, r.stderr.strip().splitlines()[-1:]))
            continue
        ops = json.loads(r.stdout)["ops"]
        kinds.update(o["kind"] for o in ops)
        verdicts[verdict(ops)] += 1
        if out:
            (out / f"{h}.json").write_text(r.stdout)

    q = sorted(times)
    print(f"## merak bench: {args.repo} {args.root or '.'} ({len(commits)} commits)\n")
    print(f"latency  median {statistics.median(q):.2f}s  p90 {q[int(0.9 * (len(q) - 1))]:.2f}s  max {q[-1]:.2f}s\n")
    print("| verdict | commits |\n|---|---|")
    for v, n in verdicts.most_common():
        print(f"| {v} | {n} |")
    print("\n| op | count |\n|---|---|")
    for k, n in kinds.most_common():
        print(f"| `{k}` | {n} |")
    for h, err in failures:
        print(f"\nFAILED {h}: {err}", file=sys.stderr)
    return 1 if failures else 0


if __name__ == "__main__":
    sys.exit(main())
