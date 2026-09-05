# The data-munging operator catalogue, and where tdy stands

*2026-09-05. A survey of the techniques that turn messy data into tidy data, one
canonical name per concept with the synonyms every other system uses, and an
honest column saying whether tdy does it, could do it, or refuses to.*

---

## 0. How to read this

### 0.1 Why a catalogue at all

"Data munging" is one activity with a dozen vocabularies. The operation R calls
`pivot_longer`, pandas calls `melt`, Potter's Wheel called `fold`, SPSS calls
`VARSTOCASES`, Stata calls `reshape long`, SQL Server calls `UNPIVOT`, Pentaho
calls a Row Normaliser and OpenRefine calls "transpose cells across columns into
rows" — all the same operator. A coverage question ("does tdy handle X?") is
unanswerable while X has nine names, because the answer keeps being "we have that
under a different name" or, worse, "we thought we had that."

So: **one canonical name per concept**, chosen for clarity rather than for
seniority, with an alias list underneath. The alias lists are the load-bearing
part. They are what let you check tdy against a stranger's mental model.

### 0.2 The scoping question this catalogue exists to settle

tdy is not a transformation engine and should not become one. It occupies a
specific slice of the pipeline:

```
  bytes on disk ──► [ tdy: the sidecar ] ──► a typed Arrow table ──► [ SQL ] ──► answers
                     structural cleaning                              analysis
```

Everything left of the arrow has to happen before a table exists, so it cannot be
expressed in SQL — you cannot `SELECT` your way out of a title block, a
three-row banner header, or a decimal comma. Everything right of it is ordinary
relational work that DataFusion already does well, and re-implementing it in the
spec language would be duplication.

That gives every operator in this catalogue one of six verdicts:

| Tag | Meaning |
|---|---|
| **`spec`** | tdy does it in the sidecar or target — declarative, fingerprinted, provable before reading a byte |
| **`sql`** | Not in the spec, but available downstream in the query tdy hands to DataFusion |
| **`partial`** | Some cases covered, named ones not |
| **`gap`** | Neither, and it plausibly belongs left of the arrow |
| **`out`** | Deliberately outside tdy's remit, with a reason |
| **`rule`** | Excluded because it would violate *tdy never silently produces a wrong value* |

The distinction between `spec` and `sql` is not cosmetic. A `spec` operator is
recorded in a file that is hashed, versioned, re-validated on every load, proved
against a declared schema, and reviewable by a human before a dataset accepts it.
An `sql` operator lives in whatever query someone happened to type. Both "work";
only one is provenance. When this report says an operator "should move left," it
means exactly that: it is being done in an unrecorded place.

### 0.3 What was checked, and how

Coverage claims about the spec language are read from `src/spec.rs`, which is the
single source of truth for `Extraction`, `Transform`, `DType` and `ValueParsing`.
Claims about downstream SQL were **probed against the actual binary**, not
recalled — DataFusion 46's feature set is not the same as PostgreSQL's, and
several things a reader would assume are present are not (§0.4). Where a probe
found something surprising it is written up in place with the command that found
it.

### 0.4 Four things verified by probe, worth knowing up front

1. **DataFusion has no `PIVOT` / `UNPIVOT` syntax.** `SELECT … PIVOT (…)` fails
   with `Unsupported ast node Pivot`. Long-to-wide downstream means
   `CASE`+`GROUP BY` by hand. (DuckDB, Snowflake, Oracle, T-SQL and Spark all
   have it; this is a DataFusion limitation tdy inherits.)
2. **No `QUALIFY`.** `Error: This feature is not implemented: QUALIFY`. Window
   filtering needs a subquery.
3. **Recursive CTEs work**, but the CTE's column alias list is not honoured —
   `WITH RECURSIVE t(n) AS (SELECT 1 …)` fails with `No field named n`; you must
   write `SELECT 1 AS n`.
4. **A hand-written `strip` can silently flip a sign.** Sniffing
   `"(1,234.50)"` correctly declines to type it (it stays `utf8`, confidence
   0.95, no wrong number). But the only tool the spec language offers for fixing
   it — `strip = "[()]"` — produces `+1234.50`, and `tdy validate` accepts the
   spec. This is the one finding in this report that touches the project's
   central rule directly; see **E5**.

### 0.5 A second pass, against the literature and the tool docs

The first draft of this catalogue was written from the code and from recall. It
has since been checked against the research literature on data cleaning and
against current tool documentation, and **Part M** records what that pass found:
four published taxonomies that cover the same ground from different angles, a
benchmark tdy can actually be scored on, and a set of measurements from a survey
of 3,712 real-world CSV files that turn several judgement calls in this report
into numbers. Where those numbers changed a verdict — B7 and B10 both moved — the
entry says so in place.

---

# Part A · Physical decoding
*Bytes → characters. Nothing here is a table yet.*

### A1 · Encoding detection and transcoding
**Also called:** `encoding=` (pandas, csv), `locale(encoding=)` / `guess_encoding`
(readr), `fileEncoding` (base R), `iconv` / `recode` (unix), chardet / charset-normalizer
(Python), chardetng (Rust), `unicode analyze`/`unicode translate` (Stata),
"File Origin" (Excel import), `Text.Encoding` (Power Query), CPG sidecar files
(Esri), `mlr utf8-to-latin1` / `latin1-to-utf8` (Miller), `encoding` in the
Frictionless CSV Dialect descriptor.

**Messy → clean:** a file written in windows-1252, latin-1, cp437, Shift-JIS or
UTF-16 read as if it were UTF-8, producing `Ã¼` or replacement characters, or
failing outright.

**`spec`** — `encoding` on `delimited` / `fixed_width` / `lines`, taking
encoding_rs labels; `src/detect.rs` guesses when it is unset. Two design points
worth keeping in mind when comparing to other tools: tdy deliberately leaves
`encoding` **unset** after sniffing when the sample is pure ASCII, because an
ASCII sample proves nothing about byte 40,000 (fixture:
`enc_late_1252_byte.csv`); and detection feeds the detector *bounded windows
around the non-ASCII bytes* rather than the whole file, which is what took a
22 MB latin-1-tainted CSV from 8.6 s to 0.96 s.

### A2 · Byte-order-mark handling
**Also called:** `utf-8-sig` (Python), `encoding="UTF-8-BOM"` (R), "remove BOM",
`sed '1s/^\xEF\xBB\xBF//'`.

**Messy → clean:** the first column of the header is named `﻿id` and every
join against it fails, invisibly, because the character is zero-width.

**`spec`** — stripped in both executors; the streaming reader strips it too,
which is called out in the architecture notes precisely because forgetting it in
one of the two paths is the classic way to make them disagree.

### A3 · Line-terminator normalisation
**Also called:** `dos2unix`, `newline=` (Python), `eol=` (data.table), CRLF/LF/CR,
"Macintosh line endings", `tr -d '\r'`.

**Messy → clean:** a trailing `\r` on the last field of every row, which turns
`"42\r"` into text and every trailing-column type inference into `utf8`.

**`spec`** — handled inside the csv reader and the line source. Not a
user-visible option, which is correct: there is no case where you want the wrong
answer here.

*Measured in the wild:* of the 3,712 real-world CSVs surveyed for the Pollock
benchmark, 1,999 use CRLF, 1,691 use LF alone, and 7 use a bare CR — so the
"non-standard" ending is very nearly half the corpus, and CR-only still exists.

### A4 · Mojibake repair
**Also called:** ftfy (Python), "fix encoding" / `value.reinterpret()` (OpenRefine),
`Encoding<-` gymnastics (R), double-encoding, "UTF-8 read as latin-1 then
re-encoded as UTF-8".

**Messy → clean:** `Ã¤` → `ä`, `â€™` → `’`. Distinct from A1: the damage is
already baked into the bytes; no encoding declaration recovers it.

**`partial`** — expressible as literal `replace` pairs, which is exactly the
"dumb and auditable" stance tdy takes on locale data, but there is no
`reinterpret` operator and no mojibake detector. For a corpus of scraped exports
this is a plausible small addition; for tdy's current corpus it has not come up.

### A5 · Invisible and confusable character cleanup
**Also called:** NBSP / ` `, zero-width space, soft hyphen, `str_squish`
(stringr), `TRIM`+`CLEAN` (Excel), `Text.Clean` (Power Query), NFC/NFKC
normalisation (`unicodedata.normalize`, `stringi::stri_trans_nfkc`), homoglyph
folding, `strip_accents` / transliteration (`unidecode`, `iconv //TRANSLIT`),
`mlr clean-whitespace` (strips leading/trailing and collapses runs).

**Messy → clean:** `1 234` where the space is U+00A0 and no numeric parser
accepts it; two "identical" category labels that differ by a combining accent and
so form two groups.

**`partial`** — `trim` runs before every cast, and `strip` (a regex) removes
whatever you name, so NBSP and friends are reachable. Unicode *normalisation* is
not available at any layer (DataFusion has no `normalize()`), so NFC/NFKC folding
has no home. Low priority, real.

### A6 · Container and compression handling
**Also called:** `compression='gzip'` (pandas), `read_csv("f.csv.gz")` (readr,
duckdb, polars — all transparent), `zcat`, zip member selection, tar extraction.

**Messy → clean:** the export arrives as `daten.csv.gz` or as one CSV inside a
zip, and every tool in the pipeline has to be taught to unwrap it.

**`gap`** — tdy reads zip internals for xlsx/xlsb/ods (that is what `xlmoney` and
`xlguard` do) but a `.csv.gz` is not readable, and a zip-of-CSVs cannot be a
dataset member. Given that `dataset()` exists to point at a pile of monthly
exports, and monthly exports arrive zipped, this is the most ordinary gap in
Part A. It also interacts with `xlguard`: a compressed member's declared size is
a claim, and the zip-expansion check already exists for xlsb.

### A7 · Statistical and binary tabular formats
**Also called:** `.dta` (Stata), `.sav`/`.zsav` (SPSS), `.sas7bdat`+`.xpt` (SAS),
`.rds`/`.rdata` (R), `.mat`, `.parquet`, `.feather`, `.avro`, `.orc`, fixed-width
EBCDIC mainframe extracts with copybooks; readers: haven (R), pyreadstat,
`import spss/sas/dta` (Stata), `PROC IMPORT`.

**Messy → clean:** these formats carry the metadata tdy has to reconstruct —
declared types, value labels, variable labels, missingness codes.

**`out`** for parquet/feather (DataFusion reads those natively; `messy()` is for
files that *need* cleaning). **`gap`, arguably** for `.dta`/`.sav`/`.sas7bdat`:
they are the single largest source of messy-but-labelled social-science data, and
their value labels are exactly the crosswalk tables **F2** describes. A large
addition; noted, not recommended.

---

# Part B · Dialect and record framing
*Characters → rows of fields. Still not a table: no header, no types.*

### B1 · Delimiter and dialect sniffing
**Also called:** `csv.Sniffer` (Python), CleverCSV, `sep=NULL` autodetect
(data.table `fread`), `read_csv_auto` / `sniff_csv` (DuckDB), Text Import Wizard
(Excel), `qsv sniff`, hypoparsr (R, research prototype).

**Messy → clean:** semicolon-delimited German exports, tab-delimited files named
`.csv`, pipe-delimited dumps, `sep=` declared in a first line.

**`spec`** — `delimiter` in `Extraction::Delimited`, sniffed with the
ambiguity rule that matters: when nothing in the file settles which of two
delimiters is meant, confidence drops rather than a coin being flipped.

*Measured in the wild* (Pollock's survey of 3,712 files): 2,754 comma, 834
semicolon, 101 comma-plus-whitespace-or-tab, 8 tab or whitespace runs. So a
quarter of real CSV files are not comma-delimited, and 12 files mix delimiters
*within* one file — typically because the preamble or footnote lines are
whitespace-separated while the table is not.

### B2 · Quoting and escaping dialect
**Also called:** `quotechar`/`doublequote`/`escapechar` (Python), `quote=`/`escape_double`
(readr), RFC 4180 conformance, "embedded newlines", `--quote-char` (xsv/qsv),
`Text.Delimited` quote styles (Power Query).

**Messy → clean:** a field containing the delimiter; a field containing a
newline; `""` vs `\"` escaping; an unbalanced quote that swallows the rest of the
file.

**`spec`** — `quote` and `escape`. Embedded newlines work because the reader is a
real CSV parser, not a line splitter — the distinction that separates tools that
survive a free-text `notes` column from tools that do not.

*Measured in the wild:* 1,596 of 3,712 files quote nothing at all, 2,090 use the
double quote, 11 use an apostrophe. Of the 2,101 files that do quote, only 250
contain an RFC-style doubled-quote escape — and Pollock notes that backslash
escaping, though absent from its survey, is the common non-standard alternative.
It also found that **only 2 of 16 systems tested loaded a non-standard escape
correctly**, the rest dropping either the rest of the cell or the whole row.

### B3 · Comment and metadata line skipping
**Also called:** `comment=` (pandas, readr), `comment.char` (base R),
`skip_lines_with` (Miller), `#` preamble, `%` (Matlab), `--comment` (qsv).

**Messy → clean:** files whose first forty lines are `# generated by …`, or where
`#` lines are interspersed as section markers throughout.

**`spec`** — `comment` on `delimited`. Note this is the *character* form; a
prose preamble with no marker is **C1** instead.

### B4 · Fixed-width field extraction
**Also called:** `read_fwf` (pandas, readr), `LRECL`/`INPUT @` column pointers (SAS),
`infix` (Stata), `substr` slicing, "positional file", COBOL copybook layouts,
`Text.Range` (Power Query), `qsv fixlengths` (the inverse).

**Messy → clean:** government and banking extracts with no delimiter at all,
where field boundaries are character positions in a monospace layout.

**`spec`** — `Extraction::FixedWidth` with `fields: [{name, start, end}]`.
tdy's positions are **character** offsets after decoding, not bytes — a
deliberate choice recorded in the type's doc comment, because byte offsets slide
every field right of the first non-ASCII character in the line.

### B5 · Regex line parsing
**Also called:** grok (Logstash), `rex`/field extraction (Splunk), `read_log`
(readr), awk with `FS`/`match()`, `str_match` + `unnest_wider` (tidyverse),
named capture groups, `--regex` parsing (Miller's `nest`/`split`), Fluentd parsers.

**Messy → clean:** an nginx/Apache/syslog line is one string with an internal
grammar; each capture group is a column.

**`spec`** — `Extraction::Lines { pattern, on_no_match }` with named captures
becoming columns. `on_no_match` is the "banner lines in the middle of a log"
policy: `skip` (default) or `error`.

### B6 · Ragged-row policy
**Also called:** `fill=TRUE` (fread), `on_bad_lines='skip'|'warn'|'error'` (pandas),
`problems()` (readr), `ignore_errors` / `null_padding` / `store_rejects` with its
`reject_scans` + `reject_errors` tables (DuckDB), `PERMISSIVE` /
`DROPMALFORMED` / `FAILFAST` + `_corrupt_record` (Spark), `unsparsify` (Miller),
error output branch (SSIS), `qsv fixlengths`.

**Messy → clean:** a row with more or fewer fields than the header — usually an
unquoted delimiter in a free-text field, sometimes a genuinely different record
type.

**`spec`** — `RaggedPolicy::{Error, PadNulls, TruncateExtra}`, defaulting to
`Error`. That default is the project's stance in miniature: the common
convenience default elsewhere (silently drop the bad line) is the exact silent
data loss tdy exists to refuse. Note that `TruncateExtra` *does* drop data and is
therefore an opt-in.

*Measured in the wild:* **1,040 of 3,712 files have an inconsistent number of
cells** — 28%. Pollock attributes this to preamble lines with different separator
counts (221 files), multiple tables in one file, and genuine schema drift within
the records. Ragged input is not an edge case; it is more than a quarter of the
population, which is worth knowing when judging a default that refuses it.

### B7 · Multi-character and regex delimiters
**Also called:** `sep='\s+'` / `delim_whitespace` (pandas), `sep="||"`,
`FS="[ \t]+"` (awk), `--ifs` regex (Miller), `Text.SplitAny` (Power Query).

**Messy → clean:** `field1 :: field2 :: field3`, or whitespace-aligned output
where runs of spaces separate fields.

**`gap`** — `delimiter` is a single `char`. Whitespace-aligned output is
partially reachable via `fixed_width` (if the columns really are aligned) or
`lines` with a regex (if they are not), so the practical gap is narrow: literal
multi-character delimiters like `||` or `;;` have no expression at all.

**Revised upward after the literature pass.** The first draft called this
"trivial, rare." Pollock's survey found **101 of 3,712 files (2.7%) delimited by
a comma *plus* whitespace or tab** — a two-character delimiter, and the third
most common dialect in the corpus after comma and semicolon. That is not rare
enough to dismiss, and it is a `char` → `String` change plus a longest-match
split.

### B8 · Record-array framing in semi-structured documents
**Also called:** JSON Pointer / JSONPath / `jq '.data.rows'`, `record_path`
(`json_normalize`), `read_json(… , records)` (DuckDB/polars), `OPENJSON` (T-SQL),
NDJSON / JSON Lines / `.jsonl`, `from_json` + `explode` (Spark).

**Messy → clean:** an API dump wraps the actual rows in
`{"meta":…, "result":{"records":[…]}}`, and there may be several arrays that
*look* like records.

**`spec`** — `Extraction::Json { lines, pointer }`, plus something most tools do
not have: when the pointer is not declared, `sniff::json_record_pointers`
enumerates every candidate array and `fit` tries the declared table against each.
Exactly one fits → proved by elimination. Several fit → `AmbiguousFrame` naming
them and the field that settles it. This is the deterministic tier of slice 5.

### B9 · Sheet and range selection
**Also called:** `sheet_name=` (pandas), `sheet=`/`range="B4:H200"` (readxl),
`usecols`, "used range", `Excel.Workbook` (Power Query), `PROC IMPORT SHEET=`,
`import excel, sheet()` (Stata).

**`spec`** — `sheet_name` / `sheet_index` / `range` (A1-style, validated before
calamine sees it). Sheet *frames* are enumerable candidates the same way JSON
pointers are: each sheet is framed independently, because a title row is a fact
about a sheet, not about a workbook.

### B10 · Multiple tables in one file or sheet
**Also called:** "multiregion files" (the term the research literature settled
on), "multiple regions", "data islands", `CurrentRegion` (VBA),
`unpivotr`/`tidyxl` cell-level partitioning (R), DeExcelerator, Senbazuru,
TableSense (Microsoft Research), Mondrian (HPI), `pd.read_excel` + manual
slicing, "stacked tables".

**Messy → clean:** one sheet holds three tables separated by blank rows, or a
summary block beside the data block.

**`partial`** — a declared `range` extracts one block, and the frame-elimination
machinery picks between *sheets*, not between *blocks within a sheet*. There is
no operator that says "this file contains N tables; here they are."

**Revised upward after the literature pass**, on two counts. First, it is
measurable: **188 of 3,712 real-world CSVs (5.1%) contain multiple tables**,
"at times with preamble lines or multiple header lines" — and that is CSVs
alone, where the layout is far rarer than in spreadsheets. Second, it is a
solved research problem with published methods: Mondrian (Vitagliano, Jiang &
Naumann, PVLDB 2022) renders a sheet's cells as coloured pixels, segments the
image into regions and fingerprints each one, precisely to detect *layout
templates* shared across a pile of files — which is very close to what
`tdy draft` does for column vocabularies. See **M4**.

### B11 · Heterogeneous record types in one file

**Also called:** `having-fields` / `group-like` / `regularize` / `sparsify` /
`unsparsify` (Miller — the richest vocabulary for this), schema merging
(`mergeSchema`, Spark), "jagged JSON", union-of-keys, mixed record types in one
NDJSON stream, multi-record-type flat files (mainframe files where a leading type
code selects the layout), `OPENJSON` with different `WITH` clauses per type.

**Messy → clean:** an NDJSON export where half the records carry a `refund`
object and half do not; a log where three line grammars are interleaved; a
mainframe extract where column 1 says which of four record layouts this line uses.

**`partial`** — NDJSON is handled properly: `discover_ndjson` makes a real pass
over the file and takes the **union** of every record's keys, because a key that
appears only in the last record still has to become a column. Interleaved *line
grammars* are handled by `Extraction::Lines` with `on_no_match = "skip"`, which
covers the log case by discarding the other grammars rather than parsing them.
What has no expression is a file that genuinely contains two record types both of
which you want — that is two tables in one file, i.e. **B10** again, seen from
the row direction rather than the block direction.

---

# Part C · Framing the table
*Rows of fields → a rectangle with a real header. This is the part SQL cannot
reach, and it is where tdy's distinctive work lives.*

### C1 · Preamble and title-block removal
**Also called:** `skip=` (readr, readxl), `skiprows=` (pandas), `FIRSTOBS=` (SAS),
`rowrange()` (Stata), "Remove Top Rows" (Power Query), `startRow` (openxlsx),
`tail -n +5`, `skip_rows` (Arrow CSV).

**Messy → clean:** four lines of "Statistik der Kantone / Stand: 31.12.2025 /
Quelle: BFS" above the actual header.

**`spec`** — `Transform::SkipRows { head }`, inferred by the sniffer. Applied
*before* rectangularisation, which is deliberate: a title row is usually one cell
wide, so rectangularising first would make the whole table look ragged.

### C2 · Header promotion
**Also called:** `header=0` / `names=` (pandas), `col_names=` (readr),
`Table.PromoteHeaders` (Power Query), "Use First Row as Headers", `firstrow`
(Stata `import delimited`), `janitor::row_to_names` (R), `header=true` (DuckDB).

**`spec`** — `Transform::PromoteHeader { rows: 1 }`. The header is a transform
rather than a flag because it has to be orderable against `skip_rows` and
`drop_rows_matching`.

### C3 · Multi-row and hierarchical headers
**Also called:** `header=[0,1]` → MultiIndex (pandas), `behead()` (unpivotr — the
purest implementation of this operator anywhere), tidyxl cell-level reads,
"banner headers", "stub-and-banner" (survey crosstabs), merged title cells,
`Table.CombineColumns` after transpose (Power Query), two-row headers in
Eurostat/OECD/BFS exports.

**Messy → clean:**
```
        │  2024        │  2025        │      ← merged title cells, blanks to the right
 Kanton │ Q1  │ Q2     │ Q1  │ Q2     │
```
becomes columns `kanton`, `2024 Q1`, `2024 Q2`, `2025 Q1`, `2025 Q2`.

*Measured in the wild:* of 3,712 real-world CSVs, 2,751 have one header line,
470 have none, and **476 (12.8%) have multiple header lines**, of which 94 are
multi-row *table* headers of exactly this kind. A reader that handles only
`header=0` mis-parses one file in eight.

**`spec`** — `PromoteHeader { rows: n, join }`. The rule that makes it correct is
worth restating because it is easy to get backwards: **upper rows fill rightward,
the last row does not.** A blank in an upper row is the right-hand side of a
horizontal merge; a blank in the last row is a nameless column, and inheriting
the left neighbour's name there attaches one column's label to another column's
data.

The closest thing to a reference implementation elsewhere is R's `unpivotr`,
whose `behead()` strips one level of headers at a time, working inward from the
edge of the table, and whose `spatter()` then re-spreads the result *while
preserving mixed data types* — because unpivotr deliberately delays type coercion
until after the table is tidy. tdy arrives at the same discipline from the other
direction: its pipeline is extract-as-strings → transforms → typed cast **last**,
so the header surgery in `promote_header` never has to reason about types
either.

### C4 · Footer and trailer removal
**Also called:** `skipfooter=` (pandas), `n_max=` (readr), `OBS=` (SAS),
"Remove Bottom Rows" (Power Query), `head -n -3`, "Total row", source notes,
copyright lines, footnote legends.

**Messy → clean:** a `Total 57'340.00` row that becomes a phantom observation and
doubles every sum; three lines of "(a) provisional; (b) revised" prose beneath.

**`spec`** — `SkipRows { tail }`, plus real detection machinery in the sniffer:
`footer_rows`, `trailing_prose_block`, `note_trailing_blocks`. Two hard-won
constraints live here. A tail skip is **not** applied to a truncated table,
because a capped read has not seen the end of the file and cannot reason about
it. And the trailing-block detector has density gates (added 2026-09-05 after it
claimed all 475 rows of a 475-row table) — a block with nothing above it is not
trailing anything.

### C5 · Repeated in-body header rows
**Also called:** "page break headers", print-range repeats, `grep -v`, OpenRefine
facet + remove matching rows, `WHERE col <> 'col'`.

**Messy → clean:** a report paginated every 50 rows re-emits its header line.

**`spec`** — `DropRowsMatching`, plus an automatic rule with a sharp edge: a
**byte-identical** repeated header is dropped automatically (it is provably not
data), while a merely *similar* one is reported and kept. Dropping rows that
merely fail to parse is the silent data loss the design refuses.

### C6 · Interior aggregate rows
**Also called:** subtotal rows, Excel `Subtotal` outline levels, "group total",
`GROUPING()` artefacts round-tripped through a spreadsheet, sectional summaries,
and — in the structure-detection literature — **`derived` lines and cells**, one
of the six element classes Strudel annotates (Jiang, Vitagliano & Naumann, EDBT
2021: a cell "that aggregates the values of some other numeric cells in the same
table").

**Messy → clean:** a `Zwischentotal` row sitting *between* data rows — worse than
a footer because tail-trimming cannot reach it and it sums the rows above it.

**`partial`** — `DropRowsMatching` removes them once you know they are there, and
`note_interior_summary` (added 2026-09-05) *detects* them and lowers confidence,
measured at 11 genuine instances across 320 corpus files with 0 false positives.
There is no automatic removal, correctly: a row labelled `Total` might be a
legitimate observation, and this is exactly the class of judgement the review
gate exists for.

### C7 · Blank spacer rows and columns
**Also called:** `remove_empty()` (janitor), `dropna(how='all')` (pandas),
`Table.SelectRows(each not List.IsEmpty)` (Power Query), "Remove Blank Rows",
`awk 'NF'`, `mlr remove-empty-columns` (Miller — the one tool surveyed with a
verb dedicated to exactly this).

**`partial`** — leading/trailing blanks are absorbed by framing; interior blank
*rows* survive as all-empty rows, and an all-blank *column* becomes a real column
of nulls (usually named `col_7`). A `remove_empty` operator would be a small,
uncontroversial addition; today the cure is projection (omit the column) and a
`WHERE` clause.

### C8 · Transposition
**Also called:** `t()` (R), `.T` / `transpose()` (pandas), `Table.Transpose`
(Power Query), `PROC TRANSPOSE` (SAS, when used to flip orientation rather than
reshape), `xpose` (Stata), `qsv transpose`, `mlr --opprint transpose`,
"Transpose cells in rows into columns" (OpenRefine), `datamash transpose`,
Alteryx Transpose tool.

**Messy → clean:** the file has variables down the left and observations across
the top —
```
 Kennzahl  │ 2021 │ 2022 │ 2023
 Umsatz    │  100 │  120 │  140
 Kosten    │   80 │   85 │   95
```
— a layout produced by every "print-friendly" export and every hand-built
management sheet.

**`gap`, and the largest one in this report.** There is no transpose transform.
The workaround is not partial, it is absent: `unpivot` turns wide into long but
cannot make the *first column* into the header. A file in this shape can only be
read as `kennzahl`, `col_2`, `col_3`, `col_4` with three text columns, which
neither conforms to a sensible target nor types correctly. Note the shape is
mechanically detectable — the first column holds the names, the header row holds
values that all parse as one type — so a sniffer note is cheap even if the
transform is not.

### C9 · Sheet or file as an implicit dimension
**Also called:** "one sheet per year", partition columns, Hive partitioning
(`/year=2024/`), `input_file_name()` (Spark), `filename=true` (DuckDB),
`read_csv(..., include_file_paths=)` (polars), `id=` argument to
`purrr::map_dfr` / `vroom(id=)`, `bind_rows(.id=)` (dplyr), `PROC APPEND` +
`INDSNAME`.

**Messy → clean:** twelve sheets named `2014`…`2025`, each an identical table;
the year exists only as the sheet's name and must become a column. Or: forty
CSVs whose canton is only in the filename.

**`gap`** — and a structural one, in two halves.
1. A sidecar is per *file*, so one file contributes exactly one sheet to a
   dataset. A twelve-sheet workbook cannot be twelve members. (`tdy draft`'s
   corpus sweep already meets this: 16 of 31 real multi-sheet workbooks were
   refused as ambiguous frames, and they were mostly one-sheet-per-year books.)
2. There is no operator producing a column from the source's *name*.
   `Transform::Constant` can hand-write it per sidecar, at the cost of the review
   gate firing on every member — which is the right gate for an arbitrary
   constant, and the wrong one for a fact mechanically derived from the path.

This is the single most valuable candidate in the catalogue, because the pile of
per-period exports is tdy's core use case and the period is frequently only in
the filename.

### C10 · Indentation as an implicit hierarchy column
**Also called:** "outline levels", `Table.Group` after level extraction,
`tidyr::fill` after level detection, indent-based stubs in statistical yearbooks,
Excel grouping/outline, `unpivotr` behead with an "indent" direction, and — in
the structure-detection literature — the **`group`** (or "group header") element
class: "the label of such a group" of rows, a first-class category in Strudel's
six-class taxonomy alongside metadata, header, data, derived and notes.

**Messy → clean:**
```
 Total                 1000
   Kanton Zürich        400
     Stadt Zürich       250
```
becomes `level_1, level_2, level_3, value` — or better, `region, parent, value`.

**`gap`** — needs leading-whitespace measurement, which nothing in the pipeline
retains (`trim` runs before every cast). Common in official statistics, awkward
everywhere, no cheap fix. Noted rather than recommended.

### C11 · Merged-cell recovery
**Also called:** `merged_cells` (openpyxl), `fill_merged_cells` (readxl doesn't;
tidyxl+unpivotr does), "unmerge and fill" (Power Query Fill Down after unmerge),
`ffill` on a category column.

**`spec`** — deliberately *not* an extraction feature. calamine surfaces a merged
range as value-in-top-left plus blanks, and the cure is `fill_down` (vertical) or
header fill-right (horizontal). The doc comment on `Extraction::Excel` says so
explicitly. This is the right factoring: it means the same operator repairs a
merged cell and a "category written once" layout, which are the same thing.

### C12 · Formula versus cached value
**Also called:** `data_only=True` (openpyxl), "Values only" paste, `formulas=TRUE`
(readxl), `#REF!`/`#DIV/0!` error cells, volatile functions.

**`out`, with a caveat** — calamine yields cached values, which is right for
ingestion. The caveat is that a workbook saved by a non-Excel writer may have no
cached values at all, and then a numeric column reads as empty. Worth a sniffer
note some day; not a transform.

### C13 · Formatting as data
**Also called:** tidyxl `xlsx_cells()` (formats as a first-class table), "the
red cells mean provisional", strikethrough as deletion, number formats,
conditional formatting, cell colour indices.

**`out` by design**, with **one deliberate exception**: `src/xlmoney.rs` reads
`xl/styles.xml` to find currency number formats, because a column of currency-
formatted numbers is money and should be `decimal`, not `float64` — the format is
evidence about the *type*, which is a fact about the column, not a fact
smuggled in from a human's colour convention. That line is worth keeping sharp:
number format → type is in scope; colour → category is not.

### C14 · Hidden rows, columns, sheets and filters
**Also called:** `sheet_state='hidden'`, autofilter state, "very hidden" sheets,
`skiphidden`.

**`out`** — but note the asymmetry: a hidden row is still read, so a file whose
author hid the subtotals still gets them. Detectable, not currently detected.

### C15 · Cell comments, notes and footnote markers
**Also called:** cell comments (xlsx), `*` / `¹` / `(a)` markers appended to
values, "see note 3", superscript footnotes.

**Split verdict.** Comments as *objects*: **`out`**. Footnote markers *inside
values*: **`spec`**, via `strip` — which is one of the two reasons `strip` exists
(the other is currency symbols).

### C16 · Multi-row records

**Also called:** *records mode* versus *rows mode* (OpenRefine's central and
much-misunderstood distinction), "one logical record spanning several lines",
key-value blocks, `mlr nest --implode`, repeating groups, COBOL `OCCURS`,
line-per-attribute exports.

**Messy → clean:**
```
 Order 4711 │ Widget  │ 2
            │ Gadget  │ 1
            │ Gizmo   │ 5
 Order 4712 │ Widget  │ 9
```
Three lines are one order; the blanks are continuation, not missing data.

**`partial`** — `fill_down` reconstructs the key, which is the whole cure when
the layout is "written once at the top." What tdy has no notion of is a *record*
as distinct from a row, so an operation that should apply per order (deduplicate,
count) applies per line. OpenRefine is the only tool in this survey that makes
the distinction first-class. For tdy this is arguably correct as-is: after
`fill_down` the table is relational and `GROUP BY` is the record notion.

---

# Part D · Shape
*The tidy-data operators. Wickham's five messy-data problems live here.*

### D1 · Unpivot (wide → long)
**Also called:** `melt` (pandas, reshape2, data.table, polars), `gather` →
`pivot_longer` (tidyr), `stack` (pandas), `fold` (Potter's Wheel, Wrangler),
`UNPIVOT` (T-SQL, Oracle, DuckDB, Snowflake), `stack()` (Spark SQL),
`VARSTOCASES` (SPSS), `reshape long` (Stata), `PROC TRANSPOSE` (SAS),
Row Normaliser (Pentaho), Unpivot transformation (SSIS), Normalizer (Informatica),
`Table.UnpivotOtherColumns` (Power Query), "Transpose cells across columns into
rows" (OpenRefine), `mlr reshape --long-to-wide`'s inverse, `unpivot` (Ibis),
Alteryx Transpose tool, `melt` (VisiData).

**Messy → clean:** Wickham's problem 1, *column headers are values*. `Jan Feb Mar`
as three columns becomes `month`, `value`.

**`spec`** — `Transform::Unpivot { id_columns, value_columns, variable_name,
value_name }`. `validate` refuses overlap between id and value columns, an empty
value list, a name collision with an id column, and `variable_name ==
value_name`. In the streaming executor it must come last (it rewrites the row
shape), and `tests/streaming.rs` pins that unpivot making output rows outnumber
input rows crosses batch boundaries correctly.

**Not covered**, relative to the richest implementation in the survey
(`tidyr::pivot_longer`): selecting value columns by pattern (`cols =
starts_with("q")`, or `Table.UnpivotOtherColumns`'s "everything except" — tdy's
list is literal, which for heterogeneous piles means the sidecar must enumerate,
fine for a planner and tedious by hand); `names_prefix` to strip a common prefix
off the new key values; `names_sep` / `names_pattern` to split the old column
name into several key columns as it is unpivoted (see **D7**); `names_transform`
to type the key column; and `values_drop_na`, which turns explicit missings into
implicit ones — the inverse of **G7**, and a data-losing option tdy would
presumably refuse rather than offer.

### D2 · Pivot (long → wide)
**Also called:** `pivot_table` / `pivot` / `unstack` (pandas), `spread` →
`pivot_wider` (tidyr), `dcast`/`acast` (reshape2, data.table), `unfold` (Potter's
Wheel, Wrangler), `PIVOT` (T-SQL, Oracle, DuckDB, Snowflake, Spark
`groupBy().pivot()`), `crosstab()` (PostgreSQL tablefunc), `CASESTOVARS` (SPSS),
`reshape wide` (Stata), `PROC TRANSPOSE`, Row Denormaliser (Pentaho), Pivot
transformation (SSIS), `Table.Pivot` (Power Query), "Columnize by key/value"
(OpenRefine), `mlr reshape --long-to-wide`, `pivot` (VisiData), Alteryx Cross Tab.

**Messy → clean:** Wickham's problem 2 in its narrow form, *multiple variables
stored in one column* — a `variable`/`value` pair that should be several typed
columns.

**`sql`, awkwardly** — and this is the one place where the DataFusion floor bites.
There is no `PIVOT` syntax (§0.4), so it is
`MAX(CASE WHEN k='x' THEN v END) AS x, …` per output column, written by hand.
More importantly there is **no spec-layer pivot**, which matters more than it
looks: a long-format file *cannot conform to a wide target*. If the declared
dataset has columns `q1, q2, q3, q4` and a member arrives in `quarter/value`
form, `fit` has no operator that could bridge it and must refuse. Whether that
should change is a design question — `unpivot`'s presence and `pivot`'s absence
is currently asymmetric, and the asymmetry is defensible only if long-format
members are rare in practice.

### D3 · Split a column into columns
**Also called:** `separate()` → `separate_wider_delim`/`_position`/`_regex`
(tidyr), `str.split(expand=True)` / `str.extract` (pandas), `split` (Stata),
`SUBSTR`/`SCAN` (SAS), `Text to Columns` (Excel), `Table.SplitColumn` /
"Split Column by Delimiter / by Number of Characters / by Positions / by
Lowercase-to-Uppercase transition" (Power Query), `Split` and `Divide` (Potter's
Wheel), `split`/`extract`/`cut` (Wrangler), Cell Splitter (KNIME),
`split_part`/`regexp_extract` (SQL), `mlr split-join`, `qsv apply operations`,
OpenRefine "Split into several columns".

**Messy → clean:** `name` → `first`, `last`; `"Zürich, ZH"` → `city`, `canton`;
`"2024-Q1"` → `year`, `quarter`; an ISO timestamp packed with a status code.

**`sql`** (`split_part`, `regexp_match`, `substr` — all present and probed) —
**but `gap` in the spec layer**, and this is the second-largest finding here.
Splitting a column is, with unpivot and fill-down, one of the three most common
munging operations in every catalogue surveyed, and it is the only one of the
three with no declarative form in tdy. Consequences:

- A file whose `betrag` column is `"CHF 1'234.50 (gebucht)"` can be stripped down
  to a number, but a file whose *two* target columns are packed into one source
  column cannot be fitted at all — `fit` looks for a source column per target
  column, and there is none.
- The escape hatch exists but is upstream: `Extraction::Lines` with named capture
  groups splits a whole *line*, which covers logs and nothing else.

A `Transform::SplitColumn { source, into, by }` — delimiter, character position,
or regex with named groups — would be the highest-yield addition to the spec
language, and it composes with `fit` the way `matches` does.

### D4 · Combine columns into one
**Also called:** `unite()` (tidyr), `+` / `str.cat` / `agg(' '.join)` (pandas),
`paste0`/`sprintf` (R), `CONCAT`/`||` (SQL), `Merge Columns` /
`Table.CombineColumns` (Power Query), `Merge` (Potter's Wheel, Wrangler),
`egen concat` (Stata), `CATX` (SAS), "Join columns" (OpenRefine).

**Messy → clean:** `year`+`month`+`day` → one date; `street`+`nr` → one address
key.

**`sql`** — `||` and `concat()`. Absent from the spec layer, but far less
pressing than D3: the composite value is usually what you *don't* want, and a
target that declares a single `date` column while the file has three parts is a
real case (**E12** below) that only D3's inverse would solve. Worth pairing with
D3 if either is built.

### D5 · Split a value into rows
**Also called:** `separate_rows()` (tidyr), `explode()` (pandas, polars, Spark),
`UNNEST` (SQL — probed working with `string_to_array`), `CROSS APPLY STRING_SPLIT`
(T-SQL), "Split multi-valued cells" (OpenRefine), `mvexpand` (Splunk),
`mlr nest --explode` (and `--implode` for the inverse), `qsv explode` (and
`qsv implode`), `stack`/`flatten`, `expand` (Stata, for duplication).

**Messy → clean:** a `tags` column holding `"a;b;c"` where each tag should be its
own row; a `,`-joined list of authors.

**`sql`** — `SELECT unnest(string_to_array(tags, ';'))` works today. Not in the
spec layer, and unlike D3 that is probably right: it changes the row count, which
makes it a modelling decision rather than a parsing one.

### D6 · Flatten nested structures
**Also called:** `json_normalize` (pandas), `unnest_wider` / `unnest_longer` /
`hoist` (tidyr), `flatten` (polars, `flatdict`), `OPENJSON … WITH` (T-SQL),
`UNNEST(… , recursive := true)` (DuckDB), `from_json` + `select col.*` (Spark),
`jq` paths, XPath/XSLT for XML, `xml2::as_list` (R), `mlr flatten` / `unflatten`
plus `json-parse` / `json-stringify` (Miller), `qsv flatten`.

**Messy → clean:** `{"addr": {"city": "Bern", "zip": "3000"}}` → `addr_city`,
`addr_zip`; an array-valued field → columns or rows.

**`partial`** — `Extraction::Json` produces the union of record keys as columns,
and a nested value is **serialized back to a JSON string**. That is an honest
fallback (nothing is lost, nothing is invented) but it means one-level flattening
is the boundary: a nested address is one text column containing JSON, and
DataFusion has no JSON functions to open it downstream. So a nested field is
effectively unreachable in tdy today unless the model or a human declares a
different pointer.

Given that JSON documents with several record arrays were "the corpus's biggest
gap" that slice 5 addressed, nested *fields* within those records are the
obvious next question. A `pointer` per column — RFC 6901 into each record — would
be the minimal form, and it fits `matches`-style declaration naturally.

### D7 · Multi-stub reshape
**Also called:** `pivot_longer(names_to = c(".value", "year"))` (tidyr — the
`.value` sentinel means "this part of the old column name *is* the new column's
name", and it overrides `values_to`), `wide_to_long(stubnames=[…])` (pandas),
`reshape long x y, i() j()` (Stata), `PROC TRANSPOSE` with `BY` + multiple `VAR`,
"melt with multiple value columns".

**Messy → clean:**
```
 id │ sales_2023 │ cost_2023 │ sales_2024 │ cost_2024
```
becomes `id, year, sales, cost` — two value columns, one new key column, from one
operation.

**`gap`** — `Unpivot` produces exactly one variable column and one value column.
The composition that would express this (unpivot everything → split the variable
name → pivot back) needs **D3** and **D2**, neither of which exists in the spec
layer. This is the sharpest illustration that D2 and D3 are not independent
wishes: together they close a family.

### D8 · Column-name normalisation
**Also called:** `clean_names()` (janitor), `make.names`/`make.unique` (base R),
`.name_repair = "universal"` (tibble), `str.lower().str.replace()` idioms,
`qsv safenames` (which calls the result "database-ready"), `mlr label` /
`rename`, "Dynamic Rename" (Alteryx), `Table.TransformColumnNames` (Power Query),
snake_case conventions, `VALIDVARNAME=` (SAS).

**Messy → clean:** `"Betrag (CHF)"` → `betrag_chf`; `"2024 Q1"` → `2024_q1`; two
columns both named `Betrag`.

**`spec`** — `sniff::sanitize` plus `dedupe_names`, and one rule that is
load-bearing for `fit`: matching is done against `RawTable::header_origin`, the
file's own spelling, **not** the deduped name. Renaming the second `Betrag` to
`Betrag_2` and then matching on that would bind one of two same-named columns
silently; keeping the origin makes the collision visible and refusable.

### D9 · Projection, ordering and dropping
**Also called:** `select()`/`relocate()` (dplyr), `usecols=` (pandas), `KEEP`/`DROP`
(SAS), `keep`/`drop`/`order` (Stata), `Drop`/`Select` (Potter's Wheel),
`csvcut` (csvkit), `qsv select`, `Table.SelectColumns`.

**`spec`** — the `columns` list *is* the projection and the output order. tdy has
no separate drop or rename transform, deliberately: one list is the whole story,
which is why the sniffer can guarantee that every `source` resolves.

---

# Part E · Values: parsing and typing
*The layer where a wrong answer is quietest, and where most of tdy's rules live.*

### E1 · Type inference and assignment
**Also called:** `col_types=`/`guess_max` (readr), `dtype=`/`converters=`/
`convert_dtypes()` (pandas), `GUESSINGROWS=` (SAS `PROC IMPORT`), `destring`/
`tostring` (Stata), `ALTER TYPE` (SPSS), "Detect Data Type" / `Table.TransformColumnTypes`
(Power Query), `sample_size=` and `auto_type_candidates` (DuckDB), `schema_overrides`
(polars), `inferSchema` (Spark), "to number"/"to date" (OpenRefine), `qsv schema`.

**`spec`** — `DType` (`utf8`, `bool`, `int64`, `float64`, `decimal`, `date`,
`timestamp`), a deliberately short list so a small grammar-constrained model picks
reliably.

**What distinguishes tdy here, and belongs in any comparison:** almost every tool
above infers from a *sample* — readr's 1,000 rows, SAS's `GUESSINGROWS`,
DuckDB's `sample_size`, Spark's one pass over a fraction. tdy samples to *guess*
and then **verifies against the whole file** (`sniff::verify_types` →
`stream::verify`), widening any column whose guess does not hold and naming the
offending values and how many of how many. That is why four real corpus files
that used to die mid-query no longer do. It costs about 0.3 s on top of the parse
on a 141 MB file and is affordable only because extraction streams; `--quick`
opts out and records that it did.

There is a benchmark for this exact task — Shah et al.'s *Benchmarking Feature
Type Inference for AutoML Platforms* (SIGMOD 2021) — and a whole adjacent
research tier tdy deliberately does not attempt, **semantic** type detection
(**F10**): deciding that a column is not merely `utf8` but *a country*.

### E2 · Decimal separator
**Also called:** `decimal=','` (pandas), `locale(decimal_mark=)` (readr),
`dec=` (fread), "using locale" (Power Query), `DECIMALSEPARATOR=` (SAS
`NUMX` informat), `decimal_separator` (DuckDB `read_csv`), `SET DECIMAL` (SPSS).

**`spec`** — `ValueParsing::decimal_separator`, and at the dataset level
`WITH (decimal_separator = …)` on a target.

### E3 · Thousands / group separator
**Also called:** `thousands='.'` (pandas), `locale(grouping_mark=)` (readr),
`FORMAT` pictures (SAS), `NUMBERFORMAT` (Excel), `1'234.50` (Switzerland),
`1 234,50` (France, with NBSP), `1.234,50` (Germany), `12,34,567` (Indian
lakh/crore grouping).

**`spec`** — `thousands_separator`, with the rule that separates tdy from the
tools that turned `1,5` into `15`: **grouping must be in threes**, checked by
`numfmt::check_grouping`, and a value that violates it is an error rather than a
silently rewritten number. `numfmt::infer` accepts a convention only when every
value in the column is consistent with it and reports `ambiguous` otherwise.

**One documented consequence:** Indian-style `12,34,567` is refused, not parsed.
That is the right refusal under the rule (the alternative is guessing), but it is
a real limitation for South Asian data and worth stating rather than discovering.

### E4 · Currency symbol and unit removal
**Also called:** `str.replace('[$,]','')` idioms, `CURRENCY` informats (SAS),
`Text.Remove` (Power Query), OpenRefine `value.replace()`, `qsv apply`,
"Data Cleansing" tool (Alteryx).

**`spec`** — `strip`, a regex removed before parsing. Covers `CHF `, `€`, a
trailing `%`, a footnote `*`.

### E5 · Signed-number conventions
**Also called:** accounting negatives `(1,234)`, trailing minus `1234-`
(mainframe/SAP/COBOL overpunch), `CR`/`DR` suffixes, leading `+`, unicode minus
`−` (U+2212), `NUMX`/`COMMAX` informats (SAS), `Format cells → Accounting`
(Excel), `Number.FromText` with a custom culture, `str.replace(r'\((.*)\)', r'-\1')`.

**Messy → clean:** `(1,234.50)` means **−1234.50**; `1234.50-` means −1234.50;
`1,234.50 CR` means −1234.50 in most ledgers and +1234.50 in some.

**`gap` — and the finding that touches the one rule.** Measured on this machine:

```
$ tdy sniff paren.csv --no-llm      # values (1,234.50) / 2,000.00 / (300.00)
… type = "utf8", confidence 0.95
```

The sniffer is **correct**: it declines to type the column, so nothing wrong is
produced automatically. But the only repair the spec language offers is `strip`,
and:

```
$ # sidecar hand-edited: strip = "[()]", thousands_separator = ",", decimal(12,2)
$ tdy validate paren.csv --stamp        →  ok
$ tdy query "SELECT * FROM messy('paren.csv')"
| A | 1234.50 |     ← was (1,234.50), i.e. −1234.50
| C |  300.00 |     ← was (300.00),   i.e.  −300.00
```

A validated, fingerprinted, dry-run spec that produces a number wrong in its
**sign**, on money, silently. It is user-authored, so it is not a defect in
inference — but the spec language currently offers no way to say the true thing,
which means the only available fix is the wrong one. Two candidate remedies, both
small:

- add `ValueParsing::negative_parens: bool` (and possibly `trailing_minus`), so
  the intent is declarable; and/or
- have `validate()` refuse a `strip` regex that can remove a parenthesis or a
  sign character from a numeric column — the pattern of "anything the executor
  would otherwise discover by panicking belongs in `validate` as a message,"
  extended to "anything the executor would otherwise discover by being wrong."

Trailing minus and CR/DR have the same shape and the same non-answer today.

### E6 · Percent handling
**Also called:** `str.rstrip('%').astype(float)/100`, `Percentage` type (Power
Query, which *does* divide), `parse_number` (readr, which does not),
`PERCENT` informat (SAS, which does divide), Excel's cell format (where 45% is
stored as 0.45).

**Messy → clean:** `45%` → `45` or `0.45`, and the two are equally defensible.

**`partial`** — `strip = "%"` gives 45; `decimal_shift = -2` gives 0.45. Both are
declarable, neither is inferred, and the ambiguity is genuinely in the data. The
current behaviour (leave it as text, confidence unchanged, let a human declare)
is consistent with the rule. Worth a sniffer *note* — "column looks like
percentages; strip to keep 45, or shift to get 0.45" — since the fix is a
one-line edit once you know which you want.

### E7 · Scale factors declared out-of-band
**Also called:** "in thousands" / "in Mio. CHF" in a title row, `SCALE=` in
statistical exports, unit rows beneath the header, magnitude suffixes (`1.2k`,
`3M`).

**Messy → clean:** a column of `1 234` that means 1,234,000 because the title
said "in Tausend".

**`spec`, manual and gated** — `decimal_shift`, which is exactly this operator.
Its design notes are the clearest statement of tdy's philosophy anywhere in the
codebase: it moves the decimal point on the *digit string* (never `* 0.01`,
because the only reason it exists is money), it is **never inferred**, and a spec
carrying it needs human acceptance before joining a dataset, because a column of
integer minor units "parses perfectly and type-checks perfectly and is wrong by a
factor of a hundred, and the error is invisible in any single row."

`k`/`M` *suffixes* inside values are a different, uncovered case (they need
per-value multipliers, not a column-wide shift) — **`gap`**, minor.

### E8 · Minor-unit integers
**Also called:** cents, Rappen, pence, "amount_in_cents" (Stripe and every
payments API), `MONEY`/`DECIMAL(19,4)` scaling, `int` cents columns.

**`spec`** — `decimal_shift = -2`, the canonical example above, with
`testdata/drifting_exports/`'s Rappen file as the fixture that must be refused by
the planner and accepted only by a human.

### E9 · Exact decimals versus floats
**Also called:** `DECIMAL(p,s)` vs `DOUBLE`, `numeric` (PostgreSQL), `Decimal128`
(Arrow), `decimal.Decimal` (Python), `bignum`, "never use float for money".

**`spec`** — `DType::Decimal { precision, scale }`, chosen automatically for
money (including via `xlmoney`'s currency-format detection), with values carrying
more fractional digits than `scale` rounded half away from zero. This is a
deliberate差 from every dynamic-language default, all of which give you a float.

### E10 · Identifier-like numbers stay text
**Also called:** the leading-zero problem, `dtype=str` for ZIP codes, `TEXT`
import for account numbers, Excel's silent `007` → `7`, `PLZ`/`SSN`/`ISBN`
mangling, `+41…` phone numbers becoming floats.

**`spec`, as an explicit rule** — leading-zero and oversized integers stay text,
listed in CLAUDE.md's statement of the one rule. Most tools get this wrong by
default; it is worth naming as covered rather than assuming.

### E11 · Date parsing with an explicit format
**Also called:** `col_date(format=)` (readr), `to_datetime(format=)` (pandas),
`dmy`/`mdy`/`ymd` (lubridate), `strptime`, `INPUT(x, DDMMYY10.)` (SAS),
`date(x,"DMY")` (Stata), `Date.FromText` with culture (Power Query),
`try_strptime` (polars), `dateformat` (DuckDB), `TO_DATE` (SQL).

**`spec`** — `DType::Date { format }` and `Timestamp { format, timezone }`,
chrono strftime. Month-year values (`%b %Y`) are accepted and pinned to day 1.
`%Y` demands four digits — another instance of the rule.

### E12 · Ambiguous date order
**Also called:** `dayfirst=` (pandas), `locale(date_format=)` (readr), the
03/04/2024 problem, `SET DATEFORMAT` (T-SQL), Excel's locale-dependent import,
"US vs European dates".

**Messy → clean:** `03/04/2024` is 3 April or 4 March, and no amount of looking
at that one value settles it.

**`spec`, with the best rule in the codebase** — ambiguity is detected *exactly*:
two candidate formats conflict only if they **disagree on a value actually
present in this file**. Only then does the target's `date_order = 'dmy'|'mdy'|'ymd'`
choose. The earlier design pruned candidates by declared order and thereby made
an ordinary ISO export unfittable, which is recorded as a mistake in
`src/fit.rs` — worth citing when comparing against pandas' `dayfirst`, which
silently applies a preference whether or not there is a conflict.

### E13 · Spreadsheet serial dates
**Also called:** the 1900 vs 1904 date systems, the deliberate 1900 leap-year
bug, `excel_numeric_to_date()` (janitor), `convert_to_date` (openxlsx),
`origin="1899-12-30"` (R), 45000 as a date, `DATEVALUE`.

**`partial`** — calamine yields typed datetimes for date-*formatted* cells, so
the ordinary case works. The uncovered case is a serial number that arrives as a
plain number, most often after a CSV export from a spreadsheet: `45000` in a CSV
is `int64`, and nothing suggests it might be 2023-03-15. Detectable heuristically
(a tight cluster of integers in the 25,000–50,000 band, in a column named
`datum`/`date`) but only as a *note*, never as a silent conversion.

### E14 · Two-digit years and century windowing
**Also called:** `YEARCUTOFF=` (SAS), pivot year, `%y` semantics, Y2K windowing,
`dmy` with 2-digit input (lubridate).

**`partial`** — `%y` parses; the window is chrono's (69/70 split) and is not
declarable. Rarely decisive, occasionally catastrophic. Worth a note in the
`Date` doc comment if nothing else.

### E15 · Partial and non-Gregorian period values
**Also called:** `yearmonth`/`yearquarter` (tsibble), `Period`/`PeriodIndex`
(pandas), `%Y-Q%q`, ISO week dates (`2024-W03`), fiscal years (`FY24/25`),
academic years (`2024/25`), Japanese eras, Hijri dates.

**`partial`** — month-year is supported and pinned to day 1, which is the
common case in official statistics. Quarters (`Q1 2024`), ISO weeks and
slash-form academic years are **`gap`**: chrono cannot parse them from a format
string, so they stay text. The pragmatic answer today is `replace` pairs plus a
month-year format, which works for quarters (`Q1` → `Jan`) if you are willing to
say so in the sidecar — worth documenting as an idiom.

### E16 · Timezones and DST
**Also called:** `tz=` / `tz_localize` / `tz_convert` (pandas), `with_tz`/`force_tz`
(lubridate), `AT TIME ZONE` (SQL), IANA/Olson names, `TIMESTAMPTZ`,
`SET TIME ZONE`, "naive vs aware datetimes".

**`spec`, with a deliberate refusal** — fixed offsets only (`UTC`, `Z`, `+02:00`,
`-0500`), declarable per column or via the target's `WITH (timezone = …)`. Named
zones like `Europe/Zurich` are **refused with a message that says why**: resolving
DST needs a rule database, and guessing is how timestamps end up an hour wrong for
half the year. This is `out` in the "deliberately excluded, reason recorded" sense
rather than a gap. It does mean a genuinely zoned local-time export cannot be
represented exactly — the honest workaround is to store what was written and
convert downstream.

### E17 · Durations and intervals
**Also called:** `Timedelta` / `to_timedelta` (pandas), `difftime`/`hms` (R),
`INTERVAL '1 day'` (SQL), ISO 8601 durations (`PT1H20M`), `1h20m`, HH:MM:SS as an
elapsed time rather than a clock time.

**`gap`** — no duration `DType`; `01:20:00` meaning "eighty minutes" becomes text
or, worse, a time-of-day. Arrow has `Duration`; adding it is mechanical. Rare in
the corpus, so low priority, but it is a *typing* gap rather than a parsing one
and therefore belongs in `spec` if it is ever wanted.

### E18 · Booleans from local vocabularies
**Also called:** `true_values=`/`false_values=` (pandas), `col_logical` (readr),
`Y/N`, `ja/nein`, `oui/non`, `1/0`, `X` for true, `TRUE`/`WAHR` (localised Excel),
`--boolean` (qsv), OpenRefine "to boolean".

**`spec`** — `true_values` / `false_values`, matched case-insensitively.

### E19 · Missing-value sentinels
**Also called:** `na_values=`/`keep_default_na` (pandas), `na=` (readr),
`NULLSTR`/`nullstr` (DuckDB), `.` (Stata and SAS's numeric missing), `-999` /
`9999` / `-1` conventions, `#N/A`/`#NULL!`/`#DIV/0!` (Excel), `N/A`, `n.v.`,
`k.A.`, `–` (en dash), `..` and `:` (Eurostat's "not available" and "not
applicable"), `missingValues` in a Frictionless Table Schema (a declared list,
the closest analogue to tdy's `na_values`), `mlr fill-empty`, and `qsv denull`,
whose one job is detecting "null sentinels masquerading as missing values".

**`spec`** — `na_values`, matched **case-insensitively** (with a recorded bug
history: `sniff::is_na` and the parser once disagreed on case, and a column typed
from a sample containing `NA` failed on a later `NULL`). Checked *before*
`true_values`/`false_values`, and `validate` refuses a token appearing in both
rather than resolving it silently.

### E20 · Distinguishing kinds of missingness
**Also called:** extended missing values `.a`–`.z` (Stata), `MISSING VALUES` /
user-missing (SPSS), `mvdecode`/`mvencode` (Stata), special missing `.A`–`.Z`
(SAS), "refused / don't know / not applicable / not asked" in survey data,
`NA` vs `NaN` vs `NULL` vs `""`, `tagged_na` (haven).

**Messy → clean:** `-8 = refused`, `-9 = don't know`, `-1 = not applicable` must
all become missing *and* their reason must survive.

**`partial`** — all three can be listed in `na_values` and become null; the
*reason* is lost. Preserving it needs a second column (`x_missing_reason`), which
no ingestion tool this catalogue surveyed does automatically and which haven
solves with tagged NAs that only R understands. Listed for completeness; the
honest answer for tdy is "declare a second column via **D3**-style extraction,"
which is another argument for D3.

### E21 · Epoch and offset-encoded timestamps

**Also called:** Unix time / POSIX time, `from_unixtime` (SQL, Hive),
`to_timestamp_seconds` / `_millis` / `_micros` (DataFusion, probed present),
`as.POSIXct(x, origin="1970-01-01")` (R), `pd.to_datetime(x, unit='s')`,
`mlr sec2gmt` / `sec2gmtdate` (a verb dedicated to it), `clock` / `.NET ticks`,
Windows FILETIME, Julian day numbers, and the spreadsheet serials of **E13**.

**Messy → clean:** `1748736000` is 2025-06-01, and `1748736000000` is the same
instant in milliseconds — the two differ by a factor of a thousand and both are
plausible integers.

**`gap`, of the same shape as E13** — `DType` has no epoch variant, so the column
types as `int64` and stays a number. Downstream `to_timestamp_seconds()` fixes it
in the query, which means the conversion is not recorded in the sidecar and not
provable against a target that declares `TIMESTAMP`. Inferring it would be
against the rule (a column of ten-digit integers is not obviously time), but
*declaring* it is exactly the kind of thing `ValueParsing` exists for, and it
would compose with a `date_order`-style dataset option for the seconds/millis
choice. Grouped with **E13** and **E17** as the "time that does not look like
time" family.

---

# Part F · Values: standardisation and recoding

### F1 · Literal value replacement
**Also called:** `replace()` / `map()` (pandas), `recode`/`case_when`/`if_else`
(dplyr), `RECODE` (SPSS), `recode` (Stata), `PROC FORMAT` (SAS), GREL
`value.replace()` (OpenRefine), `Table.ReplaceValue` (Power Query),
`sed s///`, `translate()` (SQL), `CASE WHEN`.

**`spec` for pre-parse fixes, `sql` afterwards** — `ValueParsing::replace` is a
list of literal `{from, to}` substring pairs applied before typing. Its stated
purpose is locale repair (`Mär` → `Mar`, `Dez` → `Dec`) and the choice is
deliberate: **no locale tables ship in the binary**, so every locale fix is
visible in the sidecar and auditable. Semantic recoding (`ZH` → `Zürich`) is a
join or a `CASE`, downstream.

### F2 · Lookup / crosswalk mapping
**Also called:** `merge`/`left_join` against a dimension table, `VLOOKUP`/`XLOOKUP`
(Excel), Lookup transformation (SSIS), `Table.NestedJoin` (Power Query),
`PROC FORMAT` + `PUT()` (SAS), value labels (Stata `label define`, SPSS
`VALUE LABELS`), reconciliation against Wikidata (OpenRefine), code lists
(SDMX, ISO 3166, NUTS, NOGA).

**`sql`** — an ordinary join, and correctly so.

### F3 · Categorical / factor representation
**Also called:** `factor()`/`forcats` (R), `Categorical`/`pd.factorize` (pandas),
`astype('category')`, Arrow `DictionaryArray`, `ENUM` (DuckDB, MySQL),
`encode`/`decode` (Stata), levels and their ordering.

**`out`, arguably `gap`** — `DType` has no dictionary/categorical variant, so a
low-cardinality text column is stored as full strings. This is a *performance and
semantics* omission rather than a cleaning one; DataFusion will dictionary-encode
internally where it helps. Ordered factors (small/medium/large) have no
representation at all, which matters for `ORDER BY` and is currently solved by a
`CASE`.

### F4 · Dummy / indicator encoding and its inverse
**Also called:** `get_dummies`/`from_dummies` (pandas), `model.matrix` (R),
one-hot encoding, `tabulate, gen()` (Stata), `CASE WHEN … THEN 1 ELSE 0`,
"Pivot to indicator columns".

**`sql`** — and out of scope for ingestion: this is modelling, not cleaning.

### F5 · Binning and discretisation
**Also called:** `cut`/`qcut` (pandas), `cut`/`findInterval` (R), `egen cut`
(Stata), `PROC RANK GROUPS=` (SAS), `WIDTH_BUCKET` (SQL), age bands, income
brackets.

**`sql`**.

### F6 · Fuzzy value canonicalisation (clustering)
**Also called:** "Cluster and edit" with key-collision methods (fingerprint,
n-gram fingerprint, metaphone3, cologne-phonetic) and nearest-neighbour methods
(levenshtein, PPM) — OpenRefine's flagship feature; `stringdist` (R),
`fuzzywuzzy`/`rapidfuzz` (Python), soundex/metaphone (SQL), `agrep`,
Fuzzy Grouping (SSIS), "Find and Replace with similarity" (Alteryx),
`clean_strings` idioms.

**Messy → clean:** `Zürich`, `Zurich`, `ZÜRICH`, `Zuerich`, `Zürich ` collapse to
one label.

**`gap`, probably `out`** — DataFusion has `levenshtein()` (probed, works), so the
*detection* half is reachable in SQL; the interactive cluster-review workflow is
not, and should not be — it is inherently a human loop over *values*, whereas
tdy's human loop is over *structure*. The one place it might belong is `tdy-tui`'s
evidence screen, as a "this column has 4 spellings of 3 values" note. Worth
considering; not a spec-language question.

### F7 · Entity resolution and record linkage
**Also called:** merge–purge, deduplication (in the *fuzzy* sense), Fellegi–Sunter
probabilistic linkage, blocking/indexing, Splink, dedupe.io, `RecordLinkage` (R and
Python), `reclink`/`matchit` (Stata), Master Data Management, "golden record",
survivorship rules, Fuzzy Lookup (SSIS), Informatica IDQ.

**`out`** — a discipline of its own, requiring training data and thresholds, and
directly at odds with the rule: probabilistic matching *is* a plausible wrong
answer machine. Correct to exclude, worth naming so the exclusion is visible.

### F8 · Domain-specific value parsing
**Also called:** address parsing (libpostal, `probablepeople`, Address Doctor),
name parsing, phone normalisation to E.164 (libphonenumber), postcode validation,
country/currency code mapping (ISO 3166/4217), geocoding, unit conversion
(udunits, pint, `units` in R), currency conversion.

**`out`** — every one of these needs a reference dataset with an update cadence,
which is a different kind of product. `replace` handles the small hand-written
cases.

### F9 · Check-digit and format validation
**Also called:** Luhn (credit cards), IBAN mod-97, EAN/UPC, ISBN, AHV/AVS number
checks, VAT-ID formats, regex format masks, `validate` (R), `pandera` checks.

**`out`** — validation of *content*, not of *shape*; belongs to Great
Expectations-style tooling downstream. See **L3**.

### F10 · Semantic type annotation

**Also called:** column type annotation (CTA), semantic type detection, Sherlock
(column-wise deep model), SATO (adds topic modelling and a CRF over the table's
other columns), Doduo (feeds the whole table to BERT), RECA, Pythagoras (numeric
columns in data lakes); commercially, Trifacta/Alteryx "data types" like
*Zipcode*, *State*, *Email*; OpenRefine's reconciliation against Wikidata;
Talend's semantic discovery; Great Expectations' `expect_column_values_to_match_regex`
used as a poor man's version.

**Messy → clean:** a column typed `utf8` holding `8001`, `3000`, `1211` is not a
number and not free text — it is *a Swiss postal code*, and knowing that enables
validation, joins to a canton table, and a refusal to sum it.

**`out`, and worth stating as a boundary rather than a gap.** tdy infers
*syntactic* types (what parses) and verifies them against the whole file; it does
not infer *semantic* types (what the values mean), and the research systems that
do are ML models with accuracy in the 80–90% range on ~78 type vocabularies.
A model that is right nine times in ten is a machine for producing plausible
wrong answers, which is the one thing the project rules out. The reachable
version, if this were ever wanted, is the mirror of tdy's existing stance:
a *target* declares the semantic type (`plz TEXT`, with a `matches` clause and
one day perhaps a `pattern`), and tdy checks rather than guesses. See **K3** —
this is the same argument about constraints, arriving from the type direction.

---

# Part G · Missing data

### G1 · Fill down (last observation carried forward)
**Also called:** `fill(.direction="down")` (tidyr), `ffill`/`pad` (pandas),
`na.locf` (zoo), `fill_null(strategy="forward")` (polars), `Table.FillDown`
(Power Query), "Fill down" (OpenRefine), `RETAIN` + conditional assignment (SAS),
`by … : replace x = x[_n-1] if mi(x)` / `carryforward` (Stata), Multi-Row Formula
(Alteryx), `mlr fill-down`, `LAST_VALUE(… IGNORE NULLS)` (SQL).

**Messy → clean:** the two canonical spreadsheet layouts — a vertically merged
category cell, and "the category is written once at the top of its group."

**`spec`** — `Transform::FillDown { columns }`. Note the ordering rule that
`tests/streaming.rs` pins: row-local operations run in **spec order**, because
fill-then-drop propagates a subtotal label into the rows beneath it and
drop-then-fill does not. Both are legitimate; the spec says which.

### G2 · Fill up (next observation carried backward)
**Also called:** `fill(.direction="up")` (tidyr), `bfill`/`backfill` (pandas),
`na.locf(fromLast=TRUE)` (zoo), `Table.FillUp` (Power Query),
`fill_null(strategy="backward")` (polars).

**`gap`** — `FillDown` has no `direction` and there is no `FillUp`. The layout it
cures (a label written at the *bottom* of its group, common in French-language
and some accounting exports) is rarer but real. A `direction` field on the
existing transform is a two-line change; it is listed here mostly because its
absence is invisible until you meet the file.

### G3 · Fill right / fill left
**Also called:** header fill for merged title cells, `Table.FillDown` after a
transpose, `t(apply(t(x), 1, na.locf))` idioms.

**`partial`** — implemented *inside* `promote_header` for upper header rows (see
**C3**), which is the case that matters. Not available for body rows. Body-row
fill-right is a genuinely rare need; noted for symmetry.

### G4 · Constant fill
**Also called:** `fillna(value)` / `replace_na` (pandas, tidyr), `COALESCE` (SQL),
`Table.ReplaceValue(null, …)`, `NVL`/`IFNULL`, `mvencode` (Stata),
`fill-empty` (Miller).

**`spec` + `sql`** — `Transform::Constant { name, value }` creates a whole column
with a fixed value, and `""` is specified as the null fill (it reads as missing in
every type). Per-value null filling is `COALESCE` downstream. Note the deliberate
asymmetry recorded in the source: a target column declared `if_missing = 'null'`
lets the planner null-fill **with no review**, because the declaration in the
reviewed `.tdy.sql` *is* the authorisation, while a hand-written constant *value*
("November is all Ticino") is data the file never contained and gates behind
`--accept`.

### G5 · Interpolation
**Also called:** `interpolate()` (pandas, with linear/time/spline/pchip),
`na.approx`/`na.spline` (zoo), `ipolate` (Stata), `Table.Fill` variants,
linear/forward/spline gap filling in time series.

**`out`** — inventing values between observations, which the rule forbids at
ingestion. Legitimate downstream analysis; DataFusion has no built-in for it, so
in practice this means Python/R after the fact.

### G6 · Statistical imputation
**Also called:** mean/median/mode imputation, hot-deck, `mice` (R),
`IterativeImputer`/`KNNImputer` (scikit-learn), `PROC MI` (SAS),
`mi impute` (Stata), regression imputation, EM.

**`rule`** — not merely out of scope: producing a plausible value where the file
has none is precisely what tdy exists not to do. Correctly excluded; the right
place for it is an analysis step that *records* it as an assumption.

### G7 · Explicit missing rows (completion)
**Also called:** `complete()`/`expand()`/`full_seq` (tidyr), `reindex`/`asfreq`
(pandas), `tsfill`/`fillin` (Stata), `PROC EXPAND` (SAS), cross join against a
calendar dimension, `generate_series` + `LEFT JOIN` (SQL — probed, works),
`unsparsify` (Miller), `upsample` (polars).

**Messy → clean:** a month with no sales has no row, so the time series has a
hole that every window function reads as adjacency.

**`sql`** — `generate_series` is present and this is the idiomatic answer.

### G8 · Dropping incomplete rows
**Also called:** `drop_na`/`dropna` (tidyr, pandas), listwise deletion,
`if !mi(x)` (Stata), `WHERE x IS NOT NULL`, `na.omit` (R).

**`sql`**.

### G9 · Declared-absent columns across heterogeneous members
**Also called:** `unionByName(allowMissingColumns=True)` (Spark),
`union_by_name` (DuckDB), `mergeSchema` (Delta/parquet), `bind_rows` filling with
NA (dplyr), `concat` with `sort=False` (pandas), schema evolution.

**Messy → clean:** eleven monthly exports have a `region` column and the twelfth
does not, and the union must not silently drop it or refuse the month.

**`spec`, and distinctive** — `if_missing = 'null'` on a nullable target column,
which the planner may then null-fill *with a note and no review*, because the
declaration is the authorisation. It is part of `target_hash`, so declaring or
retracting a fill voids existing proofs. Most tools in the alias list do this
silently by default; tdy makes it a declaration, which is the whole difference.

---

# Part H · Rows

### H1 · Structural row filtering
**Also called:** `filter`/`subset`/`WHERE`, `DELETE` (Wrangler), `drop if`
(Stata), `IF … THEN DELETE` (SAS), `Table.SelectRows`, `grep -v`,
facet-and-remove (OpenRefine).

**`spec` for junk, `sql` for analysis** — `DropRowsMatching { pattern, column }`
exists to remove *structural* debris (repeated headers, page breaks, subtotal
lines), with `column = None` testing the whole row joined by tabs. Analytical
filtering belongs in the query, and keeping that line clear is what stops the
sidecar becoming a second query language.

### H2 · Exact deduplication
**Also called:** `distinct()`/`drop_duplicates()`, `SELECT DISTINCT`,
`PROC SORT NODUPKEY` (SAS), `duplicates drop` (Stata), `unique()` (R),
"Blank down" + facet (OpenRefine), `qsv dedup`, `Table.Distinct`, `mlr uniq`.

**`sql`** — `DISTINCT` and (probed) `DISTINCT ON`.

### H3 · Keyed deduplication with survivorship
**Also called:** `distinct(.keep_all=TRUE)` (dplyr), `drop_duplicates(subset=,
keep='last')` (pandas), `ROW_NUMBER() OVER (PARTITION BY … ORDER BY …) = 1`,
`duplicates tag` (Stata), golden record / survivorship rules, `MERGE`/upsert.

**`sql`** — window functions and `DISTINCT ON` both work; `QUALIFY` does not
(§0.4), so the row-number form needs a subquery.

### H4 · Row numbering and source provenance
**Also called:** `row_number()`, `_N` (Stata), `_n_` (SAS), `rownames`,
`filename` / `file_row_number` (DuckDB), `input_file_name()` (Spark),
`monotonically_increasing_id`, `id=` in `vroom`/`map_dfr`, `mlr --nr-progress`.

**Messy → clean:** an error message says "row 4,812 has a bad date" and you need
to find that line in the original file; or a union of forty members needs to know
which member each row came from.

**`gap`, and it argues against the project's own strengths.** tdy proves which
files a dataset contains (the lockfile), what shape they land on (conformance),
and what a human accepted (the review gate) — but a row in the result carries no
trace of *which member and which line* produced it. `stream::analyse` already
locates offending rows precisely when a parse fails, so the information exists at
error time; it is just never a column. Two natural forms: a virtual `_file` /
`_row` available to a query, or opt-in target columns. Pairs closely with **C9**:
"which file did this come from" and "the year is in the filename" are the same
missing capability seen from two sides.

### H5 · Sorting
**Also called:** `arrange`/`ORDER BY`/`PROC SORT`/`sort`, natural sort,
locale-aware collation, `mlr sort -f`.

**`sql`** — with one deliberate note: `dataset()` reads members in **lock order,
as a single partition**, so row order across a union is deterministic. That is a
`--frozen` guarantee, not an accident.

---

# Part I · Aggregation and windows
*Almost entirely downstream, and correctly so. Listed for completeness of the
catalogue, since these are what "data munging" means to half the field.*

### I1 · Group-by aggregation
**Also called:** `summarise`/`group_by` (dplyr), `groupby().agg()` (pandas),
`PROC MEANS`/`PROC SUMMARY` (SAS), `collapse` (Stata), `AGGREGATE` (SPSS),
`Table.Group` (Power Query), pivot tables, `datamash groupby`, `mlr stats1`.
**`sql`.**

### I2 · Rollup, cube, grouping sets
**Also called:** `GROUP BY ROLLUP/CUBE/GROUPING SETS` (SQL — probed, works),
`margins=True` (pandas `pivot_table`), `addmargins` (R), subtotal rows,
`PROC SUMMARY` with `TYPES=`/`WAYS=`.
**`sql`** — and worth noting that this operator is what *creates* the interior
subtotal rows of **C6** when a report is exported. The catalogue closes a loop
here.

### I3 · Window functions
**Also called:** `OVER (PARTITION BY … ORDER BY …)`, `lag`/`lead`/`cumsum`/
`rank`/`dense_rank`/`row_number`, `rollapply` (zoo), `rolling()` (pandas),
`frollmean` (data.table), `RETAIN` (SAS), Multi-Row Formula (Alteryx).
**`sql`** — probed working; `QUALIFY` is absent (§0.4).

### I4 · Temporal binning and resampling
**Also called:** `resample()`/`asfreq` (pandas), `floor_date` (lubridate),
`date_trunc` (SQL — probed), `PROC TIMESERIES` (SAS), `tsset`+`collapse` (Stata),
`group_by_dynamic` (polars), `Table.Group` on a derived period.
**`sql`.**

### I5 · As-of and range joins
**Also called:** `merge_asof` (pandas), rolling join (`data.table` `roll=`),
`ASOF JOIN` (DuckDB, ClickHouse, kdb+), `LATERAL` + `ORDER BY … LIMIT 1`,
temporal joins, `merge … , nearest` idioms.
**`gap` in the downstream tier** — DataFusion 46 has no `ASOF JOIN`, so the
correlated-subquery form is the only route. Not tdy's problem to solve, but worth
recording because "join a price to the nearest earlier timestamp" is common
enough that someone will hit it.

---

# Part J · Combining datasets

### J1 · Vertical union with schema reconciliation
**Also called:** `bind_rows` (dplyr), `concat` (pandas), `UNION ALL` (SQL),
`append` (Stata), `PROC APPEND`/`SET` (SAS), `unionByName` (Spark),
`union_by_name` (DuckDB), "Append Queries" (Power Query), `ADD FILES` (SPSS),
`csvstack` (csvkit), `mlr cat`.

**Messy → clean:** twelve monthly exports, each with slightly different column
names, orders, types and encodings, becoming one table.

**`spec`, and this is tdy's thesis.** `dataset('t.tdy.sql')` unions the lockfile's
members in lock order as a **single partition**, and it can do so as a bare
concatenation with nothing to coerce *because conformance already proved every
member produces an identical schema*. The contrast with the alias list is the
point: an ordinary `UNION ALL` lets the engine widen `Int64` + `Utf8` to `Utf8`
in silence, and `union_by_name`/`unionByName` reconcile by name at runtime with
whatever coercion the engine prefers. tdy refuses to run rather than coerce.

Three supporting mechanisms have no clean analogue elsewhere and belong in any
comparison: membership comes from a **lock**, never from expanding a glob at query
time (so the answer cannot change because a file landed overnight); `target_hash`
fingerprints *meaning* rather than bytes, so a comment edit does not void twelve
proofs; and a partial lock is refused outright, because "a dataset silently
missing a month" must not be the default outcome of a bad afternoon.

### J2 · Horizontal joins
**Also called:** `left_join`/`inner_join`/`anti_join` (dplyr), `merge` (pandas, R),
`JOIN` (SQL), `merge 1:1`/`m:1` (Stata), `MERGE … BY` (SAS), `MATCH FILES` (SPSS),
`tMap` (Talend), Lookup (SSIS), "Merge Queries" (Power Query), `csvjoin`, `mlr join`.
**`sql`** — including semi/anti via `EXISTS`/`IN`, and cross joins.

### J3 · Fuzzy joins
**Also called:** `fuzzyjoin` (R), `recordlinkage` (Python), `merge_asof` on strings,
Fuzzy Lookup (SSIS), `reclink` (Stata).
**`out`** — see **F7**.

### J4 · Multi-file discovery and partition columns
**Also called:** globbing (`read_csv("data/*.csv")` in DuckDB/polars/Spark),
Hive partitioning (`/year=2024/month=03/`), `hive_partitioning=true` (DuckDB),
`basePath` (Spark), `map_dfr(list.files(), read_csv, .id=)` (R),
`Folder.Files` (Power Query).

**`partial`** — a target's `files` globs and `exclude` list resolve to members
recorded in the lock (strong), but nothing derives a *column* from the path
(**H4**, **C9**). For a `data/2024/03/umsatz.csv` layout, the year and month are
visible in the manifest and invisible in the data.

### J5 · Upsert, SCD and incremental merge
**Also called:** `MERGE` (SQL), Type-1/Type-2 slowly changing dimensions,
SCD transformation (SSIS), snapshots (dbt), `update`/`replace` (Stata),
change data capture.
**`out`** — tdy reads files; it does not maintain a warehouse.

---

# Part K · Validation and quality

### K1 · Schema declaration and conformance
**Also called:** Table Schema + CSV Dialect / Data Package (Frictionless — whose
dialect descriptor is the nearest published cousin of tdy's `Extraction`, with
`delimiter`, `quoteChar`, `escapeChar`, `commentChar`, `header`, and even
`headerRows: [2, 3]` for the multi-row case of **C3**), CSVW (W3C),
JSON Schema, Avro/Protobuf schemas, `pandera` (Python), `dbt` model contracts,
`col_types` as an assertion (readr), `pointblank`/`validate` (R),
`assertr`, Delta Lake schema enforcement, `CREATE TABLE` DDL itself.

**`spec`, and the layer everything else in tdy rests on** — `src/target.rs`
parses one `CREATE TABLE` statement (via DataFusion's re-exported `sqlparser`, so
the vocabulary is SQL's and costs no dependency), and `src/conform.rs` proves a
spec lands on it by comparing `engine::schema_of(spec)` to `Target::arrow_schema()`
field by field **with no I/O**. `tdy check <TARGET> --against <FILE>` is the CI
gate.

The stance is worth quoting because it is unusual: **anything a target declares
that tdy would not enforce is refused, not widened** — `SMALLINT`, `VARCHAR(n)`,
`TIMESTAMP(3)`, table constraints, a second statement, an option set twice. A
schema language that accepts a declaration it will not check teaches people to
write declarations nobody checks.

### K2 · Whole-file type verification
**Also called:** `guess_max = Inf` (readr), `GUESSINGROWS=MAX` (SAS),
`sample_size=-1` (DuckDB), two-pass CSV readers, `problems()` (readr), strict
mode.
**`spec`** — see **E1**; this is where tdy differs from nearly every sibling.

### K3 · Value-level constraints
**Also called:** `CHECK` constraints (SQL), expectations (Great Expectations),
`Check`/`Column(checks=)` (pandera), tests (dbt), `validator` rules (Frictionless),
Deequ/Soda checks, range and domain rules, `assert_that` (assertr).

**`out` by design, deliberately and narrowly** — `Target::parse` refuses table
constraints with a message, and the reasoning recorded in `target.rs` is that a
declaration tdy will not enforce is a promise it will not keep. That is right for
constraints tdy cannot check without reading data; it is worth being explicit
that the consequence is that **tdy has no data-quality assertions at all** —
"`betrag` must be positive" has no home. The honest positioning is that
conformance answers *shape* and Great Expectations answers *content*, and tdy
should say so rather than leave people expecting the second.

### K4 · Uniqueness and referential integrity
**Also called:** `PRIMARY KEY`/`UNIQUE`/`FOREIGN KEY`, `get_dupes` (janitor),
`isid` (Stata), `assert_unique_key`, dbt's `unique`/`relationships` tests.
**`out`** — same reasoning as K3; and unlike ranges, uniqueness *is* checkable in
one pass, so this is the most defensible candidate if K3 is ever revisited.

### K5 · Profiling and discrepancy detection
**Also called:** `describe()`/`info()` (pandas), `skimr`/`summarytools` (R),
ydata-profiling, `PROC CONTENTS`/`PROC FREQ` (SAS), `codebook` (Stata),
facets and numeric facets (OpenRefine), Potter's Wheel's *structure extraction*
and discrepancy detection (the 2001 paper that named this operator),
`Table.Profile` (Power Query), `qsv stats`/`frequency`, Deequ's analyzers,
`mlr summary`.

Potter's Wheel's version is worth describing precisely, because it is the
ancestor of what tdy's evidence screen is reaching for: it infers a *structure*
(a pattern built from a library of domains) for each column, and then defines a
discrepancy as a value that does not match the inferred structure — the
detection running continuously, in the background, while the user works.

**`partial`** — `tdy sniff` reports notes, confidence, and a preview; the
workbench's evidence screen shows the raw head and a bounded sheet grid; the
corpus sweep produces a survey. What does not exist is per-column profiling:
cardinality, value-frequency, min/max, pattern distribution ("94% match
`\d{4}-\d{2}-\d{2}`, 6% match `\d{2}\.\d{2}\.\d{4}`"). That last one is Potter's
Wheel's central idea and would be a natural fit for the evidence screen, since it
is exactly the evidence a human needs to judge a `matches` clause or a
`date_order`.

### K6 · Error routing and quarantine
**Also called:** error output branch (SSIS), `badRecordsPath` /
`PERMISSIVE`+`_corrupt_record` (Spark), `ignore_errors`/`rejects_table` (DuckDB),
`problems()` (readr), reject files (Informatica), dead-letter queues.

**`out`, on principle** — tdy fails loudly and names the row rather than diverting
bad rows to a side channel. The limited exceptions are `RaggedPolicy` and
`NoMatchPolicy`, both explicit. DuckDB's `store_rejects` is the closest thing to a model
tdy might one day adopt: it writes two temporary tables, `reject_scans` (the
scanner's parameters) and `reject_errors` (one row per faulty line, with the
error), so the rejects are *queryable* rather than silently gone. If tdy ever
softens here, that is the shape to copy — and note it would compose with the
`_file`/`_row` provenance of **H4**, since a reject is useless without a
line number.

### K7 · Constraint-based automatic repair
**Also called:** HoloClean, NADEEF, conditional functional dependencies, denial
constraints, `PROC STANDARD`, "data repair" in the database-theory literature.
**`rule`** — automatic repair is a machine for producing plausible wrong values.

---

# Part L · Process and provenance

### L1 · A recorded, replayable transformation script
**Also called:** OpenRefine's operation history (extractable and applicable as
JSON), Wrangler/Trifacta's generated code, Power Query's M script, dbt models,
`.do` files (Stata), `.sas` programs, Kettle `.ktr` transformations,
"reproducible pipeline".

**`spec`** — the sidecar *is* this artifact, and it is the reason tdy exists in
the shape it does. Distinctive relative to the alias list: the script is
**per-file**, sits **beside** the file, is **fingerprinted against it**, and is
re-validated on every load because it is hand-editable and therefore untrusted
input.

### L2 · Reproducibility guarantees
**Also called:** lockfiles (`poetry.lock`, `Cargo.lock`, `renv.lock`),
frozen environments, content hashing, `--frozen`, pinned schema versions,
Delta/Iceberg snapshots, DVC.

**`spec`** — `--frozen` skips the inference pre-pass and errors on an absent or
stale sidecar; `lockfile.rs` records members and blake3 hashes and computes drift;
a new, edited or removed member and a changed declaration are all drift, and the
query fails naming the file. Comparable in spirit to a package lockfile and, as
far as this survey found, without a direct equivalent in any data-wrangling tool.

### L3 · Human-in-the-loop acceptance
**Also called:** approval gates, dbt's `--full-refresh` prompts (weakly),
manual QA steps, "review before merge", four-eyes principle. No real analogue in
a wrangling tool.

**`spec`, and unique to tdy** — `fit::review_reasons` finds steps whose acceptance
rests on a *judgement* rather than a proof; `Member { review, accepted }` records
them; `dataset()` refuses an unaccepted member. An acceptance carries over while
the bytes and the declaration are unchanged (asking every run trains people to
answer without reading) and drift expires it. The MCP server refuses acceptance
entirely unless started with `--allow-accept`, because delegating a human's
judgement to an agent must be the operator's explicit act.

### L4 · Programming by example
**Also called:** FlashFill (Gulwani 2011, and Excel's Flash Fill),
"Column From Examples" (Power Query), predictive interaction (Wrangler/Trifacta,
including Transform-By-Example), Foofah (layout transformation by example),
Wrangler's ranked suggestion menu, `datamaid` idioms, LLM-based "clean this
column" assistants.

The 2024–25 successors are LLM-based: **CleanAgent** wraps an LLM in a code
executor for standardisation tasks, and **AutoDCWorkflow** takes a table plus an
analysis purpose and emits *a sequence of OpenRefine operations* — which is
notable here because it targets a recorded, replayable operation list (**L1**)
rather than editing values directly, the same instinct that makes tdy emit a
sidecar rather than a cleaned copy.

**`gap`, filled sideways** — tdy has no by-example interface; its analogue is the
tier-2 model call, which is asked for the *frame only* and whose output is proved
downstream but gated behind review because nothing can prove the model's frame is
the only reading. The workbench's ranked remedy menu (`--propose`: which of the
file's columns can actually produce the declared type) is a close cousin of
Wrangler's ranked suggestions, arrived at independently.

### L5 · Interactive preview-driven iteration
**Also called:** OpenRefine's live preview, Wrangler's inline previews,
Power Query's preview pane, VisiData, `head`/`glimpse` loops, notebook
iteration.
**`spec`** — `tdy sniff`'s preview, `dry_run`, and the workbench's raw-head panel
("what tdy sees"), which is deliberately the *raw* bytes-as-text head plus a
bounded 20×12 sheet grid, because the file's own header spelling is what a
`matches` clause needs. The bounded read *says* it is bounded (`…` cells and a
final `…` row) — a window shown as if it were the whole sheet is how someone
writes a `matches` clause for a column they never saw.

---

# Part M · Research foundations and external validation

*Everything above was written from the code and from practice. This part checks
it against the published literature — four taxonomies that carve the same
territory differently, a benchmark tdy can be scored on, and a survey of 3,712
real files whose numbers are quoted throughout Parts A–C.*

### M1 · Rahm & Do (2000): the field's founding taxonomy

*Data Cleaning: Problems and Current Approaches*, IEEE Data Engineering Bulletin
23(4). Still the reference division of the problem, and it cuts on two axes at
once — **single-source vs multi-source**, and **schema level vs instance level**:

| | Schema level | Instance level |
|---|---|---|
| **Single source** | illegal values, violated attribute dependencies, uniqueness violations, referential integrity violations | missing values, misspellings, cryptic values/abbreviations, **embedded values**, **misfielded values**, word transpositions, duplicated records, contradicting records, wrong references |
| **Multi source** | naming conflicts (homonyms/synonyms), structural conflicts (attribute vs table, different component structure, different types, different constraints) | different value representations, different interpretations (Dollar vs Euro), different aggregation levels, inconsistent timing, the object-identity / merge-purge problem |

Placing tdy on that grid is the clearest statement of its scope this report can
make:

- **Single-source instance level** is tdy's home. "Embedded values"
  (`name="J. Smith 12.02.70 New York"` — several values in one field) is
  Rahm & Do's name for what **D3** says tdy cannot declaratively split, and
  "misfielded values" (`city="Germany"`) is what **K3**'s absent domain checks
  would catch. Both of this report's top gaps have names in a paper from 2000.
- **Multi-source schema level** is what the target layer does, and does
  unusually well: naming conflicts are `matches`, structural conflicts are
  refused rather than reconciled, and the reconciliation is *proved* rather than
  attempted (**J1**, **K1**).
- **Multi-source instance level** is deliberately out (**F7**, **J3**) — the
  object-identity problem is the one Rahm & Do spend the most space on and the
  one tdy will not touch, for the reason the rule gives.

Their five-phase process — *data analysis → definition of the transformation
workflow → **verification** → transformation → backflow of cleaned data* — is
also worth reading against tdy's pipeline. tdy implements the first four almost
literally (`sniff` → sidecar → `validate` + dry run → execute), and deliberately
omits the fifth: cleaned data never replaces the source, because the source file
and its fingerprint are the evidence.

### M2 · Strudel (EDBT 2021): the six element classes of a verbose file

*Structure Detection in Verbose CSV Files* (Jiang, Vitagliano & Naumann) attacks
exactly tdy's Part C, and does it by classifying every line and every cell into
one of six classes, over an annotated corpus of **226 files and 97,000+ lines**.
The classes, and what in tdy corresponds to each:

| Strudel class | What it is | tdy |
|---|---|---|
| **metadata** | preamble, title, provenance lines | **C1** `skip_rows.head`, sniffed |
| **header** | the column names, possibly several lines | **C2**/**C3** `promote_header` |
| **group** | a label for the rows beneath it | **C10** — *no detector, no operator* |
| **data** | the actual records | the table |
| **derived** | "aggregates the values of some other numeric cells in the same table" | **C6** `note_interior_summary`, detection only |
| **notes** | footnotes, legends, explanations of marks | **C4** `trailing_prose_block` |

Five of the six have a tdy detector; the sixth, **group**, does not — and its
absence is the C10/C5 hierarchy gap seen from the annotator's side. The corpus is
public, which makes this the second concrete external test available to tdy
(after Pollock): the line-classification task maps directly onto "did the sniffer
frame this file correctly."

### M3 · Pollock (VLDB 2023): a benchmark tdy could actually be scored on

*Pollock: A Data Loading Benchmark* (Vitagliano, Hameed, Jiang, Reisener, Wu &
Naumann, PVLDB 16). The design is unusually well suited to this project:

- It defines a **pollution**: a systematic, grammar-level transformation of a
  file into a content-equivalent file written in a *dialect* of the standard
  grammar. Each pollution is isolated, so a failure names one cause.
- The pollutions are drawn from a survey of **3,712 real-world CSV files** from
  government portals — the source of every "measured in the wild" figure in
  Parts A–C above — and the released benchmark is **2,290 polluted files**.
- Scoring is not binary. Alongside a binary **success** (did it load at all),
  it computes **precision, recall and F1 at three levels — header, record and
  cell** — by exporting the system's loaded content back to RFC 4180 and
  comparing against the known polluted content. Cell-level scores catch values
  lost or invented regardless of where they ended up.

**This is the right instrument for tdy, with one caveat that is itself
interesting.** tdy's whole stance is *refuse rather than be wrong*, so on a
pollution it cannot read confidently it scores a **0 for success** while a
guessing parser scores 1 and then loses cells. A tool designed as tdy is should
therefore show a distinctive profile: lower success, and precision/recall at or
near 1 wherever success is 1. Nobody has published that profile for a
refuse-first loader, and producing it would be a genuine result rather than a
box-ticking exercise — as well as the cheapest external audit of Parts A–C
available, since the benchmark is open source
(`github.com/HPI-Information-Systems/Pollock`) and the pollutions are exactly the
dialect axes tdy claims to sniff.

Note also what Pollock found about everyone else: **only 2 of 16 systems** loaded
a file with a non-standard escape character correctly, the rest dropping the rest
of the cell or the whole row silently. That is the failure mode this project
exists to refuse, measured in a controlled setting.

### M4 · The structure-detection family

Four more systems bear directly on Parts B and C, and together they show that
every framing problem tdy solves heuristically has a research literature:

- **CleverCSV** (van den Burg, Nazábal & Sutton, DMKD 2019) — dialect detection
  by scoring row and type *patterns* rather than by counting delimiters; the
  published baseline for **B1**, and the direct ancestor of tdy's "accept a
  convention only if every value is consistent with it."
- **hypoparsr** (Döhmen et al.) — enumerates parsing *hypotheses* (dialect ×
  header × types), ranks them by a quality score, and returns the best. tdy's
  frame-elimination in **B8**/**B9** is the same idea with a different stopping
  rule: rather than ranking, it requires that exactly one candidate survive, and
  reports an ambiguity when several do.
- **Mondrian** (Vitagliano, Jiang & Naumann, PVLDB 2022) — renders a
  spreadsheet's cells as coloured pixels, segments the image into regions, and
  fingerprints each region, in order to find *layout templates shared across a
  pile of files*. That is **B10**, and it is also strikingly close to what
  `tdy draft` does when it clusters a directory by column-name overlap — Mondrian
  clusters by layout where draft clusters by vocabulary.
- **SURAGH** (Hameed, Vitagliano, Jiang & Naumann, EDBT 2022) — identifies
  ill-formed *records* by syntactic pattern matching, i.e. finds the rows that do
  not look like their neighbours. The natural companion to **B6** and to
  `stream::analyse`, which already locates offending rows by halving.

### M5 · Data smells: the vocabulary for "legal but suspicious"

Foidl, Felderer & Ramler (2022) coined **data smells** for values that are
*valid* yet suspect, deliberately by analogy with code smells, and split them
into **believability smells** (outliers, suspect duplicates, implausible values),
**understandability smells** — subdivided into **encoding smells** (ambiguous or
inconsistent representation) and **syntactic smells** (irregular formats,
ambiguous names) — and **consistency smells**. Recupito et al. later extended the
catalogue to ~50 smells; Qin, Li & Merlo (2024) reorganised the whole space into
*illegal* (integrity issues) versus *legal* (data smells) and cross-cut it with
a scope notation — SCSR, SCMR, MCSR, MCMR (single/multiple column × single/
multiple row) — that is a genuinely useful way to say how much context a check
needs.

This is the literature's name for what tdy's **confidence score and notes** are:
not errors, but reasons to look. Mapping the two vocabularies is a cheap way to
audit the sniffer's note list — most of tdy's notes are syntactic and encoding
smells, and it emits nothing at all in the believability category, which is
consistent with **G6**/**K3** and worth saying out loud rather than leaving as an
accident. The SCSR/SCMR/MCSR/MCMR axis also explains *why* tdy's checks stop
where they do: everything it verifies is single-column (SCSR or SCMR — one column
over all its rows), and every check it refuses (**K3**, **K4**, **F7**) is
multi-column or multi-table.

### M6 · The operator algebras this catalogue descends from

Two systems defined the vocabulary everything since has borrowed. Scoring tdy
against Potter's Wheel's ten transforms is the most compact coverage statement in
this report:

| Potter's Wheel (2001) | Meaning | tdy |
|---|---|---|
| **Format** | apply a function to a column | `partial` — `replace`/`strip`/parse, no general function |
| **Add** | add a column | `spec` — `Constant` (**G4**) |
| **Drop** | remove a column | `spec` — omit from `columns` (**D9**) |
| **Copy** | duplicate a column | `sql` |
| **Merge** | join delimited columns | `sql` (**D4**) |
| **Split** | split a column by regex or position | **`gap`** (**D3**) |
| **Divide** | split a column in two by a predicate | `sql` (`CASE`) |
| **Fold** | wide → long | `spec` — `unpivot` (**D1**) |
| **Unfold** | long → wide | `sql` only (**D2**) |
| **Select** | keep rows matching a condition | `spec` for structural junk, `sql` otherwise (**H1**) |

Nine of the ten have a home; **Split is the one operator with no expression
anywhere in tdy's spec language**, which is the same conclusion Part D reached
from the modern tools' side. Wrangler (Kandel, Paepcke, Hellerstein & Heer, CHI
2011) adds `extract`, `cut`, `fill`, `lag`, `lookup` and `join` to that base, and
contributes the idea this project's workbench independently rebuilt: **ranked
suggestions** — the system proposes candidate transforms and the human picks,
rather than the system deciding. tdy's `--propose` remedy menu is that idea,
narrowed to "which of this file's columns can actually produce the declared
type."

### M7 · Where the automation frontier now is

Worth knowing, if only to place tdy's tier 2 on the map. The by-example line runs
FlashFill (Gulwani, POPL 2011) → Wrangler's predictive interaction → Trifacta's
Transform-By-Example → Foofah (layout transformation by example). The current
line is LLM-based: **CleanAgent** (an LLM plus a code executor for standardisation),
**AutoDCWorkflow** (table + analysis purpose → *a sequence of OpenRefine
operations*, benchmarked over 142 purposes on 96 tables), and a 2025 line of work
on LLM agents cleaning tabular ML datasets.

Two observations for tdy. First, AutoDCWorkflow's choice to emit **an operation
list rather than a cleaned table** is the same instinct as tdy's sidecar: the
artifact is the recipe, and it is reviewable. Second, none of these systems has
tdy's gate — the model's output is evaluated by benchmark accuracy, not blocked
until a human accepts it. tdy asks the model for the *frame only*, proves
everything downstream, and still refuses to let a dataset use it unaccepted
(**L3**). In a field measuring "how often is the LLM right," a design whose
answer is "it does not matter, because nothing it says is trusted without proof
or a human" is an unusual and defensible position — and one worth writing up
rather than leaving implicit in the code.

---

# Part N · Coverage summary

### N1 · By part

| Part | `spec` | `sql` | `partial` | `gap` | `out`/`rule` | entries |
|---|---|---|---|---|---|---|
| A · Physical decoding | 3 | – | 2 | 1 | 1 | 7 |
| B · Dialect & framing | 8 | – | 2 | 1 | – | 11 |
| C · Table framing | 6 | – | 3 | 3 | 3 | 16 |
| D · Shape | 3 | 4 | 1 | 1 | – | 9 |
| E · Parsing & typing | 13 | – | 5 | 3 | – | 21 |
| F · Standardisation | 1 | 3 | – | 1 | 5 | 10 |
| G · Missing data | 3 | 2 | 1 | 1 | 2 | 9 |
| H · Rows | 1 | 3 | – | 1 | – | 5 |
| I · Aggregation | – | 4 | – | 1 | – | 5 |
| J · Combining | 1 | 1 | 1 | – | 2 | 5 |
| K · Validation | 2 | – | 1 | – | 4 | 7 |
| L · Process | 4 | – | – | 1 | – | 5 |
| **Total** | **45** | **17** | **16** | **14** | **17** | **110** |

(Counted from the verdict line of each numbered entry; C15's split verdict is
counted in the entry total but in neither column.)

Read the `out`/`rule` column as a feature, not a deficit: every entry there has a
recorded reason, and four of them — G6 imputation, G5 interpolation, K7
constraint repair, F7 probabilistic linkage — exist because doing the operation
would mean producing a value the file does not contain.

### N2 · The gaps, ranked by what they cost

**Tier 1 — files tdy cannot read at all today.**

1. **C8 · Transposition.** No operator. Variables-down-the-left is a whole file
   shape, not an edge case, and it is mechanically detectable.
2. **C9 + H4 · Source identity as data.** A sheet or filename that carries the
   period cannot become a column, and a result row cannot say which member and
   line it came from. Two halves of one missing capability, and the one most
   directly in tension with tdy's provenance claims.
3. **D3 · Split a column.** The most common munging operation with no
   declarative form. Its absence blocks `fit` on any file that packs two target
   columns into one source column, and it is a prerequisite for D7.

**Tier 2 — files tdy reads but cannot fully clean.**

4. **E5 · Signed-number conventions.** The only finding here that can produce a
   silently wrong number through a validated spec. Small fix, high stakes.
5. **D6 · Nested JSON fields.** One level of flattening is the boundary, and
   DataFusion has no JSON functions to finish the job downstream.
6. **D2 · Pivot (long → wide).** Absent from the spec layer (so a long-format
   member cannot conform to a wide target) and awkward downstream because
   DataFusion lacks `PIVOT`.
7. **A6 · Compressed inputs.** `.csv.gz` and zipped monthly exports are the
   ordinary shipping format for the pile `dataset()` is designed to read.

8. **B10 · Multiple tables in one file.** *Moved up from tier 3 by the
   literature pass:* 5.1% of real-world CSVs, more in spreadsheets, and a solved
   research problem (Mondrian) rather than an open one.

**Tier 3 — small, cheap, occasionally decisive.**

9. **G2 · Fill up** — a `direction` field on an existing transform.
10. **E15 · Quarters and ISO weeks** — or at least a documented `replace` idiom.
11. **E13 + E21 · Time that does not look like time** — spreadsheet serials and
    Unix epochs both type as integers. A sniffer note; a declarable parse; never
    a silent conversion.
12. **B7 · Multi-character delimiters** — *moved up from "trivial, rare":* 2.7%
    of real CSVs use comma-plus-whitespace, the third most common dialect.
13. **E17 · Duration type**, **F3 · ordered categoricals**, **C7 · blank-column
    removal**, **A5 · Unicode normalisation** — all real, all minor.

**Worth deciding explicitly rather than leaving implicit:**

- **K3/K4 · value-level constraints.** Currently refused with a good reason
  (a declaration tdy will not enforce is a promise it will not keep), which
  leaves tdy with no data-quality assertions at all. Uniqueness is checkable in
  one pass and is the strongest candidate if that line ever moves.
- **K5 · column profiling.** Pattern-frequency profiling is Potter's Wheel's
  central idea and is exactly the evidence the workbench's review screen exists
  to present.
- **C10 / M2 · the `group` class.** Strudel's six-class taxonomy is the closest
  external audit of tdy's framing detectors, and `group` — a label row for the
  rows beneath it — is the one class tdy neither detects nor represents.
- **F10 · semantic types.** Currently out, for a rule-consistent reason. The
  reachable version is the same shape as everything else here: the *target*
  declares, tdy checks.

**And one thing to measure rather than decide:**

- **Run tdy against Pollock** (§M3). 2,290 polluted files, open source, scored on
  success plus header/record/cell precision and recall. It is the cheapest
  external audit of Parts A–C that exists, it directly tests the claim this
  project rests on, and the expected result — low success, near-perfect precision
  — has never been published for a refuse-first loader. The corpus sweep in
  `tests/corpus.rs` answers "does it crash or lie on real files"; Pollock answers
  "how does it compare, on a controlled axis, to the sixteen systems everyone
  else uses." Strudel's 226-file annotated corpus is the second such test, for
  framing specifically.

### N3 · What tdy does that the survey found nowhere else

Worth recording, because a coverage report that only lists gaps misrepresents the
tool:

- **Whole-file type verification** (E1/K2). Every sibling samples.
- **Exact ambiguity detection for dates** (E12) — two formats conflict only if
  they disagree on a value *in this file*, rather than a global `dayfirst`
  preference.
- **Grouping-in-threes enforcement** (E3) — a separator that does not group
  correctly is an error, not a rewritten number.
- **Union by proved-identical schema** (J1), with membership from a lock rather
  than a glob, and a `target_hash` that fingerprints meaning rather than bytes.
- **Declared-absent columns** (G9) as an authorisation rather than a silent
  default.
- **The review gate** (L3) — a mechanically-checked plan that still refuses to
  run until a human accepts the steps that rest on judgement.
- **Frame proof by elimination** (B8/B9) — hypoparsr ranks parsing hypotheses and
  returns the best; tdy tries the declared table against every
  candidate record array or sheet, and treats "several fit" as an error naming
  them rather than a ranking problem.

Every one of those is a consequence of the same rule, which is the strongest
evidence the rule is doing real work rather than decorating the README.

---

# Appendix · Sources

The literature pass of 2026-09-05. Everything in Part M and every "measured in
the wild" figure in Parts A–C comes from these; tool aliases were checked against
current documentation.

**Taxonomies and surveys**
- Rahm, E. & Do, H.H. (2000). *Data Cleaning: Problems and Current Approaches.*
  IEEE Data Eng. Bull. 23(4). <https://dbs.uni-leipzig.de/files/research/publications/2000-1/pdf/TBDE2000.pdf>
- Wickham, H. (2014). *Tidy Data.* J. Statistical Software 59(10). — the five
  messy-data problems referenced throughout Part D.
- Hameed, M. & Naumann, F. (2020). *Data Preparation: A Survey of Commercial
  Tools.* SIGMOD Record 49(3). <https://sigmodrecord.org/2020/11/29/data-preparation-a-survey-of-commercial-tools/>
- Qin, Q., Li, H. & Merlo, E. (2024). *Wrangling Data Issues to be Wrangled:
  Literature Review, Taxonomy, and Industry Case Study.* <https://arxiv.org/abs/2405.16033>
- Foidl, H., Felderer, M. & Ramler, R. (2022). *Data Smells: Categories, Causes
  and Consequences, and Detection of Suspicious Data in AI-based Systems.*
  <https://arxiv.org/pdf/2203.10384>

**Structure detection and parsing**
- Raman, V. & Hellerstein, J.M. (2001). *Potter's Wheel: An Interactive Framework
  for Data Cleaning and Transformation.* <https://pubs.dbs.uni-leipzig.de/dc/node/679>
- Kandel, S., Paepcke, A., Hellerstein, J. & Heer, J. (2011). *Wrangler:
  Interactive Visual Specification of Data Transformation Scripts.* CHI.
  <https://idl.uw.edu/papers/wrangler>
- van den Burg, G.J.J., Nazábal, A. & Sutton, C. (2019). *Wrangling Messy CSV
  Files by Detecting Row and Type Patterns.* DMKD 33(6). — CleverCSV.
- Jiang, L., Vitagliano, G. & Naumann, F. (2021). *Structure Detection in Verbose
  CSV Files.* EDBT. <https://edbt2021proceedings.github.io/docs/p32.pdf> ·
  code: <https://github.com/lanchiang/strudel>
- Vitagliano, G., Jiang, L. & Naumann, F. (2022). *Detecting Layout Templates in
  Complex Multiregion Files.* PVLDB 15(3) — Mondrian.
  <http://vldb.org/pvldb/vol15/p646-vitagliano.pdf>
- Vitagliano, G., Hameed, M., Jiang, L., Reisener, L., Wu, E. & Naumann, F.
  (2023). *Pollock: A Data Loading Benchmark.* PVLDB 16.
  <https://www.vldb.org/pvldb/vol16/p1870-vitagliano.pdf> ·
  code: <https://github.com/HPI-Information-Systems/Pollock>
- Hameed, M., Vitagliano, G., Jiang, L. & Naumann, F. (2022). *SURAGH: Syntactic
  Pattern Matching to Identify Ill-Formed Records.* EDBT.
- Koci, E. et al. — cell classification and layout inference in spreadsheets;
  DECO dataset. TableSense (Microsoft Research); DeExcelerator.

**Type inference**
- Hulsebos, M. et al. (2019). *Sherlock: A Deep Learning Approach to Semantic
  Data Type Detection.* KDD. · Zhang, D. et al. (2020). *SATO: Contextual
  Semantic Type Detection in Tables.* PVLDB 13. <https://www.vldb.org/pvldb/vol13/p1835-zhang.pdf>
  · Suhara, Y. et al. — Doduo.
- Shah, V. et al. (2021). *Towards Benchmarking Feature Type Inference for AutoML
  Platforms.* SIGMOD.

**By-example and LLM-based wrangling**
- Gulwani, S. (2011). *Automating String Processing in Spreadsheets Using
  Input-Output Examples.* POPL — FlashFill.
- Jin, Z. et al. — Foofah (transformation by example).
- Li, L. et al. (2024). *AutoDCWorkflow: LLM-based Data Cleaning Workflow
  Auto-Generation and Benchmark.* <https://arxiv.org/abs/2412.06724>
- Qi, D. & Wang, J. (2024). *CleanAgent: Automating Data Standardization with
  LLM-based Agents.*

**Tool documentation checked**
- Miller verbs: <https://miller.readthedocs.io/en/latest/reference-verbs/>
- qsv commands: <https://github.com/dathere/qsv>
- OpenRefine manual (transposing, cell editing, column editing):
  <https://openrefine.org/docs/manual/transposing>
- tidyr `pivot_longer`: <https://tidyr.tidyverse.org/reference/pivot_longer.html>
- unpivotr `behead()` / `spatter()`: <https://nacnudus.github.io/unpivotr/>
- DuckDB faulty-CSV handling (`ignore_errors`, `store_rejects`, `reject_scans`,
  `reject_errors`): <https://duckdb.org/docs/current/data/csv/reading_faulty_csv_files>
- Frictionless Table Dialect and Table Schema:
  <https://specs.frictionlessdata.io/csv-dialect/> ·
  <https://specs.frictionlessdata.io//table-schema/>
- Power Query M function reference:
  <https://learn.microsoft.com/en-us/powerquery-m/power-query-m-function-reference>

**Probed locally** (DataFusion 46 via the `tdy` binary, 2026-09-05): `PIVOT`
unsupported, `QUALIFY` unsupported, recursive CTEs work without the column alias
list, `unnest`/`string_to_array`/`levenshtein`/`generate_series`/`DISTINCT ON`/
`GROUPING SETS`/window functions/`date_trunc`/`arrow_cast`/`named_struct` all
present; and the E5 sign-flip demonstrated end to end.
