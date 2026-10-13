#!/usr/bin/env python3
"""Sample corpus files, run tdy sniff on each, render self-contained
labeling panels (compact decision summary + preview table), emit rows
JSONL for the Prolific labeling batch.

Usage: python3 results/labeling/build_labeling_items.py
Writes: results/labeling/label_rows.jsonl, label_refusals.jsonl,
        label_sample_manifest.json
"""
import json, os, random, subprocess, sys, tomllib

ROOT = "/home/umatter/tools/tdy"
CORPUS = os.path.join(ROOT, "corpus")
OUT = os.path.join(ROOT, "results", "labeling")
SEED = 20260903
STRATA = {"csv": 90, "excel": 30, "json": 15, "other": 15}
GROUP_SIZE = 15
HEAD_LINES, LINE_CAP = 12, 180
PREVIEW_LINES = 14

def stratum(path):
    e = path.rsplit(".", 1)[-1].lower() if "." in path else ""
    if e == "csv": return "csv"
    if e in ("xlsx", "xls", "xlsm", "ods"): return "excel"
    if e in ("json", "ndjson", "jsonl"): return "json"
    if e in ("tsv", "txt", "dat", "log"): return "other"
    return None

def collect():
    pools = {k: [] for k in STRATA}
    for dirpath, _, files in os.walk(CORPUS):
        for f in files:
            if f.startswith(".") or f.endswith(".tdy.toml"): continue
            s = stratum(f)
            if s:
                p = os.path.join(dirpath, f)
                try:
                    if 0 < os.path.getsize(p) < 20_000_000:
                        pools[s].append(p)
                except OSError:
                    pass
    return pools

def raw_head(path):
    try:
        with open(path, "rb") as f:
            data = f.read(16384)
        text = data.decode("utf-8", errors="replace")
        window = text[:500]
        printable = sum((c.isprintable() and c != '\ufffd') or c.isspace() for c in window)
        if printable < 350 or window.count('\ufffd') > 25:
            return "(binary workbook file - judge from tdy's reading on the right)"
        lines = [l[:LINE_CAP] for l in text.splitlines()[:HEAD_LINES]]
        return "\n".join(lines) if lines else "(empty)"
    except OSError as e:
        return f"(cannot read: {e})"

def summarize_spec(sidecar_path):
    with open(sidecar_path, "rb") as f:
        sc = tomllib.load(f)
    spec = sc.get("spec", {})
    ex = spec.get("extraction", {})
    parts = []
    fmt = ex.get("format", "?")
    if fmt == "delimited":
        parts.append(f"read as delimited text, separator {ex.get('delimiter', '?')!r}")
    elif fmt == "excel":
        parts.append(f"read as spreadsheet, sheet {ex.get('sheet_name', ex.get('sheet_index', '?'))!r}")
    else:
        parts.append(f"read as {fmt}")
    for t in spec.get("transforms", []):
        op = t.get("op")
        if op == "skip_rows":
            parts.append(f"skipped {t.get('rows', t)} leading row(s)")
        elif op == "promote_header":
            parts.append(f"used row(s) {t.get('rows', 1)} as the header")
    cols = spec.get("columns", [])
    coldesc = []
    for c in cols[:14]:
        d = c.get("dtype", {})
        ty = d.get("type", "?")
        if ty == "date": ty = f"date ({d.get('format', '?')})"
        if ty == "decimal": ty = "decimal number"
        if ty == "int64": ty = "whole number"
        if ty == "utf8": ty = "text"
        coldesc.append(f"{c.get('name')}: {ty}")
    more = f" (+{len(cols)-14} more)" if len(cols) > 14 else ""
    summary = "; ".join(parts)
    summary += f"\ncolumns tdy found: {', '.join(coldesc)}{more}"
    conf = spec.get("confidence")
    if conf is not None:
        summary += f"\ntdy's own confidence: {conf:.2f}"
    for n in spec.get("notes", [])[:3]:
        summary += f"\nnote: {n[:LINE_CAP]}"
    return summary

def sniff(path):
    """Run installed tdy sniff; returns (ok, panel_text)."""
    r = subprocess.run(["tdy", "sniff", path, "--no-llm"],
                       capture_output=True, text=True, timeout=300)
    sidecar = path + ".tdy.toml"
    panel = ""
    ok = r.returncode == 0
    if ok:
        try:
            panel = summarize_spec(sidecar)
        except Exception as e:  # a panel must never silently vanish
            panel = f"(could not summarize spec: {e})"
        # preview table sits at the end of stdout
        out = r.stdout.splitlines()
        try:
            pi = max(i for i, l in enumerate(out) if l.startswith("preview"))
            preview = [l[:LINE_CAP] for l in out[pi:pi + PREVIEW_LINES]]
            panel += "\n\n" + "\n".join(preview)
        except ValueError:
            panel += "\n\n(no preview table produced)"
    else:
        panel = (r.stdout + "\n" + r.stderr).strip()[:1200]
    try: os.remove(sidecar)
    except OSError: pass
    return ok, panel

def main():
    random.seed(SEED)
    pools = collect()
    sample = []
    for s, n in STRATA.items():
        pool = sorted(pools[s])
        random.shuffle(pool)
        take = pool[:n]
        print(f"stratum {s}: pool {len(pool)}, taking {len(take)}", file=sys.stderr)
        sample += [(s, p) for p in take]

    rows, refusals = [], []
    for i, (s, p) in enumerate(sample):
        rel = os.path.relpath(p, CORPUS)
        try:
            ok, parse = sniff(p)
        except subprocess.TimeoutExpired:
            ok, parse = False, "(sniff timed out)"
        item = {
            "file_label": f"File {i+1:03d} ({s.upper()})",
            "raw_head": raw_head(p),
            "tdy_parse": parse,
            "META_path": rel,
            "META_stratum": s,
        }
        (rows if ok else refusals).append(item)

    for j, r in enumerate(rows):
        r["META_TASK_GROUP_ID"] = f"g{j // GROUP_SIZE:02d}"

    with open(os.path.join(OUT, "label_rows.jsonl"), "w") as f:
        for r in rows: f.write(json.dumps(r) + "\n")
    with open(os.path.join(OUT, "label_refusals.jsonl"), "w") as f:
        for r in refusals: f.write(json.dumps(r) + "\n")
    with open(os.path.join(OUT, "label_sample_manifest.json"), "w") as f:
        json.dump({"seed": SEED, "strata": STRATA,
                   "sniffed_ok": len(rows), "refused": len(refusals),
                   "groups": (len(rows) + GROUP_SIZE - 1) // GROUP_SIZE,
                   "files": [dict(path=r["META_path"]) for r in rows]}, f, indent=1)
    print(f"rows: {len(rows)}  refusals: {len(refusals)}  "
          f"groups: {(len(rows)+GROUP_SIZE-1)//GROUP_SIZE}")

if __name__ == "__main__":
    main()
