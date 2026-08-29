#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""Generate the `csv-torture` fixture set for tdy: delimited files that are
legal CSV/TSV/PSV but structurally hostile.

Run from the repo root:  python3 testdata/gen/03_csv_torture.py
Deterministic and idempotent: byte-identical output on every run.
All files live in testdata/csv_torture/ and are named csv_torture_*.

Each entry below says WHAT IS HARD and WHAT A CORRECT PARSE MUST PRODUCE
(columns / dtypes / row count / at least one exact value). "row N" counts
data rows 1-based, after structural transforms. Date32 values are given as
days since 1970-01-01 so an assertion can be written directly.

--------------------------------------------------------------------------
1. csv_torture_bom_crlf.csv
   Hard: UTF-8 BOM (EF BB BF) before the header, CRLF terminators, umlauts.
   Correct: delimited(delimiter=",", encoding utf-8); promote_header rows=1.
     columns  datum Date32 "%Y-%m-%d" | region Utf8 | umsatz_chf Float64
              (decimal(12,2) equally correct)
     rows     6
     exact    row 1 = (2025-01-15 => Date32 20103, "Zürich", 1200.50)
              sum(umsatz_chf) = 9149.90
     BOM      the promoted header's first cell must be exactly "Datum"
              (5 chars, no leading U+FEFF); the U+FEFF must not survive into
              a column name or a `source` string.

2. csv_torture_quoted.csv
   Hard: CRLF record terminators, but a *bare LF* inside a quoted field;
   quoted fields containing the delimiter; doubled quotes ("") for a literal
   quote; one empty quoted field.
   Correct: delimited(",", quote='"').
     columns  id Int64 | bemerkung Utf8 | betrag Float64
     rows     5
     exact    row 1 bemerkung = 'Rabatt, 10% auf Artikel A'
              row 2 bemerkung = 'Zeile1\\nZeile2'   (one LF, no CR)
              row 3 bemerkung = 'Er sagte "Hallo, Welt" und ging'
              row 4 bemerkung IS NULL (empty quoted field)
              sum(betrag) = 492.74

3. csv_torture_semicolon_decimal.csv
   Hard: ';' delimiter, continental decimals ("1.234,56", "0,45"), quoted
   fields that contain a comma, "-" as an NA token.
   Correct: delimited(";"); preis needs decimal_separator="," and
   thousands_separator=".".
     columns  artikel Utf8 | menge Int64 | preis_chf Decimal(10,2)
              (Float64 acceptable) | bemerkung Utf8 na_values=["-"]
     rows     5
     exact    row 1 preis_chf = 1234.56, row 2 preis_chf = 0.45
              sum(preis_chf) = 2347.04
              row 5 bemerkung IS NULL
     Trap:    sniff::guess_type tries (thousands=',', decimal=None) BEFORE
              ('.' , ','), and that convention also "parses" every value
              here, so tier 1 yields 1.23456 / 45.0 / 1200.0 / 1.09995 /
              8.0 (sum 1255.33) at confidence 0.95 — silently ~100x wrong,
              no warning, no escalation. Invariant 6.

4. csv_torture_ragged.csv
   Hard: short rows (2 and 3 fields) AND a long row (5 fields) around a
   4-field header.
   Correct: ragged="pad_nulls"; the 5th field is junk and is dropped by the
   column projection (it lands in a column the engine names "Menge_2").
     columns  datum Date32 "%Y-%m-%d" | region Utf8 | produkt Utf8 |
              menge Int64
     rows     7
     exact    menge = [10, NULL, 25, 7, NULL, NULL, 3]; sum(menge) = 45
              row 2 produkt = "Gadget", row 6 produkt IS NULL
              row 3 = (2025-01-07, "West", "Widget", 25) — "storniert" gone
     Trap:    promote_header rectangularizes first, padding the header row
              to width 5; sniff::looks_like_header then rejects the header
              because it now contains a blank cell -> no header at all
              (col_1..col_4, 8 all-Utf8 rows, confidence 0.35).

5. csv_torture_title_block.csv
   Hard: 4 title lines, a blank line, then the header. Title line 3 has
   exactly 3 comma-separated fields — the same arity as the data — so
   "skip leading rows until the arity is modal" stops two rows too early.
   Correct: skip_rows{head=4 (5 if the reader materialises the blank line)},
   promote_header rows=1.
     columns  datum Date32 "%Y-%m-%d" | region Utf8 | umsatz_chf Float64
     rows     6
     exact    row 1 = (2025-02-01 => Date32 20120, "Ost", 510.25)
              sum(umsatz_chf) = 6155.90
     Trap:    tier 1 skips 2 rows, promotes title line 3 to the header
              ("erstellt_05_01_2026", "abteilung_controlling", "intern")
              and keeps title line 4 as data: 8 all-Utf8 rows, confidence
              ~0.60 — a warning, but the wrong spec is still written.

6. csv_torture_total_footer.csv
   Hard: a trailing "Total" row that is *type-compatible* — valid date,
   empty produkt, numeric umsatz — so nothing structural marks it.
   Correct: skip_rows{tail=1} (or drop_rows_matching "^Total$" on region).
     columns  datum Date32 "%Y-%m-%d" | region Utf8 | produkt Utf8 |
              umsatz Float64 (decimal(12,2) equally correct)
     rows     10
     exact    sum(umsatz) = 14337.00, row 1 = (2025-01-31 => 20119, "Ost",
              "Widget", 1200.50); no row has region = "Total"
     Trap:    sniff_delimited never emits a `tail` (only sniff_excel does),
              so tier 1 keeps 11 rows and sum(umsatz) = 28674.00 — exactly
              double, at confidence 0.95. The footer value is deliberately
              equal to the sum so the doubling is the tell.

7. csv_torture_dup_header.csv
   Hard: the header contains "Betrag" twice (Soll and Haben), with clearly
   different data underneath.
   Correct: engine::promote_header dedupes to "Betrag"/"Betrag_2", so the
   spec must use source="Betrag" and source="Betrag_2".
     columns  buchung Utf8 | betrag_soll Float64 (source "Betrag") |
              betrag_haben Float64 (source "Betrag_2") | waehrung Utf8
     rows     3
     exact    row 1 = ("B-001", 100.00, 250.00, "CHF")
              sum(betrag_soll) = 192.50, sum(betrag_haben) = 1267.24
     Trap:    sniff::guess_columns dedupes the *output* names but sets
              source to the raw cell for both, i.e. source="Betrag" twice,
              so betrag_2 silently repeats column 1 (sum 192.50). The
              sniffer proposes a spec whose meaning is wrong although it
              executes — invariants 1 and 6.

8. csv_torture_blank_header.csv
   Hard: header cell 1 is blank (an unnamed row-index column) and cell 3 is
   blank (an unnamed region column).
   Correct: engine::promote_header fill-right names them "col_1" (nothing
   to its left) and "Datum_2" (inherits from "Datum").
     columns  lfd_nr Int64 (source "col_1") | datum Date32 "%Y-%m-%d"
              (source "Datum") | region Utf8 (source "Datum_2") |
              umsatz_chf Float64 (source "Umsatz")
     rows     3
     exact    row 1 = (1, 2025-01-15 => 20103, "Zürich", 1200.50)
     Trap:    looks_like_header rejects any header row with a blank cell,
              so tier 1 emits col_1..col_4 and keeps the header as data
              row 1 (4 rows; row 1 = ["", "Datum", "", "Umsatz"]). Only
              3 of 4 columns type as Utf8, so the utf8 penalty does not
              fire: confidence lands at 0.85, above the 0.8 threshold —
              the wrong spec ships with no warning at all.

9. csv_torture_sanitize_header.csv
   Hard: header names needing sanitisation — an uppercase umlaut, a
   percent sign, a superscript, parentheses, a space, and a pure-numeric
   name.
   Correct (tdy's own convention: ASCII snake_case, umlaut folding, no
   leading digit, unique, and no letter dropped):
     columns  datum Date32 "%Y-%m-%d" | aenderung Float64 ("Änderung %") |
              groesse_m Float64 ("Größe (m²)"; "groesse_m2" also fine) |
              umsatz_chf Float64 ("Umsatz (CHF)") | c_2025 Int64 ("2025") |
              region_name Utf8 ("Region Name")
     rows     3
     exact    row 1 = (2025-01-15 => 20103, -3.5, 12.75, 1200.50, 42,
                       "Zürich Nord")
     Traps:   (a) sniff::sanitize only folds LOWERCASE umlauts, so "Änderung
              %" becomes "nderung" — the leading letter is dropped.
              (b) looks_like_header rejects the whole header because the
              cell "2025" parses as f64, so tier 1 produces col_1..col_6
              and 4 all-Utf8 rows.

10. csv_torture_pipe.psv
    Hard: '|' delimiter, unknown extension (FormatGuess::Unknown), and
    fields containing a literal '"' mid-field, a comma and a semicolon.
    Correct: delimited("|"); the quote char never starts a field, so every
    '"' is literal data.
      columns  artikel Utf8 | beschreibung Utf8 | preis Float64 |
               lager Bool (true_values ["ja"], false_values ["nein"])
      rows     5
      exact    row 1 beschreibung = '12" Monitor, gebraucht'
               row 3 beschreibung = 'Maus "Pro", kabellos'
               row 1 lager = true, row 3 lager = false
               sum(preis) = 387.35
      Tier 1 should get this one right (delimiter score 1.10, conf 0.95).

11. csv_torture_tab.tsv
    Hard: TSV where a field *starts* with '"' but is not a quoted field
    ('"Der Sturm" (Roman)'), plus zero-padded customer numbers.
    Correct:
      columns  kunden_nr Utf8 (leading zeros preserved) | titel Utf8 |
               datum Date32 "%d.%m.%Y" | betrag Float64
      rows     4
      exact    row 1 = ("0042", '"Der Sturm" (Roman)', 15.01.2025 =>
                        Date32 20103, 19.90)
               row 3 kunden_nr = "0042" (not 42), titel contains ';'
               sum(betrag) = 235.60
      Traps:   (a) ParseSpec cannot express "no quoting": extract_delimited
               only calls builder.quote() when quote is Some, and the csv
               crate's default quote is '"' — so quote=None still quotes.
               The two U+0022 in row 1's titel are eaten and the value
               silently becomes 'Der Sturm (Roman)'. No spec can fix it.
               (b) tier 1 types kunden_nr as Int64, turning "0042" into 42.
               (c) worst: guess_type tries the separator conventions BEFORE
               the date formats, and the continental pair (thousands='.',
               decimal=',') "parses" every %d.%m.%Y date — so datum is
               typed Float64 and 15.01.2025 silently becomes 15012025.0.
               This fires for ANY dotted-date column, at confidence 0.95.

12. csv_torture_one_row.csv
    Hard: exactly one data row — the minimum at which header detection is
    allowed to work at all.
      columns  datum Date32 "%Y-%m-%d" | region Utf8 | umsatz_chf Float64
      rows     1
      exact    (2025-07-04 => Date32 20273, "Ticino", 2750.00)
      Tier 1 should get this right (confidence 0.95).

13. csv_torture_empty.csv
    Hard: zero bytes.
    Correct: a clear, non-panicking error ("no rows detected in ...") or a
    0-row / 0-column relation. Never a spec that silently yields garbage.
      rows     0 (or a hard error)

14. csv_torture_header_only.csv
    Hard: a header and no data rows.
    Correct: 4 named columns, 0 rows. Dtypes are unknowable from data;
    Utf8 for all four is acceptable.
      columns  datum | region | produkt | umsatz
      rows     0
      Trap:    looks_like_header returns false for a single-row table, so
              tier 1 emits col_1..col_4 and ONE data row whose values are
              the header strings ("Datum", "Region", "Produkt", "Umsatz").

15. csv_torture_comments.csv
    Hard: three leading '#' comment lines; comment line 3 happens to have
    exactly the modal arity (4 fields); a data value legitimately contains
    '#' ("Ticket #42").
    Correct: delimited(",", comment="#"); promote_header rows=1.
      columns  datum Date32 "%Y-%m-%d" | region Utf8 | produkt Utf8 |
               umsatz Float64
      rows     6
      exact    row 5 produkt = "Ticket #42" (the '#' must survive: only a
               line-leading '#' is a comment)
               row 1 = (2025-01-15 => 20103, "Ost", "Widget", 1200.50)
               sum(umsatz) = 9099.40
      Trap:    the sniffer never proposes `comment`, and skip_head stops at
              comment line 3, which is then promoted to the header
              ("columns_datum", "region", "produkt", "umsatz") with the real
              header kept as data (7 rows, all Utf8, confidence 0.75).

16. csv_torture_bigtail.csv
    Hard: 500 data rows (> the 16 KiB sample cap and > the 300-row sniffer
    probe cap) whose only anomalies are in the last four lines: data rows
    498-500 use Swiss apostrophe thousands separators, and the file ends
    with a type-compatible "Total" row.
    Correct: skip_rows{tail=1}; umsatz needs thousands_separator="'".
      columns  datum Date32 "%Y-%m-%d" | region Utf8 | beleg Utf8 |
               umsatz Float64 (decimal(12,2) equally correct)
      rows     500
      exact    data row 498 umsatz = 12345.60, row 499 = 9876.05,
               row 500 = 1002.35; row 1 = (2024-01-01 => Date32 19723,
               "Zürich", "BEL-000001", 50.00); row 500 beleg = "BEL-000500";
               sum(umsatz) over the 500 kept rows = 270057.98, which is
               exactly the amount on the dropped Total row (so a leaked
               footer shows up as 540115.96). The filler amounts of rows 2..497 are seeded
               (SEED = 20260828) and stable, but no assertion needs them.
      Traps:   (a) three independent caps hide the tail: the 16 KiB sample
               (its head slice is 12 KiB, ~row 340), the 300-row sniffer
               probe, and guess_type's 200-value slice. Tier 1 therefore
               proposes Float64 with no thousands separator, which fails
               loudly at data row 498 ("cannot parse \"12'345.60\"") —
               invariant 6 respected — while the Total row is still
               included silently once the separator is fixed by hand.
               (b) the sample is cut mid-line, so the sniffer sees one torn
               record, flags the whole file `ragged` (pad_nulls, -0.15) for
               a reason that does not exist in the file, and lands at
               confidence ~0.80 — exactly on the escalation threshold.
"""

from __future__ import annotations

import random
from datetime import date, timedelta
from decimal import Decimal
from pathlib import Path

SEED = 20260828
random.seed(SEED)
RNG = random.Random(SEED)

ROOT = Path(__file__).resolve().parents[2]
OUT = ROOT / "testdata" / "csv_torture"


def write(name: str, data: bytes) -> None:
    assert name.startswith("csv_torture_"), name
    path = OUT / name
    path.write_bytes(data)
    print(f"wrote {path.relative_to(ROOT)} ({len(data)} bytes)")


def lf(lines) -> bytes:
    """Join with LF, terminate the last line."""
    return ("\n".join(lines) + "\n").encode("utf-8")


def crlf(lines) -> bytes:
    """Join with CRLF, terminate the last line."""
    return ("\r\n".join(lines) + "\r\n").encode("utf-8")


BOM = b"\xef\xbb\xbf"


def build() -> None:
    OUT.mkdir(parents=True, exist_ok=True)

    # --- 1. BOM + CRLF ---------------------------------------------------
    write(
        "csv_torture_bom_crlf.csv",
        BOM
        + crlf(
            [
                "Datum,Region,Umsatz",
                "2025-01-15,Zürich,1200.50",
                "2025-02-03,Genève,980.25",
                "2025-03-11,Bern,1543.00",
                "2025-04-27,Basel-Stadt,2200.75",
                "2025-05-09,Zürich,175.40",
                "2025-06-30,Genève,3050.00",
            ]
        ),
    )

    # --- 2. quoting torture ----------------------------------------------
    # CRLF record terminators; row 2 carries a *bare LF* inside its quoted
    # field, so the embedded newline differs from the record terminator.
    quoted = (
        "id,bemerkung,betrag\r\n"
        '1,"Rabatt, 10% auf Artikel A",100.00\r\n'
        '2,"Zeile1\nZeile2",250.50\r\n'
        '3,"Er sagte ""Hallo, Welt"" und ging",99.99\r\n'
        '4,"",0.00\r\n'
        '5,"Trailing comma in text, ",42.25\r\n'
    )
    write("csv_torture_quoted.csv", quoted.encode("utf-8"))

    # --- 3. semicolon + comma decimals ------------------------------------
    write(
        "csv_torture_semicolon_decimal.csv",
        lf(
            [
                "Artikel;Menge;Preis (CHF);Bemerkung",
                'Schraube M4;120;1.234,56;"Paket, klein"',
                "Mutter M4;80;0,45;Standard",
                'Unterlagscheibe;1500;12,00;"gross, verzinkt"',
                "Winkel 90;12;1.099,95;Stahl",
                "Dübel 8mm;350;0,08;-",
            ]
        ),
    )

    # --- 4. ragged: short AND long ----------------------------------------
    write(
        "csv_torture_ragged.csv",
        lf(
            [
                "Datum,Region,Produkt,Menge",
                "2025-01-05,Ost,Widget,10",
                "2025-01-06,Ost,Gadget",  # short: 3 fields
                "2025-01-07,West,Widget,25,storniert",  # long: 5 fields
                "2025-01-08,West,Gadget,7",
                "2025-01-09,Süd,Widget,",  # 4 fields, empty last
                "2025-01-10,Süd",  # short: 2 fields
                "2025-01-11,Nord,Gadget,3",
            ]
        ),
    )

    # --- 5. title block, blank line, then the header ----------------------
    write(
        "csv_torture_title_block.csv",
        lf(
            [
                "Muster AG — Umsatzübersicht 2025",
                "Vertraulich / nur für den internen Gebrauch",
                # exactly 3 comma-separated fields: the same arity as the data
                "Erstellt: 05.01.2026, Abteilung Controlling, intern",
                "Quelle: SAP-Export",
                "",
                "Datum,Region,Umsatz (CHF)",
                "2025-02-01,Ost,510.25",
                "2025-02-02,West,1420.00",
                "2025-02-03,Süd,99.95",
                "2025-02-04,Nord,2380.10",
                "2025-02-05,Ost,745.60",
                "2025-02-06,West,1000.00",
            ]
        ),
    )

    # --- 6. trailing Total row (type-compatible) --------------------------
    footer_rows = [
        ("2025-01-31", "Ost", "Widget", "1200.50"),
        ("2025-02-28", "Ost", "Gadget", "980.25"),
        ("2025-03-31", "West", "Widget", "1543.00"),
        ("2025-04-30", "West", "Gadget", "2200.75"),
        ("2025-05-31", "Süd", "Widget", "175.40"),
        ("2025-06-30", "Süd", "Gadget", "3050.00"),
        ("2025-07-31", "Nord", "Widget", "88.10"),
        ("2025-08-31", "Nord", "Gadget", "4321.05"),
        ("2025-09-30", "Ost", "Widget", "15.60"),
        ("2025-10-31", "West", "Gadget", "762.35"),
    ]
    total = sum((Decimal(r[3]) for r in footer_rows), Decimal("0"))
    assert total == Decimal("14337.00"), total
    write(
        "csv_torture_total_footer.csv",
        lf(
            ["Datum,Region,Produkt,Umsatz"]
            + [",".join(r) for r in footer_rows]
            + [f"2025-12-31,Total,,{total}"]
        ),
    )

    # --- 7. duplicate header names ----------------------------------------
    write(
        "csv_torture_dup_header.csv",
        lf(
            [
                "Buchung,Betrag,Betrag,Waehrung",
                "B-001,100.00,250.00,CHF",
                "B-002,80.50,17.25,CHF",
                "B-003,12.00,999.99,EUR",
            ]
        ),
    )

    # --- 8. blank header cells --------------------------------------------
    write(
        "csv_torture_blank_header.csv",
        lf(
            [
                ",Datum,,Umsatz",
                "1,2025-01-15,Zürich,1200.50",
                "2,2025-02-03,Genève,980.25",
                "3,2025-03-11,Bern,1543.00",
            ]
        ),
    )

    # --- 9. header names needing sanitisation ------------------------------
    write(
        "csv_torture_sanitize_header.csv",
        lf(
            [
                "Datum,Änderung %,Größe (m²),Umsatz (CHF),2025,Region Name",
                "2025-01-15,-3.5,12.75,1200.50,42,Zürich Nord",
                "2025-02-03,0.0,8.20,980.25,17,Genève Ville",
                "2025-03-11,12.25,15.00,1543.00,8,Bern Mitte",
            ]
        ),
    )

    # --- 10. pipe-delimited -------------------------------------------------
    write(
        "csv_torture_pipe.psv",
        lf(
            [
                "Artikel|Beschreibung|Preis|Lager",
                'A-100|12" Monitor, gebraucht|149.00|ja',
                "A-101|Kabel HDMI 2m; schwarz|9.90|ja",
                'A-102|Maus "Pro", kabellos|39.50|nein',
                "A-103|Tastatur DE|59.00|ja",
                "A-104|Dock 4-in-1|129.95|nein",
            ]
        ),
    )

    # --- 11. tab-delimited --------------------------------------------------
    tab_rows = [
        ["Kunden-Nr", "Titel", "Datum", "Betrag"],
        ["0042", '"Der Sturm" (Roman)', "15.01.2025", "19.90"],
        ["0117", "Kurz & Bündig", "03.02.2025", "7.50"],
        ["0042", "Atlas der Alpen; Band 2", "11.03.2025", "124.00"],
        ["0900", "Notizbuch, A5", "27.04.2025", "84.20"],
    ]
    write("csv_torture_tab.tsv", lf(["\t".join(r) for r in tab_rows]))

    # --- 12. exactly one data row -------------------------------------------
    write(
        "csv_torture_one_row.csv",
        lf(["Datum,Region,Umsatz (CHF)", "2025-07-04,Ticino,2750.00"]),
    )

    # --- 13. empty file ------------------------------------------------------
    write("csv_torture_empty.csv", b"")

    # --- 14. header only -----------------------------------------------------
    write("csv_torture_header_only.csv", lf(["Datum,Region,Produkt,Umsatz"]))

    # --- 15. leading '#' comments --------------------------------------------
    write(
        "csv_torture_comments.csv",
        lf(
            [
                "# tdy fixture: csv-torture comments",
                # 3 fields
                "# exported 2026-08-28, department: controlling, format: csv",
                # 4 fields == the modal arity of the data
                "# columns: datum, region, produkt, umsatz",
                "Datum,Region,Produkt,Umsatz",
                "2025-01-15,Ost,Widget,1200.50",
                "2025-02-03,Ost,Gadget,980.25",
                "2025-03-11,West,Widget,1543.00",
                "2025-04-27,West,Gadget,2200.75",
                "2025-05-09,Süd,Ticket #42,175.40",
                "2025-06-30,Nord,Widget,2999.50",
            ]
        ),
    )

    # --- 16. anomalies only in the tail ---------------------------------------
    regions = ["Zürich", "Genève", "Bern", "Basel"]
    start = date(2024, 1, 1)
    body = []
    amounts = []
    swiss_tail = {497: "12'345.60", 498: "9'876.05", 499: "1'002.35"}
    for i in range(500):
        d = start + timedelta(days=i)
        region = regions[i % 4]
        if i in swiss_tail:
            shown = swiss_tail[i]
            value = Decimal(shown.replace("'", ""))
        elif i == 0:
            # Pinned so an assertion can name the first row exactly.
            value = Decimal("50.00")
            shown = "50.00"
        else:
            # Seeded, hence reproducible; no documented assertion depends on
            # these filler amounts.
            cents = 5000 + RNG.randrange(0, 89000)
            value = Decimal(cents) / 100
            shown = f"{value:.2f}"
        amounts.append(value)
        body.append(f"{d.isoformat()},{region},BEL-{i + 1:06d},{shown}")
    big_total = sum(amounts, Decimal("0"))
    # Format the footer the same Swiss way, so it is type-compatible with
    # the tail rows and invisible to a head-only sample.
    int_part, frac = f"{big_total:.2f}".split(".")
    grouped = ""
    while len(int_part) > 3:
        grouped = "'" + int_part[-3:] + grouped
        int_part = int_part[:-3]
    footer_amount = int_part + grouped + "." + frac
    last = (start + timedelta(days=500)).isoformat()
    write(
        "csv_torture_bigtail.csv",
        lf(
            ["Datum,Region,Beleg,Umsatz"]
            + body
            + [f"{last},Total,,{footer_amount}"]
        ),
    )


if __name__ == "__main__":
    build()
