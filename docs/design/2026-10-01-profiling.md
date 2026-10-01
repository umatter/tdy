# Profiling: what a column holds, as evidence

*Design, 2026-10-01. Status: implemented 2026-10-01 (taxonomy K5) — `src/profile.rs`,
`tdy profile`, `.profile`, `p` in the workbench, the `profile` MCP tool. A profile is
evidence for a person; nothing in tdy reads one to change a spec.*

## 1. Why

Every judgement tdy leaves to a person — a `matches` clause, a `date_order`, whether
a refused column is worth declaring `TEXT` — is made today from the raw head: the
first rows of the file. The head lies in exactly the way the type verification
exists to catch (`testdata/late_surprise_*`): the six values that break a date
format are on row 4,000. K5 in the taxonomy names the missing piece as Potter's
Wheel's central idea: per column, how many values, how many distinct, and which
*shapes* they take — "94% look like `9999-99-99`, 6% like `99.99.9999`".

## 2. What a profile is

`src/profile.rs`, one function and plain data:

```rust
pub fn profile(path: &Path, frame: &ParseSpec, limits: Limits, opts: ProfileOpts) -> Result<Profile>

pub struct Profile { pub path: String, pub sheet: Option<String>, pub rows: u64,
                     pub complete: bool, pub columns: Vec<ColumnProfile> }
pub struct ColumnProfile {
    pub name: String,            // the file's own spelling (`header_origin`), not the sanitized one
    pub position: usize,         // 1-based, in file order
    pub non_empty: u64, pub empty: u64,
    pub distinct: Distinct,      // Exact(n) | AtLeast(cap)
    pub min: Option<String>, pub max: Option<String>,   // of the raw strings, by byte order
    pub top: Vec<(String, u64)>, // the 5 most frequent values; empty when distinct is AtLeast
    pub shapes: Vec<Shape>,      // every shape, most frequent first, capped (see below)
}
pub struct Shape { pub pattern: String, pub count: u64, pub example: String }
```

All of it serialises (`--json`), like `PileReport`.

**The table profiled is the framed raw table**: after the spec's framing transforms
(`transpose`, `skip_rows`, `promote_header`, a region window or sheet `range`) and
before any body transform or cast — the strings `fit` binds against, under the
names the file itself uses. Values are trimmed as every cast trims them; a value in
the column's `na_values` counts as empty.

**Which frame**: a fresh sidecar's spec when the file has one; otherwise the
sniffer's own frame, heuristics only — never the model, and never written to disk.
A refused member is the screen that most needs a profile and has no sidecar; this
is the rule `console::raw_head` already follows.

## 3. Shapes

`profile::shape(value) -> String`, a pure function over a trimmed value:

- an ASCII digit becomes `9`; a run of digits keeps its length up to 8 and is
  `9+` beyond (so `2025-01-28` is `9999-99-99`, `28.01.2025` is `99.99.9999`, and
  an account number does not make one shape per length);
- a letter becomes `A` (uppercase) or `a` (lowercase); a run of the same case
  longer than one collapses to `A+` / `a+` (`Bern` is `Aa+`, `CHF` is `A+`);
- whitespace becomes one space; every other character is kept as itself
  (`1'234.50` is `9'999.99`).

Shapes are counted exactly; a column with more than 64 distinct shapes keeps the 64
most frequent seen and reports the rest as one `(other)` row — free text has no
shape worth listing. At most 10,000 distinct shapes are tracked per column: a shape
first seen past that is counted only in `(other)`, and the column says
`shapes_complete: false` — then no renderer names a "most frequent shape", since the
true one may be among those not tracked, and the detail says the list is incomplete.

## 4. Bounds, stated in the output

- `distinct` and `top` track at most 10,000 distinct values per column (shapes have
  their own 10,000 bound, §3); past that
  the column reports `AtLeast(10000)` and no `top` (an approximate top-5 would be a
  number nobody can check).
- Text formats are read in one streamed pass — memory is O(columns × caps), not
  O(file) — through the same source `stream` reads. Excel and JSON materialise
  within `[limits]`, as everywhere.
- The whole file is read by default: a profile of the head is the lie this exists
  to remove. `--head N` profiles the first N rows, sets `complete: false`, and
  every renderer says so on its first line.

## 5. Where a person meets it

One library function, four doors, as for everything else:

- `tdy profile <FILE> [--sheet NAME] [--rows A-B] [--pointer /P] [--column NAME] [--head N]`
  (`--json` is the global flag). `--rows` names one stacked table the way a region
  member's title counts it — 1-based and inclusive, `--rows 6-9` — read in a fresh
  sidecar's frame when one reads exactly that block, else in the frame `fit` gives a
  block (`fit::region_frame`); with `--sheet` the rows are the sheet's. `FILE` may
  also be a member reference (`report.csv#2`, `book.xlsx#Q1#2`), which reads the
  block its fresh sidecar names and refuses without one. `--column '#N'` names a
  column by position, for a file with two columns of one name (`--column '\#N'` for
  a column literally named `#N`). With `--sheet`, `--rows` are the sheet's own A1 row
  numbers — what Excel and a sheet block's `range` show — refused outside the used
  range, and the heading names the A1 range read. `--pointer /q2` reads one record
  array of a JSON document holding several; without it the heading names the array
  read and the other candidates. A member of a pile carries its block's rows
  (`MemberReport.rows`, fitted or refused, dry run too), which is what `p` passes. Without `--column`: one line per column (name, non-empty/empty,
  distinct, min, max, most frequent shape with its share). With it: that column's
  top values and every shape with count, share and one example.
- the console: `.profile <file> [--sheet NAME] [--rows A-B] [--pointer /P] [--column NAME] [--head N]`, text
  identical to the CLI's (`commands::profile_text`), `Payload::Profile(Profile)`.
- the workbench: `p` in the File and Member contexts dispatches the same
  `.profile` line a person would type (the one-code-path rule), and the main pane
  shows `Context::Profile` — a table of columns; Enter on a column opens its detail
  (top values, shapes), Esc goes back. A member's profile is of that member: its
  sheet, and its region window when it has one — `--rows` (with `--sheet` for a
  sheet block) from the rows the report carries for every block member.
- MCP: a read-only `profile` tool, confined to `--root` like every other.

## 6. What it does not do

It infers nothing and writes nothing. No sidecar, lock or target is touched; no
sniffer decision reads a profile; a profile never lowers or raises confidence. It
does not suggest a `matches` clause or a date format — it shows what the column
holds, and the declaration stays the person's sentence to write. Out of scope:
histograms, cross-column profiling, sampling strategies, and any automatic use of
the shapes (the sniffer's own type verification already reads the whole file).

## 7. Tests

- unit: `shape` over the examples above, including the 8-digit boundary and a
  non-ASCII letter (`Zürich` is `Aa+`);
- `tests/profile.rs` over a new generator `19_profile.py`:
  `profile_mixed_dates.csv` (100 rows: 94 ISO dates, 6 dotted, the six after row
  60 so the head does not show them) pins the two shapes and their counts;
  a file with 12,000 distinct ids pins `AtLeast(10000)` and an empty `top`;
  `--head 10` on the mixed-dates file reports one shape and `complete: false`;
  a refused member of `testdata/drifting_exports` is profiled with no sidecar and
  none is written;
- `tests/console.rs`: `.profile` text equals the binary's;
- `tdy-tui/tests/workbench.rs` and `wb_render.rs`: `p` dispatches the same line a
  typed `.profile` does; the table and the detail draw the pinned numbers;
- `tests/mcp.rs`: the tool answers and refuses a path outside the root;
- memory: an `#[ignore]`d large-file test in the regions style, run by hand under
  `/usr/bin/time`, with the measured peak recorded in CLAUDE.md.
