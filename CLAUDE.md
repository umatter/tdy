# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What this is

`tdy` — a Rust CLI that runs stock DataFusion SQL over messy files (`messy('file.xlsx')`)
by keeping all structural cleaning in a per-file **sidecar** (`<file>.tdy.toml`) holding a
`ParseSpec`. README.md documents the user-facing spec language and CLI; this file covers
what you need to change the code.

## Commands

```bash
cargo build --release
cargo test --lib --tests                # 298 tests (skips doc-tests; see note below)
cargo test --test regression            # one suite
cargo test german_decimal_comma         # one test by name
cargo test --test adversarial           # ~55s: sweeps every fixture for panics/hangs
python3 gen_fixtures.py                 # regenerate all fixtures (openpyxl + xlwt)
python3 gen_fixtures.py 04 --list       # one generator / list them
cargo run -- sniff testdata/umsatz.xlsx --no-llm
cargo run -- validate <file> --stamp    # re-fingerprint a hand-edited sidecar
cargo run -- schema                     # JSON Schema derived from spec.rs

# The inference tier, against a real model (costs money; never runs in CI):
OPENROUTER_API_KEY=... TDY_LIVE_MODEL=google/gemini-2.5-flash \
  cargo test --test live_backend -- --nocapture
```

Every test runs with `backend = none`; nothing needs a network or a model.
On this machine plain `cargo test` ends with a spurious doc-test failure (`rustdoc` cannot
load `libLLVM.so...` — a toolchain install issue, not code); `--lib --tests` avoids it.
Rust ≥ 1.88 (DataFusion 46), a floor set by reqwest → url → icu_*, checked in CI. `[profile.dev] debug = false` is deliberate (slow builds) —
flip it locally when you need a debugger, don't commit it.

## Where this is going

`docs/design/2026-08-30-target-schema.md` is the agreed direction: you declare the dataset you
want in SQL DDL, point tdy at a pile of messy heterogeneous files, and it plans each file onto
that target by composing operators that already exist — proving `engine::schema_of(spec)`
equals the declared Arrow schema *before* reading a byte. It inverts today's inference
(file -> spec becomes target+file -> spec) and it makes the safety property stronger, because
a declared shape is mechanically provable in a way "did the head parse?" never was. Read it
before adding anything to `spec.rs`, `sniff.rs` or `provider.rs` — several of its slices land
there, and its section 3 records which review recommendations were overruled and why.
`*-review.md` beside it is the long-form design review it came from.

**Slice 1 is in.** `src/target.rs` parses the SQL DDL (via DataFusion's re-exported
`sqlparser`, so the type vocabulary is SQL's and costs no dependency); `src/conform.rs`
proves a spec lands on it by comparing `engine::schema_of(spec)` to `Target::arrow_schema()`
field for field, with no I/O; `tdy check <TARGET> --against <FILE>` is the CI gate.
`tests/conform.rs` carries the assertion the whole layer rests on — that `schema_of` is the
schema execution really produces — swept over 84 fixtures on **both** executors, because a
gate that can disagree with the executor is worse than no gate.

A target holds its columns as **Arrow** types, not `DType`, and that is load-bearing:
`DType::Date` carries a per-file strftime format, so comparing `DType`s would force a target
to pin one, and twelve exports with twelve date formats could never land on one column.

Not yet: `tdy fit` (the planner), the lock, globs, `dataset()`.

## The one rule

**tdy never silently produces a wrong value.** Ambiguity resolves to the right answer or a
loud error naming the row — never a plausible wrong number. Most of the non-obvious code
exists to hold that line, and a change that trades it for convenience is a regression even
if every test passes. Concretely: thousands separators must group in threes (only when the
separator could also be a decimal point), `%Y` demands four digits, ambiguous date orders
drop confidence below the escalation threshold, leading-zero and oversized integers stay
text, money becomes `decimal`.

## Architecture

Data flow for `tdy query`:

```
SQL text ──tokenize──► provider::prepare_specs (async pre-pass, per messy() path)
   (sqlscan)              │  sidecar::load → Fresh? done
                          │  else sample::build (head+tail only) → sniff::sniff
                          │       confidence < threshold && backend != none → infer::infer_spec
                          │  check_spec = validate() + dry_run, then sidecar::save (atomic)
                          ▼
DataFusion planning ──► provider::MessyFunc::call (SYNC, cached per path)
                          └─► engine::execute_batches → MemTable (64k-row batches, N partitions)
```

Things that only become clear from reading several modules:

- **`spec.rs` is the single source of truth.** The same structs are (a) what the engine
  deserializes from a sidecar, (b) what `schemars` turns into the JSON Schema used for
  constrained decoding, and (c) `deny_unknown_fields` so a hallucinated field produces a
  precise error for the retry loop. `validate()` is a real gate, not a formality: **anything
  the executor would otherwise discover by panicking belongs there as a message**, because a
  sidecar is hand-editable and therefore untrusted input. Adding a transform or dtype means:
  variant in `spec.rs` → arm in `engine::apply_transforms`/casting → `validate()` rule → the
  schema updates for free.
- **The sniffer derives its columns from the post-transform header.** `sniff::finish()` is
  the only place `ColumnSpec`s are built, and it reads `table.header` *after* the transforms
  have been applied to the probe table. That is what makes "the sniffer can never emit an
  unexecutable or mis-mapped spec" structural rather than aspirational — the old code guessed
  from the raw header, and two columns named `Betrag` silently both read the first one.
  Don't reintroduce a second notion of what the columns are called.
- **Sync/async split is load-bearing.** `TableFunctionImpl::call` runs inside SQL planning, so
  inference lives in `prepare_specs`, which finds `messy('path'[,'hint'])` with `sqlscan`
  (a small SQL tokenizer — comments and string literals are not file references) before
  planning. `--frozen` = skip the pre-pass and error on an absent/stale sidecar. Anything slow
  or networked must go in the pre-pass, never in `MessyFunc`.
- **`numfmt` decides separators by shape, not by trial.** "Try each convention, keep the first
  that parses" is what turned `1,5` into `15`. `numfmt::infer` accepts a convention only if
  every value is consistent with it, reports `ambiguous` when nothing in the column settles it,
  and `check_grouping` is what the executor uses to turn a wrong spec into an error.
- **`ExtractOpts` bounds the work.** With `max_rows` set, `read_text` reads at most a 4 MiB
  prefix and drops the torn last line, so `preview`/`dry_run`/the sniffer's probe cost the
  same on a 2 GB file as on a 2 MB one. `preview` caps *output* rows, not extracted rows —
  capping extraction meant a ten-row preview of a file with a twelve-line title block had
  nothing left to promote a header from. A capped table sets `truncated`, and anything
  reasoning about the *end* of the data must not trust it — that is why `SkipRows{tail}` is
  skipped on a truncated table, and why Excel sniffing deliberately does *not* cap (calamine
  materializes the sheet anyway, and the last row is where "Total" lives).
- **Engine pipeline order matters:** extract (all strings) → transforms in spec order →
  projection + typed cast last. Rectangularization is lazy so `skip_rows` can remove title
  rows before the ragged policy applies. `promote_header` fills right only on rows *above*
  the last header row.
- **Deliberate omissions:** no drop/rename transforms (the `columns` list is the only
  projection), no locale tables (literal `replace` pairs in the sidecar), no named timezones
  (fixed offsets only — DST cannot be guessed from a value).
- **`infer.rs`** puts the JSON Schema in the *prompt*, not only in
  `response_format`. Verified against OpenRouter: OpenAI's strict mode rejects a
  schema of this shape (12 violations of its subset — optional properties absent
  from `required`, `oneOf`, nesting depth), and a non-strict schema is advisory,
  so models invented fields (`locale`) or omitted required ones (`pattern`) until
  the contract was stated outright. It targets two wire formats with one schema:
  OpenAI-compatible
  `response_format` with a weakening ladder (`json_schema` → `json_object` → none;
  `strict:false` because the schema uses `$ref`), and an Anthropic forced tool call.
  Transport failures retry the same prompt; *spec* problems go back to the model as text.
  Bump `PROMPT_VERSION` when changing the prompt — it is recorded in sidecar provenance.
- **Bounded I/O lives in `fileio`**: head/tail sampling by seek, streaming blake3, atomic
  sidecar writes (temp + rename).
- **Two providers, chosen by size.** Under `LAZY_ABOVE_BYTES` (64 MB, `TDY_LAZY_ABOVE_BYTES`)
  `messy()` parses once into a cached `MemTable` — right when a query names the file twice.
  Over it, a `StreamingTable` whose `SpecPartition` runs the parse on a blocking task and
  feeds batches through a **bounded** channel (capacity 2); the bound is the whole point, as
  it is what makes memory O(batch) instead of O(file). Two things there are load-bearing and
  easy to break: a producer error must reach the consumer as an error — swallowing it would
  return the rows read so far and look like a short file, the exact silent-wrong-answer this
  project exists to prevent — and a closed receiver (a `LIMIT`) must end the parse quietly
  rather than report failure. `engine::schema_of` gives DataFusion the schema before any
  batch exists, derived by building each column over *zero* rows so it cannot drift from the
  code that types real data.
- **`stream` is the executor for text formats; `engine` is the fallback and the reference.**
  It is plumbing only — where an answer could differ (`promote_header_from`,
  `build_column_at`) it calls the same function `engine` calls, deliberately, so the two
  cannot drift. It covers delimited, `lines`, `fixed_width` and NDJSON — everything whose rows are
  independent — behind a `Source` enum; Excel and a JSON *array* cannot stream, since each
  is one document with no records until it is parsed whole. NDJSON's header is the union of
  every record's keys, so `discover_ndjson` makes a real pass rather than guessing from a
  prefix: a key appearing only in the last record still has to become a column. It accepts only
  `[skip_rows]? [promote_header]? (drop_rows_matching | fill_down)* [unpivot]?`;
  `can_stream` returns false for anything else and the caller falls back, so an unusual spec
  is never *refused*, only executed the old way. `TDY_NO_STREAM=1` forces `engine` — that is
  `stream::enabled()`, kept separate from `can_stream()` so turning streaming off cannot make
  the shape predicate lie.

  Row-local ops run in **spec order** (`RowOp`), not a fixed one: fill-then-drop propagates a
  subtotal label into the rows beneath it and drop-then-fill does not, and
  `tests/streaming.rs` pins that both executors fall into that identically.

  A second pass is needed only when the width must be discovered (delimited, because
  `promote_header` rectangularises first, so the header's width — hence the column names —
  depends on the widest row in the file) or when a `skip_rows` tail makes the row count
  matter; a log with neither is read once. Counting goes through `next_width`, which returns
  an arity without building a row — materialising a `Vec<String>` per row just to drop it
  cost ~100 MB resident on a 3M-row file.

  `Source` owns a `BufRead`, not a borrowed `&str`, and that is what removed the last term
  proportional to the file. `open_input` streams raw bytes when the encoding is UTF-8; when a
  spec leaves `encoding` unset — which sniffing does deliberately, since an ASCII-only
  *sample* proves nothing about the rest (`enc_late_1252_byte.csv`) — `streamable_as_utf8`
  answers the same question `decode_owned` would, incrementally, in a fixed buffer. Three
  traps, all of which bit during the work: the whole-file decoder strips a BOM, so the
  streaming reader must too; it *replaces* invalid sequences rather than erroring, so the
  delimited source reads `ByteRecord`s and applies `from_utf8_lossy` instead of letting the
  csv crate reject them; and the counting source and body source must be opened **in
  sequence, never both at once** — holding both kept two decoded copies alive and took a
  987 MB CSV to 2 GB.

  Batches are bounded by `BATCH_CELLS`, not `BATCH_ROWS` — a row is as wide as the file, so
  65,536 rows of a 1,000-column file is 65 million strings, and a 134 MB fixture measured at
  4.2 GB until this was fixed. Width was the one dimension nothing bounded. Up to 16 columns
  the two work out the same, so the common case kept exactly the batches it had.

  Measured `count(*)`: 140 MB / 3M-row CSV 1,676 -> **86 MB**; 190 MB / 2M-line nginx log
  1,376 -> **98 MB**; 987 MB / 21M-row CSV refused -> **88 MB**; 134 MB / 1,000-column CSV
  4,156 -> **114 MB**; 138 MB / 1.5M-record NDJSON 2,128 -> **78 MB**. Memory does not track
  file size or width any more.
- **`xlguard` bounds a spreadsheet before it is read.** Every other limit is checked against a
  table that already exists — fine for text, useless for a format whose size is a *claim*: a
  899-byte `.ods` was measured at 4.8 GB and SIGABRT, which is the one failure mode the design
  forbids. `preflight()` runs *before* `open_workbook_auto` because calamine's Ods reader
  parses content.xml eagerly (opening it is already the allocation); xlsx/xlsm are lazy per
  sheet, so their check rides on `XlsxCellReader::dimensions()` inside
  `engine::checked_worksheet_range`, which every workbook-touching path must go through —
  `extract_excel`, `excel_sheet_shapes` *and* `sample::build_excel_sample` (that last one was
  missed on the first pass and left the whole sniff path exposed). `xls` is bounded by BIFF8's
  16-bit indices, `xlsb` only by the zip-expansion check. The scan counts cells carrying a
  *value*: LibreOffice pads every sheet to the full grid, so counting the claim refuses
  ordinary files — `declared_size_ods_padded_like_libreoffice.ods` is the control that keeps
  that honest. `max_cells` is calibrated from measured cost (~122 B/cell spreadsheet,
  ~46 B/cell delimited), not chosen.

## Performance

Measured on a 141 MB / 3M-row CSV (release build), before → after the hardening pass:

| | before | after |
|---|---|---|
| `sniff` (needs a 16 KB sample) | 6.04 s, 1.20 GB RSS | **0.13 s, 24 MB** |
| `count(*)` over the whole file | 6.79 s, 1.40 GB | **2.96 s, 87 MB** |
| same file referenced twice | 2 full parses | 1 (cached, under 64 MB) |

The `count(*)` figure is the streaming executor; `TDY_NO_STREAM=1` on the same file is
3.13 s / 1,676 MB, which is what the materialising path still costs for the formats that
cannot stream.

Peak RSS no longer follows the size of a text file at all — `stream` holds one batch, not the
rows and not the decoded text. It is still ~8x for the formats that cannot stream (Excel,
JSON), which is what `[limits]` is calibrated against. If you change extraction, re-measure
with `/usr/bin/time -f "wall %es peak_rss %MkB"` and on a file large enough to tell the
difference: everything under 64 MB takes the cached path and will not show it.

## Test layout

- unit tests beside the code (139) — `numfmt`, `sqlscan`, `detect`, `spec::validate`, casting,
  `xlguard`'s ODS geometry scan (which is pure-function over a string, so it is tested there
  rather than through a fixture)
- `tests/e2e.rs` — the canonical messy-Excel fixture and SQL end to end
- `tests/formats.rs` — what each extraction/transform *means*, with hand-written specs
- `tests/regression.rs` — one test per defect ever found, written against the **correct**
  behaviour rather than the observed one
- `tests/fixtures.rs` — exact values read from the committed hard fixtures (sums, encodings,
  row counts, dtypes). `adversarial.rs` proves nothing crashes; this proves the answers are
  right, which a parser returning nothing would also satisfy
- `tests/streaming.rs` — the specification of `stream`: not a list of cases but *equality*
  with `engine` over every text fixture (delimited, `lines`, and `fixed_width` against the
  committed reports with the character offsets generator 04 documents), plus the batch-boundary cases a chunked
  pipeline gets wrong (a `fill_down` carry crossing 65,536 rows, a `skip_rows` tail the
  reader has not reached yet, `unpivot` making output rows outnumber input ones)
- `tests/adversarial.rs` — sweeps every fixture in `testdata/`: never panic, never hang, and
  anything sniffable must be queryable and reproducible under `--frozen`. It picks up new
  fixtures automatically. Note it runs the binary with output to *files*, not pipes: a
  100k-column sidecar is megabytes, and an undrained pipe deadlocks at 64 KB.

## Fixtures

`testdata/` is generated, never hand-edited — `python3 gen_fixtures.py`. Each generator in
`testdata/gen/` owns a disjoint set of files and documents in its docstring what each file
stresses. `10_declared_size.py` is the odd one out: two of its three files are *meant* to be
refused, and the third is the control proving the refusal does not catch ordinary documents. `testdata/large/` is gitignored (perf fixtures, generated on demand).
`tests/e2e.rs::umsatz_spec()` is the hand-written reference spec for `umsatz.xlsx`.

Generators need `openpyxl` (xlsx/xlsm), `xlwt` (the only pure-Python BIFF8 writer, for
`.xls`) and **`lxml`** — nothing imports lxml, but openpyxl serialises through it when it is
installed and through ElementTree when it is not, and the two disagree (`<tag/>` vs
`<tag />`), so its absence silently rewrites every `.xlsx` in the tree. `09_legacy_formats.py`
skips the `.xls` files with a notice rather than failing if xlwt is absent, and writes its
`.ods` files with stdlib `zipfile`.

Anything that writes a zip must pin entry timestamps *and* patch `dcterms:modified` —
openpyxl rewrites that at save time whatever `wb.properties` says. Getting this wrong is not
cosmetic: it stales the blake3 fingerprint in every sidecar pointing at the file. `umsatz()`
in `gen_fixtures.py` and `08_adversarial.py` both had it wrong until it was fixed; the check
is `python3 gen_fixtures.py` twice and `git status` clean.
