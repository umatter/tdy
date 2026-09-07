#!/usr/bin/env python3
"""Sweep the corpus's multi-sheet workbooks through draft -> fit.

The population workbook members were built for: a workbook with several
sheets, drafted into a target from its own columns, then fitted as a pile.
Before workbook members (2026-09-07) `fit` refused 16 of 31 of these as
`AmbiguousFrame`; after, each of those becomes one member per fitting sheet.

    scripts/download_corpus.sh              # once; ~7 GB under corpus/
    cargo build --release
    scripts/sweep_workbooks.py              # prints a summary and one line per workbook

Outcomes: `plain fit` (one member, one fitting sheet), `expanded N` (N sheet
members), `refused (...)` (a gap or error, with its first line), `draft
failed`, or a timeout. Counts only xlsx/xlsm (the sheet count comes from the
zip's workbook.xml, no dependency); one workbook at a time, so a hang or a
panic names its file.
"""
import json
import os
import re
import shutil
import subprocess
import sys
import tempfile
import zipfile
from collections import Counter

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
CORPUS = os.environ.get("TDY_CORPUS", os.path.join(ROOT, "corpus"))
TDY = os.environ.get("TDY_BIN", os.path.join(ROOT, "target", "release", "tdy"))


def multi_sheet_workbooks():
    out = []
    for dp, _dn, fn in os.walk(CORPUS):
        if ".git" in dp:
            continue
        for f in fn:
            if not f.lower().endswith((".xlsx", ".xlsm")) or f.startswith("~$"):
                continue
            p = os.path.join(dp, f)
            try:
                with zipfile.ZipFile(p) as z:
                    n = len(re.findall(r"<sheet ", z.read("xl/workbook.xml").decode("utf-8", "replace")))
            except Exception:
                continue
            if n > 1:
                out.append((n, p))
    return sorted(out)


def sweep_one(n, p):
    d = tempfile.mkdtemp(prefix="sweep-")
    env = dict(os.environ, TDY_BACKEND="none")
    try:
        book = os.path.join(d, "book" + os.path.splitext(p)[1])
        shutil.copy(p, book)
        try:
            dr = subprocess.run([TDY, "draft", book], capture_output=True, text=True, timeout=120, env=env)
        except subprocess.TimeoutExpired:
            return "draft timeout", ""
        if dr.returncode != 0 or "CREATE TABLE" not in dr.stdout:
            tail = dr.stderr.strip().splitlines()
            return "draft failed", (tail[-1][:120] if tail else "")
        t = os.path.join(d, "t.tdy.sql")
        with open(t, "w") as fh:
            fh.write(dr.stdout)
        try:
            fr = subprocess.run([TDY, "--json", "fit", t, "--dry-run"], capture_output=True, text=True, timeout=300, env=env)
        except subprocess.TimeoutExpired:
            return "fit timeout", ""
        try:
            rep = json.loads(fr.stdout)
        except Exception:
            tail = fr.stderr.strip().splitlines()
            return "fit error", (tail[-1][:120] if tail else "")
        ms = rep.get("members", [])
        sheets = [m.get("sheet") for m in ms if m.get("sheet")]
        st = [m.get("status") for m in ms]
        ok = ("fits", "needs_review")
        if sheets and all(s in ok for s in st):
            return f"expanded {len(sheets)}", ", ".join(sheets)[:100]
        if len(ms) == 1 and st[0] in ok:
            return "plain fit", ""
        bad = [m for m in ms if m.get("status") not in ok]
        msg = bad[0]["problems"][0]["message"].splitlines()[0][:110] if bad and bad[0].get("problems") else ""
        return f"refused ({','.join(sorted(set(st)))})", msg
    finally:
        shutil.rmtree(d, ignore_errors=True)


def main():
    if not os.path.isdir(CORPUS):
        sys.exit(f"no corpus at {CORPUS}; run scripts/download_corpus.sh first")
    if not os.path.exists(TDY):
        sys.exit(f"no binary at {TDY}; cargo build --release (or set TDY_BIN)")
    rows = []
    for n, p in multi_sheet_workbooks():
        kind, msg = sweep_one(n, p)
        rows.append((p, n, kind, msg))
        print(f"{kind:22} {n:>2} sheets  {os.path.relpath(p, CORPUS)[:70]:70} {msg}", flush=True)
    print(Counter(r[2].split(" ")[0] for r in rows))


if __name__ == "__main__":
    main()
