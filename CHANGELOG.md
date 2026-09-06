# Changelog

Notable changes to `tdy` and `tdy-tui`. The two crates are versioned together.

## Unreleased

### Fixed — both found by the Pollock benchmark

Running `scripts/run_pollock.py` over 2,290 files with one isolated deviation
from RFC 4180 each turned up two things nothing in the tree had:

- **A width mismatch on a nameless table now explains itself.** Two files
  failed with `resolving output column col_13: no column named col_13;
  available columns: [col_1, …, col_12]` — true, and useless to the person
  reading it. The spec's columns come from a 16 KB head+tail sample while the
  table comes from the file, and when parse state crosses the boundary between
  them (an unbalanced quote is the usual way) the two split rows differently
  and disagree about the width. When both the wanted name and every name the
  table has are the generated `col_N`, the error now gives the file's real
  width and names the likely cause — verified: re-reading that file with
  `quote = "'"` takes 61 of its 84 rows to a uniform 9 fields.

  The *outcome* was already safe — a loud error, no sidecar written — and
  stays so. What changed is that the message is about the file.

- **Discarding the top of a file and finding no header is no longer a
  confident read.** 32 files reached confidence 0.80 — exactly the escalation
  threshold — in a state where tdy had thrown a leading row away *and* could
  not name a single column. Skipping the top of a file is a guess that a
  header below it corroborates; with no header found anywhere, nothing
  corroborates it, and the row discarded may have *been* the header, malformed.
  The two doubts now compound (0.80 → 0.65), which puts such a file in the band
  where a human looks and a backend escalates.

  Discarding the row is unchanged and deliberate: a three-field row is not a
  header for nine columns, and inventing names from a mis-parsed row is exactly
  the mis-mapping the design refuses.

### Added

- **`source_name`: where a file *is* becomes a column.** The period a monthly
  export covers is very often only in its filename, and forty CSVs whose
  canton appears nowhere but their path are an ordinary pile.

  ```toml
  [[spec.transforms]]
  op = "source_name"
  name = "jahr"
  from = "file_stem"      # file_stem | file_name | sheet | path
  pattern = "(\\d{4})"     # optional: the capture becomes the value
  ```

  `constant` could hand-write this per file, at the cost of the review gate
  firing on every member — the right gate for an arbitrary constant and the
  wrong one for a fact tdy can read off the path. This is derived, not
  invented, so it carries no review. It may only add, never shadow, and a
  pattern that does not match is an **error**: a silently empty `jahr` on one
  member of twelve is invisible in any single file and is exactly what this
  prevents.

  Internally, `RawTable` now carries where it was read from, set by `extract`
  — the only place that knows — rather than a path being threaded through
  nine `apply_transforms` call sites. Same reasoning as `col_offset`: it is a
  property of the extraction, and passing it separately would be a second
  thing that could disagree with the rows.

- **`WITH (provenance = true)`: a row can say where it came from.** Adds
  `_member` (the member's lock-relative path) and `_row` (1-based **within
  that member**) to what `dataset()` returns.

  tdy proved which files a dataset contains, what shape they land on and what
  a human accepted — and then a row in the result carried no trace of which
  member and which line produced it. An error message could always name a row;
  a query never could.

  Opt-in, because a dataset's schema is what the declaration says it is and no
  column may appear that the declaration did not ask for. Part of
  `target_hash` for the same reason `if_missing_null` is: turning it on
  changes the shape, so it voids the proofs that were about the old one.
  Conformance still compares a member's spec against the *declared* columns —
  `_member` and `_row` are `dataset()`'s to fill, since only it knows which
  member a row came from.

- **`transpose`: rows become columns.** The cure for a file laid out for
  reading rather than analysis — variables down the left, observations across
  the top, which is what every print-friendly export and hand-built management
  sheet produces. Nothing in the spec language could reach such a file before:
  `unpivot` turns wide into long but cannot make the first *column* into the
  header.

  ```toml
  [[spec.transforms]]
  op = "transpose"

  [[spec.transforms]]
  op = "promote_header"
  rows = 1
  ```

  No options, deliberately. After the flip the values that were the first
  column are the first row, so `promote_header` does what it always does and
  the header a `matches` clause addresses is the file's own spelling of those
  labels. It must come before `promote_header` — the two disagree about which
  direction the names run — and `validate` refuses the other order, and a
  second transpose, before anything is read.

  A truncated table is refused rather than flipped: every row a partial read
  never saw would have been a *column*, so the result would be the wrong shape
  rather than merely short. It runs on the materialising executor, since the
  first output row cannot be emitted until the last input row is read.

  **Never inferred.** The sniffer notes the shape — "the header is 3 period
  labels and the first column holds names" — and names *both* cures, because a
  transposed report and an ordinary wide report have the same signature and
  what separates them is what the rows mean. Measured on 400 real corpus files:
  the note fires once, on a file that genuinely wants `unpivot`.

  Closes the largest gap the operator catalogue found (C8).

- **`split_column`: one column becomes several.** By a literal delimiter, by
  character positions, or by a regex's capture groups; the source column is
  replaced in place, so the header keeps its shape and `columns` addresses the
  parts by name.

  ```toml
  [[spec.transforms]]
  op = "split_column"
  source = "name"
  into = ["last", "first"]
  by = { kind = "delimiter", value = ", " }
  ```

  It is **total by construction**: a delimiter split stops after `into.len()`
  parts and keeps the remainder in the last one, so a value can never yield
  more parts than there are names for it. It can yield fewer, and that is an
  error naming the row and the value — padding a short row silently is how a
  split loses the second half of every value that happened to contain no
  separator. `on_short = "null"` declares the tail optional (`"Zürich"` beside
  `"Zürich, ZH"`), fills the missing parts with nulls and keeps the head.

  Never inferred: choosing where to cut a value is a judgement, and the file
  does not contain it. Runs on the materialising executor for now — it rewrites
  the header's width as well as each row's, which the streaming planner
  establishes once up front.

  This closes the one gap the operator catalogue found in every survey it ran:
  Potter's Wheel's `Split`, tidyr's `separate`, Power Query's "Split Column by
  Delimiter" — the most common munging operation, and the only one of the three
  most common with no declarative form in tdy.

- **`fill_down` takes a `direction`.** `down` (the default, and what it always
  did) carries the last non-empty value forward — the merged-cell and
  written-once-at-the-top layout. `up` carries it backward, which is the same
  layout with the label written at the *bottom* of its group, as
  French-language and some accounting exports write it. The two are different
  readings of the same file, so it is declared rather than guessed.

  A spec with `direction = "up"` runs on the materialising executor:
  `stream::can_stream` refuses the shape, because carrying a value from a row
  the reader has not reached yet is the one thing a forward-only pass cannot
  do. No spec is refused for it — only executed the older way.

  *Library note:* `Transform::FillDown` gained a field, so code constructing
  the variant directly needs `direction: Default::default()`. Sidecars are
  unaffected; the field defaults and is omitted when it is `down`.

## 0.2.1 — 2026-09-06

One correctness fix, in the same class as 0.2.0's: a spec that passed every
gate and produced a number wrong in its **sign**.

### Fixed

- **Accounting negatives no longer read as positive.** `(1,234.50)` is minus
  1234.50 in every ledger ever printed, and `1234.50-` is the same claim in
  mainframe dialect. Neither parses as a number, so the only repair the spec
  language offered was `strip` — which deletes the marker and yields
  **+1234.50**, through `validate`, through the dry run, into a fingerprinted
  sidecar, invisible in any single row and wrong by twice itself.

  Three changes close it:

  - **`parse.negative = "parentheses" | "trailing_minus"`** says what the
    marker *means*, so the value can be read correctly. It applies after
    `strip` and before the separators, so `(CHF 1'234.50)` works: strip takes
    the symbol, `negative` takes the bracket. Only valid on a numeric column.
  - **A `strip` that would eat a sign marker is now refused at execution**,
    naming the row, the value and the remedy. It is checked against the data
    rather than refused in `validate`, because a `strip` that never meets a
    bracket is perfectly fine and only the file knows which it is.
  - **The sniffer reports the shape instead of guessing at it.** A column
    written that way stays text, gains a note saying which declaration would
    type it, and loses confidence. It is never inferred: `(5)` is a footnote
    marker at least as often as it is minus five, and that is a judgement
    about what the file's author meant.

  `(-5)` — a sign *and* a marker — is an error rather than a guess, since it
  reads as minus five to one author and plus five to another.

**No existing results change**, unless a hand-written sidecar was stripping
sign markers: such a spec now fails loudly where it used to return positive
numbers. That is the fix. Sniffed sidecars are unaffected — the sniffer never
typed these columns in the first place.

## 0.2.0 — 2026-09-05

A correctness release. Two systematic audits of a 9,881-file corpus of real
public data found nine defects in 0.1.0 where tdy returned a plausible wrong
answer instead of a right one or a loud error — the single thing this tool
promises not to do. All nine are fixed, each with a regression test written
against the correct behaviour and a committed fixture reproducing the shape.

**Upgrading changes results on affected files, deliberately.** Row counts go
up where rows were being dropped; a spreadsheet money column that came back
`float64` now comes back `decimal`, which changes the Arrow schema downstream
code sees; and confidences and `notes` move. That is why this is 0.2.0 and
not 0.1.1.

**Existing sidecars are NOT invalidated by upgrading, and this matters here.**
A sidecar is fresh when the file's blake3 still matches; the tool version
that wrote it is recorded but never compared. So a `<file>.tdy.toml` written
by 0.1.0 keeps being used verbatim — including a spec carrying one of the
defects below, such as a `skip_rows` that eats real rows or a money column
typed `float64`. **To get these fixes on files you have already sniffed, you
must re-sniff them**: delete the sidecar and run `tdy sniff` again, or
`tdy fit` the target again for a planned pile. Sidecars marked
`method = "manual"` are yours and are left alone either way.

### Fixed — silent data loss

- **Rows dropped while hunting for a header.** The Excel sniffer chose the
  header as "the first row with ≥60% of the grid width populated, followed by
  a row with ≥50%", then skipped everything above it. Both bars are fractions
  of the *grid* width and the second is asked of the row *after* the
  candidate, so on a wide ragged sheet a header filling every column is
  rejected because the first data row under it is sparse — and the scan walks
  into the data until two consecutive rows happen to be dense enough, keeping
  the skip even after deciding the row it found was not a header. One audited
  workbook returned 61,952 of 62,168 rows, discarding seven universities,
  while reporting only "skipped 217 leading row(s) before the header". tdy now
  never skips past a row that is itself a header.
- **An ordinary last row deleted as a "total".** The footer check dropped a
  file's last row whenever any field matched a total-like label, with no
  corroboration. Where `subsector = "total"` is a routine category (2,288
  occurrences in one audited file), the last row — an ordinary record — was
  silently deleted. A trailing row is now treated as a summary row only when
  its label occurs at most once elsewhere in that column, corroborated against
  the sample's tail rather than a head-only probe.
- **A data record absorbed into a column name.** Header promotion fired
  whenever row 1 looked like a row of distinct labels, even when every row in
  the file has that shape — a path list, a `requirements.txt`, a single-line
  version file — deleting a record. Promotion is refused for a one-row table,
  and for a single-column table whose rows all share the first row's shape,
  with a doubt naming the ambiguity.

### Fixed — silent wrong values

- **Accented text returned as mojibake.** A file that is valid UTF-8 as a
  whole, but whose 4 KB tail sample begins mid multi-byte sequence, failed the
  "valid UTF-8 is UTF-8" check once head and tail were fused into one buffer,
  and fell through to charset detection, which settled on windows-1252 at
  confidence 0.80 with no warning. Head and tail are now checked
  independently.
- **Money read through binary floating point.** A spreadsheet column whose
  cells carry a *currency* number format is now typed `decimal`, not
  `float64`, so sibling money columns cannot disagree and results do not carry
  IEEE noise where the sheet shows cents. calamine does not expose the cell
  format, so `tdy` reads `xl/styles.xml` and the sheet XML from the workbook
  zip directly. The scale is the widest fractional part any sampled value
  actually has, so parsing at it never rounds a digit away; a currency column
  whose separator is ambiguous, or whose noise would need an implausible
  scale, is left as `float64` and says why rather than round a real value.
- **Interior aggregate rows counted as data.** The summary-row check only ever
  saw the file's *last* row, so anything below a `Total` — a blank line, a
  disclaimer, a legend block — hid it completely and it was read as an
  ordinary record, doubling every `sum()` over that column. Across 320 corpus
  files this now identifies 11 such rows, all genuine, none spurious. The row
  is reported rather than removed: which rows are safe to drop is a decision
  for a human with `drop_rows_matching`.
- **A late-starting column stayed text forever.** Whole-file type verification
  only widened a guess that had failed, and a text guess never fails, so a
  column whose first 500 rows are all null stayed text even when the rest of
  the file was clean numbers. Such a column is now typed from the whole file —
  and never narrowed to `float64`, which would have reintroduced float money.

### Added — warnings where tdy cannot be sure

- A legend or footnote block below the table, and a header repeated mid-file
  between concatenated sections, are both now reported. Neither is removed.
- A single-column all-text file gets a doubt naming *why* header and record
  are indistinguishable there, instead of the generic "no header row
  detected".

### Changed — warnings that were firing on correct output

- "Nearly every column typed as text; the layout may be misread" no longer
  fires when the file supplied its own column names. It was unearned on data
  that is simply textual — recipes, categories, episode notes, names — where
  it was the only thing pushing an otherwise correct parse below the
  escalation threshold, which is how people learn to ignore warnings.
- The trailing legend/footnote detector no longer claims a whole table. Its
  backward walk stops only at a full-width row, so on a sheet of uniformly
  sparse rows it ran to the top and advised deleting every row of data. A
  block must now have data above it and be mostly prose.

## 0.1.0 — 2026-09-03

First release.
