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
cargo test --lib --tests                # 228 tests (skips doc-tests; see note below)
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

## Performance

Measured on a 141 MB / 3M-row CSV (release build), before → after the hardening pass:

| | before | after |
|---|---|---|
| `sniff` (needs a 16 KB sample) | 6.04 s, 1.20 GB RSS | **0.24 s, 24 MB** |
| `count(*)` over the whole file | 6.79 s, 1.40 GB | **4.84 s, 1.10 GB** |
| same file referenced twice | 2 full parses | 1 (cached per path) |

Peak RSS is roughly 8× the size of a delimited file; `[limits]` caps it rather than letting
it OOM. If you change extraction, re-measure with `/usr/bin/time -f "wall %es peak_rss %MkB"`.

## Test layout

- unit tests beside the code (127) — `numfmt`, `sqlscan`, `detect`, `spec::validate`, casting
- `tests/e2e.rs` — the canonical messy-Excel fixture and SQL end to end
- `tests/formats.rs` — what each extraction/transform *means*, with hand-written specs
- `tests/regression.rs` — one test per defect ever found, written against the **correct**
  behaviour rather than the observed one
- `tests/fixtures.rs` — exact values read from the committed hard fixtures (sums, encodings,
  row counts, dtypes). `adversarial.rs` proves nothing crashes; this proves the answers are
  right, which a parser returning nothing would also satisfy
- `tests/adversarial.rs` — sweeps every fixture in `testdata/`: never panic, never hang, and
  anything sniffable must be queryable and reproducible under `--frozen`. It picks up new
  fixtures automatically. Note it runs the binary with output to *files*, not pipes: a
  100k-column sidecar is megabytes, and an undrained pipe deadlocks at 64 KB.

## Fixtures

`testdata/` is generated, never hand-edited — `python3 gen_fixtures.py`. Each generator in
`testdata/gen/` owns a disjoint set of files and documents in its docstring what each file
stresses. `testdata/large/` is gitignored (perf fixtures, generated on demand).
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
