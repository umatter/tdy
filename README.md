# tdy

Pure SQL over messy files. The structural cleaning — title blocks, two-row
headers, merged cells, Swiss number formats, subtotal rows, log-line regexes —
lives in an auditable, versionable *parsing spec*, never in your query.

```sql
SELECT region, monat, sum(umsatz_chf) AS umsatz
FROM messy('umsatz_2025.xlsx')
GROUP BY region, monat
ORDER BY region, monat
```

```bash
tdy query "SELECT ... FROM messy('umsatz_2025.xlsx') ..." -o umsatz.parquet
```

## The idea

Every messy file gets a **sidecar**: `umsatz_2025.xlsx.tdy.toml`, sitting
next to the raw file, checked into your repo. It records:

- a blake3 fingerprint of the file (stale specs are never silently reused),
- provenance (heuristic / llm / manual, model + prompt version, how many
  bytes the model saw),
- the **ParseSpec**: extraction → ordered structural transforms → typed
  output columns.

`messy('file')` in SQL exposes the file *as if it were already tidy*. SQL
stays pure — the dialect is stock DataFusion, no cleaning functions bolted
on.

## The rule that decides every design question

**tdy never silently produces a wrong value.** Where a file is ambiguous, the
acceptable outcomes are the right answer or a loud error naming the row —
never a plausible-looking wrong number. Concretely, that is why:

- a `thousands_separator` that could also be a decimal point (`.` or `,`)
  must group the integer part in threes, so declaring `,` a thousands
  separator over the German price `1,5` is an error rather than the number
  `15`. An apostrophe or a space can never be a decimal point, so sloppy
  Swiss grouping (`1000'000.00`) has exactly one reading and is accepted;
- `%Y` requires a four-digit year in the data, so `01/02/25` is an error
  rather than a date in the year 25;
- a column of `01/02/2025`-style dates that fits both day-first and
  month-first order is parsed *and* reported as ambiguous, with the
  confidence dropped below the escalation threshold;
- identifiers with leading zeros (`0234`) and integers too large for `i64`
  stay text instead of quietly losing information;
- money becomes an exact `decimal`, not a float.

## Two-tier inference

1. **Heuristics** (always, instant, deterministic): delimiter and encoding
   detection, title-row skipping, header detection, footer detection,
   per-column type inference including separator conventions (`1'234.56`,
   `1.234,56`, `1,5`), date formats and NA tokens — plus recognition of
   common log formats (nginx/apache, syslog, ISO-timestamped application
   logs) and column-aligned fixed-width reports. Confident result → the spec
   ships with `method = "heuristic"`.
2. **LLM** (only below the confidence threshold, only if configured): the
   model sees a *rendered sample* (never the whole file; Excel is rendered
   as a grid, not zip bytes) plus the heuristic draft, and emits a corrected
   spec. Decoding is grammar-constrained by a JSON Schema derived from the
   same Rust types the executor deserializes, then the result is validated
   and **dry-run against the actual file** before it's accepted; failures
   feed back into a bounded retry loop.

The model emits *instructions, not data* — every byte in your output went
through the deterministic executor, and the instructions are in the sidecar
for review.

**What tier 1 gets you on its own** (the default, with no backend
configured): delimited files with title blocks, footers, quoting, mixed line
endings and any encoding; JSON and NDJSON including a nested records array;
nginx/apache, syslog and ISO-timestamped logs; clean fixed-width reports; and
single-row Excel headers. What it does *not* do alone is invent structure —
a two-row merged header that needs unpivoting, a currency prefix that needs a
`strip` regex, a report with a decorated title block. For those it reports a
confidence below the threshold, says in `notes` what it could not read, and
either escalates to the model or waits for you to write the extraction by
hand. It does not guess and call it a result.

Backends: `none` (default — nothing ever leaves your machine), `local` (any
OpenAI-compatible server: llama.cpp, Ollama, vLLM), `anthropic`, and
`openrouter` (one endpoint in front of many models).

`local` is a promise about *where the server is*, so it is checked rather
than trusted: pointed at a non-loopback `base_url` it counts as remote, and
tdy prints how many bytes are leaving before they do.

```bash
export OPENROUTER_API_KEY=...
tdy sniff umsatz_2025.xlsx --backend openrouter --model google/gemini-2.5-flash
# note: sending 1169 bytes sampled from umsatz_2025.xlsx to openrouter (...)
```

The schema goes into the prompt as well as into `response_format`: most
providers do not actually enforce a schema that shape (OpenAI's strict mode
rejects it outright), and a model that has never seen the contract invents
fields. `TDY_MAX_RETRIES` raises the correction budget — each round carries
the exact failure back, so a hard file often converges given a few more.

## Commands

```bash
tdy query "SELECT ... FROM messy('f.xlsx')"        # pretty table to stdout
tdy query "..." -o out.parquet                     # or .csv / .ndjson
tdy query -f "..."                                 # --frozen: CI mode — fresh
                                                    # sidecars required, no
                                                    # inference, no writes
tdy sniff data/export.csv                          # infer + print spec + preview
tdy sniff weird.txt --hint 'nginx access log'      # nudge the LLM tier
tdy sniff f.xlsx --force --no-llm                  # re-run heuristics only
tdy validate data/export.csv                       # spec valid? fingerprint fresh?
                                                    # does it still parse?
tdy validate data/export.csv --stamp               # re-fingerprint a hand-edited
                                                    # spec against the current file
tdy schema                                         # the JSON Schema (the grammar)
tdy config init                                    # sample config + location
```

Config: `~/.config/tdy/config.toml`, overridable via `TDY_BACKEND`,
`TDY_MODEL`, `TDY_BASE_URL`, `TDY_MAX_RETRIES`, or
`--backend/--model/--base-url`.

`tdy validate --stamp` is what makes "edit the sidecar by hand" a real
workflow: it keeps your spec and re-computes the fingerprint, so a hand-written
extraction survives the next run.

## The spec language (sidecar body)

```toml
[spec.extraction]
format = "excel"            # delimited | excel | fixed_width | lines | json
sheet_name = "Umsatz"

[[spec.transforms]]         # applied in order, on raw strings
op = "skip_rows"            # skip_rows | promote_header | drop_rows_matching
head = 3                    # | fill_down | unpivot
tail = 1

[[spec.transforms]]
op = "promote_header"
rows = 2                    # two-row header: the upper rows fill right, then
                            # the rows are joined with " "

[[spec.transforms]]
op = "fill_down"
columns = ["Region"]        # vertically merged cells

[[spec.transforms]]
op = "unpivot"
id_columns = ["Region", "Produkt"]
value_columns = ["2025 Jan", "2025 Feb", "2025 Mär", "2025 Dez"]
variable_name = "monat_raw"
value_name = "umsatz_raw"

[[spec.columns]]            # a projection: unlisted columns are dropped
name = "monat"
source = "monat_raw"
dtype = { type = "date", format = "%Y %b" }
parse = { replace = [{ from = "Mär", to = "Mar" }, { from = "Dez", to = "Dec" }] }

[[spec.columns]]
name = "umsatz_chf"
source = "umsatz_raw"
dtype = { type = "decimal", precision = 12, scale = 2 }
parse = { thousands_separator = "'", strip = "^CHF\\s*", na_values = ["n/a"] }
```

Types: `utf8 | bool | int64 | float64 | decimal(p,s) | date | timestamp` —
mapping 1:1 onto Arrow. Typing happens *last*, so transforms never reason
about types and every cast failure points at an exact row.

Details worth knowing:

- **`promote_header` fills right only on the rows above the last one.** A
  horizontally merged title ("2025" spanning four month columns) leaves
  blanks to its right, and those inherit. A blank in the *final* header row
  does **not** inherit from its left neighbour — that would label one column
  with another column's meaning. It takes whatever the upper header rows
  contribute at that position, and becomes `col_N` only if they are blank
  too.
- **`fixed_width` offsets are character positions**, not bytes: the columns
  you would count in a monospace editor. Byte offsets shift by one for every
  non-ASCII character earlier in the line, sliding every later field into its
  neighbour.
- **`timestamp.timezone` converts, it does not relabel.** An Arrow timestamp
  with a timezone is a UTC instant, so `10:00` declared `+02:00` is stored as
  08:00 UTC. Only fixed offsets are accepted (`UTC`, `+02:00`, `-0500`); a
  named zone like `Europe/Zurich` is rejected rather than guessed, because
  daylight saving cannot be resolved from the value alone. If the format
  itself parses an offset (`%z`), that offset wins.
- **`decimal` rounds half away from zero** when a value has more fractional
  digits than `scale`. When the sniffer sees an inconsistent number of
  fractional digits it says so in `notes`, because rows it never read may be
  rounded.
- **`columns` is a projection.** There is no drop or rename op; `source` →
  `name` is the only renaming, and unlisted columns do not appear.

## Design notes

- **Sync/async split**: `TableFunctionImpl::call` runs during SQL planning
  and is synchronous, so LLM inference happens in an async pre-pass over the
  query text that materializes sidecars on disk; the UDTF only ever loads a
  sidecar or falls back to synchronous heuristics. `--frozen` is that split
  turned into a guarantee. The pre-pass *tokenizes* the SQL rather than
  regexing it, so a `messy()` call inside a comment or a string literal is
  not a file reference.
- **Every spec is gated before it is written.** Whether it came from the
  sniffer or the model, a spec is validated *and* dry-run against the real
  file before it reaches a sidecar. A sidecar loaded later is re-validated on
  every load — it is hand-editable, so it is untrusted input — and its
  fingerprint must still match the file, which is what makes re-running the
  dry run unnecessary. `tdy validate` runs it on demand.
- **The sniffer's columns come from the post-transform header.** It builds its
  column list from the header of a table that has already had every transform
  applied, so `source` always resolves and always resolves to the right
  column — including when two columns share a name.
- **Locale fixes are literal**: German month names are handled by explicit
  `replace` pairs in the sidecar, not by locale tables shipped in the
  binary. Dumber and fully auditable.
- **Bounded work**: sampling reads the head and tail of a file, not the file;
  previews and dry runs cap the extraction itself; output is produced in
  64k-row batches spread across partitions so queries use more than one core.

### Limits

`[limits]` in the config caps what a single run will attempt, so a
pathological file fails with a sentence instead of the OOM killer:
`max_file_bytes` (default 4 GiB) and `max_cells` (default 400M). Parsing is
in-memory: expect peak RSS of roughly 8× the size of a delimited file.

## Install

```bash
cargo install --path .        # puts `tdy` on your PATH
```

## Build & test

```bash
cargo build --release
cargo test --lib --tests    # 217 tests; plain `cargo test` also runs doc-tests
python3 gen_fixtures.py     # regenerate every fixture under testdata/
```

The suite is in five parts: unit tests beside the code; `tests/e2e.rs` (the
canonical messy-Excel fixture and SQL end to end); `tests/formats.rs` (what
each extraction and transform *means*, pinned with hand-written specs);
`tests/regression.rs` (one test per defect ever found, each written against
the correct behaviour rather than the observed one); `tests/fixtures.rs`
(exact values — sums, encodings, row counts — read from the committed hard
fixtures); and `tests/adversarial.rs`, which sweeps every generated fixture
and asserts that tdy never panics, never hangs, and that anything it can
sniff it can also query and reproduce under `--frozen`.

Everything above runs with `backend = none`; no test needs a network or a
model. The inference tier has its own suite, skipped unless you ask for it
because it costs money:

```bash
export OPENROUTER_API_KEY=...
TDY_LIVE_MODEL=google/gemini-2.5-flash cargo test --test live_backend -- --nocapture
```

It holds the model to the hand-written reference spec: given `umsatz.xlsx` —
title block, two-row merged header, merged Region cells, a subtotal row, a
Total footer, Swiss numbers and German month names — the spec it writes must
produce the same sixteen amounts totalling 21'244.25. The *shape* is left to
the model (long or wide are both faithful readings); the arithmetic is not.
`google/gemini-2.5-flash` and `anthropic/claude-sonnet-4.5` pass;
`openai/gpt-4o-mini` does not.

Rust ≥ 1.85 (DataFusion 46; a transitive dependency needs edition2024). Outputs Parquet/CSV/NDJSON — drop the Parquet
straight into DuckDB.
