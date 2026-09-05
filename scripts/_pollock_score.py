#!/usr/bin/env python3
"""Score converted files with Pollock's own metrics, and write the summary.

Split from run_pollock.py because it must run under a python carrying
Pollock's metric dependencies (chardet, numpy, joblib, multiset, dateutil,
price_parser) with Pollock's package importable. Nothing here reimplements a
metric: `successful_csv` and `header_record_cell_measures_csv` are the
benchmark's own, so a tdy row lands in the same table as the paper's.

argv: <pollock_repo> <clean_dir> <out_dir> <dataset> mode=<loaded_dir> ...
"""
import csv
import os
import re
import statistics
import sys

pollock_repo, clean_dir, out_dir, dataset = sys.argv[1:5]
modes = dict(a.split("=", 1) for a in sys.argv[5:])
sys.path.insert(0, pollock_repo)
import pollock.metrics as metrics  # noqa: E402

# The paper's own grouping of the pollutions, verbatim from its evaluate.py.
FAMILIES = {
    "table (header rows, preamble, several tables)":
        r"file_double.*|file_header.*|file_no.*|file_one.*|file_multi.*|file_preamble.*",
    "inconsistent rows (extra/missing separators)": r"row_less.*|row_more.*",
    "structural (delimiter, quote, escape, newline)":
        r"file_field.*|row_field.*|file_quote.*|file_record_delimiter.*"
        r"|row_extra_quote.*|file_escape.*",
}
COLS = ["header_p", "header_r", "header_f1", "record_p", "record_r", "record_f1",
        "cell_p", "cell_r", "cell_f1"]
# Published rows, from the benchmark's own results/aggregate_results_polluted_files.csv.
# duckdbauto types its columns as tdy does; duckdbparse reads everything as text.
REFERENCE = {
    "duckdbauto (typed)": (1.000, 0.978, 0.919, 0.792),
    "duckdbparse (all text)": (1.000, 1.000, 0.991, 0.996),
    "pandas": (0.999, 0.995, 0.978, 0.991),
    "clevercsv": (1.000, 0.975, 0.840, 0.904),
    "rhypoparsr": (1.000, 0.198, 0.124, 0.604),
    "postgres": (0.017, 0.015, 0.013, 0.012),
}


def confidences(mode):
    path = os.path.join(out_dir, f"pollock_confidence_{mode}.csv")
    if not os.path.exists(path):
        return {}
    with open(path, newline="") as f:
        return {r["file"]: (float(r["confidence"]) if r["confidence"] else None)
                for r in csv.DictReader(f)}


def score(loaded_dir):
    rows = []
    for name in sorted(os.listdir(clean_dir)):
        if not name.endswith(".csv"):
            continue
        out = os.path.join(loaded_dir, name + "_converted.csv")
        if not os.path.exists(out):
            continue
        try:
            m = metrics.header_record_cell_measures_csv(os.path.join(clean_dir, name), out)
        except Exception:
            m = [0.0] * 9
        rows.append({"file": name, "success": float(metrics.successful_csv(out)),
                     **{c: float(v) for c, v in zip(COLS, m)}})
    return rows


def mean(subset, col):
    return statistics.fmean([r[col] for r in subset]) if subset else float("nan")


results = {}
for mode, d in modes.items():
    rows = score(d)
    conf = confidences(mode)
    for r in rows:
        r["confidence"] = conf.get(r["file"])
    results[mode] = rows
    with open(os.path.join(out_dir, f"pollock_rows_{mode}.csv"), "w", newline="") as f:
        w = csv.DictWriter(f, fieldnames=["file", "success", "confidence"] + COLS)
        w.writeheader()
        w.writerows(rows)
    print(f"scored {len(rows)} files [{mode}]", file=sys.stderr)

n = len(next(iter(results.values()))) if results else 0
with open(os.path.join(out_dir, "pollock_summary.md"), "w") as f:
    f.write(f"# tdy on Pollock — `{dataset}`\n\n")
    f.write(f"{n} polluted files, each isolating one deviation from RFC 4180. "
            f"Metrics are Pollock's own `pollock.metrics`, so these rows sit in "
            f"the same table as the paper's.\n\n")
    f.write("`typed` is what `tdy query` returns. `text` is the same structural "
            "read with every column left as `utf8` — the benchmark compares cells "
            "as exact strings, so any system that parses a date loses cell score "
            "for writing it back in ISO form. Pollock ships both flavours of "
            "DuckDB for that reason, and the two tdy rows are the same comparison.\n\n")

    f.write("| system | success | header F1 | record F1 | cell F1 |\n|---|---|---|---|---|\n")
    for mode, rows in results.items():
        f.write(f"| **tdy ({mode})** | {mean(rows,'success'):.3f} | {mean(rows,'header_f1'):.3f} "
                f"| {mean(rows,'record_f1'):.3f} | {mean(rows,'cell_f1'):.3f} |\n")
    for name, (s, h, r, c) in REFERENCE.items():
        f.write(f"| {name} | {s:.3f} | {h:.3f} | {r:.3f} | {c:.3f} |\n")

    for mode, rows in results.items():
        f.write(f"\n## tdy ({mode}), by pollution family\n\n")
        f.write("| group | files | success | " + " | ".join(c.replace("_", " ") for c in COLS)
                + " |\n|" + "---|" * (len(COLS) + 3) + "\n")
        groups = [("**all**", rows)]
        matched = set()
        for fam, pat in FAMILIES.items():
            sub = [r for r in rows if re.match(pat, r["file"])]
            matched |= {r["file"] for r in sub}
            groups.append((fam, sub))
        groups.append(("other", [r for r in rows if r["file"] not in matched]))
        for title, sub in groups:
            if not sub:
                continue
            f.write(f"| {title} | {len(sub)} | {mean(sub,'success'):.3f} |"
                    + "".join(f" {mean(sub,c):.3f} |" for c in COLS) + "\n")

        ok = [r for r in rows if r["success"] == 1.0]
        perfect = [r for r in ok if r["cell_f1"] == 1.0 and r["record_f1"] == 1.0
                   and r["header_f1"] == 1.0]
        f.write(f"\n- loaded: **{len(ok)}/{len(rows)}** ({100*len(ok)/max(len(rows),1):.1f}%)\n")
        f.write(f"- exactly right on every header, record and cell: **{len(perfect)}/{len(rows)}** "
                f"({100*len(perfect)/max(len(rows),1):.1f}%)\n")
        # The claim tdy actually makes is not "it loads everything" but "when
        # it is confident, it is right, and when it is not, it says so." That
        # is a conditional accuracy, and it is what this block measures.
        banded = [r for r in rows if r.get("confidence") is not None]
        if banded:
            sure = [r for r in banded if r["confidence"] >= 0.8]
            unsure = [r for r in banded if r["confidence"] < 0.8]
            f.write("\n| tdy's own verdict | files | cell F1 | record F1 | header F1 | exactly right |\n")
            f.write("|---|---|---|---|---|---|\n")
            for title, sub in (("confident (\u2265 0.80)", sure), ("flagged (< 0.80)", unsure)):
                if not sub:
                    continue
                exact = [r for r in sub if r["cell_f1"] == 1.0 and r["record_f1"] == 1.0
                         and r["header_f1"] == 1.0]
                f.write(f"| {title} | {len(sub)} | {mean(sub,'cell_f1'):.3f} | "
                        f"{mean(sub,'record_f1'):.3f} | {mean(sub,'header_f1'):.3f} | "
                        f"{len(exact)}/{len(sub)} ({100*len(exact)/len(sub):.0f}%) |\n")
            wrong_and_sure = [r for r in sure if r["cell_f1"] < 1.0]
            f.write(f"\n**Wrong while confident: {len(wrong_and_sure)}/{len(sure)}** — "
                    f"the number this project's rule is about.\n")
            if wrong_and_sure:
                f.write("\n")
                for r in sorted(wrong_and_sure, key=lambda r: r["cell_f1"])[:20]:
                    f.write(f"- `{r['file']}` — confidence {r['confidence']:.2f}, "
                            f"cell F1 {r['cell_f1']:.3f}, header F1 {r['header_f1']:.3f}\n")
                if len(wrong_and_sure) > 20:
                    f.write(f"- …and {len(wrong_and_sure)-20} more\n")

        bad = sorted([r for r in ok if r["cell_f1"] < 1.0], key=lambda r: r["cell_f1"])
        if bad:
            f.write(f"\nWorst loaded files ({len(bad)} with cell F1 < 1):\n\n")
            for r in bad[:30]:
                f.write(f"- `{r['file']}` — cell F1 {r['cell_f1']:.3f} "
                        f"(P {r['cell_p']:.3f} / R {r['cell_r']:.3f}), "
                        f"record F1 {r['record_f1']:.3f}, header F1 {r['header_f1']:.3f}\n")
            if len(bad) > 30:
                f.write(f"- …and {len(bad)-30} more (see pollock_rows_{mode}.csv)\n")
