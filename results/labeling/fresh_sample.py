#!/usr/bin/env python3
"""Draw a FRESH stratified corpus sample, disjoint from the 2026-09-03 audit.

The original 148 files are no longer a fair test set: the fixes were written
against them, so re-measuring there flatters the tool. This draws a new sample
from files never examined, with a different seed, so the resulting defect rate
is an honest estimate of where tdy stands now.

Writes results/labeling/fresh_rows.jsonl (panels, same shape as the first study) and
results/labeling/fresh_manifest.json.
"""
import json, os, random, subprocess, sys, tomllib

ROOT = "/home/umatter/tools/tdy"
CORPUS = os.path.join(ROOT, "corpus")
OUT = os.path.join(ROOT, "results", "labeling")
SEED = 20260904          # different seed from the first draw (20260903)
STRATA = {"csv": 90, "excel": 30, "json": 15, "other": 15}
LINE_CAP, PREVIEW_LINES, HEAD_LINES = 180, 14, 12

def stratum(name):
    e = name.rsplit(".", 1)[-1].lower() if "." in name else ""
    if e == "csv": return "csv"
    if e in ("xlsx", "xls", "xlsm", "ods"): return "excel"
    if e in ("json", "ndjson", "jsonl"): return "json"
    if e in ("tsv", "txt", "dat", "log"): return "other"
    return None

def main():
    already = {r["path"] for r in json.load(open(f"{OUT}/label_sample_manifest.json"))["files"]}
    pools = {k: [] for k in STRATA}
    for dirpath, _, files in os.walk(CORPUS):
        for f in files:
            if f.startswith(".") or f.endswith(".tdy.toml"):
                continue
            s = stratum(f)
            if not s:
                continue
            p = os.path.join(dirpath, f)
            rel = os.path.relpath(p, CORPUS)
            if rel in already:          # disjoint from the first audit
                continue
            try:
                if 0 < os.path.getsize(p) < 20_000_000:
                    pools[s].append(p)
            except OSError:
                pass

    random.seed(SEED)
    sample = []
    for s, n in STRATA.items():
        pool = sorted(pools[s]); random.shuffle(pool)
        take = pool[:n]
        print(f"{s:6} pool {len(pool):>5} (excl. {len(already)} already audited) -> taking {len(take)}",
              file=sys.stderr)
        sample += [(s, p) for p in take]

    rows = []
    for i, (s, p) in enumerate(sample):
        rel = os.path.relpath(p, CORPUS)
        r = subprocess.run(["tdy", "sniff", p, "--no-llm"],
                           capture_output=True, text=True, timeout=300)
        sidecar = p + ".tdy.toml"
        conf, notes, panel = None, [], ""
        if r.returncode == 0:
            try:
                sc = tomllib.load(open(sidecar, "rb"))
                spec = sc.get("spec", {})
                conf = spec.get("confidence")
                notes = spec.get("notes", [])
            except Exception:
                pass
            out = r.stdout.splitlines()
            try:
                pi = max(j for j, l in enumerate(out) if l.startswith("preview"))
                panel = "\n".join(l[:LINE_CAP] for l in out[pi:pi + PREVIEW_LINES])
            except ValueError:
                panel = "(no preview)"
        else:
            panel = (r.stdout + "\n" + r.stderr).strip()[:800]
        try: os.remove(sidecar)
        except OSError: pass
        rows.append(dict(n=i + 1, path=rel, stratum=s, ok=(r.returncode == 0),
                         confidence=conf, notes=notes[:4], preview=panel))

    with open(f"{OUT}/fresh_rows.jsonl", "w") as f:
        for x in rows: f.write(json.dumps(x) + "\n")
    json.dump({"seed": SEED, "strata": STRATA, "n": len(rows),
               "disjoint_from": "label_sample_manifest.json",
               "files": [dict(path=r["path"], stratum=r["stratum"]) for r in rows]},
              open(f"{OUT}/fresh_manifest.json", "w"), indent=1)

    flagged = sum(1 for r in rows if (r["confidence"] or 1.0) < 0.8)
    declined = sum(1 for r in rows if not r["ok"])
    print(f"\nsampled {len(rows)} fresh files: {declined} declined, "
          f"{flagged} flagged under 0.8 confidence, {len(rows)-flagged-declined} confident")

if __name__ == "__main__":
    main()
