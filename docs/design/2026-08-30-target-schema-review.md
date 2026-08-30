<!--
The long-form output of the design review that produced
2026-08-30-target-schema.md: four independent designs, each attacked by two
adversarial reviewers (one on the never-wrong-value rule, one on architectural
fit), then synthesised. Kept because it carries detail the decision document
deliberately compresses — the full gate taxonomy, the review reasons, the drift
model, per-slice test obligations and effort estimates.

Read the decision document first. Where the two disagree, the decision document
wins: it overrules this one on the target's syntax (SQL, not TOML) and on where
per-file repair lives (the sidecar, not the target). See its section 3.
-->

# Design: `tdy fit` — target-directed planning with a proved landing

**Status:** recommended design, ready to implement.
**Supersedes:** the four candidate designs (Target files; `CREATE DATASET` DDL; Frame-and-Bind; Dataset-as-Project). This document takes Frame-and-Bind's planner as its spine, Dataset-as-Project's lock and review gate as its record, Target-files' zero-I/O re-proof as its freshness model, and `CREATE DATASET`'s contract-not-proposal argument as its policy — and resolves, one by one, every fatal flaw the eight critics raised.

New artifacts on disk: `sales.tdy-target.toml` (the contract), `sales.tdy-lock.toml` (the proof record), `exports/2025-07.csv.sales.tdy.toml` (one ordinary sidecar per file *per target*).
New SQL: `dataset('sales.tdy-target.toml')`.
New verbs: `tdy fit`, `tdy check`, `tdy explain`, `tdy target init`.

---

## 1. The vision, restated precisely

The author's words:

> "an sql-like way to describe the clean dataset we want, and the input are potentially many messy files in different formats. the tool figures out how to get to that clean data output without writing code, just by combining existing super efficient tdy rust code for data manipulation steps."

Restated as a set of obligations tdy takes on:

1. **The user declares a target**: an ordered list of output columns, each with a name, one of the seven `DType`s, and a nullability. This declaration is a checked-in text file, reviewed in a git diff, and it is the *only* statement of intent in the system. It replaces `--hint`.
2. **The user points tdy at a heterogeneous pile**: `.xlsx`, `.csv`, fixed-width dumps, NDJSON — files that disagree about column names, layout, date order, numeric convention, and magnitude.
3. **tdy plans, per file, toward the target**: it chooses an extraction, a transform list, and for each *declared* column exactly one post-transform header cell plus a parse configuration — selecting and parameterising operators that already exist in `spec.rs`. Neither the user nor the model writes cleaning code.
4. **tdy proves the landing before it runs**: for every member file, `engine::schema_of(spec)` is field-for-field identical to the target's Arrow schema. Not "the head parsed" — *this spec produces exactly these columns with exactly these types, for every row it will ever emit, on both executors*.
5. **tdy refuses rather than approximates**: a file that cannot reach the target is an error naming the file and the column. A judgement that is semantic rather than mechanical — a unit shift, an alias no one declared, a column padded with nulls — is blocked until a human accepts it, and the acceptance is recorded against the file's hash and the target's hash.
6. **The result is one relation**: `SELECT ... FROM dataset('sales.tdy-target.toml')`, reproducible row-for-row, with no `UNION ALL` and no DataFusion type coercion anywhere in the path.

The non-negotiable rule is unchanged and takes precedence over every convenience in this document:

> **tdy never silently produces a wrong value.** Ambiguity resolves to the right answer or a loud error naming the row — never a plausible wrong number.

A target *raises* the stakes on that rule rather than lowering them, because an aggregate over twelve files launders a wrong value past any row a user could point at. Section 7 makes that the deciding argument for every failure-semantics choice here.

---

## 2. Why this inverts today's architecture, and why the safety property gets stronger

### Today

```
file ──sniff──► ColumnSpecs derived from whatever header cells exist
             ──► one f32 confidence
             ──► escalate to the model below 0.80
             ──► gate: validate() + dry_run(200 head rows)
```

Two properties of that pipeline matter here. First, **the output shape is an output, not an input**: nothing anywhere says which columns should come out, so nothing can check that they did. Second, **the gate proves the wrong thing**: `check_spec` proves "this spec parses the head of this file". A spec that parses the file into entirely the wrong columns is accepted, written to disk, and queried.

### Under a target

```
target + file ──frame enumeration──► candidate (extraction, structural transforms) + post-transform header
              ──binding────────────► each DECLARED column ↦ exactly one header cell, or a Gap
              ──typing─────────────► check_type(values, declared dtype) → ValueParsing, or a Gap
              ──gates──────────────► G-V validate, G-S shape, G-M mapping, G-P parse, G-X extent
              ──cross-member───────► G-D divergence
              ──review─────────────► G-R acceptances
```

The loop inverts: `sniff::finish` iterates over header cells and emits a `ColumnSpec` for each; `fit::bind` iterates over *declared* columns and resolves each to a header cell or refuses. That single inversion buys four things the current architecture structurally cannot have:

**(a) The shape becomes mechanically provable, for free, before any I/O.** `engine::schema_of` is a pure function of `spec.columns` — verified: it calls `build_column_at(col, &[], 0)` over an empty value slice (engine.rs:913-921), touching no file. Its result is therefore the schema of *every* batch the spec will ever emit, on both executors, for all rows. Comparing it to a declared target is a total, deterministic, microsecond proof. `dry_run` has never proved anything of that class; it proves that 200 rows from a 4 MiB prefix parsed.

**(b) Ambiguity stops being a discount and becomes a refusal.** Today, two `DATE_FORMATS` parsing every probe value costs 0.25 confidence and ships anyway; `numfmt::infer` returning `ambiguous` costs another 0.25 and ships anyway. Against a declared type, "I could not tell whether `03/04/2025` is March or April" is not a discount on a self-assessment, it is a fact that the answer is unknown — and a `Date32` column with the wrong month is exactly the plausible wrong number the rule forbids. Under a target these become `Gap`s: hard, per-column, structured.

**(c) The escalation gate stops being a float.** Today a heuristic spec clearing 0.80 never sees any user intent at all, and a spec below 0.80 with `backend = none` is warned about and used anyway. Both are wrong once a hard contract exists. The new trigger is boolean: **tier 1 either produced a plan that passes every gate, or it left a specific `Gap`.** Confidence survives as reported evidence in the lock; it decides nothing in the fit path.

**(d) The model's job shrinks from author to proposer.** With the columns declared, the model is never asked to invent a column, a type, a magnitude, or a value. It is asked for a *frame* — an extraction and a structural transform list — and for alias *proposals* that a human pastes into the target. Its output type stops being `ParseSpec` (Section 5.4), which turns README.md:70's policy ("the model emits instructions, not data") from prose into a type.

### The honest limit, stated once and repeated in the CLI

Shape is proved statically over the whole file. **Values are not.** A per-row parse failure, a grouping violation, a two-digit `%Y`, a null in a NOT NULL column — all of those are proved by `build_column_at` per row, loudly, naming the row, at execution. A conformance gate that let "conforms" sound like a whole-file guarantee would be a new way to be quietly wrong. Every artifact and every CLI line in this design states which half it proved (`verified = "head" | "full"`), and the design's default is to pay for the strong half on a member's first fit (Section 6.6).

---

## 3. The target language

### 3.1 Where it lives

Three artifacts, all beside the data, all in git:

| path | who writes it | what it is |
|---|---|---|
| `exports/sales.tdy-target.toml` | a human | the contract: columns, types, aliases, per-file overrides |
| `exports/sales.tdy-lock.toml` | `tdy fit` | membership, hashes, verdicts, the mapping chosen, acceptances, per-member statistics |
| `exports/2025-09.xlsx.sales.tdy.toml` | `tdy fit` | an **ordinary sidecar** — today's format, today's `Sidecar` envelope |

The suffix `.tdy-target.toml` cannot collide with a sidecar (`<file>.tdy.toml`) or with the user config (`~/.config/tdy/config.toml`). The target is referenced in SQL and on the command line **by explicit relative path**, never by a searched name:

```sql
FROM dataset('sales.tdy-target.toml')
```

There is deliberately no name resolution, no `$TDY_TARGET_PATH`, no walk-up discovery, and no `[target]` table in the user config. "If it looks like a name, try a search path; otherwise treat it as a path" is a guess, and `messy()` already establishes that the argument is a path. Walk-up discovery also fails badly in CI (no `.git` boundary in an extracted tarball means parsing every `*.tdy.sql` up to `/`), and it makes `--frozen` depend on cwd.

Globs *inside* the target resolve relative to **the target file's directory**, so the same command works from anywhere in the repo. The lock lives beside the target and stores member paths relative to that same directory.

### 3.2 The type vocabulary is a projection of `DType`, not a new language

```
TEXT / utf8      -> DType::Utf8                     -> Utf8
bool             -> DType::Bool                     -> Boolean
int64            -> DType::Int64                    -> Int64
float64          -> DType::Float64                  -> Float64
decimal(p, s)    -> DType::Decimal{precision,scale} -> Decimal128(p, s)
date             -> DType::Date{format: per file}   -> Date32
timestamp(tz)    -> DType::Timestamp{format: per file, timezone: tz} -> Timestamp(µs, tz)
```

**strftime `format` is deliberately not declarable per column.** It is a property of a file, it differs between the twelve exports, and it does not reach the Arrow schema — so a target carrying it would be lying about what it constrains. Twelve files with twelve date formats land on one `date` column with no ceremony. This is the single biggest ergonomic win of basing conformance on the Arrow schema rather than on comparing `DType`s, and it comes from Design 2.

What replaces it, because "let the planner pick a format" is not acceptable when two formats both parse: an optional **dataset-level `date_order`** (`"dmy" | "mdy" | "ymd"`), which is *not* part of the Arrow type and constrains only the planner's candidate set, plus a per-file `format = "%d.%m.%Y"` pin in an override block. Both are human declarations in a versioned file.

`timezone` **is** declarable, because it is part of the Arrow type. A named zone (`Europe/Zurich`) is rejected by the target's `validate()` with the same message and the same reason as in a sidecar — a target is exactly where a user will try to write one, so the refusal must live in the parser.

**There is no `unit` keyword.** The critics were right: a unit label whose failure mode is silence is not a unit system, it is a comment with a keyword, and declaring `unit = "CHF"` on a column no file was ever checked against reads to a reviewer as a verified property. What tdy offers instead is (a) a per-file, human-declared `decimal_shift`, review-gated; (b) a cross-member magnitude divergence gate that may only ever *refuse*, never rescale; (c) a README omission that says plainly: tdy has no unit system and will not convert currencies.

### 3.3 The document, on the twelve-messy-exports scenario

`exports/` holds twelve monthly Swiss/German sales exports:

- `2025-01.csv` … `2025-06.csv` — `Datum;Region;Betrag`, windows-1252, `1'234.50`, `31.01.2025`
- `2025-07.csv` — same layout, but the amount column is `Betrag Rp.` and holds integer Rappen
- `2025-08.csv` — same, but with **two** columns literally named `Betrag` (net and gross)
- `2025-09.xlsx` — sheet "Umsatz": a title row, then a merged band `Umsatz 2025` over C:E, then `Datum | Region | Betrag CHF`
- `2025-10.xlsx` — an English-locale export: `Date, Region, Amount, Discount`, `1234.50`, `2025-10-31`
- `2025-11.csv` — a partial export with no region column at all
- `2025-12.csv` — normal, plus an extra `Kundennummer` the target does not declare

```toml
# exports/sales.tdy-target.toml
# The clean dataset we want. Hand-written, reviewed in git, versioned beside the data.
# Editing anything under [dataset] or any column's `match` set invalidates every
# fitted plan; editing a [files."..."] block invalidates only that member.

target_version = 1

[dataset]
name        = "sales"                       # [a-z0-9_]+; also names the sidecars
description = "Monthly CH sales exports, Abacus + the Excel workbook"
files       = ["2025-*.csv", "2025-*.xlsx"] # globs, relative to THIS file's directory
exclude     = ["*-entwurf.csv"]
match       = "normalized"                  # "exact" | "normalized" (default)
date_order  = "dmy"                         # planner-only; not part of the Arrow type
verify      = "full"                        # "full" (default on a member's first fit) | "head"
on_unfittable = "error"                     # "error" (default) | "quarantine"

[[column]]
name     = "month"
type     = "date"
nullable = false
match    = ["Datum", "Date", "Buchungsdatum", "Monat"]

[[column]]
name     = "region"
type     = "utf8"
nullable = false
match    = ["Region", "Kanton", "Gebiet", "Filiale"]

[[column]]
name      = "amount_chf"
type      = "decimal"
precision = 14
scale     = 2
nullable  = false
match     = ["Betrag", "Betrag CHF", "Umsatz Betrag CHF", "Amount", "Umsatz"]

[[column]]
name     = "discount_pct"
type     = "float64"
nullable = true
absent   = "null"        # THE escape hatch. Default is "error". Requires nullable = true.
match    = ["Rabatt", "Rabatt %", "Discount"]

[[column]]
name     = "source_file"
type     = "utf8"
nullable = false
from     = "file_stem"   # the value comes from the file's NAME, not its bytes

# ---------------------------------------------------------------------------
# Per-file overrides. Every line below is a HUMAN ASSERTION about one file,
# versioned and reviewable in a diff. tdy never writes into this file and
# never infers any of it.
# ---------------------------------------------------------------------------

[files."2025-07.csv"]
note = "Abacus exported July in Rappen; reconciled against the ledger 2026-08-14"
columns.amount_chf.source        = "Betrag Rp."
columns.amount_chf.decimal_shift = -2      # exact decimal-point move: 128450 -> 1284.50

[files."2025-08.csv"]
note = "two literal `Betrag` columns: net (col 3) and gross (col 4). We want net."
columns.amount_chf.source = "Betrag@3"     # positional disambiguation, see 5.2

[files."2025-09.xlsx"]
note = "merged year band above the real header"
sheet      = "Umsatz"
skip_rows  = { head = 1 }
promote_header = { rows = 1 }
columns.amount_chf.source = "Betrag CHF"
```

An override block may pin any of: `sheet`, `range`, `delimiter`, `encoding`, `skip_rows`, `promote_header`, `drop_rows_matching`, `fill_down`, `unpivot` (structure), and per column `source`, `format`, `decimal_shift`, `na_values`, `replace`, `strip`, `decimal_separator`, `thousands_separator`, `true_values`, `false_values` (cleaning). That is the *full* `ValueParsing` vocabulary — the escape hatch has to be able to express everything the executor can do, or the user is stuck when the planner refuses (this is the flaw that sank the DDL design, whose override grammar covered seven of fourteen knobs).

A structural pin does not bypass anything: the planner overwrites the sniffed value **and re-runs the probe**, so the header the binder matches against is still the header the executor will index.

### 3.4 The lock, after `tdy fit sales.tdy-target.toml`

```toml
# exports/sales.tdy-lock.toml — written by tdy, reviewed in git, never hand-edited.
lock_version = 1
target_hash  = "b3:9f2c4a1e…"     # contract fields + match sets + match mode + date_order
tool_version = "0.4.0"
fitted_at    = "2026-08-30T09:14:02Z"

[[member]]
path           = "2025-01.csv"
blake3         = "b3:1a4f…"
bytes          = 88213
override_hash  = "b3:0000…"       # this member's [files."..."] block (empty here)
sidecar_digest = "b3:77de…"       # canonical JSON of the sidecar's [spec] table
verdict        = "conforms"
verified       = "full"
method         = "heuristic"
header         = ["Datum", "Region", "Betrag"]
header_origin  = ["original", "original", "original"]
rows           = { extracted = 4113, header = 1, skipped = 0, dropped = 0, out = 4112 }
mapping = [
  { column = "month",        source = "Datum",  tier = "alias", how = "date %d.%m.%Y" },
  { column = "region",       source = "Region", tier = "alias", how = "utf8" },
  { column = "amount_chf",   source = "Betrag", tier = "alias", how = "decimal(14,2) thousands=' decimal=." },
  { column = "discount_pct", source = "«absent»", tier = "absent", how = "null column" },
  { column = "source_file",  source = "«file_stem»", tier = "filename", how = "constant \"2025-01\"" },
]
stats = [ { column = "amount_chf", n = 4112, median_abs = 1284.50, min = 0.05, max = 91204.00 } ]
review = { required = ["absent_by_policy:discount_pct"],
           accepted_by = "umatter", accepted_at = "2026-08-30T09:20:00Z",
           accepted_file = "b3:1a4f…", accepted_target = "b3:9f2c4a1e…",
           accepted_override = "b3:0000…" }

[[member]]
path     = "2025-07.csv"
verdict  = "conforms"
verified = "full"
method   = "reused"                # spec shape reused from 2025-01.csv; ALL gates re-run
reused_from = "2025-01.csv"
# … mapping shows amount_chf <- "Betrag Rp." with decimal_shift = -2 …
review = { required = ["unit_shift:amount_chf", "first_use_of_source:amount_chf"], … }

[[member]]
path     = "2025-11.csv"
verdict  = "unfittable"
reason   = "target column `region`: no candidate; header after the best of 21 frames is [\"Datum\", \"Betrag\", \"Bemerkung\"]"
```

Two hashes and one digest do all the freshness work:

- `target_hash` — blake3 over a canonical JSON rendering of *only* the contract fields: every column's `name`, `type`, `precision`, `scale`, `timezone`, `nullable`, `absent`, `from`, and `match` set, in order, plus `match` mode and `date_order`. **Not** `description`, **not** `note`, **not** the per-file blocks. So a comment edit invalidates nothing, and adding an alias invalidates every member (correctly — an added alias can retroactively make a previously-unique binding ambiguous).
- `override_hash` — per member, blake3 of that member's `[files."..."]` block. A per-file pin invalidates one plan, not twelve. This is what makes hand-tuning survivable.
- `sidecar_digest` — canonical JSON (sorted keys, `serde_json`, not TOML field order) of the sidecar's `[spec]` table, so a cosmetic reorder of `spec.rs`'s struct fields does not produce repo-wide drift.

The acceptance is keyed to *all three* plus the file's blake3. Change the file, the contract, or that file's override, and the human's judgement evaporates and must be re-made. That is the point: an acceptance is a statement about a specific set of bytes under a specific contract.

---

## 4. Architecture

### 4.1 New modules

```
src/target.rs      the contract: parse, validate, hash, arrow_schema, norm()
src/conform.rs     Gate S: Mismatch, conforms()
src/fit/mod.rs     FitPlan, Gap, ReviewReason, fit_file(), the gate chain
src/fit/frame.rs   frame enumeration (extraction × structural transforms)
src/fit/bind.rs    the binding ladder, injectivity, header-origin rules
src/fit/typing.rs  check_type(): the inversion of sniff::guess_type
src/fit/score.rs   FrameScore
src/lockfile.rs    Lock, MemberRecord, Verdict, Acceptance, load/save, plan_work()
src/drift.rs       Drift enum + drift(); what `tdy check` reports and CI fails on
src/dataset.rs     DatasetFunc (the UDTF), DatasetPartition, MemberSource
```

### 4.2 `src/target.rs`

```rust
pub const TARGET_FORMAT_VERSION: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Target {
    pub target_version: u32,
    pub dataset: DatasetDecl,
    #[serde(rename = "column")]  pub columns: Vec<TargetColumn>,
    #[serde(default, rename = "files")] pub overrides: BTreeMap<String, FileOverride>,
    #[serde(skip)] pub dir: PathBuf,   // the target file's directory; glob base
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DatasetDecl {
    pub name: String,                                   // [a-z0-9_]+
    #[serde(default)] pub description: Option<String>,
    pub files: Vec<String>,
    #[serde(default)] pub exclude: Vec<String>,
    #[serde(default)] pub r#match: MatchMode,           // Normalized | Exact
    #[serde(default)] pub date_order: Option<DateOrder>,
    #[serde(default)] pub verify: VerifyLevel,          // Full | Head
    #[serde(default)] pub on_unfittable: UnfittablePolicy, // Error | Quarantine
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TargetColumn {
    pub name: String,
    #[serde(flatten)] pub ty: TargetType,
    #[serde(default = "yes")] pub nullable: bool,
    #[serde(default)] pub absent: AbsentPolicy,   // Error (default) | Null
    #[serde(default)] pub from: Option<Origin>,   // FileStem | FileName | Match{re, template}
    #[serde(default)] pub r#match: Vec<String>,
}

/// Exactly the Arrow-visible projection of spec::DType. `format` is per-file
/// and therefore absent; everything the Arrow field carries is present.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum TargetType {
    Utf8, Bool, Int64, Float64,
    Decimal { precision: u8, scale: i8 },
    Date,
    Timestamp { #[serde(default)] timezone: Option<String> },
}

impl Target {
    pub fn load(path: &Path) -> Result<Target>;                  // toml + validate()
    pub fn validate(&self) -> Result<(), Vec<String>>;           // Vec<String>, like spec::validate
    pub fn arrow_schema(&self) -> Result<Schema>;
    pub fn target_hash(&self) -> String;
    pub fn override_hash(&self, member: &str) -> String;
    pub fn column(&self, name: &str) -> Option<&TargetColumn>;
    pub fn probe_columns(&self) -> Result<Vec<ColumnSpec>>;      // synthetic, for arrow_schema
}
```

**There is exactly one `DType` → Arrow mapping in the codebase, and this does not add a second.** `engine.rs` grows one function and `schema_of` is refactored onto it (verified feasible: `schema_of` already calls `build_column_at(col, &[], 0)` at engine.rs:916 and discards the array):

```rust
// src/engine.rs
pub fn arrow_field_of(col: &ColumnSpec) -> Result<Field> {
    build_column_at(col, &[], 0).map(|(f, _)| f)
}
pub fn schema_of(spec: &ParseSpec) -> Result<Schema> {
    let mut fields = Vec::with_capacity(spec.columns.len());
    for col in &spec.columns { fields.push(arrow_field_of(col)?); }
    Ok(Schema::new(fields))
}
```

`Target::arrow_schema` builds one synthetic `ColumnSpec` per declared column (filling `Date`/`Timestamp` with the inert placeholder format `%Y-%m-%d` / `%Y-%m-%dT%H:%M:%S`, which never reaches a parser because the value slice is empty) and runs them through `arrow_field_of`. Both sides of the conformance comparison are therefore produced by the code that types real data, and the canonicalised `+HH:MM` timezone label and `Decimal128(p, s)` cannot drift between them. `tests/conform.rs` pins that the placeholder is invisible in Arrow.

**`norm()` — the matching normaliser — is a new function and deliberately not `sniff::sanitize`.** Verified: `sanitize` ends with `.trim_end_matches('_')` (sniff.rs:1145), so `Umsatz %` and `Umsatz` both normalise to `umsatz`, which would bind a percent column to a money column. The two functions have different jobs: `sanitize` produces a legal SQL identifier; `norm` decides whether two human labels are the same label.

```rust
/// Trim whitespace; NFKC; fold umlauts/accents (shared with sniff via
/// crate::text::fold); lowercase; map each significant symbol to a token
/// (% -> "pct", ‰ -> "permille", € -> "eur", $ -> "usd", £ -> "gbp");
/// collapse each remaining run of non-alphanumerics to one '_';
/// drop a trailing '_' produced only by insignificant punctuation.
///
///   "Betrag CHF"   -> "betrag_chf"
///   "Betrag (CHF)" -> "betrag_chf"
///   "Umsatz %"     -> "umsatz_pct"     <- does NOT collide with "umsatz"
///   "Rabatt %"     -> "rabatt_pct"
pub fn norm(s: &str) -> String;
```

`Target::validate()` enforces, file-free, in the `spec::validate` idiom:
non-empty `columns`; no empty or duplicate `name`; **no two `match` entries colliding under `norm`, within a column or across columns** (a latent ambiguity, rejected at authoring time rather than at fit time); decimal precision `1..=38` and scale `0..=precision`; a `Timestamp` timezone that is a fixed offset (a named zone rejected with today's message); `absent = "null"` implies `nullable = true`; a `from` column may not also carry a `match` set; every `files."..."` key matches at least one glob in `files` (an override that matches no member is a silenced assertion — reject it, unless `--only` scoped the run); `from = { match = ... }` regex compiles and every `$n` in its template has a capture group; `name` is `[a-z0-9_]+`.

The decimal/timezone/format rules are *shared*, not restated: they are extracted from `spec::validate`'s column loop into `impl DType { fn validate(&self) -> Vec<String> }` first, and both validators call it. A drifted copy of the scale rule is a target that accepts a spec the executor rejects.

### 4.3 `src/conform.rs` — Gate S

```rust
#[derive(Debug)]
pub enum Mismatch {
    Arity        { want: usize, got: usize },
    Name         { at: usize, want: String, got: String },
    Type         { column: String, want: DataType, got: DataType },
    Nullability  { column: String, want: bool, got: bool },
}
impl fmt::Display for Mismatch { /* one model-feedback-grade sentence each */ }

/// Positional, exact, both directions. No widening, no reordering,
/// no extras, no subsumption of nullability.
pub fn conforms(spec: &ParseSpec, target: &Target) -> Result<(), Vec<Mismatch>>;
```

Positional rather than set-based, for two reasons: column order is `SELECT *` order and therefore semantic, and `StreamingTableExec::try_new` applies `schema.eq(partition_schema)` at scan time anyway, so any laxity here becomes a late DataFusion plan error instead of an early tdy error. Nullability is compared, not subsumed: a `nullable = true` spec does not satisfy a `NOT NULL` target column, because the entire point of declaring NOT NULL is to arm `build_column_at`'s per-row null check (engine.rs:1021), which a spec without the flag never arms. **`nullable` defaults to `true` in `ColumnSpec` (discovered) and `true` in `TargetColumn` too** — but the target is where a user writes `nullable = false` and means it, and doing so is what finally makes that enforcement path live for inferred specs.

### 4.4 `src/fit/` — the planner's types

```rust
pub struct FitPlan {
    pub spec: ParseSpec,
    pub method: InferenceMethod,          // Heuristic | Llm | Reused | Manual
    pub mapping: Vec<Bound>,
    pub review: Vec<ReviewReason>,
    pub rows: RowAccounting,
    pub stats: Vec<ColumnStats>,
    pub header: Vec<String>,
    pub header_origin: Vec<HeaderOrigin>,
    pub verified: VerifyLevel,
}

pub enum FitOutcome { Conforms(Box<FitPlan>), Unfittable(Vec<Gap>) }

pub struct Gap { pub column: Option<String>, pub kind: GapKind }

pub enum GapKind {
    NoCandidate        { tried: Vec<String>, available: Vec<String>, nearest: Vec<(String,String)> },
    Ambiguous          { candidates: Vec<(String, usize)> },     // name + column position
    NonInjective       { columns: Vec<String>, source: String },
    SyntheticName      { source: String, why: &'static str },    // col_7 / Betrag_2, unpinned
    TypeUnreachable    { want: TargetType, offenders: Vec<(usize, String)> },  // <= 5 values
    AmbiguousNumeric   { source: String },                       // numfmt::infer == ambiguous
    ConventionMismatch { source: String, declared: String, inferred: String },
    AmbiguousDate      { source: String, formats: Vec<String> },
    UnpivotNotTotal    { unaccounted: Vec<String> },
    NoFrame            { frames_tried: usize, best: Box<FrameReport> },
    ExtractionFailed   { why: String },
}

pub enum ReviewReason {
    UnitShift        { column: String, shift: i8 },
    AffixStripped    { column: String, affix: String },      // "%", "CHF ", "TCHF"
    PrecisionLoss    { column: String, observed_scale: u8, declared_scale: i8 },
    AbsentByPolicy   { column: String },
    FirstUseOfSource { column: String, source: String, modal: Option<String> },
    AliasFromModel   { column: String, source: String },
    ExtentLoss       { what: &'static str, rows: usize, of: usize },
    ExtractionAsserted { kind: &'static str },   // fixed_width / lines / sheet / range
    MagnitudeDivergence { column: String, member_median: f64, dataset_median: f64 },
    DuplicateContent { other: String },
}
```

`Gap` and `ReviewReason` are the structured, per-column, machine-readable reasons that today's whole-spec `confidence: f32` and explicitly-never-machine-interpreted `notes` have nowhere to put. One `Display` impl each serves three consumers: the CLI gap report, the lock, and the model's feedback string.

### 4.5 Changes to existing modules

**`src/engine.rs`**

1. `arrow_field_of` (above), `schema_of` refactored onto it.
2. `RawTable` gains a **header origin record** — this is the fix to the flaw five of eight critics called fatal:
   ```rust
   pub enum HeaderOrigin {
       Original,                    // the file's own name, verbatim
       Deduped { raw: String },     // dedupe_names renamed a collision; `raw` is what the file said
       Invented,                    // blank cell, or no header at all (col_7)
   }
   pub struct RawTable { …, pub header_origin: Vec<HeaderOrigin>, }
   ```
   Verified necessary: `ensure_header` (engine.rs:156-174) blanks-fills to `col_N` and then calls `dedupe_names` *before* any header index exists, so a file with two literal `Betrag` columns presents to every consumer as `["Betrag", "Betrag_2"]`. Under name matching that is exactly **one** candidate, and the planner would silently read the first of two columns the file did not distinguish — the historical defect CLAUDE.md records, returning through the alias layer. `dedupe_names` gains a sibling `dedupe_names_recording(&mut [String]) -> Vec<HeaderOrigin>`; `ensure_header`, `promote_header_from` (engine.rs:756) and the unpivot header rebuild (engine.rs:864) populate the vec. `stream.rs` is untouched: origins are needed only on the fit-time probe, and `tests/streaming.rs::assert_paths_agree` already pins that both paths produce the same header.
3. `Transform::Constant` arm in `apply_transforms`: `table.ensure_header()?` first (so the table is rectangular and the header exists — the same thing `FillDown` does at engine.rs:816), then push one cell per row and one name to the header, erroring if the name already exists (`header_index` is first-wins, so a duplicate would silently shadow).
4. `ValueParsing::decimal_shift` applied inside `build_column_at`, **after** separator normalisation and `numfmt::check_grouping` and **before** the parse, as an exact decimal-point move on the digit string. Never a float multiply.
5. `apply_transforms` returns `TransformStats` for extent accounting:
   ```rust
   pub struct TransformStats {
       pub rows_in: usize, pub skipped_head: usize, pub skipped_tail: usize,
       pub header_rows: usize, pub dropped: usize, pub unpivot_factor: usize,
       pub rows_out: usize,
   }
   ```

**`src/spec.rs`** — two additions, each the documented four-step change:

```rust
Transform::Constant { name: String, #[serde(default)] value: Option<String> }  // None = a NULL column
ValueParsing::decimal_shift: Option<i8>
```

New `validate()` rules: `Constant.name` non-empty and, when an `Unpivot` follows, present in its `id_columns` (otherwise the unpivot silently drops it — engine.rs:834-868 rebuilds rows from `id_idx` only); `Constant` may not precede a `PromoteHeader` (its cell would be promoted into the header); `decimal_shift` in `-9..=9` and **legal only on a `Decimal` column** — on `Float64` it is a multiplication with representation error and on `Int64` a negative shift is not integral, and a "silently slightly wrong" value is exactly what the rule forbids.

**One further `validate()` rule, which is a bug fix independent of any target work** (this resolves the timestamp flaw): a `DType::Timestamp` whose `format` contains `%z` / `%:z` / `%#z` and whose `timezone` is `None` is invalid. Verified at engine.rs:1271-1306: a `%z` format returns `dt.timestamp_micros()` (a true instant, ignoring the declared timezone) while a naive format with `timezone: None` returns `naive.and_utc().timestamp_micros()` (a wall clock read as UTC) — and **both produce `Timestamp(Microsecond, None)`**. Two files, both conforming to `timestamp` with no timezone, unioned into one column whose rows are hours apart in meaning, with no parse failure to catch it. The message is: *"a format carrying an offset produces an absolute instant; declare `timezone` (e.g. `"UTC"`) so the column's Arrow type says so."* This is a tightening that will reject a small number of existing sidecars, loudly, with a one-line fix — see Section 9.4.

**`src/sidecar.rs`**

```rust
pub fn sidecar_path_for(file: &Path, target: Option<&str>) -> PathBuf;
// None       -> "2025-09.xlsx.tdy.toml"      (today's, untouched)
// Some("sales") -> "2025-09.xlsx.sales.tdy.toml"
```

The suffix stays `.tdy.toml`, so every existing gitignore pattern and `tests/adversarial.rs`'s fixture sweep keep matching. A file can serve `messy()` and any number of targets simultaneously, each independently fingerprinted, each independently reviewable — which dissolves the "one file cannot hold two specs" obstacle for six lines. `SourceFingerprint.path` (written today, read nowhere) becomes live: a member whose recorded file name differs from its current name is `Drift::Renamed`, which matters because a `from = "file_stem"` constant is baked into the spec and a rename must invalidate it.

`sidecar::load` is **not** given a target parameter. Making it target-aware would make `sidecar.rs` cwd- and `Overrides`-dependent (it would have to resolve which target a `<file>.<name>.tdy.toml` belongs to). Gate S runs at the *callers* — `check_spec` and `DatasetFunc::call` — which is where the target is already in hand.

**`src/provider.rs`** — one signature change, which reaches both tiers because it is the single chokepoint every written spec passes (callers at provider.rs:255 and :349):

```rust
fn check_spec(spec: &ParseSpec, path: &Path, limits: Limits,
              target: Option<&Target>, method: InferenceMethod) -> Result<()>
```

`None` is byte-for-byte today's behaviour. `Some(t)` adds Gate S before `dry_run`. `method` is there for the model-forbidden-operator policy (Section 5.4).

**`src/sqlscan.rs`** — the tokenizer is untouched. `MessyRef` generalises:

```rust
pub enum Func { Messy, Dataset }
pub struct TableRef { pub func: Func, pub args: Vec<String> }
pub fn find_refs(sql: &str) -> Vec<TableRef>;
pub fn find_messy_refs(sql: &str) -> Vec<MessyRef>;   // kept as a wrapper; its tests unchanged
```

**`src/dataset.rs`** — the UDTF, registered beside `messy` in `provider::session`:

```rust
pub struct DatasetFunc { frozen: bool, limits: Limits,
                         cache: Mutex<HashMap<(PathBuf, Option<PathBuf>), Arc<dyn TableProvider>>> }

enum MemberSource {
    Streamed   { spec: Arc<ParseSpec>, path: PathBuf },
    Materialised { spec: Arc<ParseSpec>, path: PathBuf },
}
pub struct DatasetPartition { members: Vec<MemberSource>, schema: SchemaRef, limits: Limits }
impl PartitionStream for DatasetPartition { … }   // ONE partition, members in lock order
```

`DatasetFunc::call` is fully synchronous and does no inference, no network, and no writes — see Section 8.3. That is what lets `dataset()` work identically frozen and unfrozen and removes the entire sync/async problem the other designs fought.

---

## 5. The planner

### 5.1 Tier 1, stage A — frame enumeration

A **frame** is `(Extraction, transforms up to and including promote_header)` plus the realised probe table it produces. Enumeration is a bounded product, not a search:

- **Extraction candidates.** Excel: one per sheet, from `engine::excel_sheet_shapes` (`sniff::pick_sheet` already ranks these and throws away every loser). Delimited: each of `, ; \t |` whose modal field count ≥ 2, at most 4. JSON: `sniff::find_record_arrays`' candidate list, capped at 8 (it already computes up to 64 and discards all but one). **Fixed-width and lines are not enumerated** — see 5.6.
- **Structural candidates.** `skip_head ∈ 0..=8` × `promote_rows ∈ {1,2,3}`, pruned to frames whose promoted header is ≥ 80% non-blank. At most 27 per extraction; typically 3-5 survive.
- **A per-file override pins whatever it names**, and the probe is re-run against the pinned value.

Cost discipline: the probe is extracted **once per extraction candidate** (`PROBE_ROWS = 2000`, 4 MiB prefix for text) and the structural candidates are evaluated by **slicing** that probe's rows, never by cloning it — `tests/adversarial.rs` sweeps a 100k-column fixture, and 27 clones of a 2000×100k probe is 200M `String`s. A new `PROBE_CELLS = 1_000_000` bound caps the probe by cells as well as rows, following `limits.max_cells`' precedent. Only the winning frame is materialised.

**Unpivot is not planned by tier 1.** Deciding that a file is wide and the target is long from evidence alone is where a bounded product becomes a search engine. It comes from a `[files."..."]` override or from tier 2. This is the main deliberate scope cut.

### 5.2 Tier 1, stage B — binding

For each frame, for each **declared** column, a strict-priority ladder. **The first non-empty tier wins outright**; if its candidate then fails typing, the *file fails* — it does not fall through to a fuzzier tier. This is `numfmt`'s discipline ("do not try conventions until one parses") lifted to the column level.

| tier | rule |
|---|---|
| 0 `Pinned` | a `[files."x"].columns.<col>.source` override. Wins over everything. If it fails, hard error: a human asserted it. |
| 1 `Exact` | header string == column name |
| 2 `Normalized` | `norm(header) == norm(column name)` |
| 3 `Alias` | `norm(header) ∈ norm(match set)` |

**Ambiguity inside a tier is an error, never a tiebreak.** Two candidates produce `GapKind::Ambiguous` naming both headers *and their column positions*, resolved by narrowing the target's `match` set or by a per-file `source` pin. A tool willing to "pick the one that types as decimal" would get `Betrag` (net) vs `Betrag CHF` (gross) right most of the time and catastrophically wrong occasionally — both type fine.

Three rules make this real rather than aspirational:

1. **The header-origin rule.** A candidate cell whose origin is `Deduped { raw }` is matched under `raw`, so a file with two literal `Betrag` columns yields **two** candidates and refuses. Without this the whole ambiguity guarantee is defeated upstream by a rename the engine performs for unrelated reasons. Disambiguation for such a file is positional: `source = "Betrag@3"` in an override, where `@N` is a 1-based post-transform column index (`Target::validate` accepts the syntax; `fit::bind` resolves it against the frame, erroring if position N's raw name is not `Betrag`).
2. **The synthetic-name rule.** A candidate cell whose origin is `Invented` (`col_7`) can be bound only by an explicit pin, never by name or alias matching. `col_7` is not what the file calls the column; it is what tdy called it.
3. **Injectivity.** No two declared columns may resolve to the same header cell → `GapKind::NonInjective`. Verified necessary: `spec::validate()` rejects duplicate *output* names only (spec.rs:448-455) and `to_record_batches` happily resolves two `ColumnSpec`s to one header index. A target with `amount_chf` and `amount_eur` sharing the generic alias `Amount` over a file carrying one `Amount` column would otherwise conform exactly, print 2/2 coverage, and hand back two identical numbers labelled CHF and EUR.

`nearest` (a `norm`-distance suggestion) exists **only** for the error message and can never become a binding.

### 5.3 Tier 1, stage C — typing by checking, not preferring

`fit::typing::check_type(values: &[&str], want: &TargetType, hints: &FileOverride, date_order) -> Fit` inverts `sniff::guess_type`. Where `guess_type` has a preference order that downgrades to `Utf8` on doubt, `check_type` has a yes/no answer against a declaration — and against a target, a downgrade to text is not a safe default, it is a silent target violation.

- **`Utf8`** — always fits. `na_values` are **not** carried over from a numeric draft: the sniffer gives typed columns `na_values` because there a token cannot be a value, and gives text columns none because "NA" is Namibia. A `-` that becomes NULL in a `GROUP BY` drops a whole category.
- **`Int64`** — every value parses as `i64`; a significant leading zero or an oversized integer is a **hard rejection**, not the silent downgrade to text `sniff` performs today.
- **`Float64` / `Decimal{p,s}`** — `numfmt::infer` must return a **definite** convention (`ambiguous` → `GapKind::AmbiguousNumeric`, where today it is a −0.25 confidence penalty and ships), *and* — critically — **the inferred convention must equal the one the spec declares** (`GapKind::ConventionMismatch`). Verified necessary: a spec declaring `thousands='.' decimal=','` over values `1.234` / `2.345` yields `1234.00` / `2345.00`, `check_grouping` accepts it (`["1","234"]` is legal three-grouping), and a *full* execution passes. The only mechanical signal that this is a 1000× error is `infer` disagreeing. This rule is what makes cross-file spec reuse safe (5.5) and it is the single most important line in `check_type`.
  `numfmt::check_grouping` then still runs per value inside the typed cast, at execution, on every row.
- **`Date` / `Timestamp`** — collect every format in `DATE_FORMATS`/`TS_FORMATS` that parses **all** probe values, filtered by the dataset's `date_order` when declared and by a per-file `format` pin when present. Exactly one → bound. Zero → `TypeUnreachable` with up to five offending values. **Two or more that disagree on at least one value → `GapKind::AmbiguousDate`, naming both.** `DATE_FORMATS`' day-first-before-month-first preference order never breaks a tie; it only reports one. `check_year`'s four-digit rule still applies.
- **`Bool`** — the target's declared token sets, else the engine defaults.

`sniff::looks_monetary`'s word list plays no role under a target: the target says `decimal` or it does not. Every guessing heuristic is demoted to a tiebreaker used only where the target is silent, and the target is silent about exactly one thing: the strftime format.

**Value cleaning is derived mechanically, from a bounded, checked catalogue**, in this order:

1. trim (always);
2. `na_values` — only for non-`Utf8` columns, only from `{"", "-", "–", ".", "n/a", "N/A", "NA", "null", "NULL"}`, only tokens actually observed;
3. `strip` — one regex from a bounded catalogue (a leading or trailing currency affix present on *every* non-null value; a trailing `%`; NBSP/narrow-NBSP as a thousands mark), accepted only if after stripping `numfmt::infer` is definite and every value parses. **Any strip whose matched text is not pure whitespace or bracketing punctuation raises `ReviewReason::AffixStripped`** — stripping a `%` changes what the number means relative to a fraction, and that is a semantic act, not a cleaning step;
4. separators from `numfmt::infer`;
5. `decimal_shift` only from an override, never derived.

If the catalogue does not reach the target, that is a `Gap`. The escape hatch is the per-file override block, which can express the whole `ValueParsing` vocabulary. Deliberate cost: German month names via `replace` pairs are a human edit under a target, where `messy()` would have asked the model. The model can *propose* the override block (`tdy fit --propose`), never apply it.

**`PrecisionLoss`:** `check_type` records the maximum fractional-digit count observed. If it exceeds the declared `scale`, that is `ReviewReason::PrecisionLoss` — because `parse_decimal` rounds half away from zero silently, and under `messy()` today the sniffer *derives* the scale from the data and always attaches a rounding note, whereas a target *declares* a scale in a document that has seen no file. Declaring a scale is declaring a rounding policy, and it needs to be acknowledged once, per file, in the lock.

### 5.4 Tier 2 — what the model is asked, and what it structurally cannot say

Tier 2 is entered **iff** tier 1 produced any `Gap` — never because a float fell below a threshold. It is asked for three things and nothing else:

```rust
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct FramePlan {
    pub extraction: spec::Extraction,          // structure only; no value-bearing fields
    #[serde(default)] pub transforms: Vec<PlanTransform>,
    #[serde(default)] pub aliases: Vec<AliasProposal>,   // {column, header, why}
    #[serde(default)] pub notes: Vec<String>,
}

/// The model-facing transform vocabulary. A DELIBERATE mirror of the five
/// structural variants of spec::Transform, omitting Constant. Adding a variant
/// to spec::Transform is a non-event here; adding one here is a deliberate
/// grant of capability, and TryFrom makes any divergence a compile error.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
pub enum PlanTransform { SkipRows{..}, PromoteHeader{..}, DropRowsMatching{..},
                         FillDown{..}, Unpivot{..} }
impl TryFrom<PlanTransform> for spec::Transform { … }
```

**The model cannot emit `columns`, `ColumnSpec`, `DType`, `ValueParsing`, `Constant`, or `decimal_shift`.** `schema_for!(FramePlan)` is roughly 3 KB compact against `ParseSpec`'s 11.7 KB, which frees prompt budget for the target block and moves *away* from Anthropic's hard-coded `max_tokens = 16000` cliff (infer.rs:482, 512 — a non-retryable, non-feedbackable failure) rather than toward it. The returned frame goes back through **stage B and C unchanged**: tdy rebinds and retypes deterministically. This makes README.md:70's policy ("the model emits instructions, not data") a type-level fact in the target path.

`aliases` are **never applied**. `tdy fit --propose` prints them as a pasteable TOML patch. Accepting one edits the target, which changes `target_hash`, which stales every plan, which replans everything — the loop closes correctly, through a git diff.

The retry loop is the existing three-gate chain plus a fourth arm placed **before** `dry_run` because it costs no I/O:

```
deserialize -> spec.validate() -> conform::conforms() -> engine::dry_run()
```

`Vec<Mismatch>` and `Vec<Gap>` render into the same single `problem: String` that `Feedback { problem, previous }` already carries. The feedback is qualitatively better than anything the loop has today — *"you proposed a frame whose post-transform header is [Datum, Region, Betrag Rp.]; target column `amount_chf` still does not bind; the declared match set is [...]"* tells the model the answer it must reach, not merely that its guess broke. `MAX_FEEDBACK_CHARS` rises to 6000 for target-directed calls, and a token budget is finally counted in `infer.rs` (nothing counts one today).

`FIT_PROMPT_VERSION = "fit-v1"` is a separate constant from `PROMPT_VERSION = "infer-v3"`; the untargeted prompt is untouched.

**The model-forbidden-operator policy, belt and braces.** `Transform::Constant` and `ValueParsing::decimal_shift` must never come from a model on *any* path, including untargeted `messy()`, where no target exists to check them and a model that decides a column is "in thousands" produces silently wrong numbers.

1. `spec::model_json_schema()` — a tested projection of `ParseSpec::json_schema()` that deletes the `constant` variant and the `decimal_shift` property. One derive, one explicit subtraction, one unit test asserting the subtraction actually happened (so a rename breaks the test rather than silently widening the grammar). `tdy schema` still prints the whole schema; `tdy schema --model` prints the projection. This is what goes into `response_format`, the Anthropic `input_schema`, and the prompt text.
2. A `MODEL_FORBIDDEN` check as an arm **inside** `infer`'s verification chain (so a model that invents the field anyway gets feedback and a retry) **and** in `check_spec` keyed on `InferenceMethod::Llm` (so it is non-bypassable). Adding a future value-bearing operator forces a decision at the `MODEL_FORBIDDEN` list.

### 5.5 Cross-file reuse — a candidate generator that never inherits a proof

Twelve monthly files from one system are one layout twelve times, and forty files inferred in total ignorance of their siblings are forty guesses free to disagree. `FitMemo` keys on

```rust
struct ShapeKey {
    extraction_kind: &'static str,
    header_norm: Vec<String>,       // norm()'d post-transform header, sorted
    numeric_conv: Vec<Option<NumericFormat>>,   // per bound cell, from numfmt::infer
    date_fmts:    Vec<Vec<String>>,             // per bound cell, formats that parse everything
}
```

A hit repoints a sibling's spec at the new path and runs **the entire gate chain on the new file's own values**. The key includes the value-shape terms precisely so a locale change produces a different key; and even a key collision cannot hurt, because `check_type`'s convention-agreement rule (5.3) re-derives `numfmt::infer` on this file's values and refuses on disagreement.

**Invariant, stated as a rule: no proof is ever inherited across files.** Reuse can only save work. This is the fix to the design that reused a numeric convention proven on a sibling and produced a 1000× error with every gate green.

Reuse is additionally **ineligible** for any member carrying a `[files."..."]` override, so the escape hatch can never be skipped by a cache hit; and `method = "reused"` plus `reused_from` are recorded in the lock so a reviewer sees which files were confirmations rather than independent derivations.

### 5.6 Scoring

```rust
#[derive(PartialEq, Eq, PartialOrd, Ord)]
pub struct FrameScore {
    bound_required: usize,           // maximize
    ambiguities:    Reverse<usize>,  // minimize
    bound_optional: usize,           // maximize
    rows_kept:      usize,           // maximize
    transforms:     Reverse<usize>,  // minimize
}
```

A derived lexicographic `Ord` on a tuple, never a weighted sum — a weighted sum is how a plausible wrong plan outscores a right one. Ties break on enumeration order, which is fixed, so the planner is reproducible.

**A frame is eligible to win only if it binds every required column with zero ambiguities.** Scoring ranks *only* frames that provably conform; incomplete frames are retained solely so `GapKind::NoFrame` can report the best near-miss. Scoring therefore cannot select a non-conforming plan; it picks among plans that all provably do.

Target coverage is what makes the structural search decidable at all: for `2025-09.xlsx`, `skip_rows{head:1} + promote_header{rows:1}` beats `promote_header{rows:2}` not because of a heuristic about merged cells but because the first binds 3/3 required columns and the second produces the header `Umsatz 2025 Betrag CHF` and binds 0/3.

### 5.7 Extractions with no header to check against

`fixed_width` names its own fields; `lines` names its own capture groups; an Excel `sheet`/`range` selects which rows exist at all. For these there is no observed header to bind against — the field boundaries are *asserted*, not observed, and a plausible-looking wrong offset produces a column of plausible-looking wrong values. So:

- tier 1 does **not** enumerate `fixed_width` or `lines`; a file that `detect` says is one of those is `GapKind::NoFrame` unless an override pins the extraction;
- when tier 2 or an override supplies one, the plan carries `ReviewReason::ExtractionAsserted`, so a human accepts it once, against that file's hash;
- the same applies to a model- or override-chosen `sheet` or `range`, because both change which rows exist and Gate X (6.4) can only account for what it was told about.

---

## 6. The conformance gate

Seven named gates. Each states what it proves and what it does not. `tdy fit` and `tdy check` print which ran.

### 6.1 G-V — validate (existing, unchanged)

`spec.validate()`: intra-spec, file-free, µs. Plus the new rules in 4.5.

### 6.2 G-S — shape (the new gate; file-free, total, µs)

`conform::conforms(spec, target)`: `engine::schema_of(spec)` compared field-for-field, positionally, against `Target::arrow_schema()` on name, `DataType` (including decimal precision/scale, `Date32`, timestamp unit and canonicalised `+HH:MM` label) and nullability. Both sides built by `arrow_field_of`, so this compares the executor's typing code against itself.

**What it proves that `dry_run` cannot:** `schema_of` is a pure function of `spec.columns` and touches no file, so its result is the schema of *every* batch the spec will ever emit, on both executors, for all rows, forever. `dry_run` proves that at most 200 typed rows from a 4 MiB prefix parsed — and `preview`'s slack estimate counts only `SkipRows{head}` and `PromoteHeader{rows}` (verified at engine.rs:1385-1395), so a spec carrying `drop_rows_matching` can yield five rows and still be called "200 rows previewed".

**What it is honestly for.** Because tier 1 constructs `columns` *from* the target, G-S is close to tautological at plan time for tier-1 output. Its real jobs are three, and all three are populations that need catching:
1. the model's output, which states `name`/`dtype`/`nullable` itself — tdy deliberately does **not** overwrite them with the target's, because doing so would make G-S vacuous and would replace a proof with a rubber stamp, and because the mismatch is the feedback that fixes the next attempt in one round;
2. a hand-edited sidecar;
3. **a target that moved under a plan** — see 6.7.

It also structurally kills both hazards verified on the current binary: positional `UNION ALL` transposition and DataFusion's silent Int64 ∪ Utf8 widening. Not because the union is checked, but because every branch was proved equal to the same schema before the union existed.

### 6.3 G-M — mapping (needs the post-transform header)

Every declared column resolves to exactly one header cell; the mapping is injective; no cell whose origin is `Invented` is bound without a pin; every column named by a `fill_down`, `drop_rows_matching{column}` or `unpivot` exists in the header at the point that transform runs (`spec::validate()` is deliberately file-blind and cannot check this; today it is discovered by `dry_run` only if the head happens to exercise it); and — the rule that closes the wide-file hole — **an `unpivot` must partition the header**: every non-`id`, non-`value` column is `GapKind::UnpivotNotTotal`. A column that is neither id nor value is silently dropped from the relation, and a spec reused onto next month's file with one extra period column would otherwise drop that month's rows with every other gate green.

G-M is re-run at read time against the header **stored in the lock**, at zero I/O (Section 6.7).

### 6.4 G-X — extent accounting (the gate nobody had)

Nothing in any candidate design counted rows, and a frame that silently discards 30% of a file binds every column, types cleanly, conforms field-for-field, and passes value verification on every surviving row — precisely the "plausible wrong number that names nothing" the failure semantics exist to forbid. So:

```
rows_out  ==  (rows_extracted - header_rows - skipped_head - skipped_tail - dropped)
              * (unpivot ? value_columns.len() : 1)
```

must balance exactly, from `TransformStats`. The counts are stored per member in the lock, so `dropped = 812` shows up in a git diff rather than nowhere. Any non-zero `dropped`, any `skip_rows{tail}`, and any Excel `range` raise `ReviewReason::ExtentLoss`.

G-X is meaningful only over a complete read, which is one of the two reasons `verify = "full"` is the default (6.6).

### 6.5 G-P — parse (values)

`check_type` over up to 500 probe values per column (including the convention-agreement rule of 5.3), plus a real execution: `engine::dry_run` under `verify = "head"`, or a full `execute` whose batches are discarded under `verify = "full"`. `verified` is recorded per member, honestly, and `tdy check --verify full` fails on any member whose lock entry says `head`.

The evidence count is printed and stored: `amount_chf: 4112/4112 values typed` versus `amount_chf: 3 values typed`. A column with three non-null values types cleanly against any declared type, and the report must say so.

### 6.6 `verify = "full"` is the default on a member's first fit

The file is already open at that moment; it is the one cheap opportunity. Consequences: G-X becomes exact rather than sampled; `skip_rows{tail}` is actually applied and its footer handling actually tested (under a truncated read it is deliberately skipped, engine.rs:762-775, so head-only verification *never* exercises the region where a "Total" row waits); and `nullable = false` — newly meaningful under a target — is proved over the whole file rather than over a head that systematically excludes the footer.

Re-fits after a content change also verify full. `verify = "head"` is an explicit opt-out for very large datasets, recorded in the lock so nobody can misread what was proved. The known asymmetry: `checked_worksheet_range` materialises a whole sheet before the row cap applies, so a full verify over a directory of workbooks costs O(total workbook bytes) where the same command over CSVs costs one pass each. `tdy fit` prints the total bytes it is about to read before starting.

### 6.7 G-R and read-time re-proof: why the target never enters the freshness fingerprint

This is the best idea in the entire candidate set and it is adopted verbatim. **Because G-S needs no file access, the target does not have to participate in sidecar freshness at all.** `DatasetFunc::call` re-runs G-V, G-S, G-M (against the stored header), and G-R (every recorded `ReviewReason` has a live acceptance whose three hashes still match) on **every** load, frozen or not, at essentially zero cost, against the *live* target file. Edit the target's types and every plan written against the old shape fails immediately, by name, at plan time — with `Sidecar`, `SourceFingerprint`, `Provenance` and the envelope's `spec_version` untouched.

This also closes a hole that exists today independent of any target work: `resolve_spec_sync` hands a `Fresh` sidecar straight to the executor with no re-check at all (provider.rs:233-258), including under `--frozen`, so a hand-edited sidecar that still parses but now produces `float64` where `decimal(18,2)` was expected is discovered in a total, not at plan time.

### 6.8 G-D — cross-member divergence (a heuristic that may only refuse)

Available only because a dataset exists; runs after all members are planned; **may never rescale, rename, or resolve anything** — it may only stop the run and ask. Four checks, all from data the lock already holds:

1. **Modal source drift.** "11 members read `amount_chf` from `Betrag`; `2025-09.xlsx` reads it from `Umsatz`." This is the mechanical answer to the alias hazard: `match` sets widen monotonically as files arrive, and a file whose `Umsatz` is gross where every sibling's `Betrag` is net binds uniquely, conforms, parses, and would otherwise be auto-accepted forever. First use of a non-modal source is `ReviewReason::FirstUseOfSource`. Zero new I/O.
2. **Convention and format divergence.** Members resolving to different `thousands_separator` or a different strftime format. Reported, not refused: `%d.%m.%Y` in a German export and `%Y-%m-%d` in an English one is normal and each was proved unambiguous on its own file.
3. **Magnitude divergence.** Per-member per-column median absolute value; a member differing from the dataset median by ≥ 10× is `ReviewReason::MagnitudeDivergence` and `tdy fit` **refuses to write** until it is accepted. This is the only mechanical signal for the Rappen file when its column is named plainly. It is a heuristic and it is stated as one; it catches the 100× class and does not catch a 1.2× net/gross swap — which is what check 1 is for. Silencing it is a per-member, per-column acceptance in the lock (a reviewed, versioned act), never a dataset-level off switch, because a safety check with a global kill switch is a check that exists in the repo and not in reality.
4. **Duplicate content.** Two members with identical blake3 (`2025-09.csv` and `2025-09-corrected.csv` both matching `2025-*.csv`) — `ReviewReason::DuplicateContent`, because double-counting a month is the same wrong sum as dropping one.

### 6.9 The test obligation this creates

G-S promotes `engine::schema_of` from an internal convenience to the load-bearing half of a user-visible contract. Its agreement with the batches the executors actually emit is true by construction today and asserted **nowhere**, while `MessyFunc::call` already trusts it in production for the lazy path. `tests/conform.rs`, in `tests/streaming.rs::assert_paths_agree`'s sweep style, is a **hard prerequisite of slice 1**:

```rust
// for every fixture with a sidecar, on both executors:
assert_eq!(engine::schema_of(&spec)?, *execute(&spec, path)?[0].schema());
// and: Target::arrow_schema() is invariant to the placeholder date/timestamp format
```

---

## 7. Failure semantics, and contract vs proposal

### 7.1 Q1 — a file that cannot reach the target is a hard error

Not a partial load. Not a skipped file. Not a null-filled column with a warning. `tdy fit` exits non-zero; a query naming a dataset with any non-conforming member fails at plan time, before a byte is read.

**Why, and this is the argument that decides every other question in this document.** The point of a target is that somebody writes `sum(amount_chf)` over twelve files. If file 7 loads with `amount_chf` silently NULL because its column did not bind, the sum is short by a month, every row is well-typed, no error is raised, and the number is plausible, stable, repeatable — so it survives review. Unlike a bad cell, an aggregate launders it past any row a user could point at. Quietly dropping file 7 from the union is the same wrong number by a different route. Both are exactly what the rule forbids, amplified by the fact that a declared target *invites* the user to trust the result.

Silence is also unnecessary here, because the failure is cheap and early: G-S needs no I/O, and `tdy fit` collects every gap across every member in one pass rather than making the user discover them one query at a time.

### 7.2 The three declared softenings, and why declaration is the load-bearing word

Each is a statement written by a human into a versioned file, visible in a diff forever — not a flag on a command line, which is a decision made once under time pressure and never reviewed.

1. **`absent = "null"` on a column.** Fires **only** when the binder found *zero* candidates at every tier — never when one or more were plausible but ambiguous. A column is never nulled because the planner gave up on it. `Target::validate()` requires `nullable = true`. The plan then carries `Transform::Constant { name, value: None }` (a column of empty strings, which the engine nulls unconditionally), so G-S stays exact equality with no special case. **The first time it fires for a given file it is `ReviewReason::AbsentByPolicy` and needs an acceptance** — which is the fix for the objection that `absent = "null"` cannot distinguish "the file lacks this column" from "my alias list missed it". A human looks once, per file, per contract version.
   And because a padded null is indistinguishable in a query result from an empty cell, coverage is *reported*: `tdy fit`/`tdy check` print `discount_pct: supplied by 8/12 members; 3,214 of 41,882 rows structurally null`, every query over such a dataset prints the same line to stderr, and it is written into the Parquet key-value metadata on a Parquet write (stderr is dropped by pipelines; the artifact must carry the fact).
2. **`on_unfittable = "quarantine"`.** The file is dropped, recorded in the lock as `verdict = "unfittable", quarantined = true` — and the query still **errors** until a human runs `tdy fit … --quarantine 2025-11.csv`, which writes the acceptance. Every query over a dataset with a quarantined member prints a banner naming it, and the banner goes into Parquet metadata too. Partial load exists; it is never a default, never silent, never un-reviewed.
3. **Per-file overrides.** A pinned `source`, a `decimal_shift`, a `format`, a full `ValueParsing`. Human assertions about specific bytes.

### 7.3 The review gate — the sharpest line in this design

> **A plan whose acceptance rests on a semantic judgement rather than a mechanical proof does not execute until a human accepts it, and the acceptance is recorded against the file's hash, the target's hash, and that file's override hash.**

Mechanical, auto-accepted: the binding was unambiguous *and* used the modal source; the schema conforms; the mapping is injective; the extent accounting balances; the values parse.

Semantic, blocked: every `ReviewReason` in 4.4 — a unit shift, an affix strip, a declared scale that rounds, an absent column padded, a source no sibling uses, an alias the model reached for, rows discarded, an asserted extraction, a magnitude outlier, duplicate content.

Note the calibration this fixes: an exact, lossless, self-evidencing `decimal_shift = -2` demands acceptance, and so does choosing between two semantically different money columns across months. Both are semantic. A design that gates only the first has its risk calibration inverted.

```
Error: 2025-07.csv needs review before it can join dataset `sales`
  the plan applies decimal_shift = -2 to `amount_chf` (source column "Betrag Rp.")
  and reads `amount_chf` from a source no other member uses
    11 members read `amount_chf` from "Betrag"; this one reads "Betrag Rp."
  tdy does not accept a value-changing step on its own judgement.

  Inspect:  tdy explain sales.tdy-target.toml 2025-07.csv
  Accept:   tdy fit sales.tdy-target.toml --accept 2025-07.csv

  The acceptance is recorded in sales.tdy-lock.toml against this file's hash,
  this target's hash, and this file's override block, and expires if any changes.
```

`accepted_by` is a convenience label, not a credential — git blame on the lock is the audit trail, and the README says so.

### 7.4 What a gap looks like

`tdy fit` never stops at the first bad file: it plans every member, prints every gap, and *then* exits non-zero. Dying on the first file makes a twelve-file fix a twelve-round game. Members that conformed have their sidecars written (so the next run replans only the failures); the dataset still refuses to resolve.

```
$ tdy fit sales.tdy-target.toml
sales: 12 files, 5 target columns (4 required)  ·  reading 41.2 MB (verify = full)

  2025-01.csv … 2025-06.csv   conforms  5/5  heuristic, then reused ×5
  2025-07.csv                 REVIEW    5/5  unit_shift, first_use_of_source
  2025-08.csv                 GAP       4/5
      amount_chf: 2 columns match at the alias tier, which is ambiguous
        "Betrag" (column 3) and "Betrag" (column 4)   ← the file names them identically
      tdy will not choose. Pin it positionally:
        [files."2025-08.csv"]
        columns.amount_chf.source = "Betrag@3"
  2025-09.xlsx                conforms  5/5  heuristic  (sheet "Umsatz", skip_rows{head:1})
  2025-10.xlsx                conforms  5/5  heuristic  (date %Y-%m-%d, decimal ".", no thousands)
  2025-11.csv                 GAP       4/5
      region: no source column binds
        tried  exact "region" · normalized · aliases [Region, Kanton, Gebiet, Filiale]
        header after the best of 21 frames: ["Datum", "Betrag", "Bemerkung"]
      Add an alias to sales.tdy-target.toml, declare `absent = "null"` if a missing
      region is meaningful here, or exclude the file:
        exclude = ["2025-11.csv"]
  2025-12.csv                 conforms  5/5  reused
      note: this file also carries "Kundennummer", which `sales` does not declare (dropped)

10 of 12 files conform; 1 needs review; 1 cannot reach the target.
Sidecars written for the 10. `dataset(...)` will not resolve until every member is settled.
```

Every message ends with the edit that fixes it. That is the product.

### 7.5 Q2 — the target is a contract, not a proposal

**The planner may never add a column to the dataset. Discovery is reported, never merged.**

Three reasons, in decreasing order of force:

1. **A proposal makes the schema depend on the directory.** If tdy added discovered columns, `SELECT *` over `sales` would change meaning the month an export arrives with an extra `Rabatt` column — two runs of the same query against the same declaration disagree, every downstream Parquet schema moves, and `--frozen` becomes unimplementable.
2. **A proposal manufactures the exact failure 7.1 forbids.** A column present in eleven of twelve files becomes a column that is NULL for one twelfth of rows with no diagnostic. Helpfulness arriving at a plausible wrong number.
3. **Widening is a verified hazard, not a hypothetical.** DataFusion's union coercion silently turns Int64 + Utf8 into Utf8 and discards a declared type with no message. A contract closes it by proving every branch equal to one schema before the union exists.

Contract does not mean the file must be tidy: an undeclared extra column is fine and is dropped, because `columns` has always been a projection and dropping a column cannot corrupt a declared one.

**Negotiation exists, out of band, in a different medium.** `tdy target init 2025-01.csv 2025-10.xlsx -o sales.tdy-target.toml` drafts a target from a pile for a human to edit. `tdy fit --propose` prints, as pasteable TOML, the alias proposals from the model and the source columns present in the files but absent from the target:

```
suggestion: 9 of 12 files also carry a column matching "Kundennummer" (int64 in 9/9)

[[column]]
name     = "customer_no"
type     = "int64"
nullable = true
absent   = "null"
match    = ["Kundennummer", "Kunden-Nr", "Customer No"]
```

The model proposes into a versioned text file reviewed in a git diff. It never proposes into a query result.

---

## 8. Multi-file semantics

### 8.1 Membership comes from the lock, never from a glob at query time

`[dataset] files` / `exclude` are globs (the `glob` crate, already in the tree via DataFusion) resolved relative to the target file's directory, expanded by `tdy fit` / `tdy check` **only**, canonicalised, deduped, `*.tdy.toml` filtered out, sorted by the project-relative path string, and materialised into the lock. Every `Err` from the glob iterator (an unreadable directory) aborts — a silently dropped member is a short sum wearing a valid schema. Zero matches is a hard error: a glob typo yielding zero rows is a wrong answer wearing a valid schema.

A directory listing is not reproducible. Without the lock, `--frozen` over a multi-file set is a promise the tool cannot keep: a member deleted by a `git rm` silently shrinks the dataset with every remaining sidecar still Fresh, and a corrected re-export silently double-counts a month. With the lock, both are `Drift` and both fail CI.

The project-relative path is the **single** member identity everywhere (resolving today's split between the pre-pass deduping by literal SQL string and the UDTF caching by canonical path).

### 8.2 Drift

```rust
pub enum Drift {
    NewFile { path: String },          // matches the globs, absent from the lock
    RemovedFile { path: String },      // in the lock, gone from disk
    ChangedFile { path: String },      // blake3 differs from the lock
    RenamedFile { was: String, now: String },
    TargetMoved { was: String, now: String },     // target_hash
    OverrideMoved { path: String },               // override_hash
    SidecarEdited { path: String },               // sidecar_digest differs
    AcceptanceStale { path: String, reason: String },
    Unfittable { path: String, reason: String },
    QuarantineUnaccepted { path: String },
    Unverified { path: String },                  // lock says "head", --verify full asked
    Divergence(ReviewReason),
}
```

`tdy status` prints them; `tdy check` exits 1 on any of them; `dataset()` refuses to resolve.

### 8.3 The union provider

`DatasetFunc::call` (synchronous, inside planning, no inference, no network, no writes):

1. load and validate the target; load the lock; refuse if `lock.target_hash != target.target_hash()`;
2. expand the globs and compare to the lock's membership; any difference is a hard error naming the file and telling the user to run `tdy fit`;
3. per member: load `<file>.<name>.tdy.toml`; hash the data file and compare to the lock (blake3 of the whole file, exactly as `messy()` does today — the file is about to be read in full anyway); compare `sidecar_digest`; run G-V, G-S, G-M-from-stored-header, G-R;
4. build `StreamingTable::try_new(target.arrow_schema(), vec![Arc::new(DatasetPartition{…})])`.

**There is no `UNION ALL` and no generated SQL.** Every member's spec has passed G-S against the same target, so every member's batches carry an identical schema *by proof, not by coercion*. Positional transposition is impossible because every member's field 3 is `amount_chf` by proof; silent widening is impossible because a member whose type differs is refused rather than coerced.

**One partition, members in lock order, rows within a member in file order.** N files as N `StreamingTable` partitions gets `UnknownPartitioning(n)` with `EmissionType::Incremental` and reintroduces exactly the non-determinism `fn partition` (provider.rs:209-220) already returns a single partition to avoid — verified on the current binary: five runs of a two-file `UNION ALL` produced two distinct stdout hashes. This design makes the same trade the codebase already made once, consistently. `TDY_DATASET_PARTITIONS=n` opts out, documented as making row order unspecified, following the `TDY_NO_STREAM` / `TDY_LAZY_ABOVE_BYTES` precedent for execution knobs that do not participate in correctness.

**Per-member path selection, which is the fix for the fatal Excel flaw.** `can_stream` returns false for `Extraction::Excel` (verified at stream.rs:81-95, the `_ => return false` arm) and `stream::execute_with` bails outright on an unstreamable spec, so a partition built only from `SpecPartition`s cannot hold a workbook at all — and an all-or-nothing member predicate would materialise 200 CSVs at ~8× peak RSS because one xlsx joined the pile. `MemberSource` therefore decides **per member**, exactly as `MessyFunc::call` does today:

```rust
impl DatasetPartition {
    fn execute(&self, ctx) -> SendableRecordBatchStream {
        // members are consumed in order, one at a time, into a bounded mpsc(2):
        //   Streamed     -> stream::execute_with (O(batch) memory)
        //   Materialised -> engine::execute_batches, pushed, then DROPPED
        // peak scan memory = max over members, not sum over members.
    }
}
```

`run_query` still collects the whole *result* before writing — unchanged, and unchanged in badness.

### 8.4 How disagreement is resolved, in precedence order

| kind of disagreement | mechanism |
|---|---|
| different names (`Betrag` / `Betrag CHF` / `Amount`) | the target's `match` set, normalized, exactly one hit required; a real collision is an error naming both, fixed by a per-file `source` pin |
| different layout (title block, merged header, another sheet, `;` among `,`) | per-file frames — each member gets its own extraction and transforms; nothing is shared and nothing needs to be |
| different date format | no declaration needed: format is per file and invisible in Arrow; `date_order` disambiguates day-first vs month-first once, at dataset level |
| different numeric convention | `numfmt::infer` per file, must be definite and must agree with what the spec declares |
| different magnitude / unit | a human-declared `decimal_shift` per file, review-gated, plus G-D's magnitude refusal |
| a column missing entirely | `absent = "null"`, review-gated per file, with coverage reported |
| a column present that nobody declared | dropped, and reported as a proposal |

**What is deliberately not reconciled:** tdy compares each file to the target, never files to each other, except for the four cheap G-D checks. It does not check that member 5 writes `Zürich` and member 6 writes `zurich`, or that months do not overlap. Those belong in SQL the user writes; pretending otherwise would be a second, weaker contract competing with the mechanical one.

### 8.5 Cost

`tdy fit` is incremental: only members whose (file blake3, `target_hash`, `override_hash`) triple is absent from the lock are replanned. The memo collapses same-layout siblings into confirmations. Tier-1 fitting is CPU-bound and writes to per-file paths, so `fit_all` uses `futures::stream::iter(...).buffer_unordered(8)`; model calls stay behind a semaphore of 2 to keep the "sending N bytes to <backend>" notice legible and the spend predictable. Completion order affects nothing — member order comes from the sort.

---

## 9. What does not change

### 9.1 `messy()` is untouched

Same UDTF, same one-literal-path contract, same optional free-text hint, same `<file>.tdy.toml` sidecar path, same per-canonical-path cache, same `MemTable`/`StreamingTable` choice, same sniffer, same confidence-vs-threshold escalation, same prompt, same `PROMPT_VERSION = "infer-v3"`, same `--frozen` semantics. `DatasetFunc` is a **second** `register_udtf` in `provider::session`, and the two compose in one query:

```sql
SELECT s.month, s.region, s.amount_chf * r.rate AS eur
FROM   dataset('sales.tdy-target.toml') s
JOIN   messy('fx/rates.csv') r USING (month);
```

`check_spec(spec, path, limits, None, method)` is byte-for-byte today's two-gate behaviour, and the compiler enforces that no call site forgets the new parameter. A repo with no target file behaves exactly as today, down to the confidence warnings.

### 9.2 `--frozen` keeps its meaning and gains a stronger one

For `messy()`: unchanged. For `dataset()`: nearly a no-op, because `dataset()` never plans under any flag — it never sniffs, never calls a model, never writes. Fitting can spend money and change committed files; a `SELECT` should not do either. What `--frozen` adds for datasets is that it hashes every member unconditionally and refuses on any `Drift`, and `tdy query --fit` is *not* provided in the core slices (see Open Questions).

Reproducibility is strengthened from "same file, same answer" to "same files, same target, same acceptances, same answer" — which is exactly what the current fingerprint structurally cannot express.

### 9.3 The streaming executor

`stream.rs` remains plumbing, importing `build_column_at`, `compile`, `dedupe_names`, `promote_header_from` and `BATCH_ROWS` from `engine`. `decimal_shift` is a `ValueParsing` field, so it costs the streaming path nothing — it arrives through the shared `build_column_at`. `Transform::Constant` needs a real arm: `Stage::RowLocal` in `can_stream`, a widening step in `Plan::build`'s header derivation (stream.rs:830-848) and in the row pipeline, and an explicit `[constant, unpivot]` ordering decision. `tests/streaming.rs::assert_paths_agree` gains fixtures for both, and `tests/conform.rs` pins that both executors emit the schema `schema_of` predicted.

`fn partition` still returns one partition; the dataset union makes the same trade.

### 9.4 The one deliberate behaviour change

The `%z`-format-with-no-declared-timezone rule (4.5) rejects a small class of specs that are accepted today. This is a bug fix, not a target feature: those specs produce a `Timestamp(µs, None)` column that mixes true instants with wall clocks, which `messy()` users are exposed to as well. It fails loudly at `validate()`, on load, with a one-line fix (`timezone = "UTC"`), which `tdy validate --stamp` prints. It is called out in the README's changelog and is the only place this design makes an existing sidecar stop working.

---

## 10. Deliberately out of scope

- **FX and non-power-of-ten unit conversion.** `decimal_shift` moves a decimal point exactly. A EUR column landing on a CHF target is `GapKind::TypeUnreachable`, loudly. tdy has no unit system and no `unit` keyword — a label with no mechanical force reads to a reviewer as a verified property, and that is worse than nothing.
- **Arithmetic of any other kind.** No multiply, no offset, no derived column from two sources, no concat, no split, no coalesce. The vocabulary is five structural transforms plus `Constant`, a projection with rename, and `ValueParsing`.
- **A pivot (long → wide).** `unpivot` exists; its inverse does not.
- **Enum / categorical dtypes and `CHECK (x IN (...))`.** A target that wants `status ∈ {open, closed}` gets `utf8`. Half-enforcing a domain constraint (`utf8` + a `replace` list with no guarantee that an unexpected value errors) is worse than not offering it. Revisit as a `ValueParsing.allowed: Vec<String>` with a row-naming error, once the rest is proven.
- **`UNIQUE` / `PRIMARY KEY` / referential integrity.** Not a shape property; needs a full pass and a second contract. Refused, not half-enforced.
- **Named timezones.** Rejected in the target for the same reason as in a sidecar: DST cannot be resolved from a value.
- **Joins across datasets, incremental append.** A dataset is one relation from one target; joins are SQL's job and re-fit is the append model.
- **Per-column confidence.** Replaced by `Gap` and `ReviewReason`, which are strictly better: structured, addressable, and machine-readable.
- **A `--allow-missing` flag, a `--no-review` flag, a global magnitude off-switch.** Every softening is a declaration in a versioned file. A flag is a decision made once under time pressure and never reviewed.
- **Fuzzy matching of any kind.** No edit distance, no substring containment, no embeddings, ever. `nearest` exists only in error text.
- **Auto-fit from a query.** `dataset()` never plans. See Open Questions.

---

## 11. Incremental path

Effort estimates are given honestly. `src/` is 9,699 lines today; this design is roughly +3,000-4,000 lines across slices 1-6, and slices 1-4 are ~2,200 of them. Any smaller number would be wrong: `spec::validate` alone is 260 lines and `Target::validate` is comparable.

---

### Slice 1 — the conformance kernel (~700-900 lines incl. tests, 2-3 days)

**Lands:** `src/target.rs` (parse, `validate`, `arrow_schema`, `target_hash`, `norm`); `src/conform.rs` (`Mismatch`, `conforms`); `engine::arrow_field_of` with `schema_of` refactored onto it; `impl DType::validate()` extracted from `spec::validate` so both validators share it; `tdy check <TARGET> --against <FILE>`; **`tests/conform.rs`** as a hard prerequisite.

**What a user can do that they could not before:** point a checked-in schema declaration at a file whose sidecar they already have (from `tdy sniff` or hand-written) and get a three-way verdict in CI —

```
$ tdy check sales.tdy-target.toml --against exports/2025-01.csv
sales.tdy-target.toml: valid, 5 columns
exports/2025-01.csv.tdy.toml:
  CONTRADICTS  column 3 `amount_chf`: target declares Decimal128(14,2), the spec produces Float64
  UNFITTED     column 2 `region`: target declares NOT NULL, the spec is nullable
               (this sidecar was sniffed, not fitted; run `tdy fit` once it exists)
```

The three-way verdict is deliberate: `sniff::guess_columns` hardcodes `nullable: true` (sniff.rs:872) and gives money columns `decimal(38, s)`, so a two-way conform/doesn't-conform verdict against sniffed sidecars would be noise on day one. Separating "this spec contradicts the target" from "this spec was never fitted to one" makes the verb useful immediately: *"do the sidecars I already have still produce the four columns and exact types my downstream model expects?"* is a question nobody can answer today, and this is a working CI gate the day it ships.

**Tested by:** `tests/conform.rs` sweeping every fixture with a sidecar for `schema_of(spec) == execute(spec, path)[0].schema()` on both executors, and for `Target::arrow_schema()`'s invariance to the placeholder date/timestamp format; unit tests for `norm` (including `"Umsatz %" != "Umsatz"` and `"Betrag (CHF)" == "Betrag CHF"`), for `Target::validate`'s rejection of `norm`-colliding aliases, and for every `Mismatch` variant's sentence.

---

### Slice 2 — the header-origin fix and the `%z` rule (~250 lines, 1 day, no target involvement)

**Lands:** `RawTable::header_origin` populated by `ensure_header`, `promote_header_from` and the unpivot rebuild; `dedupe_names_recording`; `apply_transforms -> TransformStats`; the `Timestamp` `%z`/`timezone: None` rule in `spec::validate`.

**What a user can do:** nothing new directly — but two latent silent-wrong-value paths in the *existing* tool are closed (a spec that silently reads the first of two identically-named columns can now be diagnosed; a timestamp column that mixes instants and wall clocks is rejected), and slices 3-5 become implementable. Shipping it separately keeps the risky engine surgery out of the target work's blast radius.

**Tested by:** a regression test per closed defect (`tests/regression.rs`), written against the correct behaviour; `assert_paths_agree` extended to compare `TransformStats` between executors.

---

### Slice 3 — target-directed tier 1, single file (~900 lines, 3-4 days)

**Lands:** `src/fit/{mod,frame,bind,typing,score}.rs`; `sidecar::sidecar_path_for` (in this slice, not later — otherwise `tdy fit` writes into `<file>.tdy.toml` and breaks `messy()`); `check_spec`'s `target` parameter; gates G-V, G-S, G-M, G-P, G-X for one file; `tdy fit <TARGET> <FILE>`; `tdy explain <TARGET> <FILE>`. Also: exporting ~10 currently-private items from `sniff.rs` (`sanitize`, `DATE_FORMATS`, `TS_FORMATS`, `pick_delimiter`, `pick_sheet`, `find_record_arrays`, `header_verdict`, `four_digit_year_ok`, `PROBE_ROWS`, `TYPE_SAMPLE`) as `pub(crate)`, freezing them as an internal API. No model, no lock, no globs, no new spec operators.

**What a user can do:** fit one messy file to a declared schema and get either a plan that provably lands on it or a per-column reason why not — the twelve-file scenario's ten ordinary files all fit here, one file at a time, with hand-written `UNION ALL` in SQL.

**Tested by:** a `testdata/gen/` generator producing the twelve-file drifting-export set (including the two-`Betrag` file, the Rappen file, the merged-header workbook, the English export and the partial export); `tests/fit.rs` asserting the exact chosen `source`, dtype and `ValueParsing` per file; one test per `GapKind` asserting the *refusal* (especially: two literal `Betrag` columns → `Ambiguous`, not a silent bind); `tests/fixtures.rs` extended with exact sums.

**Owed obligation:** `sniff::guess_type` is re-implemented as `check_type` applied over the candidate types in preference order, so there is one type-inference engine and not two. `stream.rs` importing `build_column_at` rather than copying it is the precedent.

---

### Slice 4 — the dataset: lock, globs, `dataset()` (~800 lines, 3-4 days)

**Lands:** `src/lockfile.rs`, `src/drift.rs`, `src/dataset.rs`; `sqlscan::find_refs`; `DatasetFunc` + `DatasetPartition` with per-member `MemberSource`; the memo; `tdy fit <TARGET>` over all members with the coverage report; `tdy check <TARGET>`; G-D; read-time G-V/G-S/G-M/G-R in `DatasetFunc::call`; `--frozen` for datasets.

**What a user can do:** the vision. `SELECT month, region, sum(amount_chf) FROM dataset('sales.tdy-target.toml') GROUP BY 1,2` over twelve heterogeneous files, reproducible row for row, with `tdy check` as a CI gate that fails when December's export lands.

**Tested by:** `tests/dataset.rs` — five runs of the same dataset query produce identical stdout hashes (the determinism property `UNION ALL` fails today); a member added / removed / renamed / edited each produces the right `Drift`; a mixed CSV+XLSX dataset takes the per-member path and its peak RSS is bounded by the largest member (measured with `/usr/bin/time`, as CLAUDE.md requires); `tests/adversarial.rs` picks up the new fixtures automatically.

---

### Slice 5 — the two operators, the review gate, and `absent` (~700 lines, 3 days)

**Lands:** `Transform::Constant` and `ValueParsing::decimal_shift` (each: variant → engine arm → `validate()` rules → `can_stream`/`Plan::build` decision → `assert_paths_agree` fixture); `spec::model_json_schema()` and the `MODEL_FORBIDDEN` check; `absent = "null"` with coverage reporting and the stderr/Parquet banner; `from = "file_stem" | "file_name" | { match, template }` and the rename-invalidates-the-plan rule; every `ReviewReason` and `tdy fit --accept` / `--quarantine`; `--reset` guarding a `Manual` sidecar; `SPEC_FORMAT_VERSION = 2` with the read-side migration the codebase lacks (accept 1, lift in memory with the new fields defaulted; **write 2 only when the spec actually uses a v2 feature**, so a plain `tdy sniff` on the new binary does not break an old binary's CI).

**What a user can do:** land the Rappen file (declared, accepted, exact), land files missing an optional column, and get a `source_file` provenance column — which also makes the magnitude and coverage checks something the user can reproduce in SQL over the full data rather than over a 500-row probe.

**Tested by:** unit tests for `decimal_shift` × `check_grouping` × `parse_decimal` ordering, including the rounding interaction; `Constant` before `promote_header` and outside an `unpivot`'s `id_columns` rejected by `validate()`; a test asserting `model_json_schema()` contains neither `constant` nor `decimal_shift` (so a rename breaks the test); an acceptance expiring when each of the three hashes moves; a v1 sidecar loading unchanged on a v2 binary and a v2-feature sidecar being refused loudly by a v1 binary.

---

### Slice 6 — the model as frame proposer (~500 lines, 2-3 days)

**Lands:** `FramePlan` + `PlanTransform` + `TryFrom`; `FIT_PROMPT_VERSION = "fit-v1"` and the rewritten target-directed system prompt; the target and gap blocks in the user prompt with a real token budget; the fourth verification arm; `tdy fit --propose`; and — a prerequisite worth doing first — **a mock `SpecInferencer` in `infer.rs`'s test module**, so the retry loop has offline coverage for the first time (today it is exercised only by `tests/live_backend.rs`, which is skipped without an API key, meaning the loop that decides what a model may do is the least-tested code in the repo).

**What a user can do:** the awkward files — a layout tier 1's bounded product does not reach, a wide file that needs an `unpivot` — start landing instead of failing, and alias proposals arrive as pasteable TOML.

**Tested by:** the mock inferencer driving every arm of the retry loop offline, including a model that emits `Constant` (rejected with feedback) and one that returns a frame that still leaves a gap (retried, then bailed with the hand-edit instruction); `tests/live_backend.rs` gains a target-directed case.

**The model coming last is not an accident of sequencing.** It is this design's claim about where correctness lives, made visible in the commit order: a fully deterministic tdy already fits most piles, and every gate the model must clear exists and is tested before the model is allowed near it.

---

### Slice 7 — ergonomics (~400 lines)

`tdy target init`; `tdy schema --target` (the `schemars`-derived `Target` grammar, so "the schema is derived from the executing types" stays true for the new layer); `tdy check --verify full`; the `(bytes, mtime)` fast path for member freshness with `--verify-hashes` and unconditional hashing under `--frozen`; README `## Declaring a dataset` and the two new deliberate omissions (no unit system; `CHECK`/`UNIQUE` refused rather than half-enforced); `CLAUDE.md` architecture note.

---

## 12. Open questions for the author

1. **`tdy query --fit`.** `dataset()` deliberately never plans, which makes the interactive loop `tdy fit` then `tdy query`. Is that friction acceptable, or is an explicit `--fit` flag on `query` (routing through the async pre-pass, with a printed spend notice) worth reintroducing a dataset pre-pass for? I lean no: a `SELECT` that can spend money and mutate committed files is not a `SELECT`.

2. **Positional disambiguation syntax.** `source = "Betrag@3"` overloads a header name with an index. Alternatives: `source_index = 3`, or a `source = { at = 3, expect = "Betrag" }` table. The `expect` form is the safest (it fails loudly if the file's columns move) and the ugliest. Which?

3. **`PrecisionLoss` as a review reason vs. an error.** A target declaring `scale = 2` over a file carrying four decimals is silently rounded on every row by `parse_decimal`. This design gates it behind one acceptance per file. The invariant-focused critics argued for a hard error with an opt-in `round` policy per column. The difference matters for money.

4. **Should `verify = "full"` be the default at all scales?** It is the honest default and the file is already open — but a first fit over a directory of workbooks is O(total workbook bytes) because `checked_worksheet_range` materialises a sheet before the row cap applies. Is a size threshold (full below N bytes, head above, always recorded) better than a flat default?

5. **The `%z` timestamp tightening.** It rejects existing sidecars. Loud, correct, one-line fix — but it is the only place this design breaks `messy()`. Ship it in slice 2 as proposed, or hold it for a major version?

6. **`accepted_by`.** Keep it as a documented convenience label with git blame as the real audit trail, or drop it because a field that looks like a credential and is not is worse than no field?

7. **Lock merge conflicts.** Twelve months added one at a time will conflict in `sales.tdy-lock.toml`. A per-member directory (`sales.tdy-lock.d/2025-07.csv.toml`) removes the conflict class at the cost of one directory and a less diffable whole. Worth it?

8. **The second grammar.** `PlanTransform` mirrors five of six `spec::Transform` variants, with `TryFrom` making divergence a compile error. That is a deliberate second surface in a codebase whose discipline is one source of truth. Is the structural guarantee ("the model cannot name a value") worth the mirror, or is `model_json_schema()`'s tested projection plus the `MODEL_FORBIDDEN` gate sufficient on its own?

9. **`match` is inert for `fixed_width` and `lines`.** Those extractions name their own columns, so the real decision (offsets, capture groups) is asserted rather than observed. This design's answer is to refuse them in tier 1 and review-gate them everywhere else. The vision names fixed-width dumps explicitly — is a per-file `fields = [...]` declaration in the target's override block (a full field list, human-written) the right escape, or is that the target growing into a second spelling of `ParseSpec`?

10. **G-D's magnitude threshold.** 10× catches the Rappen class and nothing smaller. A partial final month or a member whose first rows are one large customer will produce false positives, and the remedy is a per-member acceptance rather than a global switch. Is a 10× default right, and should the check use the median over the *full* verified read (available under `verify = "full"`) rather than the probe?