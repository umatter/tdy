# Changelog

Notable changes to `tdy` and `tdy-tui`. The two crates are versioned together.

## 0.3.0 — 2026-10-02

*0.2.1 was never published; its entries, below, ship with 0.3.0.*

The pile release. 0.2.0 made a single file read correctly; this one is about
the dataset a pile of them becomes. A member of a dataset may now be one sheet
of a workbook or one of several tables stacked in a file; compressed exports are
read rather than refused; the spec language gains the shape operators the
operator catalogue (`docs/design/2026-09-05-munging-taxonomy.md`) found missing;
readings no value in a file can establish — a rounding, an epoch unit, the
century of a two-digit year — are declared in the target rather than guessed;
and `tdy profile` shows what a column holds. Two more checks ask a person
before a pile joins: a member in a different unit from its siblings, and a
reading the target did not declare.

**Upgrading changes what some piles do, deliberately**, and the next section
lists every such change. As with 0.2.0, a fresh sidecar is not invalidated by
the upgrade: its file's blake3 still matches, so a `<file>.tdy.toml` written by
0.2.0 keeps being used — and then re-proved, against the target and by a dry
run, on every `tdy fit` and every `dataset()` query, which is where most of the
changes below take effect.

### Changed — what you will notice upgrading from 0.2.0

- **Rounding onto a declared scale is refused unless the target says so.** In
  a pile, a value with more fractional digits than its `DECIMAL` column's scale
  is a gap naming the value, unless the target column declares
  `OPTIONS(round = 'half_away')` (the one mode, part of `target_hash`). The
  executor enforces it too, not only the planner: a fitted sidecar carries
  `parse.round = "error"` unless the column declared rounding, so whole-file
  verification refuses a late value the probe never saw, naming its row. A
  sidecar with `round` unset — every sidecar 0.2.0 wrote, and every sniffed one
  — still rounds half away from zero, and its note says so; `messy()` is
  unaffected.
- **An epoch reading in a pile member waits on a person unless the target
  declares the unit.** Any `epoch = …` in a member's sidecar, or a format
  reading `%s` (which is epoch seconds), is now a review reason — "`datum`
  reads integers as time (epoch = …), which no value in the file states" —
  dropped only when the target column declares that very unit with
  `OPTIONS(epoch = '<unit>')`. A declared unit is enforced, not advised: a
  member column read with another unit or none is refused, naming both. A
  hand-written epoch sidecar that joined a pile silently under 0.2.0 asks for
  one `--accept` (or the declaration); `messy()` queries are unaffected, since
  review is a pile concept.
- **A `%y` date column in a pile waits on a person, naming the century window
  in force** — declared with `year_pivot`, or chrono's default (00–69 → 20xx,
  70–99 → 19xx). A target column declaring exactly that window with
  `OPTIONS(year_pivot = 'N')` authorises it with a note and no review.
- **Body transforms see rows after the ragged policy on both executors.** The
  streaming executor always applied `ragged` as it read; the materialising one
  applied it later, so a whole-row `drop_rows_matching` or `remove_empty` on a
  headerless file tested the row before the policy had judged it. Under
  `ragged = "error"`, a file whose too-wide row such a `drop_rows_matching`
  removed is now refused by the materialising executor as the streaming one
  already refused it; under `truncate_extra` both transforms now test the
  truncated row, so a row count can change. The sniffer chooses `pad_nulls`, so
  a sniffed spec is unaffected.
- **A sheet's blank body rows are no longer all-NULL records.** They inflated
  `count(*)` on any sheet with spacer rows. They are dropped past the last
  framing transform, so a hand-written `skip_rows` still counts the rows it
  was written against, and `fill_down` leaves a blank row blank rather than
  filling it into a record.
- **Members are named relative to the target**, however the glob or
  `--accept` spells them. An absolute glob used to put absolute paths in the
  lock, which `--accept` could then name by neither spelling. A lock written
  that way drifts once: `dataset()` refuses it, naming each member as not in
  the lock, and the next `tdy fit` re-plans the members under their relative
  names and asks for their acceptances again.
- **`transpose` and the plain types refuse stray keys.** serde accepts and
  drops any key beside the tag of a unit variant, so `op = "transpose"` beside
  `rows = 5`, or `type = "utf8"` beside a `format`, loaded silently. Both are
  now refused, naming the field. The JSON Schema is unchanged.
- **`PROMPT_VERSION` is `infer-v5`** (0.2.0 recorded `infer-v3`): the JSON
  Schema stated in the prompt carries the spec additions below, the last two
  bumps for `remove_empty` and `year_pivot`, then `excel_days`. It is recorded
  in an inferred sidecar's provenance and compared with nothing.
- **A workbook with several fitting sheets, or a file with several stacked
  tables, becomes several members in a pile.** 0.2.0 refused the first as an
  ambiguous frame. Each sheet member is fitted on its own and joins without
  review; each table split out of a stacked file waits on `--accept` (see
  *Added — piles*). A single file given to `tdy fit TARGET FILE` still refuses
  several fitting sheets, and names a stacked split it did not make.
- **A member 10× above or below its siblings waits on a person.** With three
  members or more, each member's median absolute value per numeric column is
  compared with the median of those medians; the reason names the column, both
  medians and the factor. It lives in the lock, not the sidecar — it is a fact
  about the pile.
- **Compressed files are read, not refused.** 0.2.0 read a gzip file as text
  and returned one confident column of mojibake. gzip, zstd, bzip2 and xz are
  now decompressed (see *Added*); lz4 and zip are refused by name.

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

### Fixed — found since

- **Padded title lines in a CSV are skipped as titles.** A leading run of lines
  whose only filled cell is the first (`Table 1. …;;`, a title padded to the
  table's width as Excel writes CSV) is skipped as one-field title lines are —
  only when the row after it is then promoted as the header with two or more
  filled fields, so a single-column file with a trailing `;`, or a first record
  with empty fields, reads exactly as before.
- **The streaming executor measured a table's width over rows `skip_rows`
  drops**, so a title line wider than the table added phantom columns the
  materialising executor never had.
- **A `truncate_extra` refusal names the right cure.** A spec naming a
  generated `col_N` beyond the modal width is told the policy truncated wider
  rows and that `pad_nulls` keeps them, on both executors, instead of being
  pointed at `quote`.
- **A long-form member is told it is long-form.** A file whose values in one
  column are the target's declared names is diagnosed as such (`kind =
  "long_form"`), rather than sent after a `matches` spelling that does not
  exist — and, being a settled answer, it is no longer sent to the model for
  another frame.
- **`tdy draft` declares what its own draft needs.** A `DECIMAL` drafted from a
  currency-formatted cell holding float noise carries `round = 'half_away'`, so
  the unedited draft fits the file it came from; a draft written to a target in
  another directory names its files (a glob relative to the current directory
  only when they are at or below it, absolute otherwise), for `tdy draft` and
  the console's `.draft --to` alike.
- **A sidecar the loader refuses is not discarded in silence.** It is still
  re-planned, and the member now carries a note saying so.

### Added — reading

- **A per-column JSON `pointer`.** `Extraction::Json` gives the union of every
  record's keys and serialises a nested value back to a JSON string — honest,
  since nothing is lost, and unreachable, since DataFusion has no JSON
  functions to open it downstream. A column may now declare an RFC 6901
  pointer into its source value:

  ```toml
  [[spec.columns]]
  name = "city"
  source = "addr"
  pointer = "/city"
  ```

  Arbitrary depth works (`/a/b/c`), and the same source column can be opened
  more than once at different paths. A pointer that does not resolve is a
  **null** — a key some records lack is the ordinary shape of a JSON export,
  and the union-of-keys rule already says so. One that lands on an object or
  an array is an **error**, because the column would quietly go back to
  holding JSON text, which is the state a pointer is declared to get out of.
  `validate` refuses a pointer on a non-JSON extraction.

- **`epoch` reads integer timestamps in milliseconds and microseconds.**
  Seconds already worked and nobody had noticed: `format = "%s"` is chrono's
  own epoch specifier and tdy passes the format straight through. What was
  missing is the two scales chrono has no spelling for — the milliseconds
  JavaScript and Java hand out, and the microseconds some databases do.

  ```toml
  parse = { epoch = "milliseconds" }
  dtype = { type = "timestamp", format = "%s" }
  ```

  `validate` requires `format = "%s"` beside it: the format and the option are
  two statements about how to read the same value, and a sidecar where they
  disagree (`%Y-%m-%d` beside `epoch = "milliseconds"`) says nothing true. On
  a `date` column the instant truncates toward the epoch, so 23:59 still
  belongs to the day it falls in. A fractional value is an error rather than a
  rounding — an epoch is a count, and `1748736000.5` in a timestamp column is
  something to look at.

  Since then: `epoch = "excel_days"` reads a spreadsheet serial (below), a
  target can declare the unit, and in a pile an undeclared epoch waits on a
  person (*Changed*, above).

- **`source_name`: where a file *is* becomes a column.** The period a monthly
  export covers is very often only in its filename, and forty CSVs whose
  canton appears nowhere but their path are an ordinary pile.

  ```toml
  [[spec.transforms]]
  op = "source_name"
  name = "jahr"
  from = "file_stem"      # file_stem | file_name | sheet | path | region
  pattern = "(\\d{4})"     # optional: the capture becomes the value
  ```

  `constant` could hand-write this per file, at the cost of the review gate
  firing on every member — the right gate for an arbitrary constant and the
  wrong one for a fact tdy can read off the path. This is derived, not
  invented, so it carries no review. It may only add, never shadow, and a
  pattern that does not match is an **error**: a silently empty `jahr` on one
  member of twelve is invisible in any single file and is exactly what this
  prevents. `from = "sheet"` reads the sheet of a sheet member and
  `from = "region"` the ordinal of a stacked table (see *Added — piles*).

  Internally, `RawTable` now carries where it was read from, set by `extract`
  — the only place that knows — rather than a path being threaded through
  nine `apply_transforms` call sites. Same reasoning as `col_offset`: it is a
  property of the extraction, and passing it separately would be a second
  thing that could disagree with the rows.

- **`WITH (provenance = true)`: a row can say where it came from.** Adds
  `_member` (the member's name as the lock records it, relative to the
  target — `book.xlsx#Q1` for a sheet member) and `_row` (1-based **within
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

- **Compressed inputs.** gzip, zstd, bzip2 and xz — recognised by their bytes,
  so a `.csv` that is really gzip counts — are decompressed once per run into a
  process-lifetime cache that every reader, both executors and drift then use,
  bounded by a new `[limits].max_decompressed_bytes` *before* the copy exists.
  The sidecar fingerprints the compressed bytes and records
  `compressed = "gzip"`. The four decoders were already in the tree through
  `zip`, so the new direct dependencies (`flate2`, `bzip2`, `xz2`, `zstd`) cost
  no compilation.
- **`remove_empty` drops all-empty rows** — a delimited row carrying only its
  delimiters (`;;;`). Rows only: the `columns` list stays the only projection,
  and an all-empty column gets a sniffer note saying to omit it. Never inferred.
- **`epoch = "excel_days"` reads a spreadsheet serial** — whole days since
  1899-12-30 on a `date`, the fraction as the time of day on a `timestamp`,
  from the digit string by integer arithmetic and rounded to the millisecond (a
  spreadsheet writes ~15 significant digits). A serial below 61 or past
  9999-12-31 is refused naming its row, and so is a time of day on a `date`.
  The sniffer notes an integer column whose name and values read like serial
  dates, and names the declaration; nothing is converted unasked.
- **`year_pivot` declares the century of a two-digit year** (0..=100): below
  the pivot is 20xx, at or above it 19xx. Only on a `date` or `timestamp` whose
  format reads `%y`.
- **Two more sniffer notes, nothing converted:** a column of percentages
  (`45%` — 45 or 0.45 is the author's to say), and the serial-date note above.
- **A header run is adopted, not only a header row.** Official statistics put
  title lines and the header in one run, blank rows, then the data; a run
  directly above a block whose last row has the block's width is that block's
  header run. Over a promoted header that reads like data it is adopted only as
  a review reason naming the row read as data.

### Added — piles

- **Workbook members.** A member is a path and an optional sheet: a workbook
  several of whose sheets produce the declared table becomes one member per
  sheet (`book.xlsx#Q1`), each fully fitted, with its own sidecar
  (`book.xlsx#Q1.tdy.toml`) and its own acceptance. Drift stays per file — the
  file's hash covers every sheet — and a file listed both whole and by sheet is
  drift. A typed reference is resolved against the members that exist, never
  split by rule, and one that could mean two members is refused naming both.
- **Regions: several tables stacked in one file or sheet.** A file or sheet is
  split at runs of blank lines or rows; each block that passes the gates
  becomes its own member (`report.csv#2`, `book.xlsx#Q1#2`), and each waits on
  `--accept`, because a blank row proves a boundary exists, not that the blocks
  are the same kind of table. Exactly one proper block is a plain member with a
  note and no review — unless something table-shaped was discarded around it,
  which is asked about. Runs too small to be tables are named in a note.
  `tdy draft` splits blocks the same way and drafts each block's columns.
  Side-by-side tables in one sheet are out of scope.
- **Declared readings.** A target column can say what no value in the file
  can: `OPTIONS(round = 'half_away')` on a `DECIMAL`, and
  `OPTIONS(epoch = '<unit>')` or `OPTIONS(year_pivot = 'N')` on a `DATE` or
  `TIMESTAMP`. All three are part of `target_hash`. A declaration authorises the reading — `fit` binds through it
  with a note and no review — and undeclared, `fit` never tries a `%y` format
  or an epoch: a column only those read is a gap naming the option.
- **The magnitude check** (*Changed*, above), and two refusals that keep
  member names unambiguous: a pile in which two members would share one name
  (a file literally called `report.csv#2` beside a split `report.csv`) is
  refused whole, and an `exclude` entry matching both a file and a member is
  refused as ambiguous.

### Added — profiling

- **`tdy profile` says what each column holds**: non-empty and empty counts,
  distinct values, min and max, the top five, and the *shapes* values take
  ("94% look like `9999-99-99`, 6% like `99.99.9999`"), over the framed raw
  table and the whole file. One library function behind four doors:
  `tdy profile`, the console's `.profile`, `p` in the workbench, and a
  read-only MCP tool. It streams — about 12 MB peak on a 50 MB CSV — and every
  bound it hits is stated in the output. A profile is evidence for a person;
  nothing reads one to change a spec.

### Added — the workbench and the console

- **The workbench draws real widgets from a palette by meaning**: the pile as
  a table with one column per declared column, green for fits, red for gaps,
  yellow for review, no lock and dry run; floating help and confirm popups;
  a header naming the root, the target and its lock state, the backend and a
  DRY RUN badge; a console that wraps.
- **Getting around a pile**: `g`/`G` jump between members that need
  attention, `/` filters the pile to them, a multi-sheet workbook's title names
  the sheet on show, a region member's names its rows, and a column decision
  shows the column's first raw values beneath it.
- **The edit loop closes**: a successful `$EDITOR` round-trip on the target or
  a member's sidecar re-fits (dry run); a change made elsewhere is named in the
  status line and never refits on its own.
- **The console's Up recalls history by what you have typed**, as fish does.

### Library notes

- `Transform` gained `SourceName`, `Transpose {}`, `SplitColumn` and
  `RemoveEmpty {}`; `FillDown` gained `direction`; `ColumnSpec` gained
  `pointer`; `ValueParsing` gained `epoch`, `round` and `year_pivot`;
  `Extraction::Delimited` gained `region` and `Extraction::Excel`
  `region_ordinal`. Code constructing these directly needs the new fields;
  sidecars are unaffected, since every new field defaults and is omitted when
  unset.
- `rustls` 0.23.45 in the lockfile, for RUSTSEC-2026-0285.

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
