# The shape slice

*2026-09-06. Four operators that close the structural gaps the operator
catalogue found, plus the two small parse additions that ride with them. Grounded
in `docs/design/2026-09-05-munging-taxonomy.md`, which is the argument for why
these four and not the other ten.*

---

## 1. What this is, and why these four together

The catalogue put 110 munging operators against tdy and produced fourteen gaps.
This slice takes the three that stop a file being readable **at all** — plus one
two-line sibling — and leaves the rest, because they are not independent wishes:

| | Catalogue | Closes | Also unblocks |
|---|---|---|---|
| **S1 `split_column`** | D3 | packed columns | D7 multi-stub reshape, E20 missingness reasons, half of Potter's Wheel's algebra |
| **S2 `transpose`** | C8 | variables-down-the-left files | nothing — but nothing else can reach them either |
| **S3 `source_name` + `WITH (provenance)`** | C9, H4 | the period that only exists in the filename | error messages that name a member and a line |
| **S4 `fill_down.direction`** | G2 | labels written at the *bottom* of a group | — |

And riding along, because each is a morning's work in code already touched:

| | Catalogue | Why now |
|---|---|---|
| **S5 per-column JSON `pointer`** | D6 | the last thing an ordinary pile can contain that no declaration can reach |
| **S6 `.gz` inputs** | A6 | monthly exports arrive zipped; `flate2` is already in the tree via `zip` |
| **S7 epoch parse** | E21 | a ten-digit integer that is a timestamp, declarable the same way a serial date should be |

The shape of every one of them is the shape the codebase already knows: a variant
in `spec.rs` → an arm in the executor → a rule in `validate()` → `schema_of` over
zero rows → streaming support or an honest fallback → an equality test between the
two executors. Nothing here needs a new concept.

## 2. Decisions

### D1 — the sniffer proposes, the target disposes, and neither guesses

Every operator here is **declared, not inferred**, with one carefully bounded
exception (D2). The sniffer may *notice* the shape and say so in a note — that is
what it already does for accounting negatives — but it will not restructure a
table on a hunch. The reason is the same one that governs `decimal_shift`: a
restructuring that is wrong produces a table that parses, types and conforms, and
is nonsense.

What *may* act on these operators is `fit`, because `fit` is handed a declaration
saying what the answer must look like, and that turns a guess into a search with a
checkable result.

### D2 — transpose is proved by elimination, not chosen by a heuristic

The exception, and it is exactly the machinery slice 5 already built for frames.
`fit` tries the declared table against the file read normally and against the file
read transposed. If exactly one produces columns that type — **proved by
elimination**, note, no review. If both do, that is `FitError::AmbiguousFrame`
naming both readings and the sidecar field that settles it. If neither, the
ordinary gap report.

This is not a new rule; it is `sniff::json_record_pointers` and
`sniff::frame_excel_sheet` with a third kind of candidate. A file that reads as a
table in two orientations *and* satisfies the same declaration in both is
genuinely ambiguous, and saying so is the right answer.

### D3 — a split that is total and typed needs no review; choosing where to split does

`split_column` is mechanically checkable in a way `decimal_shift` never was: if
every non-null value yields exactly *N* parts and each part builds under
`engine::build_column_at`, nothing was invented and nothing was lost. So a
**hand-written** split is not review-gated.

A split the **planner proposes** is, because choosing the delimiter and deciding
which part is `first_name` rather than `last_name` is a judgement no property of
the file settles. `fit::review_reasons` gains one reason; `--propose` ranks
candidate splits the way it already ranks candidate columns.

The gate is on the *provenance* of the step, not on the operator. That
distinction already exists — a `method = "manual"` sidecar is left alone by
`tdy fit` and still proved — and this is the first operator to use both sides
of it.

### D4 — provenance columns are opt-in, and they are part of the target's meaning

`_file` and `_row` do not appear unless a target says `WITH (provenance = true)`,
because a dataset's schema is what the declaration says it is and no column may
appear that the declaration did not ask for. They are therefore part of
`target_hash`: turning provenance on voids the existing proofs, exactly as
`if_missing_null` does. That is the right cost — the schema changed.

`source_name`, by contrast, is an ordinary transform in a sidecar: it adds one
declared column whose value is derived from the member's own path. Derived, not
invented, so **no review** — the same argument that lets `if_missing = 'null'`
null-fill without one.

### D5 — what stays out, and why it stays out of *this* slice specifically

- **`pivot` (long → wide, D2 in the catalogue).** The asymmetry with `unpivot` is
  real and deliberate for now: no pile in the corpus arrives long-format, and
  adding a spec-level pivot means `schema_of` can no longer derive a schema
  without reading data — the output columns *are* the data. That is a different
  and much larger change; it would have to be a declared list of expected keys,
  and that is a design conversation, not a transform.
- **Sheets and regions as members (C9's other half, B10).** Touches the lock, the
  sidecar's identity, `fit_pile` and `dataset()`. Its own document.
- **Indentation as hierarchy (C10), named timezones (E16), anything
  probabilistic.** Reasons recorded in the catalogue.

## 3. The operators

### S1 · `split_column` — **built, 2026-09-06**

```toml
[[spec.transforms]]
op = "split_column"
source = "name"                    # a post-transform column, by its file spelling
into = ["first_name", "last_name"] # exactly the parts produced
by = { kind = "delimiter", value = ", " }
#     { kind = "positions", at = [4, 6] }        -- character offsets, like fixed_width
#     { kind = "regex", pattern = "^(\\d{4})-Q([1-4])$" }  -- capture groups, in order
on_short = "error"                 # or "null"
```

**Two changes from the design as written**, both simplifications found while
building it:

- **`limit` is gone.** `into.len()` already determines how many parts to make,
  so a separate limit could only ever disagree with it. The split stops after
  `into.len()` parts, which is what makes the operation total.
- **Capture groups are positional, not named.** `into` supplies the names;
  requiring the pattern to repeat them is a second place for them to disagree.
  `validate` checks the group count against `into.len()`.

And one deferral: it runs on the **materialising executor**. It rewrites the
header's width as well as each row's, and the streaming planner establishes the
header once up front — the same reason `constant` falls back. No spec is
refused for it, only executed the older way.

**Semantics.** Runs in transform order, on the string table, before typing —
where every other structural operator runs. The source column is **replaced** by
its parts, in place, so the header keeps its position and `columns` addresses the
parts by name.

**Totality is the whole safety argument.** A row that does not yield exactly
`into.len()` parts is an **error naming the row**, never a short row padded with
nulls. Padding is how a split silently loses the second half of every value that
happened to contain no comma. Two escape hatches, both declared:
`limit` (split into at most N, remainder stays in the last part — the standard
`split(sep, maxsplit)` semantics) and `on_short = "error" | "null"`, defaulting to
`error`.

**`validate()`** refuses: an empty or single-element `into`; a name in `into` that
collides with an existing column (the projection would silently bind one of two);
a `positions` list not strictly increasing; a `regex` whose named-group count
differs from `into.len()`; a `delimiter` that is empty.

**Streaming.** Row-local, so it joins `RowOp` and runs in spec order alongside
`drop_rows_matching` and `fill_down` — `tests/streaming.rs` gains it in the same
equality sweep. It changes the header's width, which `promote_header` has already
established by then, so `can_stream` keeps it after the header stage and before
`unpivot`.

**Catalogue debt it pays:** D3 directly; D7 becomes expressible as
`unpivot` → `split_column` on the variable column (though not in one operation);
E20's missingness reasons become a declarable second column.

### S2 · `transpose` — **built, 2026-09-06**

```toml
[[spec.transforms]]
op = "transpose"
```

**`header_from` is gone.** After the flip, the values that were the first
column *are* the first row, so `promote_header` reads them with no help — the
option would have been a second way to say the same thing, and §6's second
question dissolves with it: the header a `matches` clause addresses is the
file's own spelling of those labels, by construction.

A truncated table is refused rather than flipped: every row a partial read never
saw would have been a **column**, so the result is the wrong shape rather than
merely short. That is `skip_rows`'s tail rule, one step stronger.

**Semantics.** Rows become columns. Runs **first** among transforms — before
`promote_header`, since after transposing it is the *first column* that holds what
were the header cells. `header_from = "first_column"` moves them into the header
directly, which is the common case and saves a second declaration.

**Bounded, and only on a materialised table.** A transpose needs the whole table
in memory by definition: the first output row cannot be emitted until the last
input row is read. `can_stream` returns false, the materialising executor handles
it, and `[limits]`'s cell ceiling applies as it does to Excel. This is stated
rather than worked around — a file laid out this way is a report, and reports are
small.

**`validate()`** refuses a transpose after any transform that has already
established a header (`promote_header`), because the two disagree about which
direction the names run.

**Detection, not application.** `sniff` gains a note when the first column's
values are unique non-numeric labels and the header row's values type
homogeneously — the mechanical signature of a transposed table. It never applies
it (D1). `fit` may, by elimination (D2).

### S3 · `source_name`, and `WITH (provenance = true)` — **built, 2026-09-06**

```toml
[[spec.transforms]]
op = "source_name"
name = "jahr"
from = "file_stem"          # file_stem | file_name | sheet | path
pattern = "(\\d{4})"        # optional: the capture becomes the value
```

**Semantics.** Adds one column whose value is derived from where the data came
from. Like `Constant` it may only add, never shadow. Unlike `Constant` it is
**not** review-gated: the value is read off the member's own path, so it is data
tdy derived rather than data tdy was told. A `pattern` that does not match is an
error, not an empty column — a silently empty `jahr` on one member of twelve is
the failure this exists to prevent.

`from = "sheet"` is accepted and resolves to the sidecar's declared sheet, which
is useful today even though one file still contributes one sheet; the multi-sheet
question is its own document.

**And separately**, on a target:

```sql
CREATE TABLE umsatz (...) WITH (provenance = true);
```

which adds `_member` (the lock-relative path) and `_row` (1-based within that
member) to the declared schema. Part of `target_hash` (D4). `dataset()` fills
them; `messy()` ignores the option since it has no lock.

### S4 · `fill_down.direction`

```toml
[[spec.transforms]]
op = "fill_down"
columns = ["region"]
direction = "down"   # down (default) | up
```

Two lines, one match arm, one `validate` no-op, and the existing spec-order rule
in `RowOp` covers it. `up` needs the same second pass a `skip_rows` tail needs,
so `can_stream` treats it the way it already treats that case.

### S5 · per-column JSON `pointer`

```toml
[[spec.columns]]
name = "city"
source = "addr"
pointer = "/city"        # RFC 6901, into the value of `addr`
dtype = { type = "utf8" }
```

`Extraction::Json` already serialises a nested value back to a JSON string, which
loses nothing and reaches nothing. A per-column pointer opens exactly one level of
that at a time, declaratively, and composes with `matches` in a target the same
way a column name does. A pointer that does not resolve is a null; a pointer that
resolves to an object or array is an error, because the column would silently
become JSON text again.

### S6 · `.gz` inputs

`open_input` gains a gzip branch keyed on the extension, checked against the
magic bytes. It streams (`flate2`'s reader is a `BufRead`), so nothing about the
memory story changes. The `xlguard` argument applies: a decompressed size is a
claim, so the existing cell ceiling does the bounding, and a bomb hits it.

### S7 · epoch parse

```toml
parse = { epoch = "seconds" }   # seconds | milliseconds | microseconds
```

Valid only on `DType::Timestamp` (and `Date`, truncating to the day). Never
inferred — a column of ten-digit integers is not obviously a time — but a note
when a column of integers sits in the plausible band with a time-ish name, which
is the same treatment E13's spreadsheet serials get.

## 4. What this does to the numbers

The catalogue's ledger, after this slice:

- tier 1 gaps: **3 → 0** (C8, C9+H4, D3)
- tier 2: **4 → 2** (D6 and A6 close; D2 pivot and E5 remain — E5 is already
  fixed in 0.2.1)
- tier 3: **6 → 4** (G2 and E21 close)
- `spec` verdicts: 45 → **52**; `gap` verdicts: 14 → **7**

More usefully: every operator in Potter's Wheel's 2001 algebra gains a home, and
the catalogue's Part M scorecard goes from nine-of-ten to ten-of-ten.

## 5. Order of work

Each step ends green, and nothing depends on a later one.

1. **S4** (`direction`) — the smallest possible exercise of the whole template:
   variant, arm, validate, streaming equality test. Half a day.
2. **S1** (`split_column`) — the largest, and the one everything else is measured
   against. Includes the `RowOp` ordering test and the `fit` proposal path.
3. **S2** (`transpose`) — needs S1's `validate` patterns and reuses slice 5's
   elimination machinery for the `fit` half.
4. **S3** (`source_name` + provenance) — touches `target.rs` and `target_hash`,
   so it lands after the transforms are stable.
5. **S5, S6, S7** — independent; whichever the day allows.

Re-run `scripts/run_pollock.py` after S1–S3 and after the slice. The benchmark
does not test any of these operators directly, but it is now the tripwire for
whether the framing they touch regressed.

## 6. Open questions for review

1. ~~**`split_column`'s `on_short`.**~~ **Settled: both, defaulting to
   `error`.** A missing trailing part becomes null, not a guessed value, so
   nothing is invented — which is what separates it from `decimal_shift` and
   why it needs no review gate. The optional suffix (`"Zürich"` beside
   `"Zürich, ZH"`) is common enough that forcing two specs for it is worse, and
   the declaration is visible in the sidecar. The head of a short value is kept
   rather than nulled along with the tail: a part that is there is data.
2. ~~**Transpose and `matches`.**~~ **Dissolved by dropping `header_from`.**
   With plain transposition followed by `promote_header`, the post-transform
   header *is* the file's own spelling of the labels that ran down the first
   column, so `header_origin` carries them with no special case.
3. ~~**`WITH (provenance = true)` and `--frozen`.**~~ **Asserted, in
   `tests/dataset.rs`** rather than left resting on the invariant. This session
   found twice that an invariant nothing checks is not one, so `_row`'s
   determinism is now a test: every member restarts its numbering at 1, and the
   existing `the_row_order_of_a_dataset_is_deterministic` covers the lock-order
   half it depends on.
