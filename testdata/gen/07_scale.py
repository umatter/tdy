#!/usr/bin/env python3
"""Scale / performance fixtures for `tdy` (job key: scale-perf).

    python3 testdata/gen/07_scale.py              # ~5 MB total, seconds
    python3 testdata/gen/07_scale.py --size big   # ~1.0 GB total, minutes

Everything lands in `testdata/large/`, which carries a `.gitignore` holding
`*`: these files are regenerable and must never be committed. Every file name
starts with `scale_perf_`. Nothing outside `testdata/large/` is touched.

Determinism: all asserted content is closed-form arithmetic on the row index,
so the bytes are identical on every run and every Python 3. The only PRNG use
is picking the filler token for the *overflow* cells of the ragged fixture
(cells that tdy's projection drops anyway); those tokens are all exactly six
characters, so even the file's byte layout does not depend on the draw.
`random.seed(20260828)` is re-applied before each file, so the fixtures do not
depend on generation order either.

---------------------------------------------------------------------------
What each fixture stresses, and what a correct parse must produce
---------------------------------------------------------------------------

Shared property, and the reason these files exist at all: every one of them is
far larger than `config.sample_bytes` (16 KiB default), so `sample::build`
shows the sniffer only the first 12 KiB (`max_bytes * 3/4`) plus a 4 KiB tail.
Every decision tier 1 makes is therefore made from a keyhole, while
`engine::extract` reads the whole file. Where the keyhole and the file
disagree, these fixtures make the disagreement observable.

1. scale_perf_wide.csv  — 500 columns.
   Stresses: per-column work that is O(columns) x O(rows) (`guess_columns`
   re-scans the probe body once per column; `to_record_batch` materialises a
   `Vec<&str>` plus a `Vec<Option<String>>` per column), 500-field Arrow
   schemas, and a header line (~2.5 KiB) that eats a fifth of the sample.
   Correct parse: header `row_id, m001 .. m499` (500 Int64 columns), one row
   per data line. Cell(row_id=r, m{c}) == (r*31 + c*7) % 1000 exactly, so
   row_id=1 has m001=38, m002=45, m499=524; row_id=600 has m001=607.

2. scale_perf_tall.csv  — 8 columns, many rows.
   Stresses: the fully materialised `Vec<Vec<String>>` in `engine::extract`
   (8 * N heap Strings before a single Arrow array is built), plus per-value
   `chrono` parsing of N timestamps and N floats.
   Correct parse: event_id Int64, event_ts Timestamp("%Y-%m-%d %H:%M:%S"),
   region Utf8, product Utf8, units Int64, price_chf Float64, discount_pct
   Float64 (na_values ["n/a"]), active Bool. Row 1 is
   (1, 2025-01-01 00:00:00, "Zurich", "Widget", 14, 1.37, 0.07, true);
   discount_pct is NULL exactly on rows where event_id % 17 == 0 ("n/a") or
   event_id % 23 == 0 (empty).

3. scale_perf_longfield.csv — one 1 MiB cell (and one 256 KiB cell).
   Stresses: a single CSV field three orders of magnitude larger than the
   whole sample; quoted fields containing commas, doubled quotes and embedded
   newlines; the fact that neither the 12 KiB head sample nor the 300-row
   probe ever sees the pathological row (doc D00601 is the 601st data row).
   Note that the head sample is cut *inside* an open quoted field, so tier 1
   reasons about a torn record - it must still come out at 4 fields.
   Correct parse: doc_id Utf8, kind Utf8, payload_len Int64, payload Utf8,
   702 data rows. `length(payload) == payload_len` must hold for every row.
   Row D00601 has payload_len 1048576, payload starting "BEGIN:D00601:" and
   ending ":END:D00601"; D00602 has 262144. Short rows have payload_len 48.

4. scale_perf_ragged.csv — arity 4/6/9/12 rows, plus rare arity-64 rows.
   Stresses: the sample/file split in the ragged path. The widest rows (64
   fields) appear only from line 998 onward, i.e. outside both the 12 KiB
   sample and the 300-row probe, while `RawTable::rectangularize` pads the
   whole file to width 64.
   Correct parse with the tier-1 heuristic spec (delimiter ',', ragged
   pad_nulls, NO transforms): `looks_like_header` rejects line 1 because
   after padding it contains blanks, so the sniffer emits col_1 .. col_6
   (`modal_fields` = 6, from the sample) and the header line survives as data
   row 1: ("id","ts","level","source","message","extra"). Data row 2 is
   ("1","2025-02-01","INFO","host02","message-1","extra-1"). Row count is
   line count (12001 at the default size). All six columns are Utf8. col_5
   and col_6 are NULL on the arity-4 rows (id % 100 in [62, 78)), e.g. id=62.
   The 58 overflow columns col_7..col_64 exist in the engine's raw table but
   are silently outside the projection - the intended, and here observable,
   consequence of "columns are a projection".

Reference numbers at the default size (measured, LF endings, pure ASCII):
   wide       1,169,562 B   500 cols x    600 rows
   tall       1,745,986 B     8 cols x 30,000 rows
   longfield  1,399,357 B     4 cols x    702 rows (1 MiB + 256 KiB cells)
   ragged       758,003 B   4/6/9/12+64 x 12,000 rows + header line
   total      5,074,006 B (~4.8 MiB), generated in ~0.5 s
At --size big: wide 500 x 20,000 (~39 MB); tall 8 x 3,000,000 (~175 MB);
longfield 640 x 1 MiB cells (~672 MB); ragged 3,000,000 rows (~190 MB)
-> ~1.05 GB total. Nothing here is committed, so the 2 MB fixture cap does
not apply.

Tier-1 sniffer outcome, simulated against the real head-sample arithmetic in
sniff_delimited (all four pick delimiter ',' unambiguously: the files contain
no ';', '|' or tab at all, so every rival candidate has modal arity 1):

   file        modal  skip_head  ragged      confidence
   wide          500      0      pad_nulls   0.95 - 0.15        = 0.80
   tall            8      0      error       0.95               = 0.95
   longfield       4      0      error       0.95               = 0.95
   ragged          6      0      pad_nulls   0.95 -0.20 -0.15
                                             -0.10 -0.15        = 0.35

Two things worth knowing about that table. (a) `wide` lands exactly on
`confidence_threshold` (0.80), and the comparison is `>=` on f32, so whether
this file escalates to the LLM tier is decided by float rounding - it is the
sample's torn last line, not the file, that costs it the 0.15. (b) `tall` and
`longfield` are rectangular *and* their 12 KiB cut happens to land after the
last comma of a record, so they keep ragged = error; move the cut (change
`sample_bytes`, or the row width) and they too drop to pad_nulls at 0.80.
"""

from __future__ import annotations

import argparse
import csv
import json
import random
from datetime import datetime, timedelta
from pathlib import Path

SEED = 20260828
PREFIX = "scale_perf_"

ROOT = Path(__file__).resolve().parents[2]
OUT = ROOT / "testdata" / "large"

# --- shapes -----------------------------------------------------------------
# (small, big) for every knob. "small" is the committed-CI-friendly default
# (~5 MB, generated in a couple of seconds); "big" is the ~1 GB perf profile.
SHAPES = {
    "small": {
        "wide_rows": 600,
        "tall_rows": 30_000,
        "longfield_giants": [(601, 1 << 20), (602, 1 << 18)],
        "ragged_rows": 12_000,
    },
    "big": {
        "wide_rows": 20_000,
        "tall_rows": 3_000_000,
        "longfield_giants": [(600 + k, 1 << 20) for k in range(1, 641)],
        "ragged_rows": 3_000_000,
    },
}

WIDE_COLS = 500  # row_id + m001..m499

REGIONS = ["Zurich", "Bern", "Geneva", "Basel"]
PRODUCTS = ["Widget", "Gadget", "Doohickey", "Sprocket", "Cog"]
BASE_TS = datetime(2025, 1, 1, 0, 0, 0)

LEVELS = ["INFO", "WARN", "ERROR", "DEBUG"]
# All exactly six characters, so the ragged file's byte layout is independent
# of which token the PRNG picks.
EXTRA_TOKENS = ["alpha7", "bravo3", "charl9", "delta1", "echo55", "foxtr8"]

# 64-character block used to build long payloads. Contains a comma, a pair of
# double quotes and a newline, so csv quoting/escaping is genuinely exercised.
_LOREM = 'lorem ipsum, dolor "sit" amet, consectetur adipiscing elit '
BLOCK = (_LOREM + "0123456789" * 2)[:63] + "\n"
assert len(BLOCK) == 64, len(BLOCK)


def _writer(fh):
    return csv.writer(fh, lineterminator="\n")


# ---------------------------------------------------------------------------
# 1. wide: 500 columns
# ---------------------------------------------------------------------------
def gen_wide(rows: int) -> dict:
    random.seed(SEED)
    path = OUT / f"{PREFIX}wide.csv"
    header = ["row_id"] + [f"m{c:03d}" for c in range(1, WIDE_COLS)]
    with path.open("w", newline="", encoding="utf-8") as fh:
        w = _writer(fh)
        w.writerow(header)
        for r in range(1, rows + 1):
            base = r * 31
            w.writerow([r] + [(base + c * 7) % 1000 for c in range(1, WIDE_COLS)])
    return {
        "path": path,
        "columns": WIDE_COLS,
        "rows": rows,
        "note": "cell(r, m{c}) == (r*31 + c*7) % 1000; all Int64",
    }


# ---------------------------------------------------------------------------
# 2. tall: 8 columns, many rows
# ---------------------------------------------------------------------------
def gen_tall(rows: int) -> dict:
    random.seed(SEED)
    path = OUT / f"{PREFIX}tall.csv"
    header = [
        "event_id",
        "event_ts",
        "region",
        "product",
        "units",
        "price_chf",
        "discount_pct",
        "active",
    ]
    with path.open("w", newline="", encoding="utf-8") as fh:
        w = _writer(fh)
        w.writerow(header)
        for i in range(1, rows + 1):
            ts = (BASE_TS + timedelta(seconds=i - 1)).strftime("%Y-%m-%d %H:%M:%S")
            if i % 17 == 0:
                discount = "n/a"
            elif i % 23 == 0:
                discount = ""
            else:
                discount = f"{((i * 7) % 50) / 100:.2f}"
            w.writerow(
                [
                    i,
                    ts,
                    REGIONS[(i - 1) % 4],
                    PRODUCTS[(i - 1) % 5],
                    (i * 13) % 97 + 1,
                    f"{((i * 37) % 9000 + 100) / 100:.2f}",
                    discount,
                    "true" if i % 3 else "false",
                ]
            )
    return {
        "path": path,
        "columns": len(header),
        "rows": rows,
        "note": "discount_pct is 'n/a' on id%17==0 and '' on id%23==0",
    }


# ---------------------------------------------------------------------------
# 3. longfield: a 1 MiB cell hidden past the sample and past the probe
# ---------------------------------------------------------------------------
def payload(doc_id: str, n: int) -> str:
    """Exactly `n` characters: BEGIN:<id>: <filler> :END:<id>."""
    prefix = f"BEGIN:{doc_id}:"
    suffix = f":END:{doc_id}"
    fill = n - len(prefix) - len(suffix)
    if fill < 0:
        raise ValueError(f"payload length {n} too small for {doc_id}")
    reps = fill // len(BLOCK) + 1
    return prefix + (BLOCK * reps)[:fill] + suffix


def gen_longfield(giants: list[tuple[int, int]]) -> dict:
    random.seed(SEED)
    path = OUT / f"{PREFIX}longfield.csv"
    giant = dict(giants)
    total_rows = max(giant) + 100
    kinds = ["text", "note", "memo"]
    with path.open("w", newline="", encoding="utf-8") as fh:
        w = _writer(fh)
        w.writerow(["doc_id", "kind", "payload_len", "payload"])
        for i in range(1, total_rows + 1):
            doc_id = f"D{i:05d}"
            n = giant.get(i, 48)
            body = payload(doc_id, n)
            w.writerow(
                [doc_id, "blob" if i in giant else kinds[(i - 1) % 3], len(body), body]
            )
    return {
        "path": path,
        "columns": 4,
        "rows": total_rows,
        "note": "giant cells at rows "
        + ",".join(f"D{i:05d}={n}" for i, n in sorted(giant.items())[:4])
        + ("..." if len(giant) > 4 else ""),
    }


# ---------------------------------------------------------------------------
# 4. ragged: arity 4/6/9/12, with rare arity-64 rows past the sample
# ---------------------------------------------------------------------------
def arity(i: int) -> int:
    if i % 997 == 0:
        return 64
    m = i % 100
    if m < 62:
        return 6
    if m < 78:
        return 4
    if m < 90:
        return 9
    return 12


def gen_ragged(rows: int) -> dict:
    random.seed(SEED)
    path = OUT / f"{PREFIX}ragged.csv"
    deep = 0
    with path.open("w", newline="", encoding="utf-8") as fh:
        w = _writer(fh)
        w.writerow(["id", "ts", "level", "source", "message", "extra"])
        for i in range(1, rows + 1):
            a = arity(i)
            if a == 64:
                deep += 1
            row = [
                str(i),
                f"2025-02-{((i - 1) % 28) + 1:02d}",
                LEVELS[(i - 1) % 4],
                f"host{(i % 7) + 1:02d}",
                f"message-{i}",
                f"extra-{i}",
            ][:a]
            for j in range(6, a):
                row.append(f"{random.choice(EXTRA_TOKENS)}{j:02d}-{i}")
            w.writerow(row)
    return {
        "path": path,
        "columns": "4/6/9/12, and 64 on id%997==0",
        "rows": rows,
        "note": f"{deep} rows of arity 64; first at id=997 (line 998), "
        "past the 12 KiB sample and the 300-row probe",
    }


# ---------------------------------------------------------------------------
def main() -> None:
    ap = argparse.ArgumentParser(
        description="Generate tdy scale/perf fixtures in testdata/large/ (never committed)."
    )
    ap.add_argument(
        "--size",
        choices=sorted(SHAPES),
        default="small",
        help="small (~5 MB, default) or big (~1.0 GB)",
    )
    args = ap.parse_args()
    shape = SHAPES[args.size]

    OUT.mkdir(parents=True, exist_ok=True)

    gitignore = OUT / ".gitignore"
    gitignore.write_text(
        "# Generated scale/perf fixtures - regenerable, never committed.\n"
        "#   python3 testdata/gen/07_scale.py [--size small|big]\n"
        "*\n"
        "!.gitignore\n",
        encoding="utf-8",
    )
    print(f"wrote {gitignore.relative_to(ROOT)} ({gitignore.stat().st_size} bytes)")

    results = [
        gen_wide(shape["wide_rows"]),
        gen_tall(shape["tall_rows"]),
        gen_longfield(shape["longfield_giants"]),
        gen_ragged(shape["ragged_rows"]),
    ]

    manifest = {"generator": "testdata/gen/07_scale.py", "size": args.size, "seed": SEED, "files": []}
    for r in results:
        p: Path = r["path"]
        n = p.stat().st_size
        manifest["files"].append(
            {
                "path": str(p.relative_to(ROOT)),
                "bytes": n,
                "columns": r["columns"],
                "rows": r["rows"],
                "note": r["note"],
            }
        )
        print(
            f"wrote {p.relative_to(ROOT)} ({n:,} bytes, "
            f"{r['columns']} cols x {r['rows']:,} rows) - {r['note']}"
        )

    mpath = OUT / f"{PREFIX}manifest.json"
    mpath.write_text(json.dumps(manifest, indent=2) + "\n", encoding="utf-8")
    print(f"wrote {mpath.relative_to(ROOT)} ({mpath.stat().st_size:,} bytes)")


if __name__ == "__main__":
    main()
