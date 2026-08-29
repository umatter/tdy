#!/usr/bin/env python3
# -*- coding: utf-8 -*-
r"""Fixture generator: encoding and Unicode hell (job key: encodings-unicode).

Run from the repo root:  python3 testdata/gen/06_encodings.py
Deterministic, idempotent, stdlib + openpyxl only. Every file it writes is
named ``testdata/enc_*`` so it can never collide with another generator.
Every invisible character in this source is written as an escape, never as a
literal, so the generator stays readable and greppable.

Nothing here is a syntactically broken file. Every fixture is a *plausible*
file that a colleague could hand you; the damage they do happens in the
decode step, where tdy has exactly one lever (``extraction.encoding``, an
encoding_rs label) and one detector (chardetng, fed the first 12 288 bytes
only -- ``sample.rs`` uses ``max_bytes * 3 / 4`` of a 16 KiB budget).

Two facts about the decode path drive most of the expectations below:

  * ``sample::decode_text`` calls ``Encoding::decode``, which performs BOM
    sniffing. A UTF-8 or UTF-16 BOM therefore *overrides* the label, and the
    BOM is stripped. It also discards the "encoding actually used" and the
    "had errors" flag that ``decode`` returns, so a file can be decoded as
    something other than the label recorded in the sidecar, and malformed
    bytes turn into U+FFFD with no diagnostic at all.
  * encoding_rs only knows WHATWG Encoding Standard labels. ``ibm437`` /
    ``cp437`` is not one of them, so ``encoding = "cp437"`` in a sidecar is
    silently ignored and replaced by chardetng's guess.

---------------------------------------------------------------------------
THE FAMILY: one identical logical table, seven byte-level spellings
---------------------------------------------------------------------------
Logical table for enc_family_*: header ``region,stadt,notiz,umsatz`` plus
6 data rows, comma-delimited, LF line endings, trailing newline.

    Ost  Zürich     Müller & Söhne     1234.50
    Ost  Genève     Café Ähre           987.25
    West Köln       Straße 5            450.00
    West Besançon   größer als ±5°C    1500.75
    Süd  Málaga     ¿Qué?                88.10
    Nord Århus      µ-Test                0.99

A correct parse of ANY family member yields 4 columns
(region utf8, stadt utf8, notiz utf8, umsatz float64), 6 rows,
stadt[0] == "Zürich" (U+00FC, one code point), notiz[3] ==
"größer als ±5°C", and sum(umsatz) == 4261.59.

  enc_family_utf8.csv          parses. The baseline.
  enc_family_utf8_bom.csv      parses. EF BB BF is eaten by BOM sniffing, so
                               the first column is `region`, not
                               `<U+FEFF>region`.
  enc_family_cp1252.csv        parses, if detection lands on windows-1252.
  enc_family_latin1.csv        parses. BYTE-IDENTICAL to the cp1252 file
                               (sha256 f0e329c6...): the payload lives
                               entirely in the A0-FF range the two encodings
                               share. Doubly interchangeable, because the
                               WHATWG label "iso-8859-1" *is* windows-1252 in
                               encoding_rs -- as are "latin1", "ascii" and
                               "us-ascii". tdy has no true ISO-8859-1
                               decoder and cannot be asked for one.
  enc_family_utf16le_bom.csv   parses -- by accident. chardetng cannot detect
                               UTF-16 and will guess an 8-bit encoding; the
                               data survives only because Encoding::decode
                               BOM-sniffs. The label written to the sidecar
                               is therefore a lie about the file.
  enc_family_utf16le_nobom.csv MUST FAIL LOUDLY. No BOM, no detector support.
  enc_family_cp437.csv         MUST FAIL LOUDLY. There is no encoding_rs
                               label for IBM437, so `encoding = "cp437"` in a
                               sidecar is silently dropped and chardetng
                               decides instead. Under windows-1252 the u-with
                               -diaeresis byte 0x81 is unassigned and becomes
                               U+0081, so stadt[0] reads "Z<U+0081>rich":
                               6 code points, wrong ones, no error. The file
                               is unparseable by design and the tool has no
                               way to say so.

---------------------------------------------------------------------------
THE REST
---------------------------------------------------------------------------
  enc_cp1252_only.csv       Uses the 80-9F block (EUR 0x80, RSQUO 0x92,
                            LDQUO/RDQUO 0x93/0x94, HELLIP 0x85, EMDASH 0x97)
                            that windows-1252 fills and iso-8859-1 leaves as
                            C1 controls. The label is not cosmetic here.
  enc_late_1252_byte.csv    43 KB of ASCII with ONE 0x92 byte at offset
                            25 035 -- past the 12 288-byte detection head and
                            before the 4 096-byte tail, so neither the
                            sniffer nor the LLM ever sees it.
  enc_late_invalid_utf8.csv Same geometry, but the head is UTF-8-heavy so
                            detection commits to utf-8; the late lone 0xE9
                            is then not valid UTF-8 and is silently replaced
                            by U+FFFD. Invariant 6, violated quietly.
  enc_nul_byte.csv          A NUL inside a field and a NUL at the end of a
                            field. Must survive; must not truncate.
  enc_mixed_eol.csv         LF, CRLF and lone CR in one file, no trailing
                            newline. All three terminate a csv record.
  enc_emoji_cjk.csv         ZWJ sequence, flag pair, skin-tone modifier, and
                            three CJK headers that sanitize() flattens to
                            col / col_2 / col_3.
  enc_rtl.csv               Hebrew and Arabic, an RLO spoof, RLM/LRM marks,
                            an Arabic comma U+060C that is not a delimiter,
                            and Arabic-Indic digits that must stay utf8.
  enc_normalization.csv     Values that differ only by NFC/NFD/NFKC, and two
                            header cells that render identically but
                            sanitize to different SQL names.
  enc_sheetname_cjk.xlsx    A worksheet whose NAME is CJK + emoji, which the
                            sniffer must round-trip byte-exactly into
                            extraction.sheet_name.

Per-file expectations are spelled out inline next to each builder.
"""

from __future__ import annotations

import datetime
import os
import random
import re
import unicodedata
import zipfile

from openpyxl import Workbook

SEED = 20260828
random.seed(SEED)

HERE = os.path.dirname(os.path.abspath(__file__))
TESTDATA = os.path.abspath(os.path.join(HERE, os.pardir))
REPO = os.path.dirname(TESTDATA)
PREFIX = "enc_"  # derived from job key "encodings-unicode"

# Invisible characters, named once so no fixture hides one in a literal.
NUL = "\x00"
ZWJ = "\u200d"
RLO = "\u202e"           # RIGHT-TO-LEFT OVERRIDE
RLM = "\u200f"           # RIGHT-TO-LEFT MARK
LRM = "\u200e"           # LEFT-TO-RIGHT MARK
ARABIC_COMMA = "\u060c"  # looks like a delimiter, is not one
ANGSTROM = "\u212b"      # ANGSTROM SIGN; NFKC-folds to U+00C5

WRITTEN: list[str] = []


def write(name: str, data: bytes) -> str:
    """Write bytes to testdata/<name> and log it."""
    assert name.startswith(PREFIX), f"fixture {name!r} must start with {PREFIX!r}"
    path = os.path.join(TESTDATA, name)
    with open(path, "wb") as fh:
        fh.write(data)
    WRITTEN.append(path)
    print(f"wrote {os.path.relpath(path, REPO)} ({len(data)} bytes)")
    return path


# ---------------------------------------------------------------------------
# The family: one table, seven spellings
# ---------------------------------------------------------------------------

FAMILY_HEADER = ("region", "stadt", "notiz", "umsatz")
FAMILY_ROWS = [
    ("Ost", "Zürich", "Müller & Söhne", "1234.50"),
    ("Ost", "Genève", "Café Ähre", "987.25"),
    ("West", "Köln", "Straße 5", "450.00"),
    ("West", "Besançon", "größer als ±5°C", "1500.75"),
    ("Süd", "Málaga", "¿Qué?", "88.10"),
    ("Nord", "Århus", "µ-Test", "0.99"),
]


def family_text() -> str:
    lines = [",".join(FAMILY_HEADER)]
    lines += [",".join(r) for r in FAMILY_ROWS]
    return "\n".join(lines) + "\n"


def build_family() -> None:
    text = family_text()

    # Every character must survive the round trip in all five spellings; if
    # this assert fires the payload was edited without checking cp437.
    for codec in ("utf-8", "cp1252", "latin-1", "cp437", "utf-16-le"):
        assert text.encode(codec).decode(codec) == text, codec
    assert sum(float(r[3]) for r in FAMILY_ROWS) == 4261.59

    write(PREFIX + "family_utf8.csv", text.encode("utf-8"))
    write(PREFIX + "family_utf8_bom.csv", b"\xef\xbb\xbf" + text.encode("utf-8"))

    cp1252 = text.encode("cp1252")
    latin1 = text.encode("latin-1")
    # The point of the pair: identical bytes, two equally correct labels.
    assert cp1252 == latin1
    write(PREFIX + "family_cp1252.csv", cp1252)
    write(PREFIX + "family_latin1.csv", latin1)

    utf16 = text.encode("utf-16-le")
    write(PREFIX + "family_utf16le_bom.csv", b"\xff\xfe" + utf16)
    write(PREFIX + "family_utf16le_nobom.csv", utf16)

    cp437 = text.encode("cp437")
    assert cp437 != cp1252  # 0x81 for u-umlaut vs 0xFC: genuinely different
    write(PREFIX + "family_cp437.csv", cp437)


# ---------------------------------------------------------------------------
# windows-1252 only: the 80-9F block that latin-1 leaves as C1 controls
# ---------------------------------------------------------------------------

def build_cp1252_only() -> None:
    r"""Expected: 4 cols (id int64, kunde utf8, preis utf8, notiz utf8), 3 rows.

    With encoding = "windows-1252": kunde[0] == "O’Brien",
    preis[0] == "€1234.50", notiz[1] == "“Sonderpreis” …".
    A hand-written spec with strip = "^€" and decimal(10,2) on `preis`
    must yield exactly 1234.50 / 89.99 / 0.75.

    Which wrong answers are reachable is worth knowing exactly. encoding_rs
    resolves the label "iso-8859-1" to windows-1252, so tdy CANNOT produce
    the classic C1-control misreading ("O" U+0092 "Brien") that Python or
    iconv give for these bytes -- that reading only shows up when the file
    leaves tdy. What tdy can produce is the utf-8 reading, "O<U+FFFD>Brien",
    with no error at all: encoding_rs signals malformed input through a
    return value that decode_text drops on the floor. Silent, and the defect
    this fixture exists to catch.
    """
    text = (
        "id,kunde,preis,notiz\n"
        "1,O’Brien,€1234.50,Grüße — herzlich\n"
        "2,Zoë Ltd,€89.99,“Sonderpreis” …\n"
        "3,Åkerman,€0.75,Café-Ausstattung\n"
    )
    data = text.encode("cp1252")
    for b in (0x80, 0x85, 0x92, 0x93, 0x94, 0x97):
        assert bytes([b]) in data, hex(b)
    write(PREFIX + "cp1252_only.csv", data)


# ---------------------------------------------------------------------------
# One odd byte, far past the detection window
# ---------------------------------------------------------------------------

HEAD_WINDOW = 12288  # sample.rs: max_bytes * 3 / 4 with max_bytes = 16 KiB
TAIL_WINDOW = 4096   # sample.rs: max_bytes / 4
BIG_HEADER = b"id,kunde,betrag\n"   # 16 bytes
BIG_ROWS = 1200                     # -> 44 416 bytes
ODD_ROW = 676                       # 0-based body row; starts at byte 25 028


def _big_row(idx: int, name: bytes) -> bytes:
    """Exactly 37 bytes: 5 id + ',' + 20 name + ',' + 9 amount + LF."""
    assert len(name) <= 20, name
    amount = f"{random.uniform(10.0, 99999.99):9.2f}".encode("ascii")
    row = f"{idx:05d}".encode("ascii") + b"," + name.ljust(20) + b"," + amount + b"\n"
    assert len(row) == 37, (idx, len(row), row)
    return row


def build_late_bytes() -> None:
    r"""Two 44 416-byte files, identical geometry, one poisoned cell each.

    Both: 3 cols (id int64, kunde utf8, betrag float64), 1200 rows, every row
    exactly 37 bytes. id[i] == i (the zero padding is stripped by the int
    cast), and the poisoned row is body row 676, i.e. the row where id == 676.

    enc_late_1252_byte.csv -- byte 0x92 at absolute offset 25 035.
      Correct: kunde[676] == "O’Brien & Co" (windows-1252).
      chardetng sees only ASCII in the head and falls back to windows-1252,
      so this happens to come out right -- for a reason no reader could
      predict from the recorded spec. Re-label the sidecar "utf-8" by hand
      and the value silently becomes "O<U+FFFD>Brien & Co"; re-labelling it
      "iso-8859-1" changes nothing, because encoding_rs resolves that label
      to windows-1252 too. The assertion that matters: kunde[676] must never
      contain U+FFFD, no other row may change, and betrag[676] == 74571.66
      either way (the two big fixtures share one amount column, seeded).

    enc_late_invalid_utf8.csv -- lone byte 0xE9 at absolute offset 25 037,
      followed by 0x20 so it cannot start a valid UTF-8 sequence. The head is
      full of two-byte "Zürich" sequences, so detection commits to utf-8
      and the byte is replaced: kunde[676] == "caf<U+FFFD> Central".
      Correct behaviour is a loud decode error (or "café Central" under
      windows-1252). Everything else in the file parses, so a test that only
      checks row counts and dtypes sails straight past the corruption.
    """
    # --- ASCII body, one cp1252 byte ---
    random.seed(SEED)
    out = bytearray(BIG_HEADER)
    for i in range(BIG_ROWS):
        if i == ODD_ROW:
            out += _big_row(i, b"O\x92Brien & Co")
        else:
            out += _big_row(i, f"Kunde {i:05d}".encode("ascii"))
    data = bytes(out)
    off = data.index(b"\x92")
    assert off == 16 + 37 * ODD_ROW + 7 == 25035, off
    assert HEAD_WINDOW < off < len(data) - TAIL_WINDOW
    assert len(data) == 16 + 37 * BIG_ROWS == 44416
    assert data.count(b"\x92") == 1
    write(PREFIX + "late_1252_byte.csv", data)

    # --- UTF-8 body, one invalid byte ---
    random.seed(SEED)
    out = bytearray(BIG_HEADER)
    for i in range(BIG_ROWS):
        if i == ODD_ROW:
            out += _big_row(i, b"caf\xe9 Central")
        else:
            out += _big_row(i, f"Zürich {i:05d}".encode("utf-8"))
    data = bytes(out)
    off = data.index(b"\xe9")
    assert off == 16 + 37 * ODD_ROW + 9 == 25037, off
    assert data[off + 1] == 0x20  # not a continuation byte -> exactly one U+FFFD
    assert HEAD_WINDOW < off < len(data) - TAIL_WINDOW
    assert len(data) == 44416
    # The detection head really is valid UTF-8 and really is multibyte-heavy.
    data[:HEAD_WINDOW].decode("utf-8")
    assert data[:HEAD_WINDOW].count(b"\xc3\xbc") > 300
    try:
        data.decode("utf-8")
        raise AssertionError("file must not be valid UTF-8 as a whole")
    except UnicodeDecodeError:
        pass
    write(PREFIX + "late_invalid_utf8.csv", data)


# ---------------------------------------------------------------------------
# NUL byte
# ---------------------------------------------------------------------------

def build_nul() -> None:
    r"""Expected: 3 cols (id int64, notiz utf8, betrag float64), 3 rows.

    notiz == ["ok", "ha<NUL>lt", "ende<NUL>"] with <NUL> = U+0000, i.e. 5
    code points and 5 UTF-8 bytes each for rows 1 and 2. U+0000 has no
    Unicode White_Space property, so the str::trim() in build_column must not
    remove the trailing one. sum(betrag) == 60.75.

    A NUL must either survive byte-exactly or be rejected loudly. The failure
    this catches is truncation at the NUL ("ha", "ende"), which is exactly the
    silent corruption invariant 6 forbids. Note that both plausible detection
    outcomes here (utf-8, windows-1252) map 0x00 to U+0000, so the label does
    not change the answer -- only a C-string bug would.
    """
    text = (
        "id,notiz,betrag\n"
        "1,ok,10.50\n"
        f"2,ha{NUL}lt,20.25\n"
        f"3,ende{NUL},30.00\n"
    )
    data = text.encode("utf-8")
    assert data.count(b"\x00") == 2
    write(PREFIX + "nul_byte.csv", data)


# ---------------------------------------------------------------------------
# Mixed line endings
# ---------------------------------------------------------------------------

def build_mixed_eol() -> None:
    r"""Expected: 3 cols (region utf8, monat utf8, umsatz int64), 6 rows.

    The csv crate's default Terminator::CRLF treats CR, LF and CRLF alike as
    record terminators, so all six rows separate correctly and NO value may
    contain a trailing CR: umsatz[0] == 100 (not a parse failure on "100\r")
    and sum(umsatz) == 2100; monat[2] == "Mär"; the file has no final
    terminator at all.

    The divergence worth pinning: an extraction.format = "lines" or
    "fixed_width" spec over the same bytes goes through str::lines(), which
    splits on LF only, so the lone-CR rows fuse -- 4 lines instead of 6, with
    "200\rOst,Mär,300" as one line. Same file, two row counts, decided by
    the extractor.
    """
    data = (
        b"region,monat,umsatz\n"
        b"Ost,Jan,100\r\n"
        b"Ost,Feb,200\r"
        b"Ost,M\xc3\xa4r,300\n"
        b"West,Jan,400\r\n"
        b"West,Feb,500\r"
        b"West,M\xc3\xa4r,600"
    )
    assert data.count(b"\r\n") == 2
    assert data.count(b"\r") - data.count(b"\r\n") == 2  # two lone CRs
    assert not data.endswith(b"\n")
    write(PREFIX + "mixed_eol.csv", data)


# ---------------------------------------------------------------------------
# Emoji and CJK
# ---------------------------------------------------------------------------

def build_emoji_cjk() -> None:
    r"""Expected: 5 cols, 4 rows.

    sniff::sanitize() keeps only [a-z0-9] plus the four German folds, so all
    three CJK headers collapse to nothing and dedupe() renames them: the
    column names are exactly ["id", "col", "col_2", "emoji", "col_3"], with
    source = "地区" / "製品 名" / "备注".
    dtypes: id int64, the other four utf8.

    Grapheme clusters must survive whole:
      emoji[0] == U+1F468 U+200D U+1F469 U+200D U+1F467  (5 cp, 18 bytes)
      emoji[1] == U+1F1E8 U+1F1ED                        (2 cp, 8 bytes)
      emoji[2] == U+1F44D U+1F3FD                        (2 cp, 8 bytes)
      col[0]   == "東京"
    Any sample truncation or column padding that cuts at a byte boundary must
    never split one of these into U+FFFD or a lone surrogate.
    """
    family = "\U0001f468" + ZWJ + "\U0001f469" + ZWJ + "\U0001f467"
    flag_ch = "\U0001f1e8\U0001f1ed"
    thumb = "\U0001f44d\U0001f3fd"
    receipt = "\U0001f9fe"
    assert (len(family), len(family.encode("utf-8"))) == (5, 18)
    assert (len(flag_ch), len(flag_ch.encode("utf-8"))) == (2, 8)
    assert (len(thumb), len(thumb.encode("utf-8"))) == (2, 8)

    header = ("id", "地区", "製品 名", "emoji", "备注")
    rows = [
        ("1", "東京", "ウィジェット", family, "ok"),
        ("2", "上海", "小工具", flag_ch, "テスト"),
        ("3", "서울", "가젯", thumb, "확인"),
        ("4", "Zürich", "Gerät", receipt, "Beleg"),
    ]
    text = ",".join(header) + "\n" + "".join(",".join(r) + "\n" for r in rows)
    for row in rows:
        for cell in row:
            assert "," not in cell and "\n" not in cell
    write(PREFIX + "emoji_cjk.csv", text.encode("utf-8"))


# ---------------------------------------------------------------------------
# RTL, bidi controls, Arabic-Indic digits
# ---------------------------------------------------------------------------

def build_rtl() -> None:
    r"""Expected: 5 cols, 4 rows. Names ["id", "col", "col_2", "menge", "note"]
    -- both Arabic headers sanitize to nothing -- dtypes id int64 and
    everything else utf8. `menge` staying utf8 is the load-bearing part:

      menge == ["١٢٣٤", "٥٦",
                "٧٨٩", "٠"]

    Rust's i64/f64 parsers reject Arabic-Indic digits, which is the right
    answer. A future "helpful" digit fold that turned U+0661 U+0662 U+0663
    U+0664 into 1234 would be precisely the silent corruption invariant 6
    forbids.

    Bidi controls are data and must round-trip byte-exactly, never be
    stripped:
      col[0] contains U+060C ARABIC COMMA -- it renders like the delimiter in
             a terminal but is a 2-byte non-delimiter, so the row still has
             exactly 5 fields.
      col[2] == "photo" U+202E "gnp.js"  (12 cp, renders as "photojs.png")
      col[3] == U+200F "مرحبا" U+200E -- the RLM/LRM
             pair must survive trim(); neither has the White_Space property,
             so a correct implementation keeps them.
    Logical (memory) order, not visual order, defines equality here: eyeball
    verification of this file in a terminal is actively misleading.
    """
    header = ("id", "الشركة",
              "المدينة", "menge", "note")
    rows = [
        ("1",
         "شركة النور"
         + ARABIC_COMMA + " ش.م.م",
         "القدس",
         "١٢٣٤", "arabic-comma"),
        ("2", "מפעל הגליל",
         "תל אביב",
         "٥٦", "hebrew"),
        ("3", "photo" + RLO + "gnp.js", "دبي",
         "٧٨٩", "rlo-spoof"),
        ("4", RLM + "مرحبا" + LRM,
         "الرياض",
         "٠", "rlm-lrm"),
    ]
    text = ",".join(header) + "\n" + "".join(",".join(r) + "\n" for r in rows)
    for row in rows:
        for cell in row:
            assert "," not in cell and "\n" not in cell  # U+060C is not U+002C
    assert text.count(ARABIC_COMMA) == 1
    assert text.count(RLO) == 1 and text.count(RLM) == 1 and text.count(LRM) == 1
    assert len(rows[2][1]) == 12
    write(PREFIX + "rtl.csv", text.encode("utf-8"))


# ---------------------------------------------------------------------------
# Unicode normalisation
# ---------------------------------------------------------------------------

def build_normalization() -> None:
    r"""Expected: 4 cols, 10 rows. tdy normalises nothing, anywhere -- so:

    Column names are ["stadt", "groesse", "gro_sse", "wert"]. Header cells 2
    and 3 are both "Größe" on screen; one is NFC (U+00F6), the other
    NFD (o + U+0308). sanitize() folds NFC 'o-umlaut' to "oe" but drops a bare
    combining mark and inserts an underscore in its place, so two visually
    identical headers become two different SQL identifiers. Assert
    groesse[0] == 10 and gro_sse[0] == 1: that pins which physical column each
    name resolved to, which no amount of squinting at the file can tell you.
    dtypes: stadt utf8, the other three int64. sum(wert) == 55,
    sum(groesse) == 550.

    `stadt` holds five places written twice -- ten byte-distinct strings:
      row 0 "Zürich" NFC (6 cp)     row 1 same, NFD (7 cp)
      row 2 "café" NFC (4 cp)       row 3 same, NFD (5 cp)
      row 4 "한국" (2 cp)        row 5 same, decomposed jamo (6 cp)
      row 6 "ＡBC" (fullwidth A)     row 7 "ABC"
      row 8 "Ångstrom"              row 9 "Ångstrom"
    SELECT count(DISTINCT stadt) must be 10, not 5, and stadt[8] != stadt[9]
    even though both render as "Angstrom" with a ring. That is CORRECT and
    must stay correct: a parser that silently NFC-folded user data would
    collapse rows 8 and 9 and change an aggregate. This fixture exists so the
    10 is pinned by a test and the surprise is documented rather than "fixed".
    """
    nfc = ["Zürich", "café", "한국", "ＡBC", "Ångstrom"]
    twins = [
        ("Zürich", unicodedata.normalize("NFD", "Zürich")),
        ("café", unicodedata.normalize("NFD", "café")),
        ("한국", unicodedata.normalize("NFD", "한국")),
        ("ＡBC", "ABC"),
        ("Ångstrom", ANGSTROM + "ngstrom"),
    ]
    cities = [c for pair in twins for c in pair]
    for a, b in twins:
        assert a != b, a
        assert unicodedata.normalize("NFKC", a) == unicodedata.normalize("NFKC", b)
    assert len(set(cities)) == 10
    assert [len(c) for c in cities] == [6, 7, 4, 5, 2, 6, 3, 3, 8, 8]
    assert nfc  # documented above; kept for the reader

    header_nfc = "Größe"
    header_nfd = unicodedata.normalize("NFD", header_nfc)
    assert header_nfc != header_nfd
    assert unicodedata.normalize("NFC", header_nfd) == header_nfc

    lines = [",".join(("stadt", header_nfc, header_nfd, "wert"))]
    for i, c in enumerate(cities):
        lines.append(f"{c},{(i + 1) * 10},{i + 1},{i + 1}")
    text = "\n".join(lines) + "\n"
    write(PREFIX + "normalization.csv", text.encode("utf-8"))


# ---------------------------------------------------------------------------
# Excel: a sheet NAME that is CJK + emoji
# ---------------------------------------------------------------------------

def build_xlsx() -> None:
    r"""Expected: one sheet named "売上 2025 " + U+1F9FE (9 cp,
    16 UTF-8 bytes), 3 cols, 4 rows.

    The sniffer copies the sheet name from calamine straight into
    extraction.sheet_name, and extract_excel matches it with `==`. If either
    side re-encodes it -- surrogate pair mangled, NFC applied, emoji dropped
    -- the lookup fails loudly with "no sheet named ...". The name must
    survive the whole round trip byte-exactly.

    Column names are ["col", "col_2", "col_3"] (all three headers are CJK and
    sanitize away), sources "地区" / "金額" / "メモ",
    dtypes utf8 / int64 / utf8. col[0] == "東京",
    col_2 == [1200, 980, 1500, 640] with sum 4320, and col_3[3] ==
    U+1F9FE + " 領収書". Excel stores strings as UTF-8 XML, so
    unlike the CSV fixtures there is no encoding to guess: anything that
    comes back wrong here is a bug in the reader, not in detection. Note that
    openpyxl writes the cell text as XML numeric character references
    (&#22320; for the first header cell, &#129534; for the supplementary-
    plane receipt emoji) while the sheet name in workbook.xml is raw UTF-8 --
    so calamine's unescape path and its raw-text path are both exercised, and
    a reader that mishandles astral &#N; references fails on col_3[3] only.
    """
    receipt = "\U0001f9fe"
    name = "売上 2025 " + receipt
    assert len(name) == 9 and len(name.encode("utf-8")) == 16

    wb = Workbook()
    ws = wb.active
    ws.title = name
    ws.append(["地区", "金額", "メモ"])
    ws.append(["東京", 1200, "確定"])
    ws.append(["上海", 980, "未確定"])
    ws.append(["서울", 1500, "مرحبا"])
    ws.append(["Zürich", 640, receipt + " 領収書"])
    assert ws.title == name

    wb.properties.creator = "tdy testdata generator"
    wb.properties.lastModifiedBy = "tdy testdata generator"
    stamp = datetime.datetime(2026, 1, 1, 0, 0, 0)
    wb.properties.created = stamp
    wb.properties.modified = stamp

    tmp = os.path.join(TESTDATA, PREFIX + "sheetname_cjk.xlsx.tmp")
    wb.save(tmp)
    # openpyxl stamps every zip entry with the wall clock; rewrite with a
    # fixed timestamp so regenerating gives byte-identical output (and hence
    # a stable blake3 for sidecar freshness).
    with zipfile.ZipFile(tmp) as zin:
        entries = [(i.filename, zin.read(i.filename)) for i in zin.infolist()]
    os.remove(tmp)
    # openpyxl overwrites dcterms:modified with the wall clock on save, so
    # patch it back to the fixed stamp before repacking.
    stamped = stamp.strftime("%Y-%m-%dT%H:%M:%SZ").encode("ascii")
    entries = [
        (fn, re.sub(rb"(<dcterms:modified[^>]*>)[^<]*(</dcterms:modified>)",
                    rb"\g<1>" + stamped + rb"\g<2>", payload)
         if fn == "docProps/core.xml" else payload)
        for fn, payload in entries
    ]
    out = os.path.join(TESTDATA, PREFIX + "sheetname_cjk.xlsx")
    with zipfile.ZipFile(out, "w", zipfile.ZIP_DEFLATED) as zout:
        for fname, payload in entries:
            info = zipfile.ZipInfo(fname, date_time=(1980, 1, 1, 0, 0, 0))
            info.compress_type = zipfile.ZIP_DEFLATED
            info.external_attr = 0o600 << 16
            zout.writestr(info, payload)
    WRITTEN.append(out)
    print(f"wrote {os.path.relpath(out, REPO)} ({os.path.getsize(out)} bytes)")


def main() -> None:
    build_family()
    build_cp1252_only()
    build_late_bytes()
    build_nul()
    build_mixed_eol()
    build_emoji_cjk()
    build_rtl()
    build_normalization()
    build_xlsx()
    for p in WRITTEN:
        assert os.path.getsize(p) < 2 * 1024 * 1024, p


if __name__ == "__main__":
    main()
