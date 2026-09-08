#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""Generate the `regions_*` fixtures for tdy (job key: regions).

Run from the repo root:  python3 testdata/gen/17_regions.py

Deterministic: the .xlsx pins zip entry timestamps and `dcterms:modified`
the same way `14_sheet_frames.py` does (its `save`/`repack` machinery is
copied here rather than imported, per generator convention); the .csv files
are plain stdlib text writes.

WHY THIS FAMILY EXISTS
--------------------------------------------------------------------------
A dataset member is normally a whole file or a whole sheet. `MemberRef`
(src/member.rs) can also name a *region* — one of several tables stacked in
a file or a sheet, addressed by a `RowWindow` (src/spec.rs). `regions_of`
(src/engine.rs) finds the candidate windows by splitting on runs of blank
rows: a run needs at least 3 rows to count as a block (a one- or two-line
banner above a table is not itself a "stacked table"), and a single block
spanning the whole file or sheet is not a region either — there is nothing
to split. These fixtures are the corpus for that split: the ordinary case
(three stacked tables), the workbook analogue (one sheet, not a file), the
negative case that already exists as `compressed_plain.csv` (one block, no
region), and the case that motivates the 3-row minimum (a short title
banner above one real table).
--------------------------------------------------------------------------
FIXTURES  (all in testdata/, named regions_*)
--------------------------------------------------------------------------
1. regions_three.csv
   Three ";"-delimited tables (Datum;Region;Betrag), each a header row
   plus 3 data rows, separated by one blank line: raw lines 0-3, 5-8,
   10-13 (14 raw lines total, `\n` endings). Block sums: 600.00 (Ost
   190.00, West 200.00, Nord 210.00), 1500.00 (Ost 490.00, West 500.00,
   Nord 510.00), 300.00 (Ost 90.00, West 100.00, Nord 110.00). Ground
   truth for `regions_of`: three RowWindows, {0,4}, {5,9}, {10,14}.

2. regions_three.xlsx
   The same three blocks and the same amounts, one sheet ("Data"), one
   fully blank row between blocks — the workbook analogue of #1:
   `regions_of` over one sheet finds the same three windows by row index
   instead of raw line number.

3. regions_summary.csv
   Block 1 from #1 (Datum;Region;Betrag / Ost,West,Nord = 190/200/210),
   a blank line, then a second, differently-shaped block that recaps the
   same numbers two columns wide: `Region;Total\nOst;190.00\nWest;200.00
   \nNord;210.00\n`. Two proper blocks of different width stacked in one
   file — `regions_of` does not need the blocks to share a shape, only a
   blank-line boundary and 3+ rows each.

4. regions_titled.csv
   A two-line title/date banner (`Bericht 2025` / `Erstellt am
   01.02.2025`), a blank line, then block 1 from #1. Ground truth: the
   banner is a run of only 2 non-blank lines, below the 3-row minimum, so
   `regions_of` returns exactly one window, {3,7} — not two — which is
   the case the minimum exists for.

Ground truth summary: regions_three.csv/.xlsx -> [{0,4},{5,9},{10,14}],
sums 600.00 / 1500.00 / 300.00; regions_titled.csv -> [{3,7}].
"""
import os
import re
import zipfile
from datetime import datetime

HERE = os.path.dirname(os.path.abspath(__file__))
REPO = os.path.dirname(os.path.dirname(HERE))
OUT = os.path.join(REPO, "testdata")
ZIP_EPOCH = (2026, 1, 1, 0, 0, 0)

MODIFIED_RE = re.compile(rb"(<dcterms:modified[^>]*>)[^<]*(</dcterms:modified>)")

HEADER = "Datum;Region;Betrag"
BLOCK1 = [
    ("05.01.2025", "Ost", "190.00"),
    ("12.01.2025", "West", "200.00"),
    ("19.01.2025", "Nord", "210.00"),
]
BLOCK2 = [
    ("05.02.2025", "Ost", "490.00"),
    ("12.02.2025", "West", "500.00"),
    ("19.02.2025", "Nord", "510.00"),
]
BLOCK3 = [
    ("05.03.2025", "Ost", "90.00"),
    ("12.03.2025", "West", "100.00"),
    ("19.03.2025", "Nord", "110.00"),
]


def note(path, what):
    print(f"wrote {os.path.relpath(path, REPO)} ({os.path.getsize(path)} bytes) - {what}")


def csv_block(rows):
    lines = [HEADER]
    lines.extend(f"{d};{r};{b}" for d, r, b in rows)
    return lines


def write_csv(name, lines, what):
    p = os.path.join(OUT, name)
    with open(p, "w", newline="\n", encoding="utf-8") as f:
        f.write("\n".join(lines) + "\n")
    note(p, what)


def repack(path):
    """Pin zip stamps and dcterms:modified; see 09_legacy_formats.py."""
    tmp = path + ".tmp"
    with zipfile.ZipFile(path) as zin:
        entries = [(i.filename, zin.read(i.filename)) for i in zin.infolist()]
    entries = [
        (n, MODIFIED_RE.sub(rb"\g<1>2026-01-01T00:00:00Z\g<2>", d)
         if n == "docProps/core.xml" else d)
        for n, d in entries
    ]
    with zipfile.ZipFile(tmp, "w", zipfile.ZIP_DEFLATED, compresslevel=6) as zout:
        for n, d in entries:
            info = zipfile.ZipInfo(n, date_time=ZIP_EPOCH)
            info.compress_type = zipfile.ZIP_DEFLATED
            info.create_system = 0
            info.external_attr = 0o644 << 16
            zout.writestr(info, d)
    os.replace(tmp, path)


def save_workbook(wb, name, what):
    wb.properties.created = wb.properties.modified = datetime(2026, 1, 1)
    p = os.path.join(OUT, name)
    wb.save(p)
    repack(p)
    note(p, what)


def build_regions_three_csv():
    lines = csv_block(BLOCK1) + [""] + csv_block(BLOCK2) + [""] + csv_block(BLOCK3)
    write_csv(
        "regions_three.csv",
        lines,
        "three stacked tables (600.00 / 1500.00 / 300.00); windows {0,4} {5,9} {10,14}",
    )


def build_regions_three_xlsx():
    from openpyxl import Workbook

    wb = Workbook()
    ws = wb.active
    ws.title = "Data"
    for i, block in enumerate((BLOCK1, BLOCK2, BLOCK3)):
        if i > 0:
            ws.append([])  # one fully blank row between blocks
        ws.append(["Datum", "Region", "Betrag"])
        for d, r, b in block:
            ws.append([d, r, b])
    save_workbook(
        wb,
        "regions_three.xlsx",
        "same three blocks as regions_three.csv, sheet \"Data\", blank row between",
    )


def build_regions_summary_csv():
    recap = ["Region;Total"] + [f"{r};{b}" for _, r, b in BLOCK1]
    lines = csv_block(BLOCK1) + [""] + recap
    write_csv(
        "regions_summary.csv",
        lines,
        "block 1, blank line, then a differently-shaped Region;Total recap of it",
    )


def build_regions_titled_csv():
    lines = ["Bericht 2025", "Erstellt am 01.02.2025", ""] + csv_block(BLOCK1)
    write_csv(
        "regions_titled.csv",
        lines,
        "2-line banner (below the 3-row minimum) above block 1; window {3,7} only",
    )


def main():
    os.makedirs(OUT, exist_ok=True)
    build_regions_three_csv()
    build_regions_three_xlsx()
    build_regions_summary_csv()
    build_regions_titled_csv()
    print("\nground truth: regions_three.{csv,xlsx} -> [{0,4},{5,9},{10,14}], "
          "sums 600.00/1500.00/300.00; regions_titled.csv -> [{3,7}]")


if __name__ == "__main__":
    main()
