#!/usr/bin/env python3
"""Score tdy on the Pollock data-loading benchmark.

Pollock (Vitagliano, Hameed, Jiang, Reisener, Wu & Naumann, PVLDB 16, 2023)
takes one clean CSV, applies 2,290 *isolated* pollutions — a different
delimiter, a multi-row header, a preamble, one row with an extra separator, a
non-standard escape — and asks a system to load each polluted file. Scoring is
not binary: alongside `success` (did it load at all) it measures precision,
recall and F1 at the header, record and cell level against the known content,
so a system that loads a file and quietly drops a column scores worse than one
that refuses to load it.

That is why this benchmark is worth running here. tdy's rule is *refuse rather
than be wrong*, which trades success for precision, and nobody has published
that trade as a number.

Two modes, because the benchmark's cell metric is exact string equality and
Pollock itself ships both flavours of DuckDB for the same reason:

  typed  what `tdy query` actually returns — dates parsed, numbers typed.
         Comparable to the published `duckdbauto` row.
  text   the same structural read with every column left as text (the sniffed
         sidecar is rewritten to `utf8` and re-run under `--frozen`).
         Comparable to `duckdbparse`, and the honest measure of *framing*,
         which is what the pollutions actually vary.

    scripts/download_pollock.sh
    scripts/run_pollock.py --venv .venv-pollock

Writes gap_reports/pollock_summary.md plus per-file rows for each mode.
Metrics come from Pollock's own `pollock.metrics`, never a reimplementation,
so the numbers sit in the same table as the paper's.
"""
import argparse
import concurrent.futures as cf
import csv
import os
import shutil
import subprocess
import sys
import tempfile

MODES = ("typed", "text")


def to_text_spec(toml_text: str) -> str:
    """Rewrite a sniffed sidecar so every column is plain text.

    Line-oriented on purpose: the sidecar is serde's own output with a
    predictable shape, and this only has to drop each column's `parse` table
    and force its `dtype`. The extraction and transforms — the structural read
    the benchmark is actually testing — are left exactly as sniffed.
    """
    out, skipping = [], False
    for line in toml_text.splitlines():
        st = line.strip()
        if st.startswith("[["):
            skipping = False
            out.append(line)
        elif st.startswith("["):
            if st == "[spec.columns.parse]":
                skipping = True
            elif st == "[spec.columns.dtype]":
                skipping = True
                out.extend([line, 'type = "utf8"'])
            else:
                skipping = False
                out.append(line)
        elif not skipping:
            out.append(line)
    return "\n".join(out) + "\n"


def original_header(toml_text: str):
    """The file's own header spelling, in output order.

    tdy renames columns to SQL-safe identifiers and keeps the original in
    `source`; reconstructing it here is the export step every Pollock wrapper
    performs, not a repair — the information never left the sidecar.
    """
    names, cols, cur = [], [], {}
    for line in toml_text.splitlines():
        st = line.strip()
        if st == "[[spec.columns]]":
            if cur:
                cols.append(cur)
            cur = {}
        elif st.startswith("[") and cur and not st.startswith("[spec.columns."):
            cols.append(cur)
            cur = {}
        elif "=" in st and cur is not None:
            k, _, v = st.partition("=")
            k, v = k.strip(), v.strip()
            if k in ("name", "source") and v.startswith('"') and k not in cur:
                cur[k] = v[1:-1]
    if cur:
        cols.append(cur)
    for c in cols:
        if "name" in c:
            names.append(c.get("source") or c["name"])
    return names


def rewrite_header(path: str, names):
    if not names or not os.path.exists(path):
        return
    with open(path, newline="", encoding="utf-8") as f:
        rows = list(csv.reader(f))
    if not rows or len(rows[0]) != len(names):
        return
    rows[0] = names
    with open(path, "w", newline="", encoding="utf-8") as f:
        csv.writer(f, quoting=csv.QUOTE_ALL).writerows(rows)


def sniffed_confidence(toml_text: str):
    for line in toml_text.splitlines():
        st = line.strip()
        if st.startswith("confidence"):
            try:
                return float(st.split("=", 1)[1])
            except ValueError:
                return None
    return None


def convert_one(job):
    """Load one polluted file with tdy and write it back out as RFC 4180.

    The file is copied into scratch first: tdy writes a sidecar beside whatever
    it reads, and the benchmark corpus has to stay pristine so the second run
    measures the same thing as the first.
    """
    tdy, src, out_path, mode, timeout = job
    name = os.path.basename(src)
    err, conf = "", None
    with tempfile.TemporaryDirectory() as work:
        local = os.path.join(work, name)
        shutil.copyfile(src, local)
        side = local + ".tdy.toml"
        base = [tdy, "--backend", "none"]
        try:
            r = subprocess.run(base + ["sniff", local], capture_output=True, text=True,
                               timeout=timeout)
            ok = r.returncode == 0 and os.path.exists(side)
            if ok:
                toml_text = open(side, encoding="utf-8").read()
                names = original_header(toml_text)
                conf = sniffed_confidence(toml_text)
                if mode == "text":
                    open(side, "w", encoding="utf-8").write(to_text_spec(toml_text))
                q = base + ["query", f"SELECT * FROM messy('{local}')",
                            "-o", out_path, "--format", "csv", "--frozen"]
                r = subprocess.run(q, capture_output=True, text=True, timeout=timeout)
                ok = r.returncode == 0 and os.path.exists(out_path)
                if ok:
                    rewrite_header(out_path, names)
            if not ok:
                err = ((r.stderr or r.stdout).strip().splitlines() or [""])[-1][:300]
        except subprocess.TimeoutExpired:
            ok, err = False, "timeout"
        except Exception as e:  # a wrapper bug must not look like a tdy refusal
            ok, err = False, f"wrapper: {e}"[:300]
    if not ok:
        with open(out_path, "w") as f:
            f.write("Application Error\n")  # Pollock's own "refused" convention
    return name, ok, err, conf


def run_mode(a, tdy, files, csv_dir, mode):
    loaded = os.path.join(a.out, "pollock_loaded", mode)
    os.makedirs(loaded, exist_ok=True)
    jobs = [(tdy, os.path.join(csv_dir, f), os.path.join(loaded, f + "_converted.csv"),
             mode, a.timeout) for f in files]
    refused, confidence = [], []
    with cf.ThreadPoolExecutor(a.jobs) as ex:
        for i, (name, ok, err, conf) in enumerate(ex.map(convert_one, jobs), 1):
            if not ok:
                refused.append((name, err))
            confidence.append((name, "" if conf is None else f"{conf:.4f}"))
            if i % 200 == 0:
                print(f"  [{mode}] {i}/{len(files)}", file=sys.stderr)
    print(f"[{mode}] loaded {len(files)} files, {len(refused)} refused", file=sys.stderr)
    with open(os.path.join(a.out, f"pollock_refused_{mode}.txt"), "w") as f:
        for name, err in refused:
            f.write(f"{name}\t{err}\n")
    # The sniffer's own confidence, so the summary can ask the question this
    # tool actually makes a claim about: when tdy says it is sure, is it right?
    with open(os.path.join(a.out, f"pollock_confidence_{mode}.csv"), "w", newline="") as f:
        w = csv.writer(f)
        w.writerow(["file", "confidence"])
        w.writerows(confidence)
    return loaded


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--pollock", default="pollock", help="clone of HPI-Information-Systems/Pollock")
    ap.add_argument("--dataset", default="polluted_files", choices=["polluted_files", "survey_sample"])
    ap.add_argument("--tdy", default="target/release/tdy")
    ap.add_argument("--out", default="gap_reports")
    ap.add_argument("--limit", type=int, default=0, help="score only the first N files")
    ap.add_argument("--jobs", type=int, default=8)
    ap.add_argument("--timeout", type=int, default=60)
    ap.add_argument("--modes", default="typed,text")
    ap.add_argument("--venv", default="", help="venv whose python has pollock's metric deps")
    a = ap.parse_args()

    csv_dir = os.path.join(a.pollock, a.dataset, "csv")
    clean_dir = os.path.join(a.pollock, a.dataset, "clean")
    if not os.path.isdir(csv_dir):
        sys.exit(f"{csv_dir} not found — run scripts/download_pollock.sh first")
    tdy = a.tdy if os.path.exists(a.tdy) else shutil.which("tdy")
    if not tdy:
        sys.exit("no tdy binary: cargo build --release, or pass --tdy")

    files = sorted(f for f in os.listdir(csv_dir) if f.endswith(".csv"))
    if a.limit:
        files = files[: a.limit]
    os.makedirs(a.out, exist_ok=True)

    modes = [m for m in a.modes.split(",") if m in MODES]
    dirs = {m: run_mode(a, tdy, files, csv_dir, m) for m in modes}

    py = os.path.join(a.venv, "bin", "python") if a.venv else sys.executable
    scorer = os.path.join(os.path.dirname(os.path.abspath(__file__)), "_pollock_score.py")
    subprocess.run([py, scorer, os.path.abspath(a.pollock), os.path.abspath(clean_dir),
                    os.path.abspath(a.out), a.dataset]
                   + [f"{m}={os.path.abspath(d)}" for m, d in dirs.items()], check=True)
    print(f"wrote {a.out}/pollock_summary.md", file=sys.stderr)


if __name__ == "__main__":
    main()
