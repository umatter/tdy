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

**Types are checked, not guessed.** A type inferred from the first 500 rows is
a guess about all the others, and real exports break it routinely — a column
that is an integer for 40,600 rows and then `NA`, a station id that turns
alphanumeric at row 708. So `tdy sniff` reads the whole file and widens any
column whose guess does not hold, saying which values broke it:

```
column `station_id`: kept as text — 5 of 999 values are not an integer:
  "TA1309000067" (row 708), "TA1309000067" (row 845). If those are strays
  rather than data, drop them with a `drop_rows_matching` transform and
  narrow the type by hand.
```

That costs a full read — about 6 s for a 141 MB CSV, 40 s for 987 MB, at
bounded memory — paid once when the sidecar is written rather than on every
query. `tdy sniff --quick` skips it, and records in the sidecar that it did.

It is also why a header sitting in the middle of a file (two exports
concatenated) is handled rather than fatal. A repeat that is *byte-identical*
to the header is provably not data, so it is dropped; one that is merely
similar is reported, never dropped, because discarding rows that fail to parse
is exactly the silent data loss this tool refuses.

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

The hardest case measured so far is the decorated fixed-width report
(`testdata/logs_fixed_width_report_ascii.txt`): ruler lines, group headers and
an overflowed numeric field defeat tier 1 *and* both models that otherwise
pass the live suite. Write that extraction by hand — `tdy validate --stamp`
exists for exactly this.

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
tdy sniff huge.csv --quick                         # skip whole-file type checking
tdy validate data/export.csv                       # spec valid? fingerprint fresh?
                                                    # does it still parse?
tdy validate data/export.csv --stamp               # re-fingerprint a hand-edited
                                                    # spec against the current file
tdy fit sales.tdy.sql exports/2025-01.csv          # plan a spec that lands on
                                                    # a declared target
tdy fit sales.tdy.sql                              # fit every member, write the lock
tdy check sales.tdy.sql                            # CI gate: is the dataset still
                                                    # exactly what the lock says?
tdy check sales.tdy.sql --against exports/*.csv     # do these sidecars still produce
                                                    # the schema I declared?
tdy schema                                         # the JSON Schema (the grammar)
tdy config init                                    # sample config + location
```

Config: `~/.config/tdy/config.toml`, overridable via `TDY_BACKEND`,
`TDY_MODEL`, `TDY_BASE_URL`, `TDY_MAX_RETRIES`, or
`--backend/--model/--base-url`. Execution has two more:
`TDY_NO_STREAM=1` forces the materialising executor, and
`TDY_LAZY_ABOVE_BYTES` sets the size above which a file is scanned lazily
rather than parsed into memory once.

`tdy validate --stamp` is what makes "edit the sidecar by hand" a real
workflow: it keeps your spec and re-computes the fingerprint, so a hand-written
extraction survives the next run.

## Declaring the dataset you want

*In progress — the first piece is here, the rest is designed in
`docs/design/2026-08-30-target-schema.md`.*

Everything above describes a **source**: the shape of a file you have. The
direction this is going is the opposite — you declare the shape of the data you
*want*, in SQL, and point tdy at a pile of messy files:

```sql
-- exports/sales.tdy.sql
CREATE TABLE sales (
  month      DATE          NOT NULL,
  region     TEXT          NOT NULL,
  amount_chf DECIMAL(14,2) NOT NULL
)
WITH (files = '2025-*.csv, 2025-*.xlsx', date_order = 'dmy');
```

It is real SQL, parsed by the same parser DataFusion uses. What a target may say
is exactly what reaches the Arrow schema — a name, a type, a nullability — and
nothing else. In particular **no date format**, because that is a property of a
file: twelve monthly exports with twelve different formats all land on one
`DATE` column, which is the whole point.

Anything a target declares that tdy would not actually enforce is refused
rather than quietly widened, and the error names the spelling that works:
`SMALLINT` (tdy has one 64-bit integer type), `VARCHAR(50)` (one string type,
no length constraint), `TIMESTAMP(3)` (microseconds), `UNIQUE`/`CHECK`
(constraints are not enforced). A `DECIMAL` without an explicit scale is
refused too — it means something different in every dialect, and money is the
wrong place to inherit a default.

`tdy fit` plans a spec for each file that provably lands on that target:

```bash
$ tdy fit sales.tdy.sql 2025-09.xlsx
2025-09.xlsx fits `sales`:
  month            <- "Datum"                  DATE  (%d.%m.%Y)
  region           <- "Region"                 TEXT
  amount_chf       <- "Betrag CHF"             DECIMAL(14,2)
```

That workbook has a title row and a merged band above the real header, its
amount column is spelt differently from every other month's, and its dates are
day-first — none of which you had to say. An English export in the same folder,
with `Date`/`Amount` and ISO dates, lands on the same three columns.

Because a target names what you *want* and the files are somebody else's
exports, a column may declare the header cells it can be read from:

```sql
amount_chf DECIMAL(14,2) NOT NULL OPTIONS(matches = 'Betrag, Betrag CHF, Amount')
```

Those are declared, in the open, in a diff — because a planner guessing at
synonyms is exactly what this tool does not do. `--propose` does the mechanical
half for you:

```
$ tdy fit sales.tdy.sql 2025-07.csv --propose
  suggestions:
    `amount_chf` (DECIMAL(14,2)):
      could be supplied by:
        "Betrag Rp."  — all 4 sampled value(s) parse as DECIMAL(14,2)
      Type-compatible is not the same as correct — a discount column parses as money too.
      If one of them is right, say so:
        amount_chf DECIMAL(14,2) OPTIONS(matches = 'Betrag, Betrag CHF, Betrag Rp.')
```

It says a column's values *parse* as the declared type. It never says they mean
the right thing, and it never edits your target.

**What it refuses is the point.** Of the twelve exports in
`testdata/drifting_exports/`, three cannot be fitted and each is a different
way to be quietly wrong:

```
2025-07.csv   `amount_chf`: no column of this file binds
                looked for "amount_chf", "Betrag", "Betrag CHF", "Amount"
                the file has ["Datum", "Region", "Betrag Rp."]

2025-08.csv   `amount_chf`: 2 columns of this file match, which is ambiguous
                column 3 named "Betrag" and column 4 named "Betrag"
                tdy will not choose between them

2025-11.csv   `region`: no column of this file binds
```

The first holds integer Rappen — it parses, it type-checks, and binding it
would be out by a factor of a hundred with the error invisible in any single
row. The second has a net and a gross column with the same name; taking the
first is right half the time and silent about it. The third is short a column,
and a null-filled `region` would make an aggregate quietly short a month.

Ambiguous dates are refused the same way. `03/04/2025` is March or April and
the file does not say, so it is a gap until the dataset declares
`date_order = 'dmy'`. The test is exact rather than nervous: two formats are
ambiguous only if they **disagree about a value actually in this file**.

The gate underneath:

```bash
$ tdy check exports/sales.tdy.sql --against exports/2025-01.csv
exports/sales.tdy.sql: `sales`, 3 column(s)

exports/2025-01.csv.tdy.toml: CONFORMS

1 of 1 file(s) conform to `sales`.
```

It reads no data. `engine::schema_of` derives a spec's output schema by building
every column over *zero* rows, so comparing it to a declared schema proves —
before a byte is read, for every row the spec will ever emit, on both
executors — that this file produces exactly those columns with exactly those
types. That is a much stronger contract than "the head parsed", and it makes a
useful CI gate on its own: *do the sidecars I already have still produce the
schema my downstream expects?*

Shape is proved; **values are not** — by that comparison. What proves the values
is reading them, and `verify` says how much of the file to read:

```sql
WITH (files = '2025-*.csv', verify = 'full')   -- the default: every row
WITH (files = '2025-*.csv', verify = 'head')   -- the bounded prefix only
```

`verify = 'full'` is the default because the prefix lies. `testdata/late_surprise_*`
is four files reduced from real exports where it does: a `station_id` that is
digits for seven hundred rows and then `TA1309000067`, a `children` column that is
an integer for forty thousand and then `NA`. A plan typed from the head of those
files is a plan that dies mid-query on a file `fit` called fittable. Proving the
declared type on every row is what makes "it fits" mean something, and `'head'`
is there for datasets where that read is too expensive to pay on every fit.

Either way a per-row parse failure, a grouping violation, or a null in a NOT NULL
column is still caught per row, loudly, naming the row, at execution.

With a lock, `tdy check <TARGET>` needs no `--against`: it runs exactly the
checks a query runs — drift, every sidecar present and fresh, every member still
conforming, nothing waiting on a human — and exits non-zero if any of them fail.
That is the one command to put in CI.

`tdy fit` with no file plans every member and records what the globs resolved
to, and then the whole pile is one relation:

```bash
$ tdy fit sales.tdy.sql
sales: 9 file(s) match, 3 declared column(s)
  2025-01.csv    fits    month<-"Datum"  region<-"Region"  amount_chf<-"Betrag"
  …
  2025-10.xlsx   fits    month<-"Date"   region<-"Region"  amount_chf<-"Amount"
9 of 9 file(s) fit `sales`.
wrote sales.tdy.lock

$ tdy query "SELECT region, sum(amount_chf) FROM dataset('sales.tdy.sql') GROUP BY 1"
```

A member is named by its path relative to the target — `exports/2025-07.csv`,
not `2025-07.csv` — and that is the name `--accept` takes.

Membership lives in the lock, and `dataset()` never expands a glob. That is
the difference between a reproducible dataset and a directory listing: if the
glob were evaluated at query time, the same query over the same declaration
would return a different number the morning after an export landed, with
nothing to point at and nothing to diff.

So a new file is **drift** — the query stops and names it:

```
Error: dataset `sales` is out of date:
  2025-13.csv matches this dataset and is not in the lock — run `tdy fit` to plan it
```

The same for an edited member, a removed one, or a change to the declaration
itself. A comment in the target does *not* invalidate anything: the lock
fingerprints what the declaration **means**, not its bytes, because the point
of writing it in SQL is that it reads like documentation.

### When a file needs a human

Some things no proof can settle. `2025-07.csv` holds integer Rappen: it parses,
it type-checks, and reading it as francs is wrong by a factor of a hundred with
the error invisible in any single row. Nothing declares that column, so the
planner refuses it — and you write its spec by hand, in its sidecar, where all
the other structural cleaning lives:

```toml
[[spec.columns]]
name   = "amount_chf"
source = "Betrag Rp."
[spec.columns.parse]
decimal_shift = -2          # exact: 123450 -> 1234.50, no float involved
```

`decimal_shift` moves the decimal point on the digit string, so nothing is
rounded and no float is introduced — which matters, because the only reason it
exists is money. A hand-written spec is marked `method = "manual"` and the
planner will not overwrite it, but it is proved exactly as a planned one is:
conformance, then a dry run.

And then it still does not run:

```
2025-07.csv   REVIEW  (hand-written spec)
    `amount_chf` applies decimal_shift = -2, which changes every value
    tdy does not accept a value-changing step on its own judgement.
    Accept:  tdy fit sales.tdy.sql --accept 2025-07.csv
```

That is the sharpest line in the design. Everything the planner does is
mechanically checked, and none of it can establish that a column of integers is
*francs* rather than *Rappen* — so a person says so, once, and the acceptance is
recorded against that file's bytes and that declaration. Re-fitting an untouched
dataset does not ask again; editing the file expires the acceptance, because it
was about those bytes.

A file with several possible *frames* — a JSON document holding several
arrays of records (the single most common "unsure" in real data), a workbook
holding a cover page, a legend and a data sheet — needs no model and no human
once a table is declared: `tdy fit` frames every candidate (each sheet gets
its own framing — its own title rows, its own footer) and tries the
declaration against each. If exactly one fits, the frame is **proved by
elimination** and the note says so; if several fit, the file is refused with
each candidate named, because two complete, well-typed answers with different
totals is a guess this tool refuses to make; if none fit, you get the
ordinary gap report.

When the layout cannot be enumerated at all — a log line, a report format no
delimiter sniff can frame — and a backend is configured, `tdy fit` asks the
model for the **frame only**: extraction and structural transforms. Its
columns are discarded exactly as the sniffer's are; binding, types,
conformance, the dry run and the whole-file check are all proved on this
side. What no gate can prove is that the model's frame is the only reading of
the file, so the member is marked for review and `dataset()` refuses it until
`--accept` — and a *proven* ambiguity (two fitting arrays, an ambiguous
separator or date order) is never sent to a model at all: declarations settle
those.

Two neighbouring cases draw the same line from both sides. A column one export
simply *lacks* — November predates `Region` — is declarable in the target:

```sql
region TEXT NULL OPTIONS(matches = 'Region', if_missing = 'null')
```

and the planner null-fills it with a note and **no** review, because the
declaration sits in the reviewed `.tdy.sql` — the planner is executing your
decision, not making one. (`if_missing` is refused on a NOT NULL column, and
`'null'` is the only declarable fallback.) But a constant *value* — "November
is all Ticino" — is data the file never contained, so it lives in the sidecar
as a hand-written transform and gates behind `--accept` like the shift does:

```toml
[[spec.transforms]]
op    = "constant"
name  = "region"
value = "Ticino"
```

A `constant` may only add a column, never shadow one the file already has.

If any member cannot be fitted, **no lock is written at all**. A dataset that
silently omits the months that did not fit is exactly the failure this is
built to prevent.

Members are read in lock order in a single partition, so row order is
deterministic and `--frozen` keeps meaning "same files, same answer". Because
conformance already proved every member has an identical schema, the union is
a concatenation — there is nothing to coerce, and no chance of the silent
Int64-plus-Utf8-becomes-Utf8 widening an ordinary `UNION ALL` would do.

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

Spreadsheets: `.xlsx`, `.xlsm`, `.xls` (BIFF8) and `.ods` all go through
`format = "excel"` and are covered by fixtures that assert the same table
reads identically in every container. `.xlsb` is routed too and calamine
reads it, but there is no pure-Python writer for it, so it has no generated
fixture and is the one spreadsheet path this suite does not check.

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
- **Text files stream.** The obvious way to run a spec — read the file into
  `Vec<Vec<String>>`, transform it, then type it — costs about eight bytes of
  memory per byte of source, because a five-character field carries a 24-byte
  `String` header and its own allocation. Delimited files, log lines and
  fixed-width reports instead go row by row into 64k-row Arrow batches, and
  that intermediate never exists:

  | `count(*)` over | materialising | streaming |
  |---|---|---|
  | 140 MB CSV, 3M rows | 3.11 s, 1,676 MB | **2.93 s, 86 MB** |
  | 190 MB nginx log, 2M lines | 3.27 s, 1,376 MB | **2.76 s, 98 MB** |
  | 987 MB CSV, 21M rows | refused: over `max_cells` | **20.9 s, 88 MB** |
  | 134 MB CSV, 1,000 columns | refused | **6.4 s, 114 MB** |
  | 138 MB NDJSON, 1.5M records | 2.9 s, 2,128 MB | **2.9 s, 78 MB** |

  Memory does not move with the file, in either dimension: a 987 MB CSV is
  read in 88 MB, the same as a 140 MB one, and a thousand-column file in 114
  MB. Nothing proportional to the source is ever held — not the rows, not the
  decoded text, and a batch is bounded by cells rather than rows, since a row
  is as wide as the file.

  Faster as well as smaller: not allocating tens of millions of strings more
  than pays for the extra passes. (A log needs no counting pass at all — its
  columns are named by the pattern's capture groups, so there is no width to
  discover — unless a `skip_rows` tail makes the row count matter.)

  A file whose sidecar names a non-UTF-8 encoding is still decoded whole,
  because choosing an encoding correctly needs the whole file: one of the test
  fixtures is ASCII for 12 KB and then is not, and a spec deliberately leaves
  `encoding` unset rather than freeze a guess made from a sample. When it is
  unset, tdy checks incrementally whether the file is valid UTF-8 throughout —
  the same question the whole-file decoder asks, answered in a fixed buffer —
  and streams if it is.

  Above 64 MB a streamable file also stops being loaded into a table at all:
  `messy()` returns a lazy provider that re-reads the file on each scan and
  hands DataFusion one batch at a time over a bounded channel, so no batch
  outlives the operator consuming it. Below that threshold the file is parsed
  once and cached instead, which is the better trade when a query names the
  same file twice. `TDY_LAZY_ABOVE_BYTES` moves the line.

  NDJSON streams too, though its header — the union of every record's keys —
  has to be discovered by a pass over the file first, since a key that appears
  only in the last record still has to become a column. Excel and a JSON
  *array* do not stream: each is one document, and no record exists until it
  has been parsed whole. Neither does an unusual transform order — those fall back to
  the materialising path, so no spec is ever *refused* for being unusual. The
  two executors are held to producing identical batches over every text
  fixture in the tree, and `TDY_NO_STREAM=1` forces the old path if you want
  to check that on a file of your own.

### Limits

`[limits]` in the config caps what a single run will attempt, so a
pathological file fails with a sentence instead of the OOM killer:
`max_file_bytes` (default 4 GiB), `max_cells` (50M) and `max_streamed_cells`
(2B).

`max_cells` is the memory bound, and it applies to work that is
*materialised*: spreadsheets, JSON, and any spec the streaming executor
declines. A cell costs about 122 bytes there, so 50M stands for a ceiling of
roughly 6 GB.

`max_streamed_cells` bounds time rather than memory, because streaming has no
memory cost that follows the cell count — it holds neither the rows nor the
decoded text. `max_file_bytes` is normally what stops a long run first.

For spreadsheets the limits are checked against what the file **declares**,
before its grid is allocated. They have to be: a spreadsheet's size is a
claim rather than a consequence, so 899 bytes of `.ods` can ask for a
billion cells, and a limit applied to the table afterwards is a limit
applied after the damage. tdy reads the declared geometry first — from
`content.xml` for `.ods` (whose reader parses eagerly, so this happens
before the workbook is opened at all) and from `<dimension>` for
`.xlsx`/`.xlsm` — and refuses in milliseconds. `max_file_bytes` is applied
to a zip container's *uncompressed* total, which is what has to be held.

The count is of cells that carry a value, not of cells the file mentions:
LibreOffice pads every sheet out to the full 1,048,576-row grid, so counting
the claim would reject almost every `.ods` ever written.

`max_cells` is calibrated from measurement — end to end a spreadsheet cell
costs ~122 bytes and a delimited one ~46 — so 50M is a ceiling of roughly
6 GB. Raise it if you have the RAM and mean it.

## Install

```bash
cargo install --path .        # puts `tdy` on your PATH
```

## Build & test

```bash
cargo build --release
cargo test --lib --tests    # 355 tests; plain `cargo test` also runs doc-tests
python3 gen_fixtures.py     # regenerate every fixture (needs openpyxl + xlwt)
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

Rust ≥ 1.88 (DataFusion 46). The floor comes from the dependency tree —
reqwest → url → icu_* — not from this crate, and CI checks it against the
committed lockfile. Outputs Parquet/CSV/NDJSON — drop the Parquet
straight into DuckDB.
