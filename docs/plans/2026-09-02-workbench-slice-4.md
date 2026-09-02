# Spec-completion and polish (slice 4) — implementation plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Close every ledgered follow-up from slices 1–3: inference notes reach the workbench instead of being printed over the alternate screen, workbook members get the raw sheet grid spec §7 promised, the scroll/selection UX debts are paid, the hardening watch-items land, the cross-crate duplicates collapse, and the docs say only true things.

**Architecture:** No new subsystems — every item strengthens an existing seam. The one lib-level change is a `progress::Event::Note` variant so `provider::report`'s confidence warnings travel through the sink the session already carries (the CLI's `stderr_sink` keeps printing them; the TUI stops having its screen written over). The workbook grid extends `RawHead` with a bounded first-sheet grid read through the same `checked_worksheet_range` guard every other workbook path uses. Everything else is `workbench.rs`/`wb_ui.rs`/`main.rs` surgery with the established test styles.

**Tech Stack:** unchanged (Rust ≥ 1.88, ratatui/crossterm, tokio, calamine behind `engine`/`xlguard`).

**Spec:** `docs/design/2026-09-01-console-and-workbench.md` — §7 (the sheet grid line: "or the sheet grid for a workbook"), §6 (status hints "for the focused pane"), §4 (progress narration through the sink). The ledgered tickets live in the git history of `docs/plans/2026-09-0{1,2}-*` execution (review reports); each task below names its origin.

## Global Constraints

- `cargo test --workspace --lib --tests` green after every task; CI clippy `-D warnings` (not installed locally): no unused imports, `writeln!` never `write!(s, "...\n")`, `too_many_arguments` threshold 8 (group into structs).
- The one rule: tdy never silently produces a wrong value — the grid read must be bounded by `xlguard` like every other workbook read; a failed read renders as an error, never as an empty-looking file.
- One code path: shortcuts stay dispatched console lines; no new direct writes.
- CLI behaviour byte-identical except where a task names the change (Task 1 changes nothing on the CLI: notes still reach stderr).
- Commit after every task; every commit message ends with exactly:
  `Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>`
  `Claude-Session: https://claude.ai/code/session_01DmEku7uNkLUeyiNE38sho8`
- No `git stash`. Tests need no network/model.

## File structure

| file | change |
|---|---|
| `src/progress.rs` | `Event::Note(String)`; `stderr_sink` prints it |
| `src/provider.rs` | `report()` gains an `Option<&Sink>` route; the query path threads the session's sink |
| `src/console/mod.rs` | sink threading for SQL; `quote_rel`/`method_label` become `pub`; `raw_head` grid |
| `src/engine.rs` | `checked_worksheet_range` stays `pub(crate)`; a new small `pub fn sheet_grid(path, sheet, limits, max_rows, max_cols)` beside `excel_sheet_shapes` |
| `tdy-tui/src/workbench.rs` | selection preservation, Enter-stages-remedy, ExcludeFile floor, scroll wiring + clamps, record_target hardening, dupes deleted |
| `tdy-tui/src/wb_ui.rs` | grid render, context-aware hints, preview-error render, dupes deleted |
| `tdy-tui/src/main.rs` | after_editing success-gating, preview error into the pane, `wb_method_label` deleted |
| `docs/design/…`, `CLAUDE.md` | Task 6 |

---

### Task 1: inference notes travel through the sink

**Origin:** slice-1/2 ledger ("run_query_rooted prints inference notes to stderr — slice 2 captures them"; never done). Today `provider.rs:652`'s `report()` `eprintln!`s over the TUI's alternate screen whenever a query touches a low-confidence sidecar.

**Files:**
- Modify: `src/progress.rs`, `src/provider.rs` (~612-670), `src/console/mod.rs` (run_sql's call), `tdy-tui/src/main.rs` (worker sink match arm)
- Test: `tests/console.rs`, `src/progress.rs` unit

**Interfaces:**
- Produces: `progress::Event::Note(String)`; `provider::report_to(prepared: &[PreparedFile], cfg: &Config, sink: Option<&progress::Sink>)` — with `Some`, each warning becomes one `Event::Note` (the header line plus its `- note` lines joined with `\n`); with `None`, today's `eprintln!`s verbatim. `report()` becomes `report_to(prepared, cfg, None)` so the CLI path is untouched. `stderr_sink`'s match gains `Event::Note(t) => eprintln!("{t}")`.
- The console: `Session::run_sql` currently calls `run_query_confined` which calls `report(...)` internally — thread the session's sink down: `run_query_confined`/`run_query_rooted` gain the sink as an `Option<&Sink>` parameter on the *confined* entry point only (add `run_query_confined_with(sql, cfg, frozen, confinement, sink)`; the existing fns delegate with `None` so every current caller is untouched); `run_sql` passes its sink. The TUI worker's sink match (`tdy-tui/src/main.rs` spawn_console_worker) adds `Event::Note(t) => WbMsg::Note(t)` (transient — NOT Progress, per the standing Msg discipline).

- [ ] **Step 1: failing tests.**
  - `src/progress.rs` unit: `stderr_sink` handles `Note` without panic (call the sink with `Event::Note("x".into())`).
  - `tests/console.rs::low_confidence_notes_reach_the_sink_not_stderr`: copy `umsatz.xlsx` into a pile tempdir, `.sniff umsatz.xlsx --no-llm` (lands ~0.60 confidence, below the 0.8 threshold), then run `SELECT count(*) FROM messy('umsatz.xlsx');` through `Session::run` with a collecting sink (`Arc<Mutex<Vec<String>>>` closure); assert one collected note contains `confidence` and `umsatz.xlsx`. (The sniff itself must NOT emit the note — only the query's prepare pass does.)
- [ ] **Step 2:** RED. **Step 3:** implement per Interfaces. **Step 4:** `cargo test --test console low_confidence` then the full suite — and verify CLI parity: `target/debug/tdy query ... 2>` still carries the note on stderr for a low-confidence file (record in the report). **Step 5: commit** "inference notes travel through the progress sink — the TUI stops being printed over".

---

### Task 2: the workbook raw grid (spec §7's missing half)

**Origin:** slice-2 T6 + slice-3 final review Important 6 — a workbook member's raw view shows only sheet shapes: no header spelling, no raw values, though §7 promises "the sheet grid for a workbook".

**Files:**
- Modify: `src/engine.rs` (new fn beside `excel_sheet_shapes` ~1443), `src/console/mod.rs` (`RawHead` + `raw_head` ~983-1010), `tdy-tui/src/wb_ui.rs` (`raw_head_lines`)
- Test: `tests/console.rs`, `tdy-tui/tests/wb_render.rs`

**Interfaces:**
- Produces: `engine::sheet_grid(path: &Path, sheet: &str, limits: Limits, max_rows: usize, max_cols: usize) -> Result<Vec<Vec<String>>>` — goes through `xlguard::preflight` + `checked_worksheet_range` exactly as `extract_excel` does (read those call sites first; every workbook-touching path must use the guard — CLAUDE.md's standing rule), formats cells with the same cell-to-string logic extraction uses (find the helper `extract_excel` maps cells with and reuse it — do NOT write a second float-formatting), truncating to `max_rows`×`max_cols`.
- `RawHead` gains `pub grid: Vec<Vec<String>>` (first sheet only; empty for text files). `raw_head`'s workbook branch fills it: `sheet_grid(path, &sheets[0].0, limits, 20, 12)` when at least one sheet exists; a grid-read error degrades to the existing shapes-only view PLUS a line in `lines` saying `cannot read sheet "N": <err>` (never silently empty — the one rule).
- `wb_ui::raw_head_lines`: when `grid` is non-empty, render it as rows of ` | `-joined cells (each cell truncated to 14 chars with `…`), after the `sheet "N": R row(s) x C col(s)` summary lines. Tab-per-sheet stays future work — note in a comment.
- `.show`'s text (`render_shown`) gains the same grid lines so console and workbench agree.

- [ ] **Step 1: failing tests.**
  - `tests/console.rs::show_on_a_workbook_carries_the_grid`: copy `testdata/umsatz.xlsx` into the pile; `.show umsatz.xlsx`; assert the payload's `raw.grid` is non-empty, its first row contains `Muster AG — Umsatzübersicht` (the title cell — the file's OWN content, exactly what the raw view exists to show) and the text contains `Umsatzübersicht`.
  - `wb_render.rs::a_workbook_member_shows_its_grid`: build a File context with `raw.grid = vec![vec!["Region".into(),"Betrag CHF".into()], vec!["Ost".into(),"1'100.00".into()]]`, draw, assert `Betrag CHF` and `1'100.00` appear (the classic preview.rs properties — spelling and separator survive).
- [ ] **Step 2:** RED. **Step 3:** implement. **Step 4:** suites + `cargo test --test adversarial` explicitly (the grid path must not panic over the hostile workbook fixtures — the sweep picks fixtures up automatically, but run it and say so). **Step 5: commit** "workbook members show their sheet grid — the file's own spelling and separators, bounded by xlguard".

---

### Task 3: scroll and selection UX

**Origin:** slice-3 final review minors 7/8/11/13 + slice-2's unbounded console PgUp + the dead `main_scroll_bound` arms.

**Files:**
- Modify: `tdy-tui/src/workbench.rs`, `tdy-tui/src/wb_ui.rs`, `tdy-tui/src/main.rs`
- Test: `tdy-tui/tests/workbench.rs`, `wb_render.rs`

Items, each with a test:
- [ ] **Selection survives a refit**: in `apply`'s `Fitted` arm (workbench.rs ~428), when the previous context was Pile/Member for the SAME target, find the previously-selected member's `path` in the new report and select it (fall back to 0 when gone). Test: Pile with selection 2 → apply a new Fitted with the same members → selected still 2; with the member removed → 0.
- [ ] **Enter stages the selected remedy** in Member context: `Enter` behaves as the digit for `remedy_selected + 1` (same guards: not busy, no overlay, target_sql present). The `▸` marker stops being decorative. Test: move to remedy 2, Enter → `pending_edit` is that remedy.
- [ ] **ExcludeFile floor**: `member_remedies` (workbench.rs:576) appends `Remedy::ExcludeFile { rel: m.path.clone() }` when the member has `review.is_some() || !m.problems.is_empty()` and the list would otherwise be empty — mirroring the classic floor. Test: a review-only member (no problems) yields exactly one remedy, the exclude.
- [ ] **Preview failure reaches the pane**: `WbMsg::Note` already carries `preview unavailable: …` (main.rs spawn_wb_preview) but the Member/File pane shows "loading…" forever. Add `Workbench::preview_failed(path, msg)` setting a `raw`-side error marker (e.g. `Context::Member.raw = Some(RawHead { lines: vec![format!("cannot read: {msg}")], .. })` when the path matches) and have the runtime call it from the error branch (send a new `WbMsg::PreviewFailed { gen, path, msg }` instead of the bare Note). Render test: the message shows in the left column.
- [ ] **Scroll wiring + clamps**: Main-focus Up/Down scroll in Pile/Member/Evidence too (the `main_scroll_bound` arms come alive — but in Pile/Member, Up/Down mean SELECTION, so scrolling gets different keys: PgUp/PgDn scroll the main pane in every context; Up/Down keep their context meaning). Evidence gains scroll via `main_scroll` in its renderer (`draw_evidence` applies the offset like `draw_file_no_spec` does). Console PgUp clamps: `scroll` in `key_console` (~656) clamps against the flattened scrollback length (a `scrollback_lines()` helper counting echo+text lines). Tests: Evidence with 40 rows scrolls; console PgUp 100× then `scroll <= scrollback_lines()`.
- [ ] Run suites; commit "scroll and selection debts: refits keep your place, Enter stages, evidence scrolls, clamps everywhere".

---

### Task 4: the hardening batch

**Origin:** slice-2/3 watch-items and parked notes.

**Files:**
- Modify: `tdy-tui/src/workbench.rs`, `tdy-tui/src/main.rs`, `tdy-tui/src/wb_ui.rs`
- Test: `tdy-tui/tests/workbench.rs`, `wb_render.rs`, `tdy-tui/src/main.rs` unit tests

Items:
- [ ] **`record_target` uses the real tokenizer**: replace the whitespace-split in workbench.rs:354 with `tdy::console::parse::tokenize(line)` (pub since slice 1), take the first non-`--`-prefixed token after the verb — fixing both the quoted-spaces watch-item and the flags-before-positional minor. Tests: `.fit "my target.tdy.sql" --dry-run` records `…/my target.tdy.sql`; `.fit --dry-run t.tdy.sql` records `…/t.tdy.sql`.
- [ ] **`after_editing` only fires on editor success**: main.rs:506's caller — gate on `run_editor(...)` returning Ok (an editor failure notes the failure, not "target edited"). Unit-testable only by extraction: split the decision into `fn edit_outcome_note(succeeded: bool, was_pile_target: bool) -> Option<&'static str>` with a table test, and have the runtime use it (the subprocess half stays untested, as before — say so).
- [ ] **Context-aware status hints**: `draw_status`'s `Focus::Main` arm becomes a match on `w.context` — Pile: `↑↓ member · enter open · f refit · t edit target`; Member: `↑↓ remedy · enter/1-9 stage · a accept · e edit · Esc back`; Evidence: `a accept · Esc close · PgUp/Dn scroll`; File/Query/Empty keep `↑↓ scroll` (File) / `Tab focus`. Render test: Member context shows `1-9`.
- [ ] **Stale-Evidence comment + test**: one comment on `Context::Evidence` (the `.cd`-between-steps degradation is deliberate) and one state test: Evidence context, apply a `.cd`-shaped Done (cwd change), `a` re-dispatch still emits the stored line (the session-side reset is the session's tested business; here we pin only that the workbench doesn't panic or wrongly clear).
- [ ] **Initial-line begin bypass comment** (main.rs run_workbench ~398): the ~60ms window note from slice-3's review, as a comment.
- [ ] **`choose_mode` several-targets explicit test**: two targets in a tempdir → `Mode::Workbench { initial: None }`.
- [ ] **Spec-line ellipsis**: `spec_lines`' `name ← "source" : TYPE` rows ellipsize the SOURCE (`…`) so TYPE never clips off the right edge at the 50% split — same status-first policy as the browser rows; render test at a narrow width asserts the TYPE substring survives.
- [ ] Run suites; commit "hardening: real tokenizer for record_target, honest edit notes, hints per context, nothing clips the type".

---

### Task 5: the cross-crate dedupe

**Origin:** slice-2/3 minors — three copies of the method-label logic, two of `quote_rel`.

**Files:**
- Modify: `src/console/mod.rs` (make `pub fn quote_rel`, `pub fn method_label` with doc comments; `method_label` keeps its serde-derived implementation — the one that tracks `InferenceMethod`'s attributes), `tdy-tui/src/workbench.rs` (delete local `quote_rel`, import), `tdy-tui/src/main.rs` (delete `wb_method_label`, import)
- Test: existing suites are the net; one unit test in `src/console/mod.rs` pins `method_label` for all three variants.

- [ ] **Step 1:** the pinning test (`method_label(&Heuristic) == "heuristic"` etc.). **Step 2:** export, delete the copies, fix imports. **Step 3:** `cargo build --workspace --tests` warning-free + full suite. **Step 4: commit** "one quote_rel, one method_label — the copies the reviews kept flagging are gone".

---

### Task 6: docs and ledger-truth

**Files:**
- Modify: `docs/design/2026-09-01-console-and-workbench.md` (§10: one italic note that slice 3 replaced `render.rs`/`app.rs` with the workbench suites; §11: a slice-4 note listing what landed incl. `Event::Note` and the grid), `CLAUDE.md` (progress paragraph mentions `Event::Note`; the workbook-gap sentence updated to "first-sheet grid, tab-per-sheet still future"; test count refreshed from the final suite run), `README.md` (test count; one sentence in "For humans" about workbook members showing their sheet grid — describe, don't transcribe)

- [ ] **Step 1:** edits per above (get the real test count from the final full-suite run). **Step 2:** coherence read of the diffs; full suite. **Step 3: commit** "docs: notes through the sink, the workbook grid, and numbers that are true".

---

## Self-review

- **Origin coverage:** every open ledger ticket from slices 1–3 maps to a task: stderr notes (T1), workbook grid + preview.rs-property restoration (T2), selection/Enter/floor/preview-error/scroll/clamps (T3), tokenizer/edit-note/hints/comments/tests/ellipsis (T4), dupes (T5), spec §10 + counts (T6). Deliberately NOT taken (each a scope judgement, recorded here): tab-per-sheet grids (future; T2 comments it), draw-order z-layering, remedy.rs single-line-target handling (root-crate behaviour change beyond polish — remains ticketed with its better-error note in the T4 origin reviews), `Cell.ok` richer styling (done in slice 3), event::read spawn_blocking (single-session process unchanged).
- **Type consistency:** `Event::Note(String)` (T1) matched in `stderr_sink` and the worker; `RawHead.grid: Vec<Vec<String>>` (T2) read by `raw_head_lines` and `render_shown`; `WbMsg::PreviewFailed { gen, path, msg }` (T3) alongside the existing Preview fields; `tokenize` from `tdy::console::parse` (T4); `pub quote_rel/method_label` (T5) imported where the copies died.
- **Placeholder scan:** none; the two untestable halves (editor subprocess note, real-TTY behaviour) are named as such with their testable extractions.
