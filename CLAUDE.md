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
cargo test --workspace --lib --tests     # 947 tests (skips doc-tests; see note below)
cargo test --test regression            # one suite
cargo test german_decimal_comma         # one test by name
cargo test --test adversarial           # ~120s: sweeps every fixture for panics/hangs
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
**CI runs `cargo clippy --all-targets -- -D warnings`, and clippy is not installed here** (no
rustup on this machine), so its lints are found by CI rather than locally. The one that has
actually bitten: `too_many_arguments` fires at 8, and `src/engine.rs::extract_delimited`
carries an explicit `#[allow]` for it. Group parameters into a context struct rather than
adding the allow — that is what `fit::Ctx` is.
CI also runs `cargo deny check advisories bans sources` (config in `deny.toml`; licenses
deliberately ungated — the reasoning is in that file). A new RUSTSEC advisory therefore
turns CI red: fix it by upgrading the crate, and keep tdy's direct `zip`/`quick-xml` pins
matching calamine's own so the tree builds one copy of each. `paste` (unmaintained, via
datafusion's proc-macros) is the one documented ignore; re-check it when datafusion is
upgraded.

On this machine plain `cargo test` ends with a spurious doc-test failure (`rustdoc` cannot
load `libLLVM.so...` — a toolchain install issue, not code); `--lib --tests` avoids it.
Rust ≥ 1.88 (DataFusion 46), a floor set by reqwest → url → icu_* and now also sat on
exactly by calamine 0.36 and zip 8, checked in CI. `[profile.dev] debug = false` is deliberate (slow builds) —
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

The layer's stance, learned the hard way from its own review: **anything a target declares
that tdy would not enforce is refused, not widened** — `SMALLINT`, `VARCHAR(n)`,
`TIMESTAMP(3)`, table-level constraints, a second statement in the file, an option set twice.
Unquoted identifiers fold to lowercase as SQL folds them (and as `sniff::sanitize` does), or a
target written the natural way could never match a column tdy produces. A zoned timestamp is
declarable via `WITH (timezone = '+02:00')`, because the offset is part of the Arrow type and
a spec carrying one would otherwise be unable to conform to anything a target can say.

**Slice 2 is in.** `src/fit.rs` is the inversion itself: `sniff` reads a file and emits
whatever columns fall out; `fit` is handed the columns you want and finds, per column, a
source that produces them — or a `Gap` saying why not. It reuses the sniffer for the *frame*
(delimiter, encoding, sheet, title rows, header rows — facts about the file) and throws away
only its columns. Types are **checked, not inferred**: the target already said what it wants,
so each candidate is built with `engine::build_column_at`, the executor's own function, and
one that type-checks here cannot fail differently there.

Two rules in there are load-bearing and were both got wrong first:

- **Match against `RawTable::header_origin`, not `header`.** `dedupe_names` renames the second
  `Betrag` to `Betrag_2` so a spec can address it; matching on that would find exactly one
  candidate and bind it silently, when the file has two columns by that name and does not say
  which is meant. `header_origin` keeps the file's own spelling so the collision stays visible.
- **`date_order` resolves a conflict; it does not prune candidates.** Pruning threw away
  `%Y-%m-%d` on a dataset declared `dmy` and made an ordinary ISO export unfittable, though an
  ISO date could never be confused with a day-first one. Ambiguity is detected exactly — two
  formats conflict only if they **disagree on a value in this file** — and only then does the
  declared order choose.

The corpus is `testdata/drifting_exports/`: nine files fit, three are refused, and
`tests/fit.rs` asserts the total (57'340.00 over 36 rows), because a planner that bound the
wrong column would still conform and still execute.

**Slice 3 is in.** `src/lockfile.rs` records what the globs resolved to (members, hashes) and
computes `drift`; `src/dataset.rs` is the `dataset('t.tdy.sql')` provider. Two things there
are the whole point:

- **Membership comes from the lock; `dataset()` never expands a glob.** A glob at query time
  makes the answer depend on the directory, so the same query returns a different number the
  morning after an export lands, with nothing to diff. New/edited/removed member and a changed
  declaration are all *drift*: the query fails naming the file, and `tdy fit` settles it.
- **`target_hash` fingerprints meaning, not bytes** — name, columns, types, nullability,
  matches, options. A comment edit must not void twelve proofs, or people learn to ignore the
  invalidation.

`tdy fit <TARGET>` with no file fits every member and writes the lock — **only if all of them
fit**. A partial lock would make "a dataset silently missing a month" the default outcome of a
bad afternoon.

A member is named relative to the target however its glob is spelled: `lockfile::member_dir`
rewrites an absolute or `./` glob directory that lies inside the target's directory (a plain
`exports/` keeps its spelling), and `--accept`/`.accept` name the argument the same way
(`member::relative_to_target`) before resolving it. An absolute glob used to make the lock's
members absolute paths, and `--accept` could then name them by neither spelling. `tdy draft`
writes a glob relative to the current directory (the MCP server's: its root) only when the
files are in it or below it, and absolute otherwise: a `..` ladder up to a shared `/tmp` was
relative to where the draft ran and named no file from a target written elsewhere.
A lock written before this, holding absolute member paths, is drift once: `dataset()` refuses
it, naming each member as not in the lock, and the next fit re-plans the members under their
relative names and asks for their acceptances again.

The union is one partition read in lock order: conformance already proved every member has an
identical schema, so it is a concatenation with nothing to coerce (an ordinary `UNION ALL`
would let DataFusion widen Int64+Utf8 to Utf8 in silence), and a single partition keeps row
order deterministic for `--frozen`. A member's sidecar is re-proved against the target on
every query, because a sidecar is hand-editable and that check costs no I/O.

**Slice 4 is in.** `ValueParsing::decimal_shift` moves the decimal point on the *digit string*
(`engine::shift_decimal_point`) — never `* 0.01`, because the only reason it exists is money
and a float would reintroduce the error `decimal` was chosen to avoid. `validate()` refuses it
on non-numeric columns and refuses a positive shift on an integer one (it would produce a
fraction the column cannot hold).

It is never inferred. The planner refuses the Rappen file; a human writes that spec by hand in
the sidecar and marks it `method = "manual"`, which `tdy fit` then leaves alone — but still
proves, with conformance and a dry run, exactly as it proves a planned one.

**The review gate** is the sharpest line here: `fit::review_reasons` finds steps whose
acceptance rests on a *judgement* rather than a proof, `Member { review, accepted }` records
them, and `dataset()` refuses an unaccepted one. Everything the planner does is mechanically
checked and none of it can establish that a column of integers is francs rather than Rappen.
An acceptance carries over while the member's bytes and the declaration are unchanged — asking
the same question every run would train people to answer without reading — and drift expires
it, because it was about those bytes.

**Constants and declared-absent columns are in.** `Transform::Constant` adds a column the
file does not have (`""` = nulls); it may only add, never shadow, and it is never inferred.
The split that matters: a target column declared `if_missing = 'null'` (nullable only,
refused otherwise) lets the planner null-fill a file that lacks it with a note and **no
review** — the declaration in the reviewed `.tdy.sql` is the authorisation — while a
hand-written constant *value* ("November is all Ticino") is data the file never contained
and gates behind `--accept` exactly like `decimal_shift`. `if_missing_null` is part of
`target_hash`, so declaring or retracting a fill voids the proofs.

**Declared readings are in (2026-10-02).** Two column options name a reading no value can
establish: `year_pivot = 'N'` (the century of a two-digit year) and `epoch = '<unit>'` (a
count: `seconds`/`milliseconds`/`microseconds` since 1970, or `excel_days`, a spreadsheet
serial — `EpochUnit::ExcelDays`, parsed from the digit string by `engine::excel_serial_micros`,
the fraction rounded to the millisecond since a spreadsheet writes ~15 significant digits,
serials below 61 or past 2958465 refused naming the row before any arithmetic, a time of day
on a DATE refused rather than dropped).
Both are DATE/TIMESTAMP only, refused when set twice, refused together, and part of
`target_hash`. **A declaration authorises a reading**: undeclared, `fit` never tries a `%y`
format or an epoch (a column only `%y` reads is a gap naming the option); declared,
`fit::dated` adds the two-digit formats — every day/month/year order per separator, or a
short list silently chooses the order — with that pivot (`date_order` settles a conflict as
for `%Y`), or binds through the epoch unit as the *only* reading, and the binding carries a
note and **no review** — the reviewed `.tdy.sql` is the authorisation, as for `round`. That
exemption is `fit::review_reasons_for(spec, target)`, used wherever a target is in hand
(`fit`, `fit_pile`'s sidecar reuse, the console's `.accept`): it drops a `%y` reason only when
the target column declares exactly the pivot the spec reads, so a hand-written `%y` under no
or another declared window still waits on a person. Likewise **any `epoch` in a spec is a
review reason** — `format = "%s"` alone included, which is epoch seconds
(`spec::effective_epoch`, the one answer review and conformance both use) ("`datum` reads integers as time (epoch = excel_days), which no value in the
file states"), dropped only when the target column declares that very unit — so a
hand-written epoch sidecar that joined a pile silently before 2026-10-02 now waits for one
`--accept` (or the declaration); a `messy()` query is unaffected, review being a pile
concept. And a declared `epoch` is **enforced**, not advised: `conform::conforms` reports a
`Mismatch::Reading` for a member column read with another unit or none, so a hand-written
sidecar CONTRADICTS and an edited one is refused by every `dataset()` query, naming both
units. `draft` declares neither. The sniffer's
serial-date note now names the declaration instead of SQL.

**`tdy draft` scaffolds a target from a pile** (`src/draft.rs`): sniffs every file, groups
columns by sanitized name, carries verbatim spellings as `matches`, merges types by widening
(Int+Decimal → DECIMAL, anything+Utf8 → TEXT with the conflict named), counts per-file
presence, and hints `date_order`. It deliberately does NOT merge synonyms (`datum`/`date`
stay two visible columns) — the declaration is where a human states intent, so the draft
makes each remaining judgement a one-line edit instead of making it. The emitted SQL always
parses (`Target::parse`), and `tests/draft.rs` pins the round trip: over a pile with one
vocabulary, the *unedited* draft fits every file it was drawn from. A drafted DECIMAL whose
scale is above 6 (the sniffer's cap for money it knows by shape, so the scale came from a
currency-formatted cell holding a float) also declares `round = 'half_away'`, with a comment
calling the scale float noise and naming DOUBLE as the other edit — a later row one place
longer would otherwise refuse the unedited draft (`draft_float_money.xlsx`, generator 18).

Swept against the real corpus (draft → fit, scratch copies, 170 CSV piles + 31 multi-sheet
workbooks): every identical-header pile fit unedited (6/6); every overlapping-header pile
needed exactly the declared-absent/exclude edits the comments point at (12/12); and the
152 heterogeneous piles (unrelated files sharing a directory) now get a **grouping note** —
draft clusters files by column-name overlap (Jaccard ≥ 0.5, greedy) and says "these look
like N datasets, draft each group separately" (fires on 147/170 piles, silent on the 23
plausible datasets). Real multi-sheet workbooks: 15/31 fit unedited, 16/31 refused as
ambiguous frames — correctly, they are one-sheet-per-year/state books where many sheets
produce the drafted table. No crashes, no wrong answers, in either sweep.

**Slice 5 is in — frames, in two tiers.** The corpus's biggest gap was JSON documents with
several record arrays ("N candidate record arrays", thousands of files). Tier one is
deterministic elimination over **enumerable frames**, in two domains: JSON record arrays
(`sniff::json_record_pointers`) and workbook sheets (`sniff::frame_excel_sheet` — each sheet
framed independently, because a title row is a fact about a sheet, and the cover page in
`sheet_frames_one_fits.xlsx` is deliberately the *biggest* sheet so ranking alone cannot
settle it). `fit` tries the declared table against every candidate — exactly one fits =
**proved by elimination** (note, no review); several fit = `FitError::AmbiguousFrame` naming
them and the sidecar field that settles it (two well-typed different answers; the `two_fit`
fixtures differ only in their sums, which is the point); none = the ordinary gap report for
the ranked candidate. The elimination pass runs cheap gates only (`Rigour::Gates`); the sole
survivor pays the whole-file verification once.

Tier two is `fit::plan` (async, what the CLI now calls): when deterministic planning fails
with something a frame could cure — `Unreadable`, or gaps that are all
`NoCandidate`/`Untypable` — and a backend is configured, the model is asked for the *frame
only* (`infer_spec` reused whole; its columns are discarded exactly as the sniffer's are).
Everything downstream is proved, but nothing proves the model's frame is the only reading,
so the plan carries a review reason naming the model and the frame, and `dataset()` refuses
it until `--accept`. A proven ambiguity (`AmbiguousFrame`, ambiguous separator/date) never
reaches the model: declarations settle those. Provenance records `method = "llm"` and the
model. `tests/frame_proposer.rs` covers both properties offline against a hand-rolled
loopback mock; `tests/live_backend.rs` has the paid version.

**The pile as data: `src/report.rs` + `--json` + `tdy mcp`.** The fit-the-pile orchestration
(sidecar reuse, acceptance carryover, all-or-nothing lock) lives in `report::fit_pile`,
returning a serializable `PileReport`; the CLI's text is `render_pile_text` over it —
line-compatible with the old output because tests read it — and `--json` (global flag; also
on sniff/check) prints it raw. Each refused member carries `Problem`s with machine-usable
fields (kind, column, want, tried, the file's header, the remedy in `message`).
`src/mcp.rs` serves the same surface over MCP stdio (`tdy mcp --root DIR`): hand-rolled
newline-delimited JSON-RPC (a page of protocol; an SDK would cost a dependency tree and an
MSRV negotiation), handlers call only the non-printing lib functions (stdout is protocol),
every path is confined to `--root` via canonicalisation (`fileio::confine`) — tool args,
refs inside SQL, the members a target's globs resolve to, and lock member paths, enforced
where each file is *opened* (`MessyFunc`, `dataset::resolve`, `fit_pile`), not only in the
pre-parse, because the pre-pass and DataFusion tokenize the SQL independently —
and **acceptance is refused unless the server was started with `--allow-accept`**: the
review gate's whole meaning is that a human judges, so delegation to an agent is the
operator's explicit act, never a default. `tests/mcp.rs` drives it as a subprocess.

**The terminal UI is `tdy-tui`, a workspace member** (`tdy` stays the root package so the
published crate keeps its small tree; **CI must say `--workspace`** or cargo builds and lints the
root package alone). The workbench is now its *only* mode — the classic target-only review
screens (`app.rs`, `ui.rs`, and their `tests/render.rs`) were deleted in slice 3 once
Pile/Member/Evidence moved behind the workbench's own `Context`; nothing in the crate reaches
them any more. `remedy.rs` survived that deletion unchanged: it edits the target **textually**
and re-parses to prove the edit took effect (never re-serialises the AST, which would delete
the human's comments), and the workbench's confirm overlay calls it exactly as the classic
accept screen did. The parity tests that used to pin the classic screens' behaviour live on
under the same names inside `tdy-tui/tests/workbench.rs` and `tdy-tui/tests/wb_render.rs`, now
asserting on the workbench's `Context::Member`/`Context::Evidence` instead.
Three rules are load-bearing: acceptance is reachable *only* from the evidence screen and only
one member at a time (`a` elsewhere does nothing, and the gate is the console's own two-step
`.accept` grammar, not a UI-only shortcut); every target write is preceded by a shown diff; and
the remedy menu is ranked by `--propose` (which of the file's columns can actually produce the
declared type) rather than listed in file order — which is why every `.fit` the workbench
dispatches carries `--propose`: the launch line (`.fit T --dry-run --propose`, in
`main::dry_run_target_mode`), `f`'s refit, and `f` on a browser target. Without it
`MemberReport.proposals` is empty and the menu falls back to the file's header in file order.
The "what tdy sees" panel is the **raw head**, read as bytes and shown as text
(`console::raw_head`) — the file's own header spelling and unparsed values, which is what a
`matches` clause needs — and it needs no sidecar, so a refused member (the one whose screen
most needs it) gets it too. For a **workbook** member, `raw_head` also carries `grid`, a
bounded 20x12 read of one sheet (xlguard-bounded, extraction's own `render_cell`), so that
panel shows the spreadsheet's own header spellings and raw values, not just its shape — the
first sheet by default, `[`/`]` in the workbench (File and Member views alike, clamped not
wrapping) or `--sheet NAME` on `.show` stepping to another; "a tab per sheet" is done in this
keyboard form, not as rendered tabs, since two keys already page the one grid the panel has
room for. The remedy menu still falls back to `Problem.header`, which it is built from
regardless of which sheet the panel shows. That read is
**bounded, and says so**: `sheet_grid` is the only place that knows both the cap and the
sheet's true extent, so it appends a `…` cell per row when it clips columns and a final `…`
row when it clips rows — a window shown as if it were the whole sheet is how someone writes a
`matches` clause for a column they never saw. Both renderers also name the sheet the grid came
from (`grid of sheet "N":`), since the panel lists every sheet directly above it. **The workbench draws real widgets, from a palette by meaning** (2026-09-06):
the pile is a ratatui `Table` whose columns are the target's declared columns, each
member's binding under the column it supplies, and `PileReport` now carries what
that needs — `columns` (name, SQL type, nullability, `matches`) and `drift` (the
lock's disagreement with the directory *before* the fit, cleared when a fresh lock is
written), both skipped in JSON when empty. Green fits, red GAP, yellow REVIEW and
"no lock"/"dry run", in the pile, the member view and the browser alike; the selected
row is reversed everywhere, as the browser's `List` already was. The member view's
raw head is sized to its content, its header line colours `--propose` candidates green
and problem-implicated cells (the two `Betrag`s, a long-form holder) yellow, and a
problem renders from `Problem`'s *structure* (tried names and header one per line),
not its CLI prose. Preview, query results (types under the names), the workbook grid
and evidence rows are `Table`s with numerics right-aligned. Two couplings to respect:
`wb_ui::pile_header_rows` is the arithmetic `Workbench::follow_pile_selection` uses,
so the pile's head lines and the table's header row are counted in one place; and
`main_scroll` stays the single scroll offset (the table is fed pre-sliced rows rather
than a `TableState`, which would keep its own). Slice 2a (2026-09-07) added the frame's
chrome: help and confirm are floating popups (`popup_rect` + `Clear`, one cell inside the
pane), help leading with the keys of the *current scope* (`HELP_KEYS` carries a `Scope`;
`current_scope` reads focus and context); the header names root (`~`-shortened, elided
from the left so the target, its lock state and the backend stay whole — `Workbench::backend`
is set by the runtime), plus a DRY RUN badge; the status line spins on `Workbench::tick`
(advanced by the runtime once per loop, zero in tests) and `Workbench::note` strips the
root from paths; the console wraps lines by character (`wrap_line`) and marks a scrolled
transcript; borders are rounded and the browser draws no right border, so the right
column's blocks draw the seam's junctions (`Seams`, `seam_set`) — the main block has no
bottom border because the console's top border is that line, which is why
`main_inner_rows` subtracts one border row for it, not two. Slice 2b: `g`/`G` jump between
members that `needs_attention` (anything not `fits`), `/` toggles `Workbench::pile_filter`
(`PileFilter::Problems` hides the rest; arrows move among `visible_pile_rows`, and the
filter is a workbench preference, not part of the context); a multi-sheet workbook's pane
title names the sheet on show (`sheet 2/3 "Umsatz"`); and a column note in the spec
summary (`column \`name\`: …`) shows the first three raw values of that column under it,
read from the raw head beside it (`decision_examples`: the grid by header cell, a text
file by the extraction's delimiter) — never a guess at which column was meant, so a
column it cannot find shows nothing. **The edit loop** (2026-09-07): `Workbench::after_edit(path, ok)` turns a successful `$EDITOR`
round-trip on the pile's target or a member's sidecar (`watched_files`, sheet sidecars included)
into a dispatched `.fit <target> --dry-run --propose`, through the console like any shortcut;
the runtime stats `watched_files` once a second (`changed_since`, re-baselined after every
command so a fit's own writes are not news) and a change made elsewhere lands in the status
line via `notice_change` as "… changed on disk — f refits", never as a refit nobody asked for.
The draft-merge view was decided against (design page §5). `tdy::progress`
(owned `Sink`, so a fit can run on a spawned task) is what lets the status line narrate; a
transient remark must use `Msg::Note`, never `Msg::Progress`, or the UI stays busy forever and
takes no keys but `q`. The same discipline now reaches query results too:
`progress::Event::Note` carries a low-confidence spec warning from the query pre-pass through
the sink instead of being printed by its caller directly, so the workbench's status line
narrates it and the CLI's `stderr_sink` prints the same text it always did — one event type,
one place either frontend has to handle it.

**The workbench is `tdy-tui` — `browser.rs`, `workbench.rs`, `wb_ui.rs`.**
`browser.rs` is a tree over `console::list_dir` (dirs first, companions folded, confined at the
root); `workbench.rs` is the frame's pure state machine — `Key` in, `WbAction` out, no I/O —
and owns focus (`Tab` cycles console → browser → main), the main pane's `Context`
(`Empty`, `File` with or without a sidecar, `Query`, `Pile`, `Member`, `Evidence`), and every
keyboard shortcut; `wb_ui.rs` reads that state and changes nothing, so `tests/wb_render.rs`
asserts on real drawn text via `TestBackend`. `main_scroll` is **one** offset shared by every
context that scrolls (all of them now), so it resets on every context *change* — Pile↔Member,
into Evidence, into a fresh Query — while a same-path `show_file` update keeps the scroll the
user set. Carrying an offset across is not cosmetic: a paged-down pile then opens a short raw
head past its end, and a blank pane reads as an empty file. **One code path**: a browser or main-pane
shortcut (`s` → `.sniff <selected>`, `f` on a target → `.fit <it>`, `t` → `.edit` the target,
`d`/`D` → mark/`.draft` the marked files, Enter on a directory → `.cd`, Backspace → `.cd ..`)
never acts directly — it produces the identical `WbAction::Dispatch(line)` a typed line would,
so the two cannot drift and the console scrollback is a complete, literal audit trail of the
session. `tests/workbench.rs` pins this as an equality, not a description
(`shortcut_and_typed_line_produce_identical_dispatches_after_cd` and its sibling). The
browser's status column is its own compact glyph vocabulary (`✓ 0.95`, `✗ stale`, `no lock` /
`locked` / `drift (N)`) — deliberately not `render_listing`'s long form (`sniffed 0.95
(heuristic)`, `stale`, …), which stays what `.ls` prints; a 26-column pane has no room for the
long form, and the ruling this slice made is that the browser and `.ls` are allowed to say the
same fact two different ways. **A single background task owns the `Session`**
(`spawn_console_worker` in `tdy-tui/src/main.rs`): the UI sends lines over an unbounded channel
and the worker runs them one at a time, in arrival order — a plain queue standing in for the
console's own one-statement-at-a-time serialization, so two shortcuts fired before the first
finishes cannot run out of order or against two different `SessionContext`s. A `.tdy.sql`
target — named, or the one discoverable file when none is named — opens the workbench rooted
at the target's directory with an initial `.fit <name> --dry-run`: a review tool must not write
on open, so the launch fit never touches the lock or carries over an acceptance, and `f`
refits for real. Anything else (no target and none or several discoverable, a directory, or a
data file) opens the plain workbench, rooted at the directory or, for a data file, its
directory, showing it. `.abort` — Ctrl-C on an empty console prompt dispatches it — discards a
buffered SQL statement, in the workbench exactly as in the plain console; a staged remedy diff
is cancelled separately and locally, with Esc or `n` on the confirm overlay.

**The console is `src/console/`** (`parse` — pure grammar; `Session::run` — one line in, an
`Outcome { echo, text, payload, ok }` out; `line` — the prompt's editor as a state machine;
`repl` — the TTY loop and the piped batch runner). `tdy` with no subcommand and both stdio ends
a TTY now opens the workbench when `tdy-tui` is on `PATH` — the design's §5 end state, landed
with slice 3 (2026-09-02): tdy-tui without a named target IS the workbench, so there is no
longer a wrong place to land someone. Without `tdy-tui` installed it falls back to the console
with a one-line stderr note; piped stdin always batches; `tdy console` still forces the plain
console explicitly, and `tdy ui`/`tdy-tui` remain its other doors. Its `text` is the CLI's text
because `src/commands.rs` produces both — the CLI arms print what `commands::*_text` return,
and `tests/console.rs` asserts the console's `.fit`/`.sniff`/`.draft`/query text equals the
binary's. A completed `CREATE TABLE` statement never reaches DataFusion (which cannot execute DDL):
`Session::create_target` parses it with `Target::parse` and writes it verbatim as
`<table>.tdy.sql` in the session's cwd — refusing to overwrite an existing target unless
the exact statement is repeated (`pending_ddl`, `.accept`'s two-step rule; any other
dot-command or blank line resets it). The query context is deliberately **not** kept across statements (a re-sniff between
two queries would serve a stale `MemTable`). `.accept` is two steps in the session itself
(`pending_accept`), and any other command in between resets it — `.abort` included, since it is
a command like any other, not a special case carved out for the review gate. `evidence` lives in the library
(`src/evidence.rs`); `tdy-tui` no longer re-exports it — every caller inside `tdy-tui` reaches
it as `tdy::evidence` directly.

The console's raw-mode line editor (`src/console/line.rs`, `src/console/repl.rs`) recalls
history by prefix when something is typed (fish's Up: `.sn` then Up skips every `.fit`;
a prefix nothing starts with leaves the draft alone) and plainly when nothing is. It needs
`crossterm` for key events, so `crossterm` is a **direct** dependency of `tdy` itself — root
`Cargo.toml`, not only `tdy-tui/Cargo.toml`. `ratatui` alone stays `tdy-tui`-only, and that
is what the root `Cargo.toml`'s workspace comment now says.

A relative path inside console SQL — `messy('x.csv')`, `dataset('t.tdy.sql')` — joins onto
the **session's** cwd, the one `.cd` moves, not onto the root. That is
`provider::Confinement { root, base }`: `base` is where a relative reference is joined,
`root` is the whole of what is allowed, and `Confinement::at_root` (the MCP server, every
older caller) makes them the same directory. Splitting them is not a convenience — with
`base` fixed at the root, a `.cd sub` followed by `SELECT ... FROM messy('x.csv')` read a
same-named file at the root instead of the one `.ls` had just listed, and exited 0. The
confinement is enforced in `MessyFunc`/`DatasetFunc`, where the file is opened;
`run_sql`'s `RestoreCwd` exists because `prepare_specs` reaches the same file by ordinary
relative I/O against the *process's* directory, and the two routes have to agree.

**The shape slice is in (2026-09-06).** `docs/design/2026-09-06-shape-slice.md`
is the design and the record of where it was wrong. Seven operators closed the
operator catalogue's tier-1 gaps: `split_column` (delimiter, character
positions or a regex's capture groups — total by construction, since it stops
after `into.len()` parts, and short values are an error unless `on_short =
"null"` declares the tail optional), `transpose` (no options, before
`promote_header`, refused on a truncated table because every unread row would
have been a *column*), `source_name` (a column read off the file's own path —
derived rather than told, so no review gate), `WITH (provenance = 'true')` on a
target (`_member` and `_row`, opt-in and part of `target_hash`), a per-column
JSON `pointer`, `epoch` scales, and `fill_down`'s `direction`. None of them is
ever inferred; `transpose`'s reason is the sharpest — its signature is shared
exactly with a wide report that wants `unpivot`, so the sniffer notes the shape
and names both.

`docs/design/2026-09-05-munging-taxonomy.md` is the survey those gaps came from:
110 operators, one canonical name each with every other system's synonyms, and a
verdict per operator. Read it before concluding tdy is missing something — and
before concluding it is not.

**Compressed inputs are in** (2026-09-07, option B of `docs/design/2026-09-06-compressed-inputs.md`).
`fileio::materialize(path, ceiling)` decompresses gzip, zstd, bzip2 or xz — by magic bytes, never
extension — into a process-lifetime cache (`$TMPDIR/tdy-<pid>/<blake3>/<inner name>`, one copy
per file, removed by `fileio::clear_cache()` at every binary's exit), bounded by
`[limits].max_decompressed_bytes` (default = `max_file_bytes`) *before* the copy exists. Every
byte reader goes through it — `read_all`, `read_head_tail`, `stream::open_input`, and
`engine::open_workbook` (the one door to calamine, `xlguard::preflight` inside it) — so sampling,
both executors and drift see a real file with byte offsets. lz4 and zip stay refused by name.
The sidecar fingerprints the **compressed** bytes and records `compressed = "gzip"`; the format
guess strips the compression extension. The four decoders were already in the tree via `zip`,
so the direct dependencies cost no compilation. The fixture family is `16_compressed.py`.
The page's earlier deferral text follows for the record:
a `.gz` has no byte offsets, so `sample::build`'s head+tail sampling has no
meaning — the file is now *refused* by magic bytes rather than read as mojibake;
the check is inside `fileio`'s two readers and the streaming opener, not at call
sites, because the first cut at call sites missed the executor that actually runs.

**Workbook members are in (2026-09-07).** `docs/design/2026-09-07-workbook-members.md`.
A member is `(path, sheet: Option<String>)` — `src/member.rs`'s `MemberRef`, two fields in
the lock and the sidecar fingerprint, textual `book.xlsx#Q1` only where a person reads or
types it (report, `_member`, `--accept`, `.accept`, `exclude`), and a typed reference is
**resolved against the members that exist** (`MemberRef::resolve`), never split by rule.
A sheet member's sidecar is `<file>#<sheet>.tdy.toml` (`sidecar::load_member`/`save_member`;
the old forms are the `None` case). `fit_pile` asks `fit::discover_sheets` which sheets pass
the gates: none or one keeps a plain member (the `one_fits` fixture and every existing lock
are unchanged); several become one member per sheet, each fully fitted by `fit::fit_sheet`
and carrying a note naming the sheets that did not fit. The single-file `fit()` still refuses
several fitting sheets with `AmbiguousFrame` — "the spec for this file" has no single answer;
the pile is where "which members?" is asked. **Drift is per file**: the file's hash covers
every sheet, so a changed workbook is one `Changed` and the refit rediscovers the sheet set;
`Duplicated` is per (path, sheet). `dataset()` never lists a workbook's sheets itself.
Regions (several tables stacked in one text file or sheet) landed 2026-09-08 — see
**Regions are in** below.
Follow-ups from its review, landed the same day: `MemberRef::resolve` returns `Err(candidates)`
when a reference could mean two members (a `#` in both a file name and a sheet name), and every
caller names both rather than picking one; `EntryStatus::Sheets(n)` is what `.ls` (`sheet specs
(n)`) and the browser (`✓ n sheets`) say about a workbook expanded into sheet members, `Stale` if
any sheet sidecar is; the workbench's fallback remedy excludes the sheet member by name, not the
whole workbook; and a pile with sheet **or region** members says "member(s)" where a plain
pile says "file(s)".
`tests/dataset.rs::acceptance_is_per_sheet_member` is the end-to-end proof that `--accept` takes
one sheet's judgement and leaves its sibling alone. **The magnitude check** (`src/magnitude.rs`,
2026-09-07): with three members or more, each member's median absolute value per numeric column
(a bounded typed head, `engine::preview`) against the median of those medians; 10× above or
below is a review reason recorded in the **lock, not the spec** — a fact about the pile — and the
console's `.accept` reads both places. Medians, not totals, so a partial month is a non-event. **Swept against the corpus**
(`scripts/sweep_workbooks.py`, 2026-09-07): of 34 multi-sheet xlsx/xlsm workbooks, 18 stay a
plain member and 16 expand into sheet members — the sixteen the draft slice had refused as
`AmbiguousFrame` — with no refusal, error or timeout. Re-run it after touching discovery.
Re-swept 2026-10-01: 19 plain, 15 expanded, none refused. The solar workbook the
declared-rounding merge (0f81fd3) had refused — a drafted `DECIMAL(38,15)` the fit then refused
for a value's sixteenth place — fits, since the draft now declares that rounding. The
expansion that merge lost stays lost, and rightly: `AssetSubsidies`' second sheet holds 16-place
floats under a `DECIMAL(38,1)` drafted from the first, which the 2026-09-07 sweep expanded by
rounding them to one place in silence (and by unioning a percentages sheet with a dollars
one); declaring the rounding by hand expands it again.

**Regions are in (2026-09-08).** `docs/design/2026-09-08-regions.md`. A member
is now `(path, sheet, region)` — `MemberRef` in `src/member.rs` gains a third
field, `region: Option<u32>`, the 1-based ordinal of a table stacked in a file
or sheet, counted in file order. Textual form is `report.csv#2` (a text file's
second block) or `book.xlsx#Q1#2` (sheet `Q1`'s second block); `MemberRef::resolve`
tries every right-to-left split, including a region candidate whenever the
rightmost segment parses as a positive integer, and keeps only members that
exist — `Q2` is a sheet, `2` is a region, and a sheet literally called `2`
alongside two regions is the ambiguity the resolver names, exactly as a sheet
name already is. The sidecar is `report.csv#2.tdy.toml` /
`book.xlsx#Q1#2.tdy.toml` (`sidecar::sidecar_path_for(file, sheet, region)`,
`load_member`/`save_member`); `EntryStatus::Regions(n)` is what `.ls` and the
browser say about a file split into region members; the lock's `Member.region`
and `Drift::MixedGranularity` extend to the triple, since a file listed whole
and by region is the same mistake as whole and by sheet.

Mechanically a region is a **row window**, not a name that carries its own
rows: `Extraction::Delimited` gains `region: Option<RowWindow>`
(`{ start, end, ordinal }`, 0-based half-open raw physical line numbers of the
file — physical, so a record carrying a quoted newline occupies as many
indices as it spans lines, which is what `regions_of` counted; both
`extract_delimited` and `stream.rs`'s `advance_raw_index` advance by
`1 + embedded`, and subtracting the embedded newlines instead (the first cut)
shifted every later block's window up by one and ate its header — applied
before anything else — `skip_rows`, `promote_header` and every transform act
inside the block, exactly as they act on a whole file). A sheet has no
row-window field at all — `range` already says which rows — so a sheet region
carries only `region_ordinal: Option<u32>` beside the `range` `fit::fit_region`
writes. Detection is `engine::regions_of(path, sheet, limits) -> Result<Regions>`:
it splits at runs of blank lines or rows, keeps blocks of at least three rows,
and returns nothing when the file has exactly one run at all — a table with
blank padding above or below it is not split. It returns what it *dropped*
alongside what it kept (`Regions { windows, dropped, block_width, window_widths, window_widest }`): with a
window applied nothing reads a run below the minimum, so every member of the
file names those lines in a note the CLI prints, and a dropped run is a review
reason (`Regions::table_shaped`) by either of two rules — any of its rows holds
two or more non-empty fields, or its first row is as wide as the first kept
block's; the `>= 3` rule says a `Total;;1500` line is not a table, and a person
rules on whether it was data, while one cell per row over a wider table (a
banner, footnotes) asks nothing. Each rule alone was tried and missed data:
same width alone missed a recap block and a 72-cell "US population" row under
a block that opens with a one-cell title; two-fields alone missed a one-column
table's own continuation. Text fields are counted by `nonempty_fields`, which
opens a quote only at a field's first byte — `Rohr 12";5;60.00` is three
fields, not one. Those blocks are *candidates*: `fit::gate_regions`
tries each against the target with the cheap gates, in the frame `fit_region` will fit, and
only the blocks that pass are members — got wrong first, when every run of three rows became
one, so a title banner was a member that fit nothing and the workbook sweep refused 15 of 16
workbooks; a failing block now joins the dropped runs (survivors renumbered), and none passing is the file read whole with no
region notes. A block whose own frame promoted no header is not a candidate
either (`fit::promotes_header`): a blank row proves a boundary, a header is what
tells one block from the next, and a headerless banner or two-cell footnote block
passed a positional `col_N` target's gates by position alone and stood in for the
sheet — the corpus's ttb workbook, which is now read whole as before regions. Nor is
a block that binds none of the declared columns, which under an all-`if_missing`
target "fit" as rows of NULLs. When no block passes, the file is read whole — and
if the split saw a block with its own header there and the target binds by name
(`Target::names_a_column`: a column not named `col_N`, or one with `matches`), that
member carries a review (`the split found a table with its own header at lines a–b
that does not fit …`), since a by-name target missing a headed table is evidence
the whole-file reading is wrong. A purely positional target binds by position by
construction, so the same failure is no such evidence — and asking on every such
sheet (15→33 reviews in the corpus sweep, first cut) is a question always answered
yes, which trains people to answer without reading. And only when the missed
block's promoted header is plausible (`fit::header_is_plausible`: no cell parses as a
number over a numeric column) — the block sniffer promoted `Alabama | 88165 | 0 | n/a`
in the corpus's ADP-31 state tables, a data row, and asked a false question on 13
sheets; such a block gets a note (`the split found a block at lines … whose header
reads like data`) instead. The test decides only whether to ask, never which blocks
are candidates. Its cost: a by-name target that misses a year-headed block
(`STATE;2008;…` over numbers) gets the note, not a review. All-headerless or
all-banner files stay unreviewed. A one-row run as wide as the block directly below
it, blank lines between, is that block's header cut off by a blank row — but only
for a block whose own frame promoted no header (`Regions::header_run`,
adopted by `fit::frame_blocks`, which `draft` shares; both executors skip the
blank row inside the window). Adopting on width alone made `Meier;Bern` the
header of a headed `Name;City` table and `Name|City` a data row, silently;
not adopting at all left a headerless block bound as `col_N`. Since 2026-10-01 the
adopted run may be title lines and the header in one run, as official statistics lay
them out (`Regions::header_run`: the run's *last* row as wide as the block, a window
above or a dropped run) — but only when the adopted frame's header ends on the run's
last row and passes the gates; otherwise the block is framed as before. `fit` adopts it
also over a promoted header that reads like data, and then the member waits on a person
(`row a (…) is read as data under the header adopted from lines x–y — accept only if
that row is data, not this table's header`), never a note alone: `Sales report;Q1 2025`
over `State;2024` is that shape, and a note let its unedited draft serve `State | 2024`
as a row (2624, not 600). `draft` never adopts over a promoted header
(`fit::Adoption::Draft`). The text sniffer skips a leading run of lines whose only filled field is
the first (`Table 1. …;;`, a title padded to the table's width as Excel writes CSV) as it
skips one-field ones — but only when the row after the run is then promoted as the header
with two or more filled fields, and never when the run reaches the end of the probe; a
single column with a trailing `;`, a headerless file whose first record has empty fields
and an `id;;;` first row are "first cell only" too, and the first cut cut their rows. So
`regions_padded_titles.csv` gives the three members its sheet layout does. ADP-31's run above the states ends in the "United States" total, so it is
not adopted and that sheet is still read whole. It streams the text
(`regions_of_lines`) rather than materialising it, so memory is O(runs), not
O(file): measured 3.9 MB peak RSS on a 50 MB fixture
(`tests/regions.rs::regions_of_streams_a_large_file`, `#[ignore]`, run by hand
under `/usr/bin/time`, since a peak-RSS claim is not a `cargo test` assertion).
`draft_target` pays this same streamed pass once per text file, on top of the
sniff's own whole-file type verification: measured on a debug build over a
50 MB single-block CSV, `tdy draft` wall time moved from ~37.6 s with the
regions pass skipped to ~39.2 s with it in, a ~4% cost. Counting every line's
non-empty fields for `table_shaped` (2026-10-01) took it to ~49.0 s against
~37.3 s (+31%, three runs each); one `match` per byte in `nonempty_fields`
brought it to ~44.1 s against ~37.7 s, ~17% — under the ~25% that would call
for more, so it is left there.

The review line: several blocks means `report::expand_units` gives each its
own member, each carrying `report::region_review_reason`'s text — "table `i`
of `n` in this file, split at blank rows — accept only if it is the same kind
of table as the others" — and `dataset()` refuses it until
`--accept report.csv#2` (or `.accept` in the console); exactly one proper
block means one plain member with the window and a note, **no review** —
but only when nothing table-shaped was discarded, because the elimination
proves the block is the only *reading*, never that the lines outside it were
not data. A region sidecar is trusted for exactly one block and both places
that say which are hand-editable, so `load_member` requires `source.region`
and the ordinal the spec's own window carries to agree, and `fit_pile`
refuses to reuse a spec whose window is not the block the split found — a
text window by its lines, a sheet block by the A1 `range` and ordinal that
`fit::block_a1` computes for the frame too; without those two, an edited window
made two members total one block twice, and a sheet block renumbered when one
more block passed the gates was read twice (3300.00 where the file held
2406.00). tdy's own sidecar that disagrees is re-planned with a note naming
both windows (`sidecar window was …; re-planned`), acceptance not carried; a
`manual` one is `CONTRADICTS`, a person's to settle. A sidecar the
loader *refuses* is still re-planned, never a hard failure, but the refusal is
now a note on the member (`sidecar refused: …; re-planned`) — discarding a
person's edit in silence left the member reading exactly as before with
nothing to say why. Two members with one name (a file literally called
`report.csv#2` beside a split `report.csv`) share one sidecar path and cannot
be two specs, so `expand_units` refuses the whole pile before anything is
fitted rather than letting the collision surface at `--accept`. The same
collision reaches `exclude`, which is applied twice — as a glob over files in
`lockfile::resolve_excluded`, then as an exact member reference in
`expand_units` — so one entry matching in both passes dropped a file *and* a
block and still exited 0; an entry that matches both is now refused as
ambiguous, which is why the glob pass reports what it removed instead of the
member pass walking the directory again. And when a
plain member reuses a hand-written *whole-file* spec, the split's dropped-run
note and review reason are reworded rather than attached — that spec reads
those lines, so the question becomes whether reading the whole file is
intended, and the gate stays. `fit::region_frame`
frames a block from the block's own rows — a text block sniffed as its own file
(`sniff::sniff_text_block`), a sheet block as a sheet of its own
(`sniff::frame_excel_block`) — because the whole file's frame let a banner
choose the separator and count itself into a `skip_rows` that, inside the
block, deleted rows the block does have; a sheet block's A1 `range` has to be
offset by the used range's own start (`col_letter`), since `regions_of` counts
rows of the used range, not of the sheet, and the naive address read a sheet
whose data starts at C5 against blank margin instead. A sheet passes
`discover_sheets` through its blocks too, but only when exactly one passes and
nothing table-shaped was discarded: sheet expansion asks nobody. `tdy draft` gains the
same split (`sniff::sniff_text_block`, via a scratch file), skipping a block with one
field per line (named in a `NOTE`, so the unedited draft of a banner-topped export fits),
drafting from the block whenever the split separated anything (one block with a dropped
line above it used to fall through to a whole-file draft that declared the title line's
values as columns, and the fit then read the real header as a data row, silently). A
workbook is split the same way since 2026-10-01, on the sheet the whole-file sniff reads
(`sniff::pick_sheet`, `fit::frame_blocks` over an `OpenSheet`), its blocks labelled as
`fit` names the members (`book.xlsx#i` — a one-sheet workbook's member stays plain); a
sheet under a banner used to draft positionally (`col_N`) from the whole-sheet sniff.
Draft skips, besides one-field banners, a block that is no `fit` candidate
(`FramedBlocks::draft_kind`: no promoted header, or on a sheet one that reads like data —
`regions_footnoted.xlsx`'s two-cell footnotes, ADP-31's states) and, when no block is
left, drafts the file whole, as `fit` reads it. A text block whose promoted header reads
like data is drafted from its own frame, as before (`State;2024` under a title line); one
under a run of title lines and a header makes the file drafted whole, with a NOTE. A sheet of a workbook
with several is drafted from its blocks only when one table is left and nothing
table-shaped (the condition on which `discover_sheets` admits it): split otherwise, the
draft fit no sheet, and the corpus sweep's ttb, ADP-31 national and occupational-health
workbooks were refused (`regions_{footnoted,three}_sheets.xlsx`). Every other block's
columns are drafted separately, naming which block each came from; presence and
heterogeneity notes count physical files, not blocks — got wrong first, when
the grouping note fired across one file's own blocks as though they were
unrelated files. `source_name` gains `from = "region"` (`SourcePart::Region`),
the block's ordinal as a column. The workbench carries `MemberReport.window`;
a region member's title reads `report.csv#2 · rows 6–9` (1-based, inclusive);
the raw head bolds the rows inside the window, dims the rows outside it, and
puts the `--propose`/problem marks on the *block's* own header line
(`window.start`, not line 0), which keeps them since that line is inside the
window. `watched_files` watches `report.csv#2.tdy.toml`, the sidecar a region
member actually has.

Follow-ups from its own review (2026-09-08): the two filesystem fallbacks
that resolve a typed member reference — `sidecar::resolve_ref` and the
console's `.accept` — asked only whether a sidecar path *existed*, and
`report.csv#2.tdy.toml` is the path of sheet `"2"` and of region 2 alike, so
`MemberRef::resolve` saw two candidates and `tdy validate`, `tdy check
--against` and `.accept` could not name a single region member.
`sidecar::declares_member` reads the sidecar's own `source` block instead;
one file, one declaration, at most one true candidate. `sheet_sidecars`'s
numeric-tail exclusion has a floor of 1 (`is_ordinal`), so a sheet literally
named `0` is listed again. `stream`'s past-the-end refusal mirrors
`engine`'s `raw_index <= start` rather than "no rows", so a window on blank
lines inside a file is empty on both executors instead of missing on one.
`tdy fit TARGET FILE` on a stacked file names the split it did not do
(`fit::stacked_note`). `tempfile` is a dev-dependency again: `draft`'s block
sniff takes its scratch file from `fileio::scratch_file`, inside the
process-lifetime cache `clear_cache()` already removes.

Out of scope, named on the design page: tables sitting **beside** each other
in a sheet (a two-dimensional segmentation problem a declared `range` already
answers by hand), splitting on anything other than blank rows, and a region
inside a JSON document — a JSON array's frame question belongs to the pointer,
not to this.

**Profiling is in (2026-10-01).** `docs/design/2026-10-01-profiling.md`; taxonomy K5.
`src/profile.rs` says, per column of the **framed raw table** — the strings `fit` binds
against, after the frame's `transpose`/`skip_rows`/`promote_header` (the same split
`engine::apply_spec_transforms` makes), a region window or a sheet `range`, before any body
transform or cast — non-empty/empty (trimmed; a column's `na_values` count as empty), distinct,
min/max of the raw strings by byte order, the top five, and every **shape** (`profile::shape`:
a digit is `9`, a digit run keeps its length up to 8 and is `9+` past it; a letter is `A`/`a`,
a same-case run `A+`/`a+`; whitespace one space; anything else itself), under the file's own
spelling (`header_origin` — two `Betrag`s stay two `Betrag`s). Three rules hold it to the
design: **it infers nothing and writes nothing** — the frame is a fresh sidecar's
(`sidecar::load_member`), else the sniffer's with `verify: false` and no backend, never saved
(a refused member is the one that most needs it); **the bounds are stated, not hidden** — past
10,000 distinct values a column says `AtLeast(10000)` and gives no top five, past 64 shapes the
rest are one `(other)` row, a shape first seen past the 10,000th tracked is counted only in
`(other)` and sets `shapes_complete: false` (then no renderer names a "most frequent shape" —
it could be the one nobody tracked), and `--head N` sets `complete: false`, which every renderer says on
its *first* line (`commands::profile_heading`, shared by the CLI text and the workbench); and
**it reads what a query reads** — text goes through `stream::framed_rows`, which is
`execute_with`'s own reading half (`drive`: measure, frame the header, hand rows over) without
`Plan::push` or the casts, so memory is O(columns × caps). Measured: a 50 MB CSV with a unique id
column profiles at **12 MB peak RSS, 2.6 s wall** including writing the file
(`tests/profile.rs::profile_streams_a_large_file`, `#[ignore]`, run by hand under
`/usr/bin/time` on the release test binary). Excel, JSON documents and a `transpose` take
`engine::extract` and materialise within `[limits]`. Four doors, one function
(`profile::profile_member`): `tdy profile FILE [--sheet] [--rows A-B] [--column NAME|#N]
[--head N]` (`--json` prints the profile), `.profile` with identical text
(`tests/console.rs` holds them equal), `p` in the workbench's File and Member contexts and on a
browser file (`Context::Profile { profile, selected, detail }`; Enter for a column's detail,
which is `profile_text`'s `--column` output verbatim; Esc back, then closed), and a read-only
`profile` MCP tool confined like the rest. A member's profile is of that member: `--rows` is the
block as its title counts it (1-based, inclusive) — physical lines of a text file, the **sheet's
own A1 rows** with `--sheet` (refused outside the used range, which is named; the heading names
the A1 `range` read) — framed by `fit::region_frame` unless a fresh sidecar reads exactly that
block. `MemberReport.rows` (and `rows_sheet`, when the member's name does not carry the sheet)
is set from the split for every block member, fitted or refused, in a dry run too, so `p`
dispatches `--rows` for every block; `window` keeps meaning the executed spec's own. A member
reference (`report.csv#2`) reads the block its fresh sidecar names and without one is refused as
"no fresh sidecar for report.csv#2 — name the block with --rows A-B …", never as a missing file;
`profile::resolve` splits references and, given a root (console, MCP), confines every candidate
data file before reading a sidecar beside it. A JSON document with several record arrays names
the one read and the candidates in the heading, and `--pointer /q2` picks another. Two columns with one name make `--column NAME` an error offering
`'#3'`/`'#4'`; picking one would be a guess (`\#3` names a column literally called `#3`).
The streamed width is measured only over the rows `skip_rows` keeps (`stream::measure`, at most
`tail` pending widths), as the engine rectangularises after the skip; measuring a skipped title
wider than the table had given the streamed table phantom `col_N` columns. A 103 MB / 3M-row
CSV profiles at 19 MB peak RSS, 4.9 s; its `count(*)` stayed at 76 MB peak across that change.

**tdy is scored on an external benchmark.** `scripts/download_pollock.sh` and
`scripts/run_pollock.py` run the Pollock data-loading benchmark (VLDB 2023,
2,290 files each with one isolated deviation from RFC 4180) through Pollock's
own metrics, so the numbers compare with the paper. Last run (2026-09-07, after the
compression guard and the long-form change; identical to the run before them):
2,287 of 2,290 load, record F1 0.991 (tying duckdbparse), cell precision 0.996
against recall 0.942 — tdy emits more cells than the source and almost never a
wrong one, which is `PadNulls` widening rather than dropping. Re-run 2026-09-30 after
regions, on the rebuilt binary: the summary is byte-identical to the 2026-09-07 run. It found two defects nothing else
had, so re-run it after touching extraction or framing.

## Real data

`scripts/download_corpus.sh` clones twenty-six public data-wrangling exercise repositories
into `corpus/` (gitignored, ~7 GB, 9,881 files). `TDY_CORPUS=corpus cargo test --release --test
corpus -- --nocapture` sweeps them: never panic, never hang, anything read confidently is
reproducible, plus a survey. `--release` is not optional: the per-file time assertion is
calibrated on an optimised build, and unoptimised calamine takes ~95 s over a 10 MB workbook
the release build reads in ~6 s. Nothing in CI sees it, so anything it *finds* has to become a
fixture in `testdata/` — that is what `12_late_surprises.py` is. The 2026-09-03 sweep's own
findings live in `gap_reports/AUDIT_FINDINGS.md` (gitignored, like every `gap_reports/`
report); its fixtures are `15_audit_defects.py` (below).

Current state (re-swept 2026-10-01 in release, at 46436b3: 3 tests passed in 796 s, no
panic or hang): of 9,881 files, 4,019 are read confidently (41%), 4,868 read unsure (49%)
and 994 declined (10%); the four declined xlsx are still Office `~$` owner-lock stubs,
which is correct. **0 of 1,374 real CSVs declined** (15 before the type-verification work).
The rise from the 2026-09-04 survey's 3,868 confident files has one cause, traced per file
over all 1,385 csv/tsv files: all 138 that rose did so between 2026-09-04 and 2026-09-08,
each losing exactly the doubt "nearly every column typed as text", which the September
audit restricted to files whose column names tdy had to invent (`!named_by_file`), and no
file's confidence has moved since.
`OxfordIHTM/messy-data`, which is purpose-built to be hard, lands at 50-75% confidence with
accurate notes, which is the documented tier-2 boundary rather than a defect.

## The one rule

**tdy never silently produces a wrong value.** Ambiguity resolves to the right answer or a
loud error naming the row — never a plausible wrong number. Most of the non-obvious code
exists to hold that line, and a change that trades it for convenience is a regression even
if every test passes. Concretely: thousands separators must group in threes (only when the
separator could also be a decimal point), `%Y` demands four digits (and a `%y` century
is chrono's 1970–2069 window unless `year_pivot` declares another — re-centred on the
parsed date, never by rewriting the string — and a `%y` member waits on review either
way), ambiguous date orders drop confidence below the escalation threshold, leading-zero
and oversized integers stay text, money becomes `decimal`, and a decimal value with more
fractional digits than the declared scale is refused unless the target column declares
`round = 'half_away'` (`spec::Rounding`; a sniffed sidecar's unset `round` still means
half-away, with its note).

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
  the last header row. A sheet's blank body rows are skipped where the framing ends
  (`engine::apply_spec_transforms`, just past the *last* `transpose`/`skip_rows`/`promote_header`
  in the spec, wherever it sits) — kept, they were all-NULL records `count(*)` counted, 12 for
  the ten states of `regions_statetable.xlsx`. Not at extraction, and not at the end of the
  leading run either: every row count a spec states (a title block's `skip_rows`, a tail placed
  after a `drop_rows_matching`) was written against rows that included the blanks, and the
  first cut, which skipped them after the leading run, made such a tail eat a data row in
  silence (`tests/regression.rs::a_skip_rows_tail_after_a_body_transform_counts_the_blank_row`).
  A body transform before that last framing transform still sees blank rows, as on main —
  except `fill_down`, which leaves a sheet's all-blank row blank (the carry runs on past it)
  so the drop point still removes it, rather than filling it into a label-only record.
- **Deliberate omissions:** no drop/rename transforms (the `columns` list is the only
  projection — `remove_empty` drops *rows* whose every cell is empty and nothing else; an
  all-empty column gets a sniffer note telling you to leave it out of `columns`), no locale tables (literal `replace` pairs in the sidecar), no named timezones
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
  A schema change (a new transform or `ValueParsing` field) counts as a prompt change,
  since the schema is pasted into the prompt.
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
  It is plumbing only — where an answer could differ (`promote_header_recording`,
  `build_column_at`) it calls the same function `engine` calls, deliberately, so the two
  cannot drift. It covers delimited, `lines`, `fixed_width` and NDJSON — everything whose rows are
  independent — behind a `Source` enum; Excel and a JSON *array* cannot stream, since each
  is one document with no records until it is parsed whole. NDJSON's header is the union of
  every record's keys, so `discover_ndjson` makes a real pass rather than guessing from a
  prefix: a key appearing only in the last record still has to become a column. It accepts only
  `[skip_rows]? [promote_header]? (drop_rows_matching | fill_down | remove_empty)* [unpivot]?`;
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
- **Types are verified against the whole file, not sampled.** `sniff::verify_types` runs
  `stream::verify` and widens any column whose guess does not hold, naming the offending
  values and how many of how many. Four real files from `corpus/` used to die mid-query on
  this (see `testdata/gen/12_late_surprises.py` for the reductions); erroring was correct but
  avoidable. `verify` has a fast path — run the real spec, and only if it fails pay for the
  raw-text analysis that finds *every* bad column — and projects away Utf8 columns, which
  cannot fail. Together those took a 141 MB sniff from 16 s to 5.9 s; `--quick` skips it and
  records that in the sidecar. This is affordable only because extraction streams.
  A byte-identical repeated header is dropped automatically (provably not data); a merely
  similar one is reported and kept, because dropping rows that fail to parse is the silent
  data loss the whole design refuses.
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
| `sniff` (16 KB sample + a whole-file type check) | 6.04 s, 1.20 GB RSS | **5.9 s, 55 MB** |
| `sniff --quick` (sample only, no type check) | — | **0.22 s, 24 MB** |
| `count(*)` over the whole file | 6.79 s, 1.40 GB | **2.96 s, 87 MB** |
| same file referenced twice | 2 full parses | 1 (cached, under 64 MB) |

The type check is a full read, which is the point of it — the sample lies (see
`testdata/late_surprise_*`). It costs ~0.3 s on top of the parse when nothing fails.
When something *does* fail, `stream::analyse` locates the offending rows by halving
rather than by asking once per row: a 22 MB export with a few bad cells in several
columns went from 21 s to 0.86 s, with identical rows and counts in the message.

The `count(*)` figure is the streaming executor; `TDY_NO_STREAM=1` on the same file is
3.13 s / 1,676 MB, which is what the materialising path still costs for the formats that
cannot stream.

**The "width" cost was encoding detection, and it is fixed.** `lemur_data.csv` (82k rows,
54 columns, 22 MB) read at ~0.55M cells/s while the same-shape synthetic read at 3M, and
the difference was one latin-1 `ö` at byte 7432: not valid UTF-8, no declared encoding, so
`detect_encoding` fed all 22 MB to chardetng (~0.15 s/MB) — twice on the streaming path
(count pass + body pass). `sample::evidence_windows` now feeds the detector bounded windows
around the non-ASCII bytes instead (the ASCII in between tells it nothing): 8.6 s → 0.96 s,
with every `enc_*` fixture detecting identically. If a file is mysteriously slow, check
whether it is barely-non-UTF-8 before blaming the parser.

Peak RSS no longer follows the size of a text file at all — `stream` holds one batch, not the
rows and not the decoded text. It is still ~8x for the formats that cannot stream (Excel,
JSON), which is what `[limits]` is calibrated against. If you change extraction, re-measure
with `/usr/bin/time -f "wall %es peak_rss %MkB"` and on a file large enough to tell the
difference: everything under 64 MB takes the cached path and will not show it.

## Test layout

- unit tests beside the code (283) — `numfmt`, `sqlscan`, `detect`, `spec::validate`, casting,
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
- `tests/adversarial.rs` (~120 s alone, more under a parallel run) — sweeps every fixture in `testdata/`: never panic, never hang, and
  anything sniffable must be queryable and reproducible under `--frozen`. It picks up new
  fixtures automatically. Note it runs the binary with output to *files*, not pipes: a
  100k-column sidecar is megabytes, and an undrained pipe deadlocks at 64 KB.

## Fixtures

`assets/` follows the same rule as `testdata/`: everything but `assets/gen_logo.py` is
generated (`python3 assets/gen_logo.py`, stdlib-only, byte-deterministic) — edit the pixel
definitions in the script, never the SVGs/PNGs. The README references the logo SVGs by
absolute raw-GitHub URL on purpose: the published crate excludes `assets/`, and crates.io
renders the same README.

`testdata/` is generated, never hand-edited — `python3 gen_fixtures.py`. Each generator in
`testdata/gen/` owns a disjoint set of files and documents in its docstring what each file
stresses. `10_declared_size.py` is the odd one out: two of its three files are *meant* to be
refused, and the third is the control proving the refusal does not catch ordinary documents.
`11_drifting_exports.py` is the other: it generates a *pile* rather than a file — twelve
monthly exports that disagree with each other, plus the target that declares what they should
all become. It is the corpus `tdy fit` will be judged on, and three of its twelve files
(Rappen, two-`Betrag`, no-region) exist to be **refused**. Its ground truth is the sum
57'340.00 over the nine that may join. `testdata/large/` is gitignored (perf fixtures, generated on demand).
`tests/e2e.rs::umsatz_spec()` is the hand-written reference spec for `umsatz.xlsx`.
`15_audit_defects.py` is the 2026-09-03 corpus audit's own regression corpus — the three
fixtures its defect fixes reference (`torn_tail_utf8.csv`, `xl_cell_newline.xlsx`,
`xl_money_siblings.xlsx`), committed ad hoc while fixing them and consolidated here so they
regenerate byte-identically like every other fixture, rather than living only as committed
bytes nothing can reproduce.

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
