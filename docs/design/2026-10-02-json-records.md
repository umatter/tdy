# A document is a record: JSON files that hold one object

*Design, 2026-10-02. Status: implemented 2026-10-02 on `json-records` (taxonomy
D6, and the largest group of files the corpus survey calls "unsure" or
"declined"); swept against villagerdb's 483 villagers and 7,443 items — the
numbers are in CLAUDE.md.*

## 1. Why

Of the 8,136 JSON files in the corpus, about 7,900 are one object per file:
`villagerdb`'s 7,444 items and 483 villagers, each `{id, name, category,
games: {…}}`. tdy has no reading for them. A root object with no array inside is
declined ("this JSON document contains no array of records"); a root object that
happens to hold a small array (`games.nl.buyPrices`, one element) is read *as
that array*, at confidence 0.70 — a one-row table of buy prices in place of the
item. The record is the document, and the table is the directory.

Everything a pile needs already exists: members, the lock, drift, the review
gate, `source_name`, provenance. What is missing is the frame — "this document
is one record" — and a way for a declared column to reach a nested field.

## 2. The frame

`Extraction::Json` gains one field:

```toml
[spec.extraction]
format = "json"
record = true            # the value at `pointer` (the root when absent) is ONE object: one row
```

- `record = true` with `lines = true` is refused by `validate()`; `pointer`
  still selects where the object is (`pointer = "/data"`).
- The value must be an object. An array, a scalar or `null` there is an error
  naming what was found — never coerced.
- The header is that object's keys in document order; a nested object or array
  is a cell of compact JSON text, exactly as a nested value inside a record
  array is today (`engine::json_scalar`). Nothing is flattened at extraction.
- It is **never implied**. A sidecar without `record` whose pointer lands on an
  object keeps today's error, so no existing sidecar changes meaning.
- One row cannot stream and does not need to: `can_stream` stays false for it.

## 3. Sniffing a lone file

`sniff_json`, for a root that is an object:

| the object holds | today | now |
|---|---|---|
| no array anywhere | declined | read as one record (`record = true`), with the note "this document is one object and is read as one record" |
| one or more arrays | the ranked array is read, 0.25 doubt when several | unchanged reading; the note also names the alternative: "…or the document itself as one record (`record = true`)" |

Keeping the second row's reading is deliberate: `messy('report.json')` on a
document whose point is its `rows` array must not start returning one row. The
doubt already says a person should look.

## 4. In a pile, the target decides

`fit` already tries a declared table against every enumerable frame — record
arrays, sheets — and calls exactly one survivor a proof (`fit_by_elimination`).
"The document is one record" joins the candidates for a root object:

- candidates = the document as one record, plus every record array
  `json_record_pointers` finds;
- exactly one passes the gates → that frame, with the note "frame proved by
  elimination", no review;
- several pass → `FitError::AmbiguousFrame` naming them and the two sidecar
  fields that settle it (`record`, `pointer`);
- none → the ordinary gap report, for the ranked candidate.

A root object with no array has one candidate and needs no elimination.

## 5. Reaching a nested field

A target column may say where in the record its value lives:

```sql
sell_price BIGINT OPTIONS(matches = 'games', pointer = '/nh/sellPrice/value')
```

- `matches` (or the column's own name) binds the top-level key, as for any
  column; `pointer` is an RFC 6901 pointer **into that key's value**. The
  planner writes it as the `ColumnSpec.pointer` the sidecar has had since the
  shape slice — no new executor code.
- Semantics are the existing ones: a pointer that resolves to nothing is null
  (so a NOT NULL column refuses the file, naming the row); one that lands on an
  object or an array is an error; a numeric token indexes an array; there is no
  wildcard and no fan-out.
- Refused by `Target::parse` when it does not start with `/`, when set twice,
  and — at fit time — on a member that is not JSON ("`pointer` reads inside a
  JSON value, and this file is read as …", the sidecar's own rule).
- Part of `target_hash`, hashed only when declared, inside its column's segment.
- A key absent from one document is absent from that member's header: the
  column needs `if_missing = 'null'` like any column a file may lack. Inside one
  record array an absent key is silently empty; across one-record members it is
  a declaration, which is the stricter and the right reading.

## 6. Draft

`tdy draft` over JSON documents:

- a document sniffed as one record contributes its top-level keys as columns,
  and every **scalar leaf** under a nested object down to depth 4 as a column
  named from its path (`games_nh_sellprice_value`), declared with `matches` for
  the top-level key and `pointer` for the rest;
- a nested **array** is not descended into; its key is drafted as one TEXT
  column of JSON, with a comment saying so;
- presence is counted per file as today, so a leaf that exists in 3,100 of
  7,444 documents says "in 3100 of 7444 file(s)" and the person adds
  `if_missing = 'null'` — the draft reports, it does not declare absence;
- `regions_of` is not run over JSON text (a blank line inside pretty-printed
  JSON is not a table boundary).

## 7. What a pile of one-row members costs, and what is deferred

Measured on 5,000 one-row members after the magnitude floor (0.3.1): see
CLAUDE.md. One sidecar per member is kept: a spec shared across members would
touch the sidecar, the lock, drift, `dataset()`, the console and the workbench,
and is its own slice. Known and accepted for now: thousands of sidecar files
beside the data; `--accept` is per member; one malformed document blocks the
lock (no partial lock, by design).

Out of scope: recursive globs (`**`); fan-out over an array inside a record
(one row per element); flattening at extraction; a shared spec.

## 8. Tests

- unit: `validate()` for `record` with `lines`; extraction of an object, and
  the error for an array/scalar/null at the pointer;
- `tests/formats.rs`: a hand-written `record = true` spec, with and without
  `pointer`, with a column `pointer` into a nested object;
- `tests/fit.rs`: a pile of one-object documents fits by name; a nested leaf
  binds through `OPTIONS(pointer = …)`; a document with both a record reading
  and an array reading that both fit is `AmbiguousFrame`; one where only the
  record reading fits is proved by elimination; a NOT NULL column whose pointer
  resolves to nothing refuses the member naming it;
- `tests/draft.rs`: the unedited draft of a pile of one-object documents with
  identical keys fits every file; leaves get `pointer`; an array key is one
  TEXT column;
- generator `20_json_records.py`: a small pile modelled on the corpus items
  (some with `games.nh`, some with `games.nl`, one with a nested array), with
  ground-truth sums;
- the corpus: draft → edit → fit → query over `villagerdb`'s items and
  villagers, row count equal to file count, a few values pinned by hand, with
  the timings recorded.
