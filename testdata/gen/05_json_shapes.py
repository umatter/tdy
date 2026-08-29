#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""Generate the `json-shapes` fixture family for tdy (testdata/json_shapes/).

Run from the repo root:  python3 testdata/gen/05_json_shapes.py
Deterministic, idempotent, stdlib only. Every file it writes is named
`json_shapes_*` so it cannot collide with another generator's fixtures.

These fixtures target `Extraction::Json` (src/engine.rs::extract_json) and the
tier-1 JSON sniffer (src/sniff.rs::sniff_json). Three implementation facts the
expectations below depend on:

  * serde_json is built WITHOUT `preserve_order`, so `Value::Object` is a
    BTreeMap: keys of a record are visited in BYTEWISE-SORTED order, duplicate
    keys collapse to the LAST occurrence, and a nested value re-serialised by
    `json_scalar` comes back with its keys sorted and no whitespace.
  * The output header is the first-seen union of those sorted key lists across
    ALL records, so a key that only shows up in the final record still becomes
    a column.
  * `json_scalar` flattens JSON null AND absent keys AND the empty string to
    the same "" cell, and the typed cast then turns "" into NULL.

WHAT EACH FIXTURE STRESSES / WHAT A CORRECT PARSE MUST PRODUCE
-------------------------------------------------------------
json_shapes_hetero_keys.ndjson
    Keys appear progressively (one only in the last record); one record omits
    a key; one record repeats a key. -> 6 cols
    [amount f64, id i64, region utf8, currency utf8, note utf8, zz_last utf8],
    7 rows; amount[5] == 2.0 (last duplicate wins), region[3] IS NULL,
    zz_last non-null only in row 7.

json_shapes_nested.ndjson
    Nested objects/arrays, empty {} and [], escapes, non-ASCII, a null object.
    Nested values must serialise BACK to compact JSON strings with sorted keys.
    -> 6 cols [arr, bag, geo, id i64, n i64, tags], 3 rows;
    geo[0] == '{"lat":47.3769,"lon":8.5417}' (source order was lon-then-lat),
    tags[1] == '[]' (a two-char string, NOT null), geo[2] IS NULL.

json_shapes_nulls_vs_missing.ndjson
    null vs missing key vs "" vs the literal string "null" vs "N/A" vs " ".
    -> 3 cols [k, v, w] utf8, 7 rows. Only v[0] == "hello" survives under the
    sniffed spec: the sniffer harvests na_values ["N/A","null"], so two genuine
    string payloads are silently nulled. With na_values = [] a correct spec
    keeps "null" and "N/A" as literals; "" / null / missing / " " remain
    indistinguishable NULLs in every spec.

json_shapes_precision.ndjson
    int64 values f64 cannot hold, and a decimal f64 would ruin.
    -> 5 cols [amount_exact, amount_lossy, id_big i64, id_max, ratio], 3 rows.
    id_big[0] must be exactly 9007199254740993 (never ...992).
    amount_exact[0] carries 19 significant digits as a JSON *string* and must
    survive as decimal(19,12) mantissa 1234567891234567891.
    ratio[0] must survive as decimal(38,34) mantissa
    1000000000000000055511151231257827.
    amount_lossy[0] was written as a JSON *number* and is already f64-rounded
    to 1234567.8912345679 before any spec runs - it must not be typed as an
    exact decimal.

json_shapes_wrapped.json
    {"meta":{...},"warnings":null,"data":[...]} - needs pointer "/data".
    -> 3 cols [amount f64, booking_date date(%Y-%m-%d), region utf8], 3 rows;
    booking_date[0] == 2026-01-31 (Date32 day 20484), amount[0] == 1200.5.

json_shapes_two_arrays.json
    TWO top-level array fields -> the tier-1 sniffer must bail (it does:
    "JSON object has 2 array fields; specify a pointer in the sidecar").
    With pointer "/records": 2 cols [id i64, status utf8], 3 rows.
    With pointer "/errors":  2 cols [code utf8, line i64], 2 rows.

json_shapes_scalar_array.json
    Top-level array of mixed scalars, no objects anywhere.
    -> 1 col [value utf8], 7 rows; value[0] == "1", value[3] IS NULL (JSON
    null), value[6] == "9007199254740993". value[5] is the literal "-", which
    the sniffer's NA heuristic nulls; a correct spec with na_values = [] keeps
    it.

json_shapes_deep_pointer.json
    Records buried at /a/b/c; zero top-level arrays -> sniffer bails
    ("JSON object has 0 array fields"). With pointer "/a/b/c":
    2 cols [celsius f64, sensor utf8], 2 rows; celsius[1] == -3.25.

json_shapes_trailing_newline.ndjson
    Well-formed NDJSON with a blank line, a whitespace-only line, and a final
    newline. -> 2 cols [id i64, v utf8], 4 rows (blank lines dropped, no
    phantom 5th row).

json_shapes_partial_tail.ndjson
    Three good records, a blank line, then a TRUNCATED final record with no
    terminating newline. A correct parse must ERROR, never drop the fragment.
    The current message is `invalid JSON on line 4` although the fragment is
    file line 5 (extract_json enumerates AFTER filtering blank lines).

json_shapes_bom.ndjson
    UTF-8 BOM (EF BB BF) before the first record. The engine handles it
    (encoding_rs strips the BOM) -> 2 cols [city utf8, id i64], 3 rows,
    city[0] == "Zürich". The tier-1 sniffer reads the file with
    fs::read_to_string, which keeps the BOM, so sniff_json currently fails with
    "file has a .json-ish extension but does not parse as JSON".

json_shapes_crlf.ndjson
    CRLF line endings plus JSON escapes for a newline, a tab and a CRLF inside
    string values, and a space-padded value. -> 3 cols [id i64, padded utf8,
    text utf8], 3 rows; text[0] == "line1\nline2" (a real newline inside one
    cell), padded[0] == "spaces" (the cast trims), padded[2] IS NULL.

json_shapes_pointer_escape.json
    The single top-level array field is named "data/2024", so RFC 6901
    requires the pointer "/data~12024". The sniffer emits "/{key}" verbatim =
    "/data/2024", which the engine cannot execute ("JSON pointer
    \"/data/2024\" matched nothing"). With "/data~12024": 2 cols
    [n i64, q utf8], 2 rows.

json_shapes_dirty_keys.ndjson
    JSON keys are arbitrary strings, so the sniffer must sanitise them into
    column names while `source` keeps pointing at the raw key (columns are a
    projection: name <- source). "user id" and "user.id" both sanitise to
    `user_id` and must be de-duplicated; "€" sanitises to nothing; "2024"
    cannot start a name. -> 6 cols, 2 rows, in bytewise key order:
    [c_2024 utf8 <- "2024", betrag_chf f64 <- "Betrag (CHF)" (thousands "'"),
    ok bool <- "ok", user_id i64 <- "user id", user_id_2 i64 <- "user.id",
    col utf8 <- "€"]; betrag_chf[0] == 1234.5, user_id[0] == 1,
    user_id_2[0] == 2, ok[0] == true.

json_shapes_mixed_records.ndjson
    NDJSON mixing objects with a bare array line and a bare string line.
    The header comes from the objects, the non-object rows are one cell wide
    and get padded, so their payload lands in the FIRST column. A correct
    implementation must reject the mixed shapes (or park them in a documented
    column) - it must not file "[1,2,3]" under `id`. Current output:
    3 cols [id utf8(!), name utf8, score i64], 5 rows, id[1] == "[1,2,3]",
    score[1] IS NULL, and confidence stays 0.95.
"""

import json
import random
from datetime import date
from decimal import Decimal, localcontext
from pathlib import Path

random.seed(20260828)  # no randomness is used; fixed for the record

ROOT = Path(__file__).resolve().parents[2]
OUT = ROOT / "testdata" / "json_shapes"

# ---------------------------------------------------------------------------
# A Python mirror of engine.rs::extract_json, used to self-verify the fixtures.
# ---------------------------------------------------------------------------


def json_scalar(v):
    """Mirror of engine.rs::json_scalar (serde_json Map == BTreeMap)."""
    if v is None:
        return ""
    if isinstance(v, str):
        return v
    if isinstance(v, bool):
        return "true" if v else "false"
    if isinstance(v, int):
        return str(v)
    if isinstance(v, float):
        return repr(v)  # shortest round-trip, same as ryu/zmij
    return json.dumps(v, sort_keys=True, separators=(",", ":"), ensure_ascii=False)


def shape(records):
    """(header, rows) exactly as extract_json would build them."""
    header, seen, any_object = [], set(), False
    for rec in records:
        if isinstance(rec, dict):
            any_object = True
            for k in sorted(rec):  # BTreeMap iteration order
                if k not in seen:
                    seen.add(k)
                    header.append(k)
    if not any_object:
        header = ["value"]
    rows = []
    for rec in records:
        if isinstance(rec, dict):
            rows.append([json_scalar(rec[k]) if k in rec else "" for k in header])
        else:
            rows.append([json_scalar(rec)])
    return header, rows


def ndjson_records(text):
    return [json.loads(l) for l in text.split("\n") if l.strip()]


WRITTEN = []


def write(name, data):
    OUT.mkdir(parents=True, exist_ok=True)
    if isinstance(data, str):
        data = data.encode("utf-8")
    path = OUT / name
    path.write_bytes(data)
    WRITTEN.append(path)
    print(f"wrote {path.relative_to(ROOT)} ({len(data)} bytes)")
    return data


def check(cond, msg):
    if not cond:
        raise AssertionError(msg)


# ---------------------------------------------------------------------------
# 1. heterogeneous keys: late keys, a missing key, a duplicate key
# ---------------------------------------------------------------------------
hetero = "\n".join(
    [
        '{"id": 1, "region": "Ost", "amount": "100.50"}',
        '{"amount": "200.00", "id": 2, "region": "West"}',
        '{"region": "Sued", "currency": "CHF", "id": 3, "amount": "300.25"}',
        '{"id": 4, "amount": "50.00"}',
        '{"note": "late key", "id": 5, "region": "Nord", "amount": "10.00", "currency": "EUR"}',
        '{"id": 6, "region": "Ost", "amount": "1.00", "amount": "2.00"}',
        '{"id": 7, "region": "West", "amount": "7.00", "zz_last": "only in the final record"}',
    ]
) + "\n"
write("json_shapes_hetero_keys.ndjson", hetero)
h, r = shape(ndjson_records(hetero))
check(h == ["amount", "id", "region", "currency", "note", "zz_last"], f"hetero header {h}")
check(len(r) == 7, "hetero rows")
check(r[5][0] == "2.00", "hetero duplicate key must keep the last value")
check(r[3][2] == "", "hetero missing region must be empty")
check(r[6][5] == "only in the final record", "hetero last-record key")

# ---------------------------------------------------------------------------
# 2. nested objects / arrays -> compact JSON strings with sorted keys
# ---------------------------------------------------------------------------
nested = "\n".join(
    [
        '{"id": 1, "n": 10, "geo": {"lon": 8.5417, "lat": 47.3769}, '
        '"tags": ["a", "b"], "bag": {}, "arr": []}',
        '{"id": 2, "n": 20, "geo": {"lat": 46.9481, "lon": 7.4474, "alt": 542}, '
        '"tags": [], "bag": {"k": null}, "arr": [1, [2, 3]]}',
        '{"id": 3, "n": 30, "geo": null, "tags": ["\\u00e4", "\\u00f6"], '
        '"bag": {"note": "quote \\" and backslash \\\\"}, '
        '"arr": [{"deep": {"deeper": [true, false, null]}}]}',
    ]
) + "\n"
write("json_shapes_nested.ndjson", nested)
h, r = shape(ndjson_records(nested))
check(h == ["arr", "bag", "geo", "id", "n", "tags"], f"nested header {h}")
check(r[0][2] == '{"lat":47.3769,"lon":8.5417}', f"nested geo[0] {r[0][2]!r}")
check(r[1][2] == '{"alt":542,"lat":46.9481,"lon":7.4474}', f"nested geo[1] {r[1][2]!r}")
check(r[2][2] == "", "nested geo[2] must be empty (JSON null)")
check(r[1][5] == "[]", "nested tags[1] must be the two-char string []")
check(r[2][5] == '["ä","ö"]', f"nested tags[2] {r[2][5]!r}")
check(r[2][1] == '{"note":"quote \\" and backslash \\\\"}', f"nested bag[2] {r[2][1]!r}")
check(r[2][0] == '[{"deep":{"deeper":[true,false,null]}}]', f"nested arr[2] {r[2][0]!r}")

# ---------------------------------------------------------------------------
# 3. null vs missing vs "" vs "null" vs "N/A" vs " "
# ---------------------------------------------------------------------------
nulls = "\n".join(
    [
        '{"k": "a", "v": "hello", "w": "x"}',
        '{"k": "b", "v": null, "w": "y"}',
        '{"k": "c", "w": "z"}',
        '{"k": "d", "v": "", "w": "w"}',
        '{"k": "e", "v": "null", "w": "v"}',
        '{"k": "f", "v": "N/A", "w": "u"}',
        '{"k": "g", "v": " ", "w": "t"}',
    ]
) + "\n"
write("json_shapes_nulls_vs_missing.ndjson", nulls)
h, r = shape(ndjson_records(nulls))
check(h == ["k", "v", "w"], f"nulls header {h}")
check([row[1] for row in r] == ["hello", "", "", "", "null", "N/A", " "], "nulls v column")

# ---------------------------------------------------------------------------
# 4. int64 / decimal precision
# ---------------------------------------------------------------------------
precision = "\n".join(
    [
        '{"id_big": 9007199254740993, "id_max": 9223372036854775807, '
        '"amount_exact": "1234567.891234567891", "amount_lossy": 1234567.891234567891, '
        '"ratio": "0.1000000000000000055511151231257827"}',
        '{"id_big": -9007199254740993, "id_max": -9223372036854775808, '
        '"amount_exact": "-9999999.999999999999", "amount_lossy": 0.005, '
        '"ratio": "0.2000000000000000111022302462515654"}',
        '{"id_big": 42, "id_max": 0, '
        '"amount_exact": "0.000000000001", "amount_lossy": -0.25, '
        '"ratio": "0.2999999999999999888977697537484345"}',
    ]
) + "\n"
write("json_shapes_precision.ndjson", precision)
h, r = shape(ndjson_records(precision))
check(h == ["amount_exact", "amount_lossy", "id_big", "id_max", "ratio"], f"precision header {h}")
check(r[0][2] == "9007199254740993", "id_big must survive verbatim")
check(r[0][3] == "9223372036854775807", "id_max must survive extraction verbatim")
check(r[0][1] == "1234567.8912345679", f"amount_lossy already f64-rounded: {r[0][1]!r}")
with localcontext() as ctx:  # engine.rs::parse_decimal is exact i128 arithmetic
    ctx.prec = 60
    check(int(Decimal(r[0][0]).scaleb(12)) == 1234567891234567891, "amount_exact decimal(19,12)")
    check(int(Decimal(r[0][4]).scaleb(34)) == 1000000000000000055511151231257827,
          "ratio decimal(38,34)")
check(float(r[0][3]) != 9223372036854775807, "id_max as f64 must be provably wrong")

# ---------------------------------------------------------------------------
# 5. wrapped document -> pointer /data
# ---------------------------------------------------------------------------
wrapped = """{
  "meta": {
    "generated_at": "2026-08-28T10:00:00Z",
    "source": "erp",
    "page": 1,
    "nested": {"note": "a nested object must not be mistaken for the records array"}
  },
  "warnings": null,
  "data": [
    {"booking_date": "2026-01-31", "region": "Ost", "amount": 1200.5},
    {"booking_date": "2026-02-28", "region": "West", "amount": 980.25},
    {"booking_date": "2026-03-31", "region": "Sued", "amount": 1500}
  ]
}
"""
write("json_shapes_wrapped.json", wrapped)
doc = json.loads(wrapped)
check([k for k, v in doc.items() if isinstance(v, list)] == ["data"], "wrapped: exactly one array")
h, r = shape(doc["data"])
check(h == ["amount", "booking_date", "region"], f"wrapped header {h}")
check(r[0] == ["1200.5", "2026-01-31", "Ost"], f"wrapped row0 {r[0]}")
EPOCH_DAY_20260131 = (date(2026, 1, 31) - date(1970, 1, 1)).days
check(EPOCH_DAY_20260131 == 20484, f"date32 day {EPOCH_DAY_20260131}")

# ---------------------------------------------------------------------------
# 6. two array fields -> the sniffer is expected to bail
# ---------------------------------------------------------------------------
two_arrays = """{
  "meta": {"job": "nightly-export", "page": 1},
  "records": [
    {"id": 1, "status": "ok"},
    {"id": 2, "status": "ok"},
    {"id": 3, "status": "late"}
  ],
  "errors": [
    {"code": "E1", "line": 7},
    {"code": "E2", "line": 19}
  ]
}
"""
write("json_shapes_two_arrays.json", two_arrays)
doc = json.loads(two_arrays)
check(sorted(k for k, v in doc.items() if isinstance(v, list)) == ["errors", "records"],
      "two_arrays: two array fields")
check(shape(doc["records"])[0] == ["id", "status"], "two_arrays /records header")
check(shape(doc["errors"])[0] == ["code", "line"], "two_arrays /errors header")

# ---------------------------------------------------------------------------
# 7. array of scalars
# ---------------------------------------------------------------------------
scalars = '[1, 2.5, true, null, "three", "-", 9007199254740993]\n'
write("json_shapes_scalar_array.json", scalars)
h, r = shape(json.loads(scalars))
check(h == ["value"], f"scalar header {h}")
check([row[0] for row in r] == ["1", "2.5", "true", "", "three", "-", "9007199254740993"],
      f"scalar rows {r}")

# ---------------------------------------------------------------------------
# 8. deep pointer /a/b/c
# ---------------------------------------------------------------------------
deep = """{
  "version": 3,
  "a": {
    "b": {
      "c": [
        {"sensor": "t1", "celsius": 21.5},
        {"sensor": "t2", "celsius": -3.25}
      ]
    }
  }
}
"""
write("json_shapes_deep_pointer.json", deep)
doc = json.loads(deep)
check([k for k, v in doc.items() if isinstance(v, list)] == [], "deep: zero top-level arrays")
h, r = shape(doc["a"]["b"]["c"])
check(h == ["celsius", "sensor"], f"deep header {h}")
check(r[1] == ["-3.25", "t2"], f"deep row1 {r[1]}")

# ---------------------------------------------------------------------------
# 9. trailing newline + blank / whitespace-only lines
# ---------------------------------------------------------------------------
trailing = (
    '{"id": 1, "v": "a"}\n'
    '{"id": 2, "v": "b"}\n'
    "\n"
    '{"id": 3, "v": "c"}\n'
    "   \n"
    '{"id": 4, "v": "d"}\n'
)
raw = write("json_shapes_trailing_newline.ndjson", trailing)
check(raw.endswith(b"\n"), "trailing newline fixture must end with LF")
h, r = shape(ndjson_records(trailing))
check(h == ["id", "v"] and len(r) == 4, f"trailing shape {h} {len(r)}")

# ---------------------------------------------------------------------------
# 10. truncated final line (no trailing newline) -> must be an error
# ---------------------------------------------------------------------------
partial = (
    '{"id": 1, "region": "Ost", "amount": 10}\n'
    "\n"
    '{"id": 2, "region": "West", "amount": 20}\n'
    '{"id": 3, "region": "Sued", "amount": 30}\n'
    '{"id": 4, "region": "No'
)
raw = write("json_shapes_partial_tail.ndjson", partial)
check(not raw.endswith(b"\n"), "partial fixture must not end with LF")
lines = partial.split("\n")
check(len(lines) == 5 and lines[4].startswith('{"id": 4'), "partial: fragment is file line 5")
nonblank = [l for l in lines if l.strip()]
check(nonblank.index(lines[4]) + 1 == 4, "partial: engine will report line 4, not 5")
try:
    json.loads(lines[4])
    raise AssertionError("partial fragment must not parse")
except json.JSONDecodeError:
    pass

# ---------------------------------------------------------------------------
# 11. UTF-8 BOM
# ---------------------------------------------------------------------------
bom_body = (
    '{"id": 1, "city": "Zürich"}\n'
    '{"id": 2, "city": "Genève"}\n'
    '{"id": 3, "city": "Bern"}\n'
)
raw = write("json_shapes_bom.ndjson", b"\xef\xbb\xbf" + bom_body.encode("utf-8"))
check(raw[:3] == b"\xef\xbb\xbf", "BOM fixture must start with EF BB BF")
h, r = shape(ndjson_records(bom_body))
check(h == ["city", "id"], f"bom header {h}")
check(r[0] == ["Zürich", "1"], f"bom row0 {r[0]}")

# ---------------------------------------------------------------------------
# 12. CRLF endings + escaped control characters inside values
# ---------------------------------------------------------------------------
crlf_lines = [
    '{"id": 1, "text": "line1\\nline2", "padded": "  spaces  "}',
    '{"id": 2, "text": "tab\\there", "padded": "x"}',
    '{"id": 3, "text": "crlf\\r\\nembedded", "padded": ""}',
]
crlf = "\r\n".join(crlf_lines) + "\r\n"
raw = write("json_shapes_crlf.ndjson", crlf)
check(raw.count(b"\r\n") == 3 and b"\n\n" not in raw, "crlf fixture must use CRLF only")
h, r = shape([json.loads(l) for l in crlf.split("\r\n") if l.strip()])
check(h == ["id", "padded", "text"], f"crlf header {h}")
check(r[0][2] == "line1\nline2", f"crlf text[0] {r[0][2]!r}")
check(r[0][1] == "  spaces  " and r[0][1].strip() == "spaces", "crlf padded[0]")
check(r[2][2] == "crlf\r\nembedded", f"crlf text[2] {r[2][2]!r}")

# ---------------------------------------------------------------------------
# 13. array field whose key needs RFC 6901 escaping
# ---------------------------------------------------------------------------
esc = """{
  "meta": {"note": "the records array key contains a slash"},
  "data/2024": [
    {"q": "Q1", "n": 1},
    {"q": "Q2", "n": 2}
  ]
}
"""
write("json_shapes_pointer_escape.json", esc)
doc = json.loads(esc)
arrays = [k for k, v in doc.items() if isinstance(v, list)]
check(arrays == ["data/2024"], f"escape: single array key {arrays}")
check(shape(doc["data/2024"])[0] == ["n", "q"], "escape header")

# ---------------------------------------------------------------------------
# 14. arbitrary JSON keys -> sanitised, de-duplicated column names
# ---------------------------------------------------------------------------


def sanitize(name):
    """Port of sniff.rs::sanitize."""
    out, prev_us = "", False
    for ch in name.strip():
        mapped = {"ä": "ae", "ö": "oe", "ü": "ue", "ß": "ss"}.get(ch)
        if mapped is None and ch.isascii() and ch.isalnum():
            mapped = ch.lower()
        if mapped is not None:
            out += mapped
            prev_us = False
        elif not prev_us and out:
            out += "_"
            prev_us = True
    out = out.rstrip("_")
    if not out:
        return "col"
    return f"c_{out}" if out[0].isdigit() else out


def dedupe(names):
    """Port of sniff.rs::dedupe."""
    seen, out = {}, []
    for n in names:
        seen[n] = seen.get(n, 0) + 1
        out.append(n if seen[n] == 1 else f"{n}_{seen[n]}")
    return out


dirty = "\n".join(
    [
        '{"Betrag (CHF)": "1\'234.50", "user id": 1, "user.id": 2, '
        '"\\u20ac": "euro sign only", "2024": "starts with digit", "ok": true}',
        '{"Betrag (CHF)": "2\'000.00", "user id": 3, "user.id": 4, '
        '"\\u20ac": "another", "2024": "also text", "ok": false}',
    ]
) + "\n"
write("json_shapes_dirty_keys.ndjson", dirty)
h, r = shape(ndjson_records(dirty))
check(h == ["2024", "Betrag (CHF)", "ok", "user id", "user.id", "€"], f"dirty header {h}")
check(dedupe([sanitize(k) for k in h])
      == ["c_2024", "betrag_chf", "ok", "user_id", "user_id_2", "col"],
      f"dirty names {dedupe([sanitize(k) for k in h])}")
check(r[0] == ["starts with digit", "1'234.50", "true", "1", "2", "euro sign only"],
      f"dirty row0 {r[0]}")
check(float(r[0][1].replace("'", "")) == 1234.5, "dirty betrag value")

# ---------------------------------------------------------------------------
# 15. NDJSON mixing object records with bare array / string records
# ---------------------------------------------------------------------------
mixed = "\n".join(
    [
        '{"id": 1, "name": "a", "score": 10}',
        "[1, 2, 3]",
        '{"id": 2, "name": "b", "score": 20}',
        '"just a string"',
        '{"id": 3, "name": "c", "score": 30}',
    ]
) + "\n"
write("json_shapes_mixed_records.ndjson", mixed)
h, r = shape(ndjson_records(mixed))
check(h == ["id", "name", "score"], f"mixed header {h}")
check(r[1] == ["[1,2,3]"] and r[3] == ["just a string"], f"mixed narrow rows {r[1]} {r[3]}")
# after rectangularize (PadNulls, target = header width) the payload lands in col 0
padded_rows = [row + [""] * (len(h) - len(row)) for row in r]
check(padded_rows[1] == ["[1,2,3]", "", ""], "mixed: array payload lands under `id`")
check(padded_rows[3] == ["just a string", "", ""], "mixed: string payload lands under `id`")

for p in WRITTEN:
    size = p.stat().st_size
    check(0 < size < 2 * 1024 * 1024, f"{p} size {size}")
