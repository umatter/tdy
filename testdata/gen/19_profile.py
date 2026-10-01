#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""Generate the `profile_*` fixtures for tdy (job key: profile).

Run from the repo root:  python3 testdata/gen/19_profile.py

Deterministic: plain stdlib text writes, `\\n` line endings, no clock and
no randomness.

WHY THIS FAMILY EXISTS
--------------------------------------------------------------------------
A profile (src/profile.rs, docs/design/2026-10-01-profiling.md) says, per
column of the framed raw table, how many values, how many distinct, and
which *shapes* they take — read over the whole file, because the head lies
in exactly the way the type verification exists to catch. These files pin
the numbers a profile must report, and the two bounds it states rather than
hides.
--------------------------------------------------------------------------
FIXTURES  (all in testdata/, named profile_*)
--------------------------------------------------------------------------
1. profile_mixed_dates.csv  (`tests/profile.rs`)
   `Datum;Region;Betrag`, 100 data rows. `Datum` holds 94 ISO dates
   (`2025-01-01` .. ) and 6 dotted ones (`DD.MM.YYYY`) at data rows 61, 70,
   75, 80, 90 and 100 — all after row 60, so a head of the file shows only
   the ISO shape. Ground truth for `Datum`: shape `9999-99-99` x 94 (first
   example `2025-01-01`), `99.99.9999` x 6 (first example `01.03.2025`),
   100 distinct values. `Region` cycles Ost/West/Nord/Sued (25 each).
   `Betrag` is `1'2NN.50`-style money; rows 31 and 32 are empty, so it has
   98 non-empty values and 2 empty.

2. profile_many_ids.csv  (`tests/profile.rs`)
   `id,kind`, 12,000 data rows. `id` is `ID000001` .. `ID012000`: 12,000
   distinct values, past the 10,000 a profile tracks, so it must report
   `AtLeast(10000)` and no top values (an approximate top five is a number
   nobody can check). `kind` cycles a/b/c: exactly 3 distinct, 4,000 each.

3. profile_sheet_named_2.xlsx  (`tests/profile.rs`)
   One sheet, literally named `2`, holding three `Datum;Region;Betrag`
   tables stacked at blank rows (rows 1-4, 6-9, 11-14). So the reference
   `profile_sheet_named_2.xlsx#2` has two true readings — the sheet `2`, and
   block 2 of the workbook's one sheet — and must be refused naming both,
   never resolved to either. Requires openpyxl (and lxml, transitively — see
   gen_fixtures.py); zip stamps and `dcterms:modified` are pinned.
"""

import os
import re
import zipfile
from datetime import datetime

HERE = os.path.dirname(os.path.abspath(__file__))
REPO = os.path.dirname(os.path.dirname(HERE))
OUT = os.path.join(REPO, "testdata")

DOTTED_ROWS = (61, 70, 75, 80, 90, 100)  # 1-based data rows
REGIONS = ("Ost", "West", "Nord", "Sued")


def write(name, lines):
    p = os.path.join(OUT, name)
    with open(p, "w", newline="\n", encoding="utf-8") as f:
        for line in lines:
            f.write(line + "\n")
    print("wrote", p)


def mixed_dates():
    lines = ["Datum;Region;Betrag"]
    iso = 0
    dotted = 0
    for row in range(1, 101):
        if row in DOTTED_ROWS:
            # 01.03.2025, 02.03.2025, ...
            dotted += 1
            datum = "%02d.03.2025" % dotted
        else:
            # 2025-01-01 .. 2025-01-31, 2025-02-01 .. — distinct, valid days
            month = 1 + iso // 28
            day = 1 + iso % 28
            iso += 1
            datum = "2025-%02d-%02d" % (month, day)
        region = REGIONS[(row - 1) % 4]
        betrag = "" if row in (31, 32) else "1'2%02d.50" % (row % 100)
        lines.append("%s;%s;%s" % (datum, region, betrag))
    assert iso == 94 and dotted == 6
    write("profile_mixed_dates.csv", lines)


def many_ids():
    lines = ["id,kind"]
    for i in range(1, 12001):
        lines.append("ID%06d,%s" % (i, "abc"[(i - 1) % 3]))
    write("profile_many_ids.csv", lines)


MODIFIED_RE = re.compile(rb"(<dcterms:modified[^>]*>)[^<]*(</dcterms:modified>)")
ZIP_EPOCH = (2026, 1, 1, 0, 0, 0)


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


def sheet_named_2():
    import openpyxl

    wb = openpyxl.Workbook()
    ws = wb.active
    ws.title = "2"
    blocks = [
        [("05.01.2025", "Ost", "190.00"), ("12.01.2025", "West", "200.00"), ("19.01.2025", "Nord", "210.00")],
        [("05.02.2025", "Ost", "490.00"), ("12.02.2025", "West", "500.00"), ("19.02.2025", "Nord", "510.00")],
        [("05.03.2025", "Ost", "290.00"), ("12.03.2025", "West", "300.00"), ("19.03.2025", "Nord", "310.00")],
    ]
    for i, block in enumerate(blocks):
        if i:
            ws.append([])
        ws.append(["Datum", "Region", "Betrag"])
        for row in block:
            ws.append(list(row))
    wb.properties.created = wb.properties.modified = datetime(2026, 1, 1)
    p = os.path.join(OUT, "profile_sheet_named_2.xlsx")
    wb.save(p)
    repack(p)
    print("wrote", p)


def main():
    mixed_dates()
    many_ids()
    sheet_named_2()


if __name__ == "__main__":
    main()
