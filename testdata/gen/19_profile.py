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
"""

import os

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


def main():
    mixed_dates()
    many_ids()


if __name__ == "__main__":
    main()
