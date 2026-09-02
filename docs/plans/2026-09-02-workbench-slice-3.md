# The workbench views (slice 3) — implementation plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** The review loop moves into the workbench — Pile, Member, and Evidence become main-pane contexts, remedies write the target behind a shown diff, acceptance runs as `.accept` console lines through the session's own gate — the classic screens are deleted, bare `tdy` finally routes to the workbench, and the tdy mark lands on the Empty and help views.

**Architecture:** Everything follows slice 2's two rules. (1) One code path: `f` dispatches `.fit`, `a` dispatches `.accept` — the two-step review gate is *the session's* (`pending_accept`), so the workbench adds no second gate, it just renders `Payload::Evidence` and re-dispatches the same line. The one exception stays the one the spec grants (§8 rule 2): a remedy writes the target directly after its diff is confirmed on screen, then dispatches `.fit` so the refit is in the scrollback. (2) `workbench.rs` decides, `wb_ui.rs` draws, the runtime does I/O. The classic `App`/`ui` screens are deleted only after every one of their load-bearing behaviours has a workbench equivalent with an equivalent test.

**Tech Stack:** Rust ≥ 1.88, ratatui 0.30/crossterm 0.29, tokio; `tdy::console::{Session, Payload::{Fitted, Evidence}, …}`, `tdy::report::{PileReport, MemberReport, MemberStatus, Problem}`, `tdy_tui::remedy::{Remedy, Edit, apply, remedies_for}` (unchanged), `assets/gen_logo.py` (stdlib-only, byte-deterministic).

**Spec:** `docs/design/2026-09-01-console-and-workbench.md` — §6 (browser keys `f`/`d`/`D`), §7 (Pile/Member/Evidence contexts, remedy overlay), §8 (the gate's three rules), §5 (entry points — this slice implements the deferred end state), §11 slice 3. The ledger-deferred polish list from slice 2's final review is Task 6.

## Global Constraints

- `cargo test --workspace --lib --tests` green after every task; CI clippy `-D warnings` (not installed locally): no unused imports, `writeln!` never `write!(s, "...\n")`, `#[allow(dead_code)]` only with a task-reference comment.
- **One code path:** browser/main shortcuts synthesize console lines (`Dispatch`); the sole direct write is the remedy's target write, always behind a shown diff (spec §8 rule 2), always followed by a dispatched `.fit`.
- **The gate is the session's.** The workbench never tracks its own accept step; `a` dispatches `.accept TARGET MEMBER` and the session decides whether that is step one (Evidence back) or step two (Fitted back). No accept-all, no skip.
- **Delete only after parity:** each classic screen's load-bearing test property (notably: the accept screen shows raw values, min/max, and every judgement) must exist against the workbench context before the classic code goes.
- `assets/` rule: everything but `gen_logo.py` is generated — the mark's Rust table is emitted by the script, never hand-edited; `python3 assets/gen_logo.py` twice + `git status` clean is the determinism check.
- Commit after every task; every commit message ends with exactly:
  `Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>`
  `Claude-Session: https://claude.ai/code/session_01DmEku7uNkLUeyiNE38sho8`
- No `git stash` (shared stash stack). Tests never need a network or model.

## File structure

| file | change |
|---|---|
| `assets/gen_logo.py` | emits `tdy-tui/src/mark.rs` (generated const grid) alongside the existing outputs |
| `tdy-tui/src/mark.rs` | **generated** — 16×16 `Option<(u8,u8,u8)>` grid + dimensions |
| `tdy-tui/src/workbench.rs` | `Context::{Pile, Member, Evidence}`, remedy/overlay state, `help` flag, `d`-marks, new key arms, `apply` arms for `Fitted`/`Evidence`, polish fixes |
| `tdy-tui/src/wb_ui.rs` | pile/member/evidence renderers, remedy overlay, help overlay + mark, polish fixes |
| `tdy-tui/src/main.rs` | `WbAction::WriteTarget` handling; classic flow deleted (`run`, `spawn_fit`, `apply_msg`, `read_key`, `spawn_evidence`, `spawn_preview`, `spawn_query`, `member_spec`, `preview_spec`, `has_limit`, `cell`, `Msg`, `discover_target`, `Mode::Classic`); `choose_mode` collapses |
| `tdy-tui/src/app.rs`, `tdy-tui/src/ui.rs`, `tdy-tui/tests/render.rs`, `tdy-tui/tests/preview.rs` | **deleted** (Task 7), remedy.rs and its tests stay |
| `src/main.rs` | no-arg TTY path routes to the workbench when `tdy-tui` is on PATH (Task 8) |
| `tdy-tui/tests/{workbench,wb_render}.rs` | grow throughout |
| `README.md`, `CLAUDE.md`, spec | Task 8 |

---

### Task 1: the mark — generated pixels on Empty and a help overlay

**Files:**
- Modify: `assets/gen_logo.py` (new `emit_rust` output), `tdy-tui/src/workbench.rs` (help flag + `?` key), `tdy-tui/src/wb_ui.rs` (mark renderer, help overlay, Empty integration), `tdy-tui/src/lib.rs` (`pub mod mark;`)
- Create (generated): `tdy-tui/src/mark.rs`
- Test: `tdy-tui/tests/wb_render.rs`, `tdy-tui/tests/workbench.rs`

**Interfaces:**
- Produces: `mark::{WIDTH: usize = 16, HEIGHT: usize = 16, GRID: [[Option<(u8,u8,u8)>; 16]; 16]}`; `wb_ui` half-block rendering (8 terminal rows × 16 cols: each text row packs two pixel rows as `▀` with fg=upper/bg=lower, transparent halves via fg-only `▀`/`▄`, both `None` → space); `Workbench.help: bool`, toggled by `?` when focus is Browser or Main (console needs the character), any key closes it.

- [ ] **Step 1: extend `gen_logo.py`.** Add after the ANSI section (reusing `snapped_raster(MARK)` and `COLORS` exactly as `logo.ansi` does):

```python
def emit_rust(grid):
    lines = [
        "//! GENERATED by assets/gen_logo.py — edit the definitions there, never this file.",
        "//! The mark's 16x16 snapped raster; None is transparent.",
        "#![cfg_attr(rustfmt, rustfmt::skip)]",
        "pub const WIDTH: usize = 16;",
        "pub const HEIGHT: usize = 16;",
        "pub const GRID: [[Option<(u8, u8, u8)>; WIDTH]; HEIGHT] = [",
    ]
    for row in grid:
        cells = []
        for px in row:
            if px is None:
                cells.append("None")
            else:
                r, g, b = px[:3]
                cells.append(f"Some(({r}, {g}, {b}))")
        lines.append("    [" + ", ".join(cells) + "],")
    lines.append("];")
    return "\n".join(lines) + "\n"
```

Study `snapped_raster`'s actual return shape first (`assets/gen_logo.py:166` — it may return a color-key grid needing a `COLORS[key]` lookup rather than RGB tuples; adapt so `GRID` holds RGB). Wire it into `main()` writing `../tdy-tui/src/mark.rs` (path relative to `assets/`; resolve via `Path(__file__).parent`). Run `python3 assets/gen_logo.py` twice; `git status` must show mark.rs identical between runs and NO change to any existing asset (byte-determinism holds).

- [ ] **Step 2: failing tests.** `wb_render.rs`: draw an Empty-context workbench at 100×30 and assert at least one `▀` or `▄` cell appears in the main pane (the mark is drawn) and the orientation lines still render; toggle help (`w.help = true`) and assert the key vocabulary appears (`.sniff`, `Tab`, `^Q`) plus a mark glyph. `workbench.rs` tests: `?` with focus Browser sets `help`; any next key clears it and is otherwise swallowed; `?` typed in Console focus goes to the editor (text contains `?`).

- [ ] **Step 3: implement.** `wb_ui::mark_lines() -> Vec<Line<'static>>` builds 8 lines from `mark::GRID` (pixel rows 2r and 2r+1; match on `(upper, lower)`: both Some → `▀` fg=upper bg=lower; upper only → `▀` fg; lower only → `▄` fg; neither → space). Empty context: mark centered above the orientation lines (skip the mark when the pane is under 10 rows). Help overlay: when `w.help`, render over the main pane area a bordered block titled ` keys ` with the mark and a two-column key list (browser keys incl. the new `f`/`a`/`d`/`D` from later tasks as they land; keep the list in one const slice so tasks 2–6 append). Register `pub mod mark;`.

- [ ] **Step 4: run** `cargo test -p tdy-tui`, full workspace suite, and the double-generation determinism check.
- [ ] **Step 5: commit** (`assets/gen_logo.py`, `tdy-tui/src/mark.rs`, workbench/wb_ui/lib, tests): "the mark reaches the terminal: gen_logo emits mark.rs, Empty and help draw it as half-blocks".

---

### Task 2: `Context::Pile` — the pile lands in the main pane

**Files:**
- Modify: `tdy-tui/src/workbench.rs`, `tdy-tui/src/wb_ui.rs`
- Test: `tdy-tui/tests/workbench.rs`, `tdy-tui/tests/wb_render.rs`

**Interfaces:**
- Produces:

```rust
// workbench.rs
Context::Pile { target: PathBuf, report: tdy::report::PileReport, selected: usize },
Context::Member { target: PathBuf, report: tdy::report::PileReport, member: usize,
                  raw: Option<RawHead>, remedy_selected: usize },   // Task 3 fills rendering
pub fn pile_selected_member(&self) -> Option<&tdy::report::MemberReport>;
```

- `apply`: `Payload::Fitted(r)` → `Context::Pile { target, report: r, selected: 0 }`. The target path comes from the Outcome's echo? No — parse nothing: thread it. `Payload::Fitted` does not carry the target path, so the workbench remembers the last dispatched `.fit`/`.accept` target: add `pub last_target: Option<PathBuf>` set in `begin()` by parsing the line's first token pair (`.fit X` / `.accept X M` / `.check X` → resolve X against `browser.dir`); `Fitted` uses `last_target`, falling back to keeping the previous Pile's target on re-fits. If `last_target` is None (cannot happen for a Fitted, but be total) keep `Context::Empty` and note it.
- Keys, Main focus, Pile context: Up/Down move `selected` (clamped to `report.members`); `Enter` → `Context::Member { …, member: selected, raw: None, remedy_selected: 0 }` returning `WbAction::PreviewFile(target_dir.join(member.path))` (member raw fill-in; `target_dir` = `target.parent()`); `Esc` → `Context::Empty`. Browser focus: `f` on a `*.tdy.sql` entry dispatches `.fit <rel>` (data files keep `s`); Main/Pile `f` re-dispatches `.fit <target rel>` .
- Renderer: one row per member — `path`, status word (`fits`/`REVIEW`/`GAP`/`accepted` from `MemberStatus` + `accepted`), and the first line of `review` or the first problem's `message`; header block above: target name, `fitted/failed/needs_review` counts, `lock_written` presence; selected row marked `▸`.

- [ ] **Step 1: failing tests.** State: a synthetic `PileReport` (reuse the member-builder pattern from the OLD `tests/render.rs:40-75` — copy the helper into `tests/workbench.rs`, it dies with that file in Task 7) applied via `outcome(".fit sales.tdy.sql", "", Payload::Fitted(report))` after `begin(".fit sales.tdy.sql")` → context is Pile with the right target; Up/Down clamp; Enter yields `PreviewFile` ending in the member path and context Member; Esc returns to Empty; browser `f` on a target entry dispatches `.fit <rel>` (tempdir with a `t.tdy.sql`). Render: pile rows show member paths, `REVIEW` for a needs-review member, counts line, `▸` on the selected row.
- [ ] **Step 2-4:** RED → implement → `cargo test -p tdy-tui` → full suite.
- [ ] **Step 5: commit** "the pile is a context: .fit's report lands in the main pane, Enter opens a member".

---

### Task 3: `Context::Member` — the gap beside the file's own rows

**Files:**
- Modify: `tdy-tui/src/workbench.rs`, `tdy-tui/src/wb_ui.rs`
- Test: both test files

**Interfaces:**
- Consumes: `remedy::remedies_for(problem: &serde_json::Value, member_path: &str) -> Vec<Remedy>` — build the JSON with `serde_json::to_value(&member.problems[i])` (MemberReport's `Problem` derives Serialize); `Remedy::label()`.
- Produces: `pub fn member_remedies(&self) -> Vec<Remedy>` (all problems' remedies, deduped in order, computed on demand — the menu is small); `set_preview` fills `Context::Member.raw` when the path matches the member (extend its match). Keys in Member context (Main focus): Up/Down move `remedy_selected`; `Esc` → back to Pile (same report, selection on this member); `e` → `Dispatch(".edit <member rel>")`; `a` → Task 5; digits `1..=9` → Task 4. Renderer: left column the member's raw head (from `raw`, "loading…" while None) — the file's OWN spelling; right column: status, `review` reason if any, each problem's `message` (wrapped), then the numbered remedy menu (`label()` per line, `▸` on selection); an accepted member says `accepted` prominently.

- [ ] **Step 1: failing tests.** State: from a Pile with a gap member (use the old render.rs `gap_member` fixture shape — `Problem { kind: "no_candidate", column: Some("region"), header: [...], … }`), Enter → Member; `member_remedies()` non-empty and first label mentions the column; Esc returns to Pile with `selected` preserved; `set_preview` with the member's path fills `raw`, with another path is dropped. Render: member view shows a problem message substring, a remedy label with its number, and the raw header line once `raw` is set.
- [ ] **Step 2-4:** RED → implement → suites.
- [ ] **Step 5: commit** "a member is a context: the gap, the file's own rows, and a ranked remedy menu".

---

### Task 4: the remedy overlay — a shown diff, then the write, then the refit

**Files:**
- Modify: `tdy-tui/src/workbench.rs`, `tdy-tui/src/wb_ui.rs`, `tdy-tui/src/main.rs`
- Test: both test files

**Interfaces:**
- Produces:

```rust
// workbench.rs
pub pending_edit: Option<(Remedy, tdy_tui::remedy::Edit, PathBuf /*target*/, String /*expected text*/)>,
WbAction::WriteTarget { path: PathBuf, expected: String, new_text: String, refit: String /* the .fit line to dispatch after */ },
pub target_sql: Option<String>,   // the target's source text, read by the runtime after every Fitted (see below)
pub fn set_target_sql(&mut self, text: String);
```

- Flow: digit `n` in Member context → `remedy::apply(&target_sql, &remedies[n-1])` → `pending_edit = Some(...)`; overlay renders `Edit::diff()` with "y writes, Esc cancels". `y` → clear overlay, return `WbAction::WriteTarget { …, refit: format!(".fit {}", target_rel) }`; `Esc`/`n` → cancel. Runtime (`act_on_wb`): `WriteTarget` calls the EXISTING `write_target(path, expected, new_text)` guard (compare-then-atomic-write — keep that function when Task 7 deletes its old caller), then on success dispatches the refit line via the normal Dispatch path (busy set, scrollback records `.fit …`); on failure (`the target changed on disk`) → `wb.note(...)` and no dispatch.
- `target_sql` freshness: the runtime reads the target file (`std::fs::read_to_string`) after every `Fitted` Done and calls `set_target_sql` — so the menu always edits the text the last fit saw; `remedy::apply` errors surface as a note.
- Remedy application while `pending_edit` is Some: all other keys swallowed (modal), except Ctrl-Q.

- [ ] **Step 1: failing tests.** State: Member context over a tempdir with a REAL minimal target file (`CREATE TABLE t (region TEXT NOT NULL OPTIONS(matches = 'Region')) WITH (files='*.csv');`) and a gap problem for `region` with header `["Datum","Kanton"]` → digit 1 sets `pending_edit` and its Edit's diff contains `Kanton`; `y` yields `WriteTarget` whose `new_text` parses (`tdy::target::Target::parse(&new_text).is_ok()`) and whose refit is `.fit t.tdy.sql`; Esc cancels; keys while modal are swallowed. Render: overlay shows a `+` diff line and the y/Esc hint.
- [ ] **Step 2-4:** RED → implement (workbench + wb_ui overlay + main.rs WriteTarget arm) → suites.
- [ ] **Step 5: commit** "remedies in the workbench: a shown diff, the guarded write, and the refit in the scrollback".

---

### Task 5: Evidence — the two-step accept as console lines

**Files:**
- Modify: `tdy-tui/src/workbench.rs`, `tdy-tui/src/wb_ui.rs`
- Test: both test files

**Interfaces:**
- Produces: `Context::Evidence { target: PathBuf, member: String, rows: Vec<tdy::evidence::Evidence>, line: String /* the exact .accept line that produced this */ }`. `apply`: `Payload::Evidence { target, member, rows }` → that context, `line` = the outcome's echo. Keys: `a` in Member context on a member with `review: Some(..)` and `!accepted` → `Dispatch(format!(".accept {} {}", target_rel, member.path))`; `a` on any OTHER member → swallowed with a status note ("nothing to accept"); `a` in Evidence context → `Dispatch(context.line.clone())` — the SAME line, which the session treats as step two; `Esc` in Evidence → back to Member (the session's pending marker then expires on the next non-accept command by the session's own rule — no workbench bookkeeping). After step two, `Payload::Fitted` lands as usual → Pile context shows the member `accepted`.
- Renderer — **this restores the classic accept screen's load-bearing property**: for each `Evidence` row render `headline()`, and for `Shift` the head pairs as `row N  raw -> parsed` plus the `smallest`/`largest` lines; for `Frame` the header and head rows; footer: "a accepts · Esc backs out". The test MUST assert raw beside parsed (e.g. `170000` and `1700.00` both present) and that min/max lines render — the same numbers the classic render test pinned.

- [ ] **Step 1: failing tests.** State: `a` on a needs-review member dispatches the right `.accept` line; applying `Payload::Evidence` (hand-built rows: one `Shift` with head pairs and smallest/largest, one `Unillustrated`) sets the context; `a` again re-dispatches the identical line; `a` on a fits-member is swallowed. Render: `170000`, `1700.00`, the smallest/largest labels, both judgements' headlines (every judgement shows — the classic screen's rule), the footer hint.
- [ ] **Step 2-4:** RED → implement → suites.
- [ ] **Step 5: commit** "evidence is a context and acceptance is a console line — the session's gate, rendered".

---

### Task 6: `d`/`D` draft marks and the polish batch

**Files:**
- Modify: `tdy-tui/src/workbench.rs`, `tdy-tui/src/wb_ui.rs`
- Test: both test files

One dispatch, many small items — each with a one-assertion test:

- [ ] **`d`/`D`** (browser focus): `d` toggles the selected DATA file in `pub marked: Vec<String>` (rel paths, browser shows `*` on marked rows); `D` with ≥1 mark dispatches `.draft <marked…>` (space-joined, quote_rel'd) and clears the marks; `D` with none → status note. Marks clear on `.cd` (rel paths went stale).
- [ ] **Zoom skips Main in the Tab cycle** (`zoom == true` → Console ↔ Browser only).
- [ ] **Workbench Ctrl-C discards pending SQL**: Console focus, empty editor, `sql_pending()` → dispatch nothing; instead call a new `WbAction::DiscardSql` handled by the runtime? No — simpler and honest: dispatch is not needed; the session buffer lives IN the session. Add `WbAction::DiscardPending`; runtime calls a new worker message? The buffer is owned by the worker's Session. Cleanest within one-code-path: dispatch the existing console behaviour — send a literal `.help`-free discard is wrong. RULING built into this plan: add a tiny console-side command for it — in `src/console/parse.rs` a dot-command `.abort` (console-only, `Command::Abort`), `Session::run` handling it by `discard_pending()` and returning `Outcome { text: "note: discarded incomplete statement: …", payload: Nothing }` (empty buffer → text "nothing pending"); the plain REPL gains it for free; the workbench Ctrl-C dispatches `.abort`. Update `.help` text and the spec §3 console-only table (one row). Tests: console-side (tests/console.rs: buffer a line, `.abort`, sql_pending false, text contains "discarded") and workbench-side (Ctrl-C with pending → Dispatch(".abort")).
- [ ] **`main_scroll` upper clamp**: clamp in `key_main` against a `pub main_content_rows: usize` the renderer... no — renderer must not write state. Clamp in `key_main` to a generous bound computed in workbench from the context (`raw.lines.len() + sheets + preview rows + 16`); exact fit not required, just "cannot scroll into the void forever"; test pins the bound.
- [ ] **Stale-sidecar footer**: `spawn_wb_preview` currently maps stale → None; keep that, but pass a flag: `WbMsg::Preview` and `set_preview` gain `stale: bool`; `Context::File` gains `stale: bool`; footer says "sidecar stale — `.sniff --force`" instead of "not sniffed — press s" when set. Render test with a hand-set flag.
- [ ] **`ESCALATION` → config**: `wb_ui` can't reach Config; thread it — `Workbench::new` gains `confidence_threshold: f32` (runtime passes `cfg.confidence_threshold`; check the real field name in `src/config.rs` and use it), stored `pub`, renderer reads it; delete the const and its false comment. Browser column: `✓ 0.42` renders red below the threshold too (reviewer's §6 note) — pass the threshold into `draw_browser` via `w`.
- [ ] **Multi-line echo cells**: in `begin`/`apply` scrollback assembly split `echo` on `\n`: first line `tdy> …`, continuation lines rendered with `   -> ` prefix (matching the real console); test with a two-line SQL echo.
- [ ] **`Cell.ok` styling**: failed cells' echo line rendered in red; keeps the field honest. Render test: a failed outcome's echo shows (assert the text; color isn't assertable — the test documents the intent, the code review checks the style).
- [ ] **Preview generation counter**: `pub preview_gen: u64` bumped on every `PreviewFile` the workbench RETURNS; `WbMsg::Preview` carries it; `set_preview` drops mismatches. Closes the stale-overwrite window found in slice 2's final review. Test: set_preview with an old gen is dropped.
- [ ] Run both suites + console tests; commit: "the polish batch: draft marks, .abort, clamps, stale footer, configured threshold, honest scrollback".

---

### Task 7: the classic screens retire

**Files:**
- Delete: `tdy-tui/src/app.rs`, `tdy-tui/src/ui.rs`, `tdy-tui/tests/render.rs`, `tdy-tui/tests/preview.rs`
- Modify: `tdy-tui/src/lib.rs` (drop `pub mod app; pub mod ui;` and the `TargetColumn` re-export if only they used it — check), `tdy-tui/src/main.rs`

**Parity checklist to verify BEFORE deleting (each already true if Tasks 2-5 landed; confirm by pointing at the test):** pile listing (T2), member gap + raw + ranked remedies (T3), diff-before-write (T4), evidence with every judgement + min/max + one-member-at-a-time (T5), `t` opens the target in $EDITOR (add now: `t` in Pile/Member context → `Dispatch(".edit <target rel>")`, one test), query results (slice 2), first-fit-is-a-dry-run — **carry it**: the initial `.fit` a target-argument launch dispatches must be `.fit <target> --dry-run` (opening a review tool must not write; `f` refits for real), one test on `choose_mode`'s initial line.

- [ ] **Step 1:** `choose_mode` collapses: a named `.tdy.sql` (or single discoverable) now yields `Mode::Workbench { root: target's dir, initial: Some(format!(".fit {} --dry-run", name)) }`; `Mode::Classic` deleted; update its unit tests (the classic expectations flip to workbench-with-initial-fit).
- [ ] **Step 2:** delete `run`, `Msg`, `apply_msg`, `read_key`, `spawn_fit`, `spawn_evidence`, `spawn_preview`, `spawn_query`, `member_spec`, `preview_spec`, `has_limit`, `cell`, `discover_target` (fold its single-target discovery INTO `choose_mode` first — it still needs to find the one target), delete `app.rs`/`ui.rs` and the two test files; KEEP `remedy.rs` (+ its tests), `write_target`, `run_editor`, `reenter`, the panic-hook.
- [ ] **Step 3:** full workspace suite; `cargo build --release -p tdy-tui` headless start checks (target arg and no-arg both reach terminal-init failure, never a parse of deleted code paths).
- [ ] **Step 4:** commit "the classic screens retire: every review behaviour lives in the workbench, opening with a target is a dry-run fit".

---

### Task 8: bare `tdy` opens the workbench; docs

**Files:**
- Modify: `src/main.rs` (no-arg TTY dispatch), `README.md`, `CLAUDE.md`, `docs/design/2026-09-01-console-and-workbench.md` (§5 both revision notes resolved, §11 slice-3 status), memory of the quick start untouched (it uses the console — verify no stale sentence).

- [ ] **Step 1:** `src/main.rs` no-arg path: stdio TTY && `workbench_on_path()` → `exec_workbench(None)`; TTY without → console with the one-line stderr note (`terminal UI not installed: cargo install --path tdy-tui`); piped → batch (unchanged); `tdy console` unchanged. This restores the §5 end state — now correct, because tdy-tui without a target IS the workbench.
- [ ] **Step 2:** spec §5: replace the deferral note in the bare-`tdy` row with the end-state text + "landed with slice 3"; §11 slice-3 status note (what shipped, incl. `.abort` and the dry-run-fit launch rule). README: "The console" section's last paragraph (bare `tdy` opens the workbench when installed, `tdy console` for the plain console); "For humans: `tdy ui`" loses the classic-flow split (it's all workbench now; `tdy ui sales.tdy.sql` opens it fitted, dry-run). CLAUDE.md: console paragraph's bare-`tdy` sentence updated; tdy-tui paragraph: classic screens gone, app.rs/ui.rs deleted, the parity tests named; `.abort` added to the console command list.
- [ ] **Step 3:** headless verify: `printf '.help\n' | tdy` still batches; `tdy </dev/null` (piped) batches; real-TTY workbench routing is the user's smoke test — say so in the report, don't fake it. Full workspace suite.
- [ ] **Step 4:** commit "bare tdy opens the workbench — the §5 end state, now that the workbench is the console plus panes".

---

## Self-review against the spec

- **§11 slice 3** — Pile/Member/Evidence behind Context (T2/3/5), remedy overlay (T4), two-step accept (T5, via the session's own gate), old screens deleted (T7), the mark on Empty and help (T1). All present.
- **§6** — `f` (T2), `d`/`D` (T6), `t` carried (T7 checklist). §8's three rules: gate reachable only through evidence and one member at a time — enforced by the SESSION (`.accept` grammar takes exactly one member; step one/two logic is slice 1's, tested there) with workbench tests for the dispatch discipline (T5); shown diff before every target write (T4); acceptance-about-bytes untouched.
- **§5** — end state (T8). Polish ledger — every slice-2 deferred minor has a T6 bullet or an explicit carry (wrapping stays deferred; `method_label`/`quote_rel` dedupe stays deferred — cross-crate visibility, noted for a future console-API pass).
- **Type consistency:** `Context::{Pile{target,report,selected}, Member{target,report,member,raw,remedy_selected}, Evidence{target,member,rows,line}}` used identically in T2-T5; `WbAction::WriteTarget{path,expected,new_text,refit}` produced T4, consumed T4's main.rs step; `.abort` = `Command::Abort` console-side (T6). `pending_edit`, `marked`, `preview_gen`, `help` all declared where first used.
- **Placeholder scan:** none — the one deliberately deferred decision (exact wrapped-line rendering widths) is bounded by named assertions, not "TBD".
- **Plan-level ruling recorded for the controller:** `.abort` is a NEW console command (grammar + session + help + spec table row) introduced by T6 — flag to the executor's ledger as a spec §3 amendment.
