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
   Nord 510.00), 900.00 (Ost 290.00, West 300.00, Nord 310.00). Ground
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

5. regions_three_offset.xlsx
   The same three blocks and amounts as #2, but the used range starts at
   C5, not A1 (two blank columns of margin, four blank rows). `regions_of`
   returns windows relative to the used range — {0,4},{5,9},{10,14}, same
   as #2 — but an A1 address for a window has to add the used range's own
   `start()` back in, or the read lands on the wrong sheet rows/columns.
   This is the case that motivates that: reading it as if the sheet's data
   began at A1 would read sheet rows 1-4 (blank) instead of 5-8.

6. regions_short_block.csv
   A two-line run (the Datum;Region;Betrag header plus one row, Ost
   100.00), a blank line, then a proper four-line block (block 2 of #1,
   sum 1500.00). The short run is below the 3-row minimum, so the split
   keeps one window, {3,7} — and *discards* lines 0-2, which are 3 fields
   wide exactly like the block that was kept. Reading only the window
   answers 1500.00 where the file holds 1600.00, so the discarded run is
   named in the member's note and, because it is shaped like a table,
   waits on a person. Ground truth: one window {3,7}; one dropped run
   {0,2} of width 3; the file's own total 1600.00.

7. regions_banner.csv
   The layout of a government statistics export (the corpus's
   `ttb_brewery_state_*.xlsx`, as text): a 6-line banner of one field
   each, a blank line, the header `State;2008;2009;2010`, a blank line
   *between the header and its data*, five data rows, a blank line, and
   three one-field footnotes. The split sees four runs: banner (6 lines),
   header (1 line), data (5 lines), footnotes (3 lines). Two rulings are
   pinned by it: a block that fails the gates (banner, footnotes) is not a
   member but a run nothing read; and a one-row run as wide as the block
   directly below it, with only blank lines between, is that block's
   header and is adopted into its window — otherwise the data binds
   positionally as `col_N`. Ground truth: one window {7,14} (header, blank,
   data); sums 2008 = 1500, 2009 = 1550, 2010 = 1600 (4650 in all).

8. regions_banner.xlsx
   The same layout on two sheets of identical shape ("Premise",
   "Bottles"), the banner and footnote cells in column D, the table in
   A-D. Both sheets pass the gates, so sheet discovery expands the
   workbook into two sheet members, each one table under a banner. Ground
   truth: "Premise" as #7; "Bottles" doubles every value, sums 3000 /
   3100 / 3200 (9300 in all).

9. regions_footnoted.xlsx
   The corpus's `ttb_brewery_state_*.xlsx` sheet reduced to one sheet
   ("Data"): the banner of #7 in column A, a blank row, the header
   `State;2008;2009;2010`, a blank row, the five rows of #7, a blank row,
   then a 3-row footnote block of TWO cells per row (a form number and its
   text). The footnote block has no header, so against a positional
   `col_N` target it would bind and pass the gates by position alone; it
   must not be a candidate, and the sheet is read whole, as before regions.
   Against a by-name target only the table passes and the footnote block
   is a data-like run nothing read. Ground truth: the table's sums are
   #7's, 1500 / 1550 / 1600.

10. regions_renumber.csv / regions_renumber.xlsx (sheet "Data")
   Three stacked tables: A (`Datum;Region;Menge`, quantities 1/2/3), B and
   C (`Datum;Region;Betrag`, B = block 2 of #1, C = block 3 of #1). Against
   `matches='Betrag'` only B and C pass the gates and are numbered #1 and
   #2; once the declaration also matches `Menge`, A passes too and the
   blocks are renumbered #1..#3 — so the sidecar written for #1 (B) now
   belongs to #2. Reusing it reads B twice and loses A. Ground truth: B+C =
   2400.00; A+B+C = 2406.00.

11. regions_banner_notes.xlsx
   A "Notes" sheet (three one-cell lines) first, then sheet "Data" laid out
   as one sheet of #8 ("Premise", banner and footnotes in column A). Only
   "Data" holds the table, so sheet discovery finds one fitting sheet: the
   member is plain, and its region split must still read "Data" — not give
   up because the workbook has two sheets. Ground truth: 1500/1550/1600.

12. regions_statetable.xlsx
   The corpus's ADP-31 StateTables sheet, reduced (one sheet, "Hispanic"):
   two title rows, a blank row, the header on row 4 (State / All workers /
   Number of actors / Share / Location quotient), a "United States" total
   row, a blank row, eight state rows (some cells `n/a`), a blank row, and
   a "Puerto Rico" row. The split's state block has no header of its own,
   but the block sniffer promotes its first row (`Alabama | 88165 | 0 |
   n/a | n/a`) as one; that "header" is a number over a numeric column, so
   it is data, and the sheet read whole against the by-name draft asks no
   question. Ground truth: the whole sheet reads 10 rows; all-workers sum
   28391970 + 3215390 (states) + 1208905 = 32816265.

13. regions_two_statetables.xlsx / regions_two_statetables.csv (sheet "Data")
   Two stacked official-statistics tables, each laid out as the corpus's
   state tables are: two title lines and the header in ONE run (no blank
   row between them), a blank row, then a headerless body of five state
   rows; two blank rows between the tables. The split sees four blocks —
   title+header, body, title+header, body — and neither body has a header
   of its own (its first row, `Alabama | 88165 | 10`, is data). Each body
   adopts the run above it whole: its frame skips the two title lines and
   promotes the header, so each table binds by name. The .csv is the same
   text, ";"-separated. Ground truth: table 1 workers 3012640, actors 965;
   table 2 workers 3032000, actors 1024.

14. regions_footnoted_sheets.xlsx / regions_three_sheets.xlsx
   The two shapes of the corpus sweep's workbooks with several sheets that
   `draft` must not split: two sheets each laid out as #9 (a table under a
   banner over a two-cell footnote block — `ttb_brewery_state_2008-2019`),
   and two sheets each laid out as #2 (three stacked tables — `occupational
   _health`). `fit` admits a sheet of such a workbook through its blocks
   only when exactly one passes and nothing table-shaped is discarded, so a
   draft from the blocks fits no sheet; each is drafted whole, as before.
   Ground truth: the second sheet doubles the first's values.

Ground truth summary: regions_three.csv/.xlsx -> [{0,4},{5,9},{10,14}],
sums 600.00 / 1500.00 / 900.00; regions_titled.csv -> [{3,7}];
regions_three_offset.xlsx -> same windows and sums as regions_three.xlsx;
regions_short_block.csv -> [{3,7}] plus a dropped table-shaped run {0,2};
regions_banner.csv -> one member, window {7,14}, sums 1500/1550/1600;
regions_banner.xlsx -> two sheet members, sums 1500/1550/1600 and 3000/3100/3200;
regions_footnoted.xlsx -> read whole positionally, or one by-name member, 1500/1550/1600;
regions_renumber.{csv,xlsx} -> 2400.00 (Betrag), 2406.00 (Betrag, Menge);
regions_banner_notes.xlsx -> one plain member, 1500/1550/1600;
regions_statetable.xlsx -> one plain member read whole, 10 rows, 32816265;
regions_two_statetables.{xlsx,csv} -> two members, 3012640/965 and 3032000/1024;
regions_{footnoted,three}_sheets.xlsx -> drafted whole, both sheets fit.
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
    ("05.03.2025", "Ost", "290.00"),
    ("12.03.2025", "West", "300.00"),
    ("19.03.2025", "Nord", "310.00"),
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
        "three stacked tables (600.00 / 1500.00 / 900.00); windows {0,4} {5,9} {10,14}",
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


def build_regions_three_offset_xlsx():
    """Same three blocks as regions_three.xlsx, but the used range does not
    start at the sheet's own A1: two blank columns (A, B) and four blank
    rows of margin push the first block's header to C5. `regions_of` reads
    row indices *of the used range*, so a window of {0,4} here is sheet row
    5, not sheet row 1 — the case that motivates reading the used range's
    own `start()` before turning a window into an A1 address, rather than
    assuming it is (0, 0).
    """
    from openpyxl import Workbook

    wb = Workbook()
    ws = wb.active
    ws.title = "Data"
    row, col = 5, 3  # first block's header lands at C5
    for i, block in enumerate((BLOCK1, BLOCK2, BLOCK3)):
        if i > 0:
            row += 1  # one fully blank row between blocks
        for j, cell in enumerate(("Datum", "Region", "Betrag")):
            ws.cell(row=row, column=col + j, value=cell)
        row += 1
        for d, r, b in block:
            for j, v in enumerate((d, r, b)):
                ws.cell(row=row, column=col + j, value=v)
            row += 1
    save_workbook(
        wb,
        "regions_three_offset.xlsx",
        "same three blocks as regions_three.xlsx, sheet \"Data\", used range starts at "
        "C5 (2 blank columns, 4 blank rows of margin)",
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


def build_regions_short_block_csv():
    short = [HEADER, "05.01.2025;Ost;100.00"]
    lines = short + [""] + csv_block(BLOCK2)
    write_csv(
        "regions_short_block.csv",
        lines,
        "a 2-line run (below the minimum, 3 fields wide) above block 2; "
        "window {3,7}, dropped run {0,2}, file total 1600.00",
    )


BANNER = [
    "Beer production by state",
    "Barrels, all premises",
    "Source: TTB statistical release",
    "Prepared 31.03.2020",
    "Unit: barrels",
    "Preliminary figures",
]
BANNER_HEADER = ["State", "2008", "2009", "2010"]
BANNER_ROWS = [
    ("Alabama", 100, 110, 120),
    ("Alaska", 200, 210, 220),
    ("Arizona", 300, 310, 320),
    ("Arkansas", 400, 410, 420),
    ("California", 500, 510, 520),
]
FOOTNOTES = [
    "1 Includes contract brewers.",
    "2 Figures are rounded.",
    "3 See the release notes for revisions.",
]


def build_regions_banner_csv():
    lines = (
        BANNER
        + [""]
        + [";".join(BANNER_HEADER)]
        + [""]
        + [f"{s};{a};{b};{c}" for s, a, b, c in BANNER_ROWS]
        + [""]
        + FOOTNOTES
    )
    write_csv(
        "regions_banner.csv",
        lines,
        "6-line banner, header, blank, 5 rows, footnotes; window {7,14}, "
        "sums 1500/1550/1600",
    )


def build_regions_banner_xlsx():
    from openpyxl import Workbook

    wb = Workbook()
    for i, (title, factor) in enumerate((("Premise", 1), ("Bottles", 2))):
        ws = wb.active if i == 0 else wb.create_sheet()
        ws.title = title
        row = 1
        for text in BANNER:
            ws.cell(row=row, column=4, value=text)
            row += 1
        row += 1  # blank
        for j, h in enumerate(BANNER_HEADER):
            ws.cell(row=row, column=1 + j, value=h)
        row += 2  # header, then a blank row before the data
        for s_, a, b, c in BANNER_ROWS:
            for j, v in enumerate((s_, a * factor, b * factor, c * factor)):
                ws.cell(row=row, column=1 + j, value=v)
            row += 1
        row += 1  # blank
        for text in FOOTNOTES:
            ws.cell(row=row, column=4, value=text)
            row += 1
    save_workbook(
        wb,
        "regions_banner.xlsx",
        "two sheets, each banner / header / blank / 5 rows / footnotes; "
        "sums 1500/1550/1600 and 3000/3100/3200",
    )


FORM_NOTES = [
    (5130.9, "Line 15: Removed for consumption or sale"),
    (5130.26, "Line 10: Beer tax-determined for use in the tavern"),
    ("**", "Increases due to the growth of new breweries"),
]


def build_regions_footnoted_xlsx():
    from openpyxl import Workbook

    wb = Workbook()
    ws = wb.active
    ws.title = "Data"
    row = 1
    for text in BANNER:
        ws.cell(row=row, column=1, value=text)
        row += 1
    row += 1  # blank
    for j, h in enumerate(BANNER_HEADER):
        ws.cell(row=row, column=1 + j, value=h)
    row += 2  # header, then a blank row before the data
    for vals in BANNER_ROWS:
        for j, v in enumerate(vals):
            ws.cell(row=row, column=1 + j, value=v)
        row += 1
    row += 1  # blank
    for code, text in FORM_NOTES:
        ws.cell(row=row, column=1, value=code)
        ws.cell(row=row, column=2, value=text)
        row += 1
    save_workbook(
        wb,
        "regions_footnoted.xlsx",
        "banner / header / blank / 5 rows / a headerless 2-cell footnote block; "
        "sums 1500/1550/1600",
    )


RENUMBER_A = [("05.01.2025", "Ost", "1"), ("12.01.2025", "West", "2"), ("19.01.2025", "Nord", "3")]


def renumber_rows():
    rows = [("Datum", "Region", "Menge")] + RENUMBER_A + [None]
    rows += [tuple(HEADER.split(";"))] + BLOCK2 + [None]
    rows += [tuple(HEADER.split(";"))] + BLOCK3
    return rows


def build_regions_renumber():
    from openpyxl import Workbook

    rows = renumber_rows()
    write_csv(
        "regions_renumber.csv",
        ["" if r is None else ";".join(r) for r in rows],
        "blocks A (Menge), B, C (Betrag); 2400.00 by Betrag, 2406.00 with Menge",
    )
    wb = Workbook()
    ws = wb.active
    ws.title = "Data"
    for i, r in enumerate(rows, 1):
        if r is None:
            continue
        for j, v in enumerate(r, 1):
            ws.cell(row=i, column=j, value=v)
    save_workbook(wb, "regions_renumber.xlsx", "regions_renumber.csv as sheet \"Data\"")


def build_regions_banner_notes_xlsx():
    from openpyxl import Workbook

    wb = Workbook()
    notes = wb.active
    notes.title = "Notes"
    for i, text in enumerate(["About this workbook", "Source: somewhere", "Contact: nobody"], 1):
        notes.cell(row=i, column=1, value=text)
    ws = wb.create_sheet("Data")
    row = 1
    for text in BANNER:
        ws.cell(row=row, column=1, value=text)
        row += 1
    row += 1
    for j, h in enumerate(BANNER_HEADER):
        ws.cell(row=row, column=1 + j, value=h)
    row += 2
    for vals in BANNER_ROWS:
        for j, v in enumerate(vals):
            ws.cell(row=row, column=1 + j, value=v)
        row += 1
    row += 1
    for text in FOOTNOTES:
        ws.cell(row=row, column=1, value=text)
        row += 1
    save_workbook(
        wb,
        "regions_banner_notes.xlsx",
        "a Notes sheet, then one banner/table/footnotes sheet; one fitting sheet",
    )


STATE_ROWS = [
    ("Alabama", 88165, 0, "n/a", "n/a"),
    ("Alaska", 26875, 0, "n/a", "n/a"),
    ("Arizona", 1033370, 45, 4.35e-05, 0.17),
    ("Arkansas", 64230, 10, 0.000156, 0.62),
    ("California", 1800000, 900, 0.0005, 1.99),
    ("Colorado", 101250, 15, 0.000148, 0.59),
    ("Connecticut", 50500, 5, 9.9e-05, 0.39),
    ("Delaware", 51000, 0, "n/a", "n/a"),
]


def build_regions_statetable_xlsx():
    from openpyxl import Workbook

    wb = Workbook()
    ws = wb.active
    ws.title = "Hispanic"
    ws.append(["Number of actors in the U.S. labor force, for all the states and Puerto Rico: 2015-2019"])
    ws.append(["Hispanic"])
    ws.append([])
    ws.append(["State", "All workers in the labor force", "Number of actors in the labor force",
               "Actors as a share of labor force", "Location quotient"])
    ws.append(["United States", 28391970, 7115, 0.00025, 1])
    ws.append([])
    for r in STATE_ROWS:
        ws.append(list(r))
    ws.append([])
    ws.append(["Puerto Rico", 1208905, 175, 0.000145, "n/a"])
    save_workbook(
        wb,
        "regions_statetable.xlsx",
        "ADP-31 StateTables shape: title, header row 4, total, a headerless state block",
    )


TWO_HEADER = ["State", "All workers", "Actors"]
TWO_TABLES = [
    (
        ["Table 1. Workers and actors by state: 2019", "Persons in the labor force"],
        [("Alabama", 88165, 10), ("Alaska", 26875, 0), ("Arizona", 1033370, 45),
         ("Arkansas", 64230, 10), ("California", 1800000, 900)],
    ),
    (
        ["Table 2. Workers and actors by state: 2020", "Persons in the labor force"],
        [("Alabama", 90000, 12), ("Alaska", 27000, 1), ("Arizona", 1040000, 50),
         ("Arkansas", 65000, 11), ("California", 1810000, 950)],
    ),
]


def two_statetables_rows():
    """Title lines + header in one run, a blank row, the body; two blank
    rows between the tables. `None` is a blank row."""
    rows = []
    for i, (titles, body) in enumerate(TWO_TABLES):
        if i > 0:
            rows += [None, None]
        rows += [(t,) for t in titles]
        rows.append(tuple(TWO_HEADER))
        rows.append(None)
        rows += body
    return rows


def build_regions_two_statetables():
    from openpyxl import Workbook

    rows = two_statetables_rows()
    write_csv(
        "regions_two_statetables.csv",
        ["" if r is None else ";".join(str(v) for v in r) for r in rows],
        "two tables, each title+header in one run, blank, headerless body; "
        "3012640/965 and 3032000/1024",
    )
    wb = Workbook()
    ws = wb.active
    ws.title = "Data"
    for i, r in enumerate(rows, 1):
        if r is None:
            continue
        for j, v in enumerate(r, 1):
            ws.cell(row=i, column=j, value=v)
    save_workbook(wb, "regions_two_statetables.xlsx", "regions_two_statetables.csv as sheet \"Data\"")


def build_regions_several_sheets():
    from openpyxl import Workbook

    wb = Workbook()
    for i, (title, factor) in enumerate((("Premise", 1), ("Bottles", 2))):
        ws = wb.active if i == 0 else wb.create_sheet()
        ws.title = title
        row = 1
        for text in BANNER:
            ws.cell(row=row, column=1, value=text)
            row += 1
        row += 1
        for j, h in enumerate(BANNER_HEADER):
            ws.cell(row=row, column=1 + j, value=h)
        row += 2
        for s_, a, b, c in BANNER_ROWS:
            for j, v in enumerate((s_, a * factor, b * factor, c * factor)):
                ws.cell(row=row, column=1 + j, value=v)
            row += 1
        row += 1
        for code, text in FORM_NOTES:
            ws.cell(row=row, column=1, value=code)
            ws.cell(row=row, column=2, value=text)
            row += 1
    save_workbook(wb, "regions_footnoted_sheets.xlsx", "two sheets laid out as regions_footnoted.xlsx")

    wb = Workbook()
    for i, (title, factor) in enumerate((("Q1", 1), ("Q2", 2))):
        ws = wb.active if i == 0 else wb.create_sheet()
        ws.title = title
        for k, block in enumerate((BLOCK1, BLOCK2, BLOCK3)):
            if k > 0:
                ws.append([])
            ws.append(["Datum", "Region", "Betrag"])
            for d, r, b in block:
                ws.append([d, r, f"{float(b) * factor:.2f}"])
    save_workbook(wb, "regions_three_sheets.xlsx", "two sheets laid out as regions_three.xlsx")


def main():
    os.makedirs(OUT, exist_ok=True)
    build_regions_three_csv()
    build_regions_three_xlsx()
    build_regions_three_offset_xlsx()
    build_regions_summary_csv()
    build_regions_titled_csv()
    build_regions_short_block_csv()
    build_regions_banner_csv()
    build_regions_banner_xlsx()
    build_regions_footnoted_xlsx()
    build_regions_renumber()
    build_regions_banner_notes_xlsx()
    build_regions_statetable_xlsx()
    build_regions_two_statetables()
    build_regions_several_sheets()
    print("\nground truth: regions_three.{csv,xlsx} -> [{0,4},{5,9},{10,14}], "
          "sums 600.00/1500.00/900.00; regions_titled.csv -> [{3,7}]; "
          "regions_three_offset.xlsx -> same sums, used range starts at C5; "
          "regions_short_block.csv -> [{3,7}] with a dropped run {0,2}")


if __name__ == "__main__":
    main()
