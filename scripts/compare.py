#!/usr/bin/env python3
"""Run Merak, sem and inspect over the same commits and keep their outputs.

    scripts/compare.py run   ~/src/immich --root server -n 60 --out runs/compare \\
        --sem ~/src/sem/crates/target/release/sem --inspect ~/src/inspect/target/release/inspect
    scripts/compare.py score runs/compare --labels scripts/labels/immich.json

Only local, deterministic modes are used: `sem diff --format json` (cloud consent off,
SEM_LOCAL=1) and `inspect diff --format json` (triage only, no LLM). All three are
scoped to the same code: files under --root that Merak analyses (non-test TS/JS).
"""

import argparse
import collections
import json
import os
import pathlib
import re
import statistics
import subprocess
import sys
import time

MERAK = pathlib.Path(__file__).resolve().parent.parent / "target" / "release" / "merak"
ENV = {**os.environ, "SEM_LOCAL": "1", "SEM_NO_TELEMETRY": "1", "DO_NOT_TRACK": "1"}
TEST = re.compile(r"(\.spec\.|\.test\.|(^|/)test/|__tests__|__mocks__)")
CODE = re.compile(r"\.(ts|tsx|js|jsx|mts|cts)$")


def in_scope(path, root):
    return path.startswith(root + "/" if root else "") and CODE.search(path) and not TEST.search(path) and not path.endswith(".d.ts")


def git(repo, *args):
    return subprocess.run(["git", "-C", repo, *args], capture_output=True, text=True, check=True).stdout


def timed(cmd, cwd=None):
    t = time.perf_counter()
    r = subprocess.run(cmd, capture_output=True, text=True, cwd=cwd, env=ENV)
    return r, time.perf_counter() - t


def run(args):
    out = pathlib.Path(args.out)
    out.mkdir(parents=True, exist_ok=True)
    path = [args.root] if args.root else []
    commits = git(args.repo, "log", "--no-merges", "--format=%h", f"-{args.n}", "--", *path).split()
    (out / "commits.json").write_text(json.dumps(commits))
    for h in commits:
        rec = {"commit": h, "subject": git(args.repo, "log", "-1", "--format=%s", h).strip()}
        cmd = [str(MERAK), "diff", f"{h}~1..{h}", "--repo", args.repo, "--json"] + (["--root", args.root] if args.root else [])
        r, t = timed(cmd)
        rec["merak"] = {"seconds": t, "ok": r.returncode == 0, "ops": json.loads(r.stdout)["ops"] if r.returncode == 0 else None}
        r, t = timed([args.sem, "diff", f"{h}~1..{h}", "--format", "json", "--", *path], cwd=args.repo)
        changes = json.loads(r.stdout)["changes"] if r.returncode == 0 else None
        rec["sem"] = {
            "seconds": t,
            "ok": r.returncode == 0,
            "changes": [
                {k: c.get(k) for k in ("changeType", "entityType", "entityName", "filePath", "structuralChange")}
                for c in changes or []
                if in_scope(c["filePath"], args.root)
            ],
        }
        r, t = timed([args.inspect, "diff", h, "--format", "json", "-C", args.repo])
        reviews = json.loads(r.stdout)["entity_reviews"] if r.returncode == 0 else None
        keep = ("entity_name", "entity_type", "file_path", "change_type", "classification", "risk_score", "risk_level", "blast_radius")
        rec["inspect"] = {
            "seconds": t,
            "ok": r.returncode == 0,
            "entities": sorted(
                ({k: e[k] for k in keep} for e in reviews or [] if in_scope(e["file_path"], args.root)),
                key=lambda e: -e["risk_score"],
            ),
        }
        (out / f"{h}.json").write_text(json.dumps(rec, indent=1))
        print(f"{h}  merak {len(rec['merak']['ops'] or [])} ops  sem {len(rec['sem']['changes'])}  inspect {len(rec['inspect']['entities'])}", file=sys.stderr)


# ---------------------------------------------------------------- scoring


def short(name):
    """`src/x.ts::Class.method` / `method` / `Class.method` → `method`, for matching across tools."""
    return re.split(r"[.:]", name.rsplit("::", 1)[-1])[-1]


def merak_hit(ops, finding):
    """How Merak reports a labeled finding: a typed op on the entity, only an unclassified one, or nothing."""
    name, file = short(finding["entity"]), finding["file"]
    # The entity itself or a closure inside it: `…::AssetRepository.upsertExif.with("audio")`.
    within = lambda subject: name in re.split(r"[.]", re.sub(r"\(.*?\)", "", subject.rsplit("::", 1)[-1]))
    on = [o for o in ops if o["layer"] != "structural" and "::" in o["subject"] and within(o["subject"]) and file.split("/")[-1] in o["subject"]]
    # Ops not named after an entity (routes, event handlers, state machines) count by evidence in the file.
    evidence = lambda o: o.get("evidence_after", []) + o.get("evidence_before", [])
    unnamed = [o for o in ops if o["layer"] != "structural" and "::" not in o["subject"]
               and any(file.endswith(l["file"]) or l["file"].endswith(file.split("/", 1)[-1]) for l in evidence(o))]
    if not on or (unnamed and all(o["kind"] in ("BEHAVIOUR_ADDED", "UNCLASSIFIED_CHANGE") for o in on)):
        on = on + unnamed
    typed = sorted({o["kind"] for o in on if o["kind"] != "UNCLASSIFIED_CHANGE"})
    if typed == ["BEHAVIOUR_ADDED"]:
        return "described", typed
    if typed:
        return "typed", typed
    if on:
        return "unclassified", ["UNCLASSIFIED_CHANGE"]
    return "missed", []


def score(args):
    runs = pathlib.Path(args.runs)
    labels = json.loads(pathlib.Path(args.labels).read_text())
    recs = {p.stem: json.loads(p.read_text()) for p in runs.glob("*.json") if p.stem != "commits"}

    print(f"## Merak vs sem vs inspect: {len(recs)} commits\n")
    lat = {t: statistics.median(r[t]["seconds"] for r in recs.values()) for t in ("merak", "sem", "inspect")}
    print("| | Merak | sem | inspect |\n|---|---|---|---|")
    print(f"| median latency | {lat['merak']:.2f}s | {lat['sem']:.2f}s | {lat['inspect']:.2f}s |")
    busy = [r for r in recs.values() if r["sem"]["changes"]]
    med = lambda xs: statistics.median(xs) if xs else 0
    print(
        f"| items per commit with code changes (median, n={len(busy)}) | "
        f"{med([len([o for o in r['merak']['ops'] or [] if o['layer'] != 'structural']) for r in busy])} ops | "
        f"{med([len(r['sem']['changes']) for r in busy])} entities | {med([len(r['inspect']['entities']) for r in busy])} entities |"
    )

    findings = [(c, f) for c, l in labels.items() if c in recs for f in l.get("findings", [])]
    print(f"\n### Labeled behavioural findings ({len(findings)} in {len({c for c, _ in findings})} commits)\n")
    tally = collections.Counter()
    rows = []
    for c, f in findings:
        r = recs[c]
        name = short(f["entity"])
        sem = any(short(x["entityName"]) == name and f["file"].endswith(x["filePath"].split("/", 1)[-1]) or short(x["entityName"]) == name for x in r["sem"]["changes"])
        ents = r["inspect"]["entities"]
        rank = next((i + 1 for i, e in enumerate(ents) if short(e["entity_name"]) == name), None)
        level = ents[rank - 1]["risk_level"] if rank else None
        how, kinds = merak_hit(r["merak"]["ops"] or [], f)
        tally["sem"] += sem
        tally["inspect_found"] += rank is not None
        tally["inspect_high"] += level in ("High", "Critical")
        tally["inspect_top3"] += rank is not None and rank <= 3
        tally["merak_" + how] += 1
        rows.append((c, f["kind"], f"{f['file'].split('/')[-1]}::{name}", "yes" if sem else "no", f"{rank}/{len(ents)} {level}" if rank else f"–/{len(ents)}", how + (f" {','.join(kinds)}" if kinds and how == "typed" else "")))
    n = len(findings) or 1
    pct = lambda k: f"{tally[k]} ({100 * tally[k] / n:.0f}%)"
    print("| | count |\n|---|---|")
    print(f"| sem lists the entity as changed | {pct('sem')} |")
    print(f"| inspect lists the entity | {pct('inspect_found')} |")
    print(f"| inspect rates it High/Critical | {pct('inspect_high')} |")
    print(f"| inspect ranks it in its top 3 | {pct('inspect_top3')} |")
    print(f"| Merak names what changed (typed op) | {pct('merak_typed')} |")
    print(f"| Merak describes the new code it is in (BEHAVIOUR_ADDED) | {pct('merak_described')} |")
    print(f"| Merak flags it, untyped (UNCLASSIFIED_CHANGE) | {pct('merak_unclassified')} |")
    print(f"| Merak misses it | {pct('merak_missed')} |")
    if args.verbose:
        print("\n| commit | finding | entity | sem | inspect rank | Merak |\n|---|---|---|---|---|---|")
        for row in rows:
            print("| " + " | ".join(row) + " |")

    # No-behaviour-change claims against labels.
    print("\n### \"No behaviour change\" claims\n")
    print("| commit | label | Merak | sem (all cosmetic) | inspect (all text/syntax) |\n|---|---|---|---|---|")
    for c, l in sorted(labels.items()):
        if c not in recs:
            continue
        r = recs[c]
        ops = r["merak"]["ops"] or []
        m_claim = any(o["kind"] == "PURE_REFACTOR" for o in ops)
        s_claim = bool(r["sem"]["changes"]) and all(x["structuralChange"] is False for x in r["sem"]["changes"])
        i_claim = bool(r["inspect"]["entities"]) and all(e["classification"].lower() in ("text", "syntax", "textsyntax", "text+syntax") for e in r["inspect"]["entities"])
        if m_claim or s_claim or i_claim or l.get("verdict") in ("refactor", "cosmetic"):
            print(f"| {c} | {l.get('verdict')} | {'refactor' if m_claim else '–'} | {'yes' if s_claim else '–'} | {'yes' if i_claim else '–'} |")


def main():
    ap = argparse.ArgumentParser()
    sub = ap.add_subparsers(dest="cmd", required=True)
    r = sub.add_parser("run")
    r.add_argument("repo")
    r.add_argument("--root", default="")
    r.add_argument("-n", type=int, default=60)
    r.add_argument("--out", required=True)
    r.add_argument("--sem", default="sem")
    r.add_argument("--inspect", default="inspect")
    s = sub.add_parser("score")
    s.add_argument("runs")
    s.add_argument("--labels", required=True)
    s.add_argument("-v", "--verbose", action="store_true")
    args = ap.parse_args()
    return run(args) if args.cmd == "run" else score(args)


if __name__ == "__main__":
    sys.exit(main())
