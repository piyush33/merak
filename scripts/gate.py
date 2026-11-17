#!/usr/bin/env python3
"""Benchmark gate: run `merak diff` over a pinned set of real commits and fail
when the derived operations or the rendered behaviour view differ from the
accepted baseline.

    cargo build --release
    scripts/gate.py pin miniflux            # once: freeze the commit list
    scripts/gate.py accept miniflux         # record the baseline
    scripts/gate.py check                   # every change: compare all suites
    scripts/gate.py check --detail          # show each changed op and view line
    scripts/gate.py accept                  # accept the last check's results

Suites live in bench/suites.json (public repositories; baselines are committed)
and bench/local/suites.json (private repositories; bench/local is gitignored,
because baselines contain code from those repositories). A suite is
{"repo": "~/path", "root": "server", "n": 60}. Set MERAK_BENCH_REPOS to point
the public suites at clones of their `ci_repo` in that directory (CI clones
them there with `scripts/gate.py clone`).

The gate is exact on purpose. A change to the engine that should not change
results (a refactor of the analysis) must leave every commit identical. A
change that should improve results shows up as a list of differences to read
commit by commit, then `accept` records it.
"""

import argparse
import collections
import concurrent.futures
import difflib
import hashlib
import json
import os
import pathlib
import subprocess
import sys
import time

ROOT = pathlib.Path(__file__).resolve().parent.parent
MERAK = ROOT / "target" / "release" / "merak"
PLACES = [ROOT / "bench", ROOT / "bench" / "local"]


def load_suites():
    suites = {}
    for place in PLACES:
        f = place / "suites.json"
        if f.exists():
            for name, s in json.loads(f.read_text()).items():
                s["place"] = place
                s["name"] = name
                suites[name] = s
    return suites


def repo_path(s):
    """The local clone, or, when MERAK_BENCH_REPOS is set (CI), the suite's clone
    of its public `ci_repo` in that directory."""
    base = os.environ.get("MERAK_BENCH_REPOS")
    if base and s.get("ci_repo"):
        return pathlib.Path(base) / s["name"]
    return pathlib.Path(os.path.expanduser(s["repo"]))


def git(repo, *args):
    return subprocess.run(["git", "-C", str(repo), *args], capture_output=True, text=True, check=True).stdout


def base_dir(name, s):
    return s["place"] / "baselines" / name


def last_dir(name, s):
    return s["place"] / "last" / name


def pinned(name, s):
    f = base_dir(name, s) / "commits.txt"
    if not f.exists():
        sys.exit(f"{name}: no pinned commits; run `scripts/gate.py pin {name}` first")
    return [c for c in f.read_text().split() if c]


def normalise(op):
    """The parts of an op that carry meaning. Ids and evidence lines are left out:
    ids are hashes of the rest, and line numbers say where, not what."""
    keep = {k: op[k] for k in ("kind", "layer", "subject", "before", "after", "note", "affects", "origin", "confidence") if k in op and op[k] not in (None, "", [])}
    return json.dumps(keep, sort_keys=True, ensure_ascii=False)


def run_one(repo, root, sha):
    base = [str(MERAK), "diff", f"{sha}~1..{sha}", "--repo", str(repo)]
    if root:
        base += ["--root", root]
    t = time.perf_counter()
    j = subprocess.run(base + ["--json"], capture_output=True, text=True)
    elapsed = time.perf_counter() - t
    if j.returncode != 0:
        return {"error": (j.stderr.strip().splitlines() or ["exit %d" % j.returncode])[-1]}, elapsed
    v = subprocess.run(base, capture_output=True, text=True)
    if v.returncode != 0:
        return {"error": "view: " + (v.stderr.strip().splitlines() or ["exit %d" % v.returncode])[-1]}, elapsed
    ops = sorted(normalise(o) for o in json.loads(j.stdout)["ops"])
    return {"ops": ops, "view": v.stdout}, elapsed


def run_suite(name, s, jobs):
    repo, commits = repo_path(s), pinned(name, s)
    results, times = {}, []
    with concurrent.futures.ThreadPoolExecutor(max_workers=jobs) as ex:
        futs = {ex.submit(run_one, repo, s.get("root", ""), c): c for c in commits}
        for f in concurrent.futures.as_completed(futs):
            r, t = f.result()
            results[futs[f]] = r
            times.append(t)
    out = last_dir(name, s)
    out.mkdir(parents=True, exist_ok=True)
    for c, r in results.items():
        (out / f"{c}.json").write_text(json.dumps(r, indent=1, ensure_ascii=False) + "\n")
    return results, sorted(times)


def kind_of(op_json):
    return json.loads(op_json)["kind"]


def compare(name, s, results, detail):
    base = base_dir(name, s)
    crashed, op_changed, view_changed, new = [], [], [], []
    delta = collections.Counter()
    lines = []
    for c in pinned(name, s):
        r = results[c]
        if "error" in r:
            crashed.append(c)
            lines.append(f"  CRASH {c[:10]}: {r['error']}")
            continue
        f = base / f"{c}.json"
        if not f.exists():
            new.append(c)
            continue
        b = json.loads(f.read_text())
        if "error" in b:
            op_changed.append(c)
            lines.append(f"  {c[:10]}: baseline crashed ({b['error']}), now runs")
            continue
        before, after = collections.Counter(b["ops"]), collections.Counter(r["ops"])
        gone, came = before - after, after - before
        if gone or came:
            op_changed.append(c)
            for o in gone.elements():
                delta[kind_of(o)] -= 1
            for o in came.elements():
                delta[kind_of(o)] += 1
            lines.append(f"  {c[:10]}: ops -{sum(gone.values())} +{sum(came.values())}")
            if detail:
                lines += [f"      - {o}" for o in sorted(gone.elements())]
                lines += [f"      + {o}" for o in sorted(came.elements())]
        if b["view"] != r["view"]:
            view_changed.append(c)
            if not (gone or came):
                lines.append(f"  {c[:10]}: view only")
            if detail:
                d = difflib.unified_diff(b["view"].splitlines(), r["view"].splitlines(), lineterm="", n=0)
                lines += ["      " + l for l in list(d)[2:]]
    return {"crashed": crashed, "ops": op_changed, "view": view_changed, "new": new, "delta": delta, "lines": lines}


def cmd_pin(args, suites):
    for name in args.suites:
        s = suites[name]
        repo = repo_path(s)
        path = [s["root"]] if s.get("root") else []
        commits = git(repo, "log", "--no-merges", "--format=%H", f"-{s.get('n', 50)}", "--", *path).split()
        d = base_dir(name, s)
        d.mkdir(parents=True, exist_ok=True)
        f = d / "commits.txt"
        if f.exists() and not args.force:
            sys.exit(f"{name}: already pinned ({f}); pass --force to re-pin")
        f.write_text("\n".join(commits) + "\n")
        print(f"{name}: pinned {len(commits)} commits from {repo}")


def cmd_clone(args, suites):
    """Fetch just the pinned commits and their parents of each public suite into
    MERAK_BENCH_REPOS, which keeps CI clones small (immich is large)."""
    base = os.environ.get("MERAK_BENCH_REPOS")
    if not base:
        sys.exit("set MERAK_BENCH_REPOS to the directory to clone into")
    for name in args.suites or sorted(n for n, s in suites.items() if s.get("ci_repo")):
        s = suites[name]
        dest = pathlib.Path(base) / name
        dest.mkdir(parents=True, exist_ok=True)
        subprocess.run(["git", "init", "-q", str(dest)], check=True)
        subprocess.run(["git", "-C", str(dest), "remote", "add", "origin", s["ci_repo"]], capture_output=True)
        subprocess.run(["git", "-C", str(dest), "fetch", "-q", "--depth=2", "origin", *pinned(name, s)], check=True)
        print(f"{name}: fetched {len(pinned(name, s))} pinned commits into {dest}")
    return 0


def cmd_check(args, suites):
    names = args.suites or sorted(suites)
    failed = False
    for name in names:
        s = suites[name]
        if not repo_path(s).exists():
            print(f"## {name}: SKIPPED (repository not found at {repo_path(s)})\n")
            continue
        results, times = run_suite(name, s, args.jobs)
        c = compare(name, s, results, args.detail)
        med = times[len(times) // 2] if times else 0
        ok = not (c["crashed"] or c["ops"] or c["view"] or c["new"])
        failed |= not ok
        status = "identical" if ok else "DIFFERENT"
        print(f"## {name}: {status}  ({len(results)} commits, median {med:.2f}s, max {times[-1]:.2f}s)")
        print(f"   crashed {len(c['crashed'])} · ops changed {len(c['ops'])} · view changed {len(c['view'])} · no baseline {len(c['new'])}")
        if c["delta"]:
            print("   op counts: " + ", ".join(f"{k} {v:+d}" for k, v in sorted(c["delta"].items()) if v))
        for l in c["lines"]:
            print(l)
        print()
    if failed:
        print("Gate FAILED. Read the differences; if they are intended, run `scripts/gate.py accept`.")
    return 1 if failed else 0


def cmd_accept(args, suites):
    names = args.suites or sorted(suites)
    for name in names:
        s = suites[name]
        last = last_dir(name, s)
        if args.rerun or not last.exists():
            if not repo_path(s).exists():
                print(f"{name}: skipped (repository not found)")
                continue
            run_suite(name, s, args.jobs)
        base = base_dir(name, s)
        n = 0
        for c in pinned(name, s):
            src = last / f"{c}.json"
            if src.exists():
                (base / f"{c}.json").write_text(src.read_text())
                n += 1
        print(f"{name}: baseline recorded for {n} commits")
    return 0


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    sub = ap.add_subparsers(dest="cmd", required=True)
    p = sub.add_parser("pin", help="freeze the commit list of suites")
    p.add_argument("suites", nargs="+")
    p.add_argument("--force", action="store_true")
    p = sub.add_parser("clone", help="CI: fetch the pinned commits of public suites into MERAK_BENCH_REPOS")
    p.add_argument("suites", nargs="*")
    for name in ("check", "accept"):
        p = sub.add_parser(name)
        p.add_argument("suites", nargs="*")
        p.add_argument("-j", "--jobs", type=int, default=os.cpu_count() or 4)
        if name == "check":
            p.add_argument("--detail", action="store_true", help="print every changed op and view line")
        else:
            p.add_argument("--rerun", action="store_true", help="run again instead of using the last check")
    args = ap.parse_args()
    if args.cmd in ("check", "accept") and not MERAK.exists():
        sys.exit(f"{MERAK} not found; run `cargo build --release` first")
    suites = load_suites()
    unknown = [n for n in getattr(args, "suites", []) if n not in suites]
    if unknown:
        sys.exit(f"unknown suite(s): {', '.join(unknown)}; known: {', '.join(sorted(suites))}")
    return {"pin": cmd_pin, "clone": cmd_clone, "check": cmd_check, "accept": cmd_accept}[args.cmd](args, suites) or 0


if __name__ == "__main__":
    sys.exit(main())
