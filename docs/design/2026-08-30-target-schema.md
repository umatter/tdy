# Declaring the dataset you want

**Status:** design, agreed in principle, not yet implemented.
**Date:** 2026-08-30

This document records where tdy is going and why. It came out of a design review
in which four independent designs were proposed against the current codebase and
each was attacked by two adversarial reviewers — one on the never-wrong-value
rule, one on architectural fit. All four scored 6–7 out of 10; none dominated, so
what follows takes the strongest spine and grafts from the rest. Where I overrule
that review, I say so and why.

---

## 1. The vision

In the author's words:

> an sql-like way to describe the clean dataset we want, and the input are
> potentially many messy files in different formats. the tool figures out how to
> get to that clean data output without writing code, just by combining existing
> super efficient tdy rust code for data manipulation steps.

Restated as obligations tdy takes on:

1. **You declare a target** — an ordered list of output columns, each with a name,
   a type, and a nullability. It is a checked-in file, reviewed in a diff, and it
   is the only statement of intent in the system.
2. **You point tdy at a heterogeneous pile** — `.xlsx`, `.csv`, fixed-width
   dumps, NDJSON; files that disagree about column names, layout, date order,
   numeric convention and magnitude.
3. **tdy plans, per file, toward that target** — choosing an extraction, a
   transform list, and for each declared column exactly one post-transform header
   cell plus a parse configuration. It composes operators that already exist in
   `spec.rs`. Neither you nor the model writes cleaning code.
4. **tdy proves the landing before it runs** — for every member file,
   `engine::schema_of(spec)` is field-for-field identical to the target's Arrow
   schema.
5. **tdy refuses rather than approximates** — a file that cannot reach the target
   is an error naming the file and the column.
6. **The result is one relation** — `SELECT … FROM dataset('sales.tdy.sql')`,
   reproducible row for row.

The governing rule is unchanged and outranks every convenience below:

> **tdy never silently produces a wrong value.** Ambiguity resolves to the right
> answer or a loud error naming the row — never a plausible wrong number.

A target *raises* the stakes on that rule rather than relaxing them. See §7.

---

## 2. The inversion, and why safety gets stronger

Today inference runs forward:

```
file ──sniff──► ColumnSpecs derived from whatever header cells exist
             ──► one confidence score
             ──► escalate to the model below the threshold
             ──► gate: validate() + dry_run over a 4 MiB prefix
```

Two things about that matter here. **The output shape is an output, not an
input** — nothing says which columns should come out, so nothing can check that
they did. And **the gate proves the wrong thing** — `check_spec` proves "this
spec parses the head of this file". A spec that parses the file into entirely the
wrong columns passes, is written to disk, and is queried.

Under a target the loop inverts. `sniff::finish` iterates over header cells and
emits a `ColumnSpec` for each; the planner iterates over *declared* columns and
resolves each to a header cell or refuses. That single inversion buys four things
the current architecture structurally cannot have.

**(a) The shape becomes provable, for free, before any I/O.**
`engine::schema_of` is already a pure function of `spec.columns`: it calls
`build_column_at(col, &[], 0)` over an empty value slice, touching no file. Its
result is therefore the schema of *every* batch the spec will ever emit, on both
executors, for all rows. Comparing it to a declared schema is a total,
deterministic, microsecond proof. `dry_run` has never proved anything of that
class.

**(b) Ambiguity stops being a discount and becomes a refusal.**
Today two date formats that both parse every probe value cost some confidence and
ship anyway; `numfmt::infer` returning `ambiguous` costs some more and ships
anyway. Against a declared type, "I could not tell whether `03/04/2025` is March
or April" is not a discount on a self-assessment — it is the statement that the
answer is unknown, and a `Date32` column with the wrong month is exactly the
plausible wrong number the rule forbids.

**(c) The escalation gate stops being a float.**
The new trigger is boolean: tier 1 either produced a plan that passes every gate,
or it left a specific gap. Confidence survives as reported evidence; it decides
nothing in the fit path.

**(d) The model's job shrinks from author to proposer.**
With columns declared, the model is never asked to invent a column, a type, a
magnitude or a value. It is asked for a *frame* — an extraction and a structural
transform list — and for alias proposals a human pastes in. That turns the
README's policy ("the model emits instructions, not data") from prose into a
type.

### The honest limit, stated once and repeated in the CLI

Shape is proved statically over the whole file. **Values are not.** A per-row
parse failure, a grouping violation, a two-digit `%Y`, a null in a NOT NULL
column — those are proved per row, loudly, naming the row, at execution. A gate
that let "conforms" sound like a whole-file guarantee would be a new way to be
quietly wrong. Every artifact and CLI line states which half it proved
(`verified = "head" | "full"`), and the default is to pay for the strong half on
a member's first fit.

---

## 3. Decisions

The review produced four viable designs. These are the calls, including where I
overruled it.

| # | Question | Decision |
|---|---|---|
| D1 | Target syntax | **SQL DDL**, parsed by `sqlparser` — *overrules the review* |
| D2 | Per-file repair | **In the sidecar**, not the target — *overrules the review* |
| D3 | File can't reach target | **Hard error.** Not partial, not skipped, not null-filled |
| D4 | Contract or proposal? | **Contract.** The planner may never add a column |
| D5 | Units (Rappen vs francs) | **No unit system.** A declared, review-gated `decimal_shift` plus a cross-member magnitude check that may only *refuse* |
| D6 | Membership | **From the lock**, never from a glob at query time |
| D7 | Model's role | **Last to land, and only a frame proposer** |

### D1 — the target is SQL, and this overrules the review

The design review put the whole target in TOML (`sales.tdy-target.toml`), on the
grounds that per-file overrides must express the full `ValueParsing` vocabulary
and DDL cannot do that cleanly. The premise is true; the conclusion does not
follow, because it conflates two different artifacts with two different authors
and lifetimes.

The vision says *sql-like*, and that is not decoration. `CREATE TABLE` is the
universal way to say "this is the shape of the data I want", it is what makes the
target legible to anyone who has ever used a database, and it is the product's
face. Burying it in TOML because the *escape hatch* needs TOML is letting the
exception design the rule.

It also costs nothing. `sqlparser` 0.54 is already in the dependency tree,
re-exported as `datafusion::sql::sqlparser` — the same parser DataFusion itself
uses. A `CREATE TABLE … WITH (…)` statement parses with the stock crate, and the
type vocabulary comes out of it for free.

### D2 — per-file repair lives in the sidecar

The review's `[files."2025-07.csv"]` override blocks are the right *capability*
in the wrong *file*. tdy's own philosophy already answers this:

> The structural cleaning … lives in an auditable, versionable *parsing spec*,
> never in your query. — README

A per-file `decimal_shift` is structural cleaning of one file. It belongs where
all the other structural cleaning lives: that file's sidecar, which is already
hand-editable, already reviewed in a diff, already expresses the complete
vocabulary because it *is* the vocabulary, and already has the workflow for
this — `tdy validate --stamp` exists precisely so a hand-written extraction
survives the next run, and `provenance.method = "manual"` already marks a spec
the planner must not overwrite.

Putting file-specific mess into the declaration of the clean dataset would
undo the separation the whole tool is built on.

**The cost, stated honestly:** overrides are spread across N sidecars rather than
centralised in one file, so "what did we have to hand-fix?" is a question you
answer with `tdy check` rather than by reading one document. `tdy check` must
therefore report which members carry a manual spec, and why. That is a reporting
obligation on the implementation, not a reason to move the data.

---

## 4. The target language

### 4.1 Syntax

A `.tdy.sql` file beside the data, in git. Stock SQL, parsed by `sqlparser`:

```sql
-- exports/sales.tdy.sql
-- The clean dataset we want. Hand-written, reviewed in git.

CREATE TABLE sales (
  month         DATE           NOT NULL,
  region        TEXT           NOT NULL,
  amount_chf    DECIMAL(14,2)  NOT NULL,
  discount_pct  DOUBLE             NULL,
  source_file   TEXT           NOT NULL
)
WITH (
  files      = '2025-*.csv, 2025-*.xlsx',
  exclude    = '*-entwurf.csv',
  date_order = 'dmy',
  verify     = 'full'
);
```

Globs resolve relative to the target file's directory, so the same command works
from anywhere in the repo. The target is referenced by explicit path — no name
resolution, no search path, no walk-up discovery, because "if it looks like a
name, try a search path" is a guess and `messy()` already establishes that the
argument is a path.

### 4.2 The type vocabulary is a projection of `DType`

```
TEXT / VARCHAR   -> DType::Utf8                     -> Utf8
BOOLEAN          -> DType::Bool                     -> Boolean
BIGINT / INT64   -> DType::Int64                    -> Int64
DOUBLE / FLOAT   -> DType::Float64                  -> Float64
DECIMAL(p, s)    -> DType::Decimal{precision,scale} -> Decimal128(p, s)
DATE             -> DType::Date{format: per file}   -> Date32
TIMESTAMP        -> DType::Timestamp{…, timezone}   -> Timestamp(µs, tz)
```

**strftime `format` is deliberately not declarable per column.** It is a property
of a file, it differs between the twelve exports, and it does not reach the Arrow
schema — a target carrying it would be lying about what it constrains. Twelve
files with twelve date formats land on one `DATE` column with no ceremony. This
is the biggest ergonomic win of basing conformance on the *Arrow schema* rather
than on comparing `DType`s.

What replaces it, because "let the planner pick a format" is unacceptable when
two formats both parse: a dataset-level `date_order` (`dmy | mdy | ymd`), which
is not part of the Arrow type and constrains only the planner's candidate set;
and a per-file `format` pin in that file's sidecar.

`timezone` **is** declarable, because it is part of the Arrow type. A named zone
(`Europe/Zurich`) is rejected by the target's validator with the same message and
the same reason as in a sidecar — a target is exactly where a user will try to
write one, so the refusal must live in the parser.

**There is no `unit` keyword.** A unit label whose failure mode is silence is not
a unit system, it is a comment with a keyword, and `unit = 'CHF'` on a column no
file was checked against reads to a reviewer as a verified property. See D5.

### 4.3 The worked scenario

`exports/` holds twelve monthly Swiss/German sales exports:

- `2025-01.csv` … `2025-06.csv` — `Datum;Region;Betrag`, windows-1252,
  `1'234.50`, `31.01.2025`
- `2025-07.csv` — same layout, but the amount column is `Betrag Rp.` in integer
  Rappen
- `2025-08.csv` — same, but with **two** columns literally named `Betrag` (net
  and gross)
- `2025-09.xlsx` — sheet "Umsatz": a title row, a merged band `Umsatz 2025` over
  C:E, then `Datum | Region | Betrag CHF`
- `2025-10.xlsx` — English locale: `Date, Region, Amount, Discount`, `1234.50`,
  `2025-10-31`
- `2025-11.csv` — a partial export with no region column at all
- `2025-12.csv` — normal, plus an extra `Kundennummer` the target does not
  declare

Ten of those fit mechanically. `2025-07` needs a declared, accepted
`decimal_shift`. `2025-08` is ambiguous and tdy refuses to choose. `2025-11`
cannot reach the target and is an error until a human excludes it or declares the
column optional. `2025-12`'s extra column is dropped, because `columns` has
always been a projection.

---

## 5. Architecture

### 5.1 New modules

```
src/target.rs    parse the DDL, validate, arrow_schema(), target_hash()
src/conform.rs   Mismatch, conforms(spec, target) -> Result<(), Vec<Mismatch>>
src/fit/         frame enumeration, binding, typing, scoring
src/lockfile.rs  membership, hashes, verdicts, acceptances
src/dataset.rs   DatasetFunc + DatasetPartition — the union provider
```

### 5.2 Nothing new is needed in the executor

This is the finding that makes the first slice small.
`ColumnSpec { name, source, dtype, nullable, parse }` already reads: *take this
file's column `source`, call it `name`, type it `dtype`*. That is precisely "map
this file onto that target". The vision needs a planner that fills `source` from
a declared target rather than from whatever the file happened to contain — not a
new execution engine.

Two operators are missing, and both land late (slice 5), not first:

- `Transform::Constant { name, value }` — for a `source_file` provenance column
  and for a declared-absent column.
- `ValueParsing::decimal_shift: i8` — an exact decimal-point move for the Rappen
  case. Not float arithmetic; not a unit system.

### 5.3 Conformance

```rust
pub fn conforms(spec: &ParseSpec, target: &Target) -> Result<(), Vec<Mismatch>>
```

Compares `engine::schema_of(spec)` field-for-field against
`target.arrow_schema()`: name, data type, nullability, in order. No file is read.
`schema_of` is derived by building each column over zero rows, so it cannot drift
from the code that types real data.

---

## 6. The planner

**Tier 1 (deterministic, no model) does most of the work.** For each declared
column, in order:

1. exact header match
2. normalised match (case, whitespace, punctuation; `Betrag (CHF)` == `Betrag CHF`)
3. declared aliases, if any

Zero candidates is a gap. Two or more candidates is a gap — **tdy does not
choose**. One candidate is a binding, and then the column's values are *checked
against the declared type* rather than having a type inferred from them. That
reuse matters: `sniff::guess_type` becomes `check_type` applied over candidate
types in preference order, so there is one type-inference engine rather than two.

**Tier 2 (the model) is asked for a frame, not a spec.** An extraction and a
structural transform list — never a column mapping that names a value, never a
constant, never a `decimal_shift`. The model's JSON Schema is a *projection* of
`spec.rs` with those variants removed, and a test asserts they are absent so a
rename breaks the build rather than widening what a model may say.

---

## 7. Failure semantics

### D3 — a file that cannot reach the target is a hard error

Not a partial load. Not a skipped file. Not a null-filled column with a warning.

**This is the argument that decides every other question here.** The point of a
target is that somebody writes `sum(amount_chf)` over twelve files. If file 7
loads with `amount_chf` silently NULL because its column did not bind, the sum is
short by a month, every row is well-typed, no error is raised, and the number is
plausible, stable and repeatable — so it survives review. Unlike a bad cell, an
aggregate launders it past any row a user could point at. Quietly dropping file 7
from the union is the same wrong number by a different route.

A declared target *invites* the user to trust the result, which is exactly why it
must not be able to be quietly incomplete.

Silence is also unnecessary, because the failure is cheap and early: conformance
needs no I/O, and `tdy fit` collects every gap across every member in one pass
rather than making the user discover them one query at a time.

### The three softenings, all declared

Each is written by a human into a versioned file and visible in a diff forever —
not a command-line flag, which is a decision made once under time pressure and
never reviewed again.

1. **A column declared optional and absent.** Fires only when the binder found
   *zero* candidates — never when candidates were plausible but ambiguous. A
   column is never nulled because the planner gave up. Coverage is reported:
   `discount_pct: supplied by 8/12 members`, on every query, and into Parquet
   metadata on write, because stderr is dropped by pipelines and the artifact
   must carry the fact.
2. **Quarantine.** A file may be dropped from the dataset — but the query still
   errors until a human accepts it, and every query over that dataset names the
   quarantined member.
3. **A hand-edited sidecar.** A human assertion about specific bytes, marked
   `method = "manual"`, which the planner will not overwrite.

### The review gate

> A plan whose acceptance rests on a **semantic judgement** rather than a
> mechanical proof does not execute until a human accepts it, and the acceptance
> is recorded against the file's hash and the target's hash.

Mechanical and auto-accepted: the binding was unambiguous and used the same
source as its siblings; the schema conforms; the mapping is injective; the values
parse.

Semantic and blocked: a unit shift, an affix strip, a declared scale that rounds,
an absent column padded, a source no sibling uses, an alias the model reached
for, rows discarded, a magnitude outlier.

Note the calibration this fixes: an exact, lossless `decimal_shift = -2` demands
acceptance, and so does choosing between two semantically different money columns
across months. Both are semantic. A design that gates only the first has its risk
calibration inverted.

### D4 — the target is a contract, not a proposal

The planner may never add a column. Discovery is reported, never merged.

1. A proposal makes the schema depend on the directory. `SELECT *` would change
   meaning the month an export arrives with an extra column; two runs of the same
   query against the same declaration would disagree; `--frozen` becomes
   unimplementable.
2. A proposal manufactures exactly the failure above — a column present in eleven
   of twelve files becomes NULL for one twelfth of rows with no diagnostic.
3. Widening is a verified hazard: DataFusion's union coercion silently turns
   Int64 + Utf8 into Utf8 and discards a declared type with no message. A
   contract closes that by proving every branch equal to one schema before the
   union exists.

Negotiation exists, out of band: `tdy target init` drafts a target from a pile
for a human to edit, and `tdy fit --propose` prints alias suggestions as pasteable
SQL.

---

## 8. Multi-file

**Membership comes from the lock, never from a glob at query time.** A glob
evaluated at query time means the answer depends on what is in the directory,
which is not reproducible and cannot be frozen. `tdy fit` resolves the glob and
records the members, their hashes and their verdicts; `dataset()` reads the lock.

A new file appearing is *drift*: `tdy check` reports it and exits non-zero,
`dataset()` refuses until `tdy fit` settles it. That is the CI gate — December's
export landing breaks the build, loudly, before it silently changes a number.

**The union is one partition, read in member order.** Because conformance proves
every member has an identical schema, concatenation is trivially valid. A single
partition preserves the row-order determinism that `--frozen` depends on, and the
streaming executor makes it cheap: peak memory stays bounded by the largest
member, not the dataset.

---

## 9. What does not change

- **`messy()` is untouched.** Single-file usage keeps working exactly as today.
- **`--frozen` keeps its meaning** and gains a stronger one: no inference, no
  network, no writes, *and* the member set is the one in the lock.
- **The streaming executor is untouched.** A dataset is N members through the
  existing per-file path.

---

## 10. Deliberately out of scope

- **No unit system, ever.** tdy will not convert currencies. What it offers is a
  declared, review-gated `decimal_shift` and a cross-member magnitude check that
  may only refuse.
- **No `CHECK` or `UNIQUE` constraints.** Half-enforced constraints are worse
  than none.
- **No joins between datasets.** That is SQL's job, and SQL is right there.
- **No writing back to source files.** tdy reads messy files; it never edits them.

---

## 11. Incremental path

Each slice is independently shippable and independently useful.

**Slice 1 — the conformance kernel.** `src/target.rs` (parse the DDL, validate,
`arrow_schema`), `src/conform.rs`, and `tdy check <TARGET> --against <FILE>`.
No planner, no lock, no globs.
*What you can do:* point a checked-in schema at a file whose sidecar you already
have and get a verdict in CI — *"do the sidecars I have still produce the exact
columns and types my downstream expects?"* is a question nobody can answer today.
The verdict is three-way — conforms / contradicts / never-fitted — because
`sniff` hardcodes `nullable: true` and gives money `decimal(38, s)`, so a two-way
verdict against sniffed sidecars would be noise on day one.

**Slice 2 — target-directed tier 1, single file.** `src/fit/`, `tdy fit <TARGET>
<FILE>`, `tdy explain`. No model, no lock, no globs, no new operators.
*What you can do:* fit one messy file to a declared schema and get either a plan
that provably lands on it or a per-column reason why not. Ten of the twelve files
land here, with hand-written `UNION ALL` in SQL.

**Slice 3 — the dataset.** `src/lockfile.rs`, `src/dataset.rs`, globs, drift,
`dataset()`, `tdy fit <TARGET>` over all members, `tdy check` as a CI gate.
*What you can do:* the vision.

**Slice 4 — the two operators and the review gate.** `Transform::Constant`,
`ValueParsing::decimal_shift`, acceptances, quarantine, declared-absent columns
with coverage reporting.
*What you can do:* land the Rappen file, exactly and accountably.

**Slice 5 — the model as frame proposer.** A target-directed prompt, the
projected model schema, `tdy fit --propose`.
*What you can do:* the awkward layouts tier 1 does not reach start landing.

> **Landed, with one addition the design did not foresee**: a deterministic
> tier *above* the model. A JSON document with several record arrays has
> finitely many frames, so the declared table can be tried against every one —
> exactly one fitting is a **proof by elimination**, no review needed; several
> fitting is refused as ambiguous, since two well-typed complete answers with
> different sums is exactly the guess this tool exists to not make. Only when
> the candidates cannot be enumerated (free-form text, a layout the sniffer
> cannot frame) is the model asked — for the frame alone, columns discarded as
> the sniffer's are — and its plan, though fully gated, carries a review
> reason: nothing proves a model's frame is the only reading, and that is a
> judgement by the book above. Proven ambiguities never reach the model.

**The model coming last is not an accident of sequencing.** It is this design's
claim about where correctness lives, made visible in the commit order: a fully
deterministic tdy already fits most piles, and every gate the model must clear
exists and is tested before the model is allowed near it.

---

## 12. Open questions

1. **`CREATE TABLE` or `CREATE TIDY`?** The former parses with stock `sqlparser`
   today. The latter reads better and needs a small pre-pass. Cosmetic, but it is
   the first thing a user sees.
2. **Rounding.** A target declaring `DECIMAL(…,2)` over a file carrying four
   decimals is silently rounded on every row. This design gates it behind one
   acceptance per file; the invariant-focused reviewers argued for a hard error
   with an opt-in per-column `round` policy. The difference matters for money.
3. ~~**Positional disambiguation.**~~ **Settled: no new syntax.** The sidecar can
   already address the second `Betrag` as `Betrag_2` (`dedupe_names` exists so a
   spec can name a duplicate), and the worry that "second column named Betrag"
   silently comes to mean a different column is unfounded: the acceptance is
   recorded against the file's blake3, so a regenerated export with reordered
   columns is *drift* — the acceptance expires and the spec is re-proved. The
   fingerprint already pins everything `{ at = 3, expect = "Betrag" }` would.
   `tests/dataset.rs::the_two_betrag_file_joins_via_a_sidecar_naming_the_deduped_column`
   is the proof, with net and gross distinguished by their sums.
4. ~~**`verify = "full"` as the default at all scales?**~~ **Settled: yes, and it
   is wired.** `fit` now proves the declared type on every row when the target
   says `verify = 'full'` (the default), and stops at `dry_run`'s bounded prefix
   when it says `'head'`. The corpus decided it: `testdata/late_surprise_*` is
   four shapes reduced from real exports where the head lies — a `station_id`
   that is digits for seven hundred rows and then `TA1309000067`. A plan typed
   from the prefix of those files is a plan `fit` calls fittable and the query
   then dies on. The O(total bytes) objection stands and `'head'` is its answer;
   what it does not justify is making the unsafe reading the default.
5. **Magnitude threshold.** 10× catches the Rappen class and nothing smaller. A
   partial final month will produce false positives, and the remedy is a
   per-member acceptance rather than a global switch.
6. ~~**Lock merge conflicts.**~~ **Settled: one file, conflicts resolved by
   regeneration.** The lock is derived state — every byte of it is a function of
   the target and the members on disk — so the resolution for *any* merge
   conflict is `tdy fit <TARGET>`, one command, never a hand-merge. A per-member
   directory would remove the conflict marker but not the underlying question
   ("which membership is right?"), and it would cost the single-file diff that
   makes a lock reviewable. The lock's header comment says this where the
   conflict happens.
