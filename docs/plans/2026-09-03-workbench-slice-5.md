# Workbench Slice 5 Implementation Plan — the recorded leftovers

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Close the deferred tickets from slices 3–4: sheet-selectable workbook grids (`.show FILE --sheet NAME` in the console, `[`/`]` in the workbench), pile scroll that follows the selection, an honest error for remedies against a single-line target, and a small tidy sweep — then docs.

**Architecture:** `RawHead` learns which sheet its grid belongs to (`grid_sheet`), `raw_head` accepts a sheet selector, and the console's `.show` grows a `--sheet` valued flag. The workbench switches sheets through the existing preview path (`WbAction::PreviewFile` gains a `sheet` field) — sheet flipping is a *view* change, like the arrow-move preview, not a command, so it does not synthesize console lines. Scroll-follows-selection is pure state-machine arithmetic fed a viewport height the runtime computes from a pure `wb_ui` helper (ui still mutates nothing).

**Tech Stack:** Rust; root crate `tdy` + workspace member `tdy-tui` (ratatui/crossterm); tests via `cargo test --workspace --lib --tests`.

**Spec:** `docs/design/2026-09-01-console-and-workbench.md` (§3 grammar, §7 contexts, §11 slice status). This slice implements §7's "tab per sheet" deferral (line ~231) and the §11 slice-4 note's "a tab per sheet remains future work".

## Global Constraints

- Commit messages end with exactly these two trailer lines:
  `Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>`
  `Claude-Session: https://claude.ai/code/session_01DmEku7uNkLUeyiNE38sho8`
- **Never use `git stash`** (shared stash stack across worktrees). To see a file at a revision use `git show <rev>:<path>`.
- CI runs `cargo clippy --all-targets -- -D warnings`; clippy is NOT installed locally. Write clippy-clean by construction: `writeln!(s, "...")`, never `write!(s, "...\n")`; keep functions at ≤7 parameters (group into the struct that already exists rather than adding `#[allow]`).
- Test command: `cargo test --workspace --lib --tests` (plain `cargo test` hits a spurious rustdoc/libLLVM doc-test failure on this machine).
- `tdy-tui/src/wb_ui.rs` renders and mutates nothing — any new function there that reads `Workbench` must be pure.
- The one rule: tdy never silently produces a wrong value. A grid must always name the sheet it came from; an unknown sheet is a loud error listing the real ones.
- `testdata/` is generated, never hand-edited. This slice adds **no** fixtures — tests use existing ones and in-memory structs.

---

### Task 1: Sheet-selectable raw head and `.show FILE --sheet NAME` (root crate)

**Files:**
- Modify: `src/console/mod.rs` (RawHead struct ~line 114, `raw_head` ~line 1008, `render_shown` ~line 1042, `Command::Show` run arm ~line 522, help text line ~1151)
- Modify: `src/console/parse.rs` (`Command::Show` variant ~line 26, `"show"` arm ~line 257, parse tests)
- Modify: `tdy-tui/src/main.rs:268-273` (`spawn_wb_preview` calls `raw_head` — pass `None` for now; Task 2 threads the real value)
- Modify: `tdy-tui/src/workbench.rs:589-594` (`preview_failed` builds a `RawHead` literal — add the new field)
- Modify: `tdy-tui/src/wb_ui.rs:600-626` (`raw_head_lines` — read `grid_sheet` instead of inferring `sheets.first()`)
- Test: `tests/console.rs`, parse tests in `src/console/parse.rs`

**Interfaces:**
- Consumes: `engine::sheet_grid(path, sheet_name, limits, 20, 12)` (exists, `src/engine.rs:1493`), `Args::collect(cmd, tokens, switch_names, value_names)` and `a.value("--flag") -> Option<String>` (exist, `src/console/parse.rs:112-146`).
- Produces (Task 2 relies on these exactly):
  - `pub struct RawHead { …, pub grid_sheet: Option<String> }` — the name of the sheet `grid` shows; `None` when `grid` is empty or the file is not a workbook.
  - `pub fn raw_head(path: &Path, limits: crate::config::Limits, sheet: Option<&str>) -> Result<RawHead>`
  - `Command::Show { file: String, sheet: Option<String> }`

- [ ] **Step 1: Write the failing parse tests** — in the `#[cfg(test)]` module of `src/console/parse.rs`, alongside the existing `p(...)` helper:

```rust
#[test]
fn show_takes_a_sheet_flag() {
    assert_eq!(
        p(".show book.xlsx --sheet Two"),
        Command::Show { file: "book.xlsx".into(), sheet: Some("Two".into()) }
    );
    assert_eq!(p(".show book.xlsx"), Command::Show { file: "book.xlsx".into(), sheet: None });
    assert_eq!(
        parse(".show book.xlsx --sheet"),
        Err(ParseError::FlagNeedsValue { command: "show", flag: "--sheet".into() })
    );
}
```

Any existing parse/console test constructing `Command::Show { file }` gains `sheet: None`.

- [ ] **Step 2: Run to verify failure**

Run: `cargo test -p tdy --lib console::parse`
Expected: FAIL — `Show` has no field `sheet`.

- [ ] **Step 3: Implement the grammar** — in `src/console/parse.rs`:

```rust
// variant:
Show { file: String, sheet: Option<String> },
// arm:
"show" => {
    let a = Args::collect("show", args, &[], &["--sheet"])?;
    a.exactly(&["FILE"])?;
    Command::Show { file: a.positional[0].clone(), sheet: a.value("--sheet") }
}
```

- [ ] **Step 4: Extend `RawHead` and `raw_head`** — in `src/console/mod.rs`. Add to the struct (keep the existing derives; document the field):

```rust
    /// Which sheet `grid` shows — the first sheet unless `.show --sheet`
    /// (or the workbench's `[`/`]`) picked another. `None` when `grid` is
    /// empty or the file is not a workbook. Renderers print this, never
    /// re-infer `sheets.first()`: once the grid is selectable, inferring
    /// would caption the wrong sheet — a silently wrong answer.
    pub grid_sheet: Option<String>,
```

Change the signature and the workbook branch (the text-file tail just gains `grid_sheet: None`):

```rust
pub fn raw_head(path: &Path, limits: crate::config::Limits, sheet: Option<&str>) -> Result<RawHead> {
    let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("").to_ascii_lowercase();
    if WORKBOOK_EXT.contains(&ext.as_str()) {
        let sheets: Vec<(String, usize, usize)> = crate::engine::excel_sheet_shapes(path, limits)?
            .into_iter()
            .map(|s| (s.name, s.rows, s.cols))
            .collect();
        let chosen: Option<String> = match sheet {
            Some(want) => match sheets.iter().find(|(n, ..)| n == want) {
                Some((n, ..)) => Some(n.clone()),
                None => {
                    let names: Vec<String> =
                        sheets.iter().map(|(n, ..)| format!("{n:?}")).collect();
                    anyhow::bail!(
                        "no sheet {want:?} in {} — sheets: {}",
                        path.display(),
                        names.join(", ")
                    );
                }
            },
            None => sheets.first().map(|(n, ..)| n.clone()),
        };
        let mut lines = Vec::new();
        let mut grid = Vec::new();
        let mut grid_sheet = None;
        if let Some(name) = &chosen {
            match crate::engine::sheet_grid(path, name, limits, 20, 12) {
                Ok(g) => {
                    grid = g;
                    grid_sheet = Some(name.clone());
                }
                // Never render an unreadable sheet as an empty grid — say
                // so, and fall back to the shapes-only view.
                Err(e) => lines.push(format!("cannot read sheet {name:?}: {e:#}")),
            }
        }
        return Ok(RawHead { lines, truncated: false, sheets, grid, grid_sheet });
    }
    if let Some(want) = sheet {
        anyhow::bail!("--sheet {want:?} applies to workbooks; {} is not one", path.display());
    }
    // …existing text-file body unchanged, plus `grid_sheet: None` in its Ok(RawHead …)
```

- [ ] **Step 5: Callers and renderers.** `grep -rn "raw_head(" src tdy-tui` and fix every call site:
  - `Command::Show` run arm (`src/console/mod.rs:524`): `let raw = raw_head(&path, self.cfg.limits, sheet.as_deref())?;` (destructure `Command::Show { file, sheet }`). The precheck arm at ~line 764 (`Command::Show { file }` in the resolve-only match) becomes `Command::Show { file, .. }`.
  - `tdy-tui/src/main.rs:270`: `raw_head(&path, cfg.limits, None)`.
  - `render_shown` (`src/console/mod.rs:1069-1071`): replace the `sheets.first()` inference with `if let Some(n) = &raw.grid_sheet { let _ = writeln!(s, "  grid of sheet {n:?}:"); }` and delete the now-unused `grid_sheet` local.
  - `wb_ui::raw_head_lines` (`tdy-tui/src/wb_ui.rs:614-617`): same swap — `if let Some(name) = &raw.grid_sheet { lines.push(Line::raw(format!("grid of sheet {name:?}:"))); }`.
  - `workbench.rs::preview_failed` RawHead literal (~line 589): add `grid_sheet: None`.
  - Help text (`src/console/mod.rs:1151`): `.show FILE [--sheet NAME]  the raw head beside what the sidecar says`.

- [ ] **Step 6: Write the failing console tests** — in `tests/console.rs`, following that file's existing Session-driving pattern (build a `Session` the way its neighbors do; `testdata/sheet_frames_one_fits.xlsx` is multi-sheet by construction — its generator makes the cover page the biggest sheet):

```rust
#[tokio::test]
async fn show_sheet_flag_selects_the_named_sheet() {
    let mut s = session_in("testdata"); // whatever helper the file already uses
    // Learn the real sheet names from the default view first — the test
    // must not hard-code generator internals.
    let all = s.run(".show sheet_frames_one_fits.xlsx").await;
    assert!(all.ok, "{}", all.text);
    let second = tdy::console::raw_head(
        std::path::Path::new("testdata/sheet_frames_one_fits.xlsx"),
        tdy::config::load(&Default::default()).unwrap().limits,
        None,
    )
    .unwrap()
    .sheets[1]
        .0
        .clone();
    let o = s.run(&format!(".show sheet_frames_one_fits.xlsx --sheet {:?}", second)).await;
    assert!(o.ok, "{}", o.text);
    assert!(o.text.contains(&format!("grid of sheet {second:?}:")), "{}", o.text);
}

#[tokio::test]
async fn show_sheet_flag_rejects_unknown_and_text_files() {
    let mut s = session_in("testdata");
    let o = s.run(".show sheet_frames_one_fits.xlsx --sheet nope").await;
    assert!(!o.ok);
    assert!(o.text.contains("no sheet \"nope\""), "{}", o.text);
    let o = s.run(".show enc_late_1252_byte.csv --sheet nope").await;
    assert!(!o.ok);
    assert!(o.text.contains("applies to workbooks"), "{}", o.text);
}
```

Adapt the helper names to what `tests/console.rs` actually defines (read its first ~60 lines); the assertions are the contract. If `raw_head` is not re-exported from `tdy::console`, export it there (it is already `pub` in the module).

- [ ] **Step 7: Run the suite**

Run: `cargo test --workspace --lib --tests`
Expected: PASS, including the two new console tests and the parse test.

- [ ] **Step 8: Commit**

```bash
git add -A src tdy-tui tests
git commit -m ".show learns --sheet: the grid names its sheet instead of assuming the first"
```

---

### Task 2: `[`/`]` switch sheets in the workbench (File and Member views)

**Files:**
- Modify: `tdy-tui/src/workbench.rs` (`WbAction::PreviewFile` ~line 107, `preview_action` ~line 645, `key_main` ~line 907, `enter_pile_member` ~line 1043, the `Payload::Sniffed` arm ~line 442)
- Modify: `tdy-tui/src/main.rs` (`spawn_wb_preview` ~line 268, the `WbAction::PreviewFile` dispatch arm ~line 352)
- Modify: `tdy-tui/src/wb_ui.rs` (`HELP_KEYS` ~line 37, `draw_status` Main/File arm ~line 837)
- Test: `tdy-tui/tests/workbench.rs`

**Interfaces:**
- Consumes: `RawHead.grid_sheet: Option<String>`, `RawHead.sheets: Vec<(String, usize, usize)>`, `raw_head(path, limits, sheet: Option<&str>)` — all from Task 1. `member_preview_path(target, member_rel)` (exists, workbench.rs:1298).
- Produces: `WbAction::PreviewFile { path: PathBuf, sheet: Option<String> }` (struct variant — every constructor and matcher updates).

- [ ] **Step 1: Write the failing state-machine tests** — in `tdy-tui/tests/workbench.rs`, using that file's existing helpers for building a `Workbench` and pressing keys:

```rust
fn two_sheet_raw() -> RawHead {
    RawHead {
        lines: vec![],
        truncated: false,
        sheets: vec![("One".into(), 5, 3), ("Two".into(), 4, 2)],
        grid: vec![vec!["a".into()]],
        grid_sheet: Some("One".into()),
    }
}

#[test]
fn bracket_keys_flip_sheets_in_file_context() {
    let mut w = wb(); // the file's existing constructor helper
    w.focus = Focus::Main;
    w.context = Context::File {
        path: PathBuf::from("/pile/book.xlsx"),
        raw: two_sheet_raw(),
        spec: None,
        preview: None,
        stale: false,
    };
    // ']' from sheet One asks for a preview of sheet Two.
    assert_eq!(
        w.key(press(']')),
        WbAction::PreviewFile { path: PathBuf::from("/pile/book.xlsx"), sheet: Some("Two".into()) }
    );
    // '[' at the first sheet has nowhere to go: no action, no I/O.
    assert_eq!(w.key(press('[')), WbAction::None);
}

#[test]
fn bracket_keys_do_nothing_without_sheets() {
    let mut w = wb();
    w.focus = Focus::Main;
    w.context = Context::File {
        path: PathBuf::from("/pile/plain.csv"),
        raw: RawHead::default(),
        spec: None,
        preview: None,
        stale: false,
    };
    assert_eq!(w.key(press(']')), WbAction::None);
}
```

Add a Member-context sibling: build the `Context::Member` the way the file's existing member tests do, set `raw: Some(two_sheet_raw())`, and assert `]` yields `WbAction::PreviewFile { path: member_preview_path(...), sheet: Some("Two".into()) }` (assert on the path the existing tests use for that member).

- [ ] **Step 2: Run to verify failure**

Run: `cargo test -p tdy-tui --test workbench bracket`
Expected: FAIL — `PreviewFile` is a tuple variant with no `sheet`, and `[`/`]` are unhandled.

- [ ] **Step 3: Make `PreviewFile` carry a sheet.** In `workbench.rs`:

```rust
    /// Compute a preview of this file for the main pane (arrow-move
    /// preview, or `[`/`]` picking another sheet of a workbook — a view
    /// change like the arrow-move preview, so it deliberately does NOT
    /// synthesize a console line; `.show FILE --sheet NAME` is the
    /// console's own spelling of the same thing).
    PreviewFile { path: PathBuf, sheet: Option<String> },
```

`preview_action` gains the parameter (still the one place the action is built):

```rust
    fn preview_action(&mut self, path: PathBuf, sheet: Option<String>) -> WbAction {
        self.preview_gen += 1;
        WbAction::PreviewFile { path, sheet }
    }
```

Update every existing `preview_action(x)` call to `preview_action(x, None)` (`Payload::Sniffed` arm, `enter_pile_member`, the browser arrow-move site — `grep -n "preview_action" tdy-tui/src/workbench.rs`).

- [ ] **Step 4: Handle the keys.** In `key_main`, directly after the PgUp/PgDn block (so File and Member both get it before their context dispatch):

```rust
        match k.code {
            KeyCode::Char('[') => return self.switch_sheet(-1),
            KeyCode::Char(']') => return self.switch_sheet(1),
            _ => {}
        }
```

And the helper (private, near `preview_action`):

```rust
    /// `[`/`]`: preview the adjacent sheet of the workbook on screen.
    /// Clamped, not wrapping — `[` on the first sheet does nothing, which
    /// is visible honesty (a wrap reads as "nothing changed" on a 2-sheet
    /// book). No-op in every context without a multi-sheet raw head.
    fn switch_sheet(&mut self, dir: isize) -> WbAction {
        let (path, raw) = match &self.context {
            Context::File { path, raw, .. } => (path.clone(), raw),
            Context::Member { target, report, member, raw: Some(raw), .. } => {
                match report.members.get(*member) {
                    Some(m) => (member_preview_path(target, &m.path), raw),
                    None => return WbAction::None,
                }
            }
            _ => return WbAction::None,
        };
        if raw.sheets.len() < 2 {
            return WbAction::None;
        }
        let cur = raw
            .grid_sheet
            .as_ref()
            .and_then(|g| raw.sheets.iter().position(|(n, ..)| n == g))
            .unwrap_or(0);
        let next = cur.saturating_add_signed(dir).min(raw.sheets.len() - 1);
        if next == cur {
            return WbAction::None;
        }
        let name = raw.sheets[next].0.clone();
        self.preview_action(path, Some(name))
    }
```

(`saturating_add_signed` on `usize` is stable; `cur.saturating_add_signed(-1)` at 0 stays 0.)

- [ ] **Step 5: Thread the sheet through the runtime.** In `tdy-tui/src/main.rs`: the dispatch arm becomes `WbAction::PreviewFile { path, sheet } => spawn_wb_preview(preview_tx.clone(), cfg.clone(), path, sheet, wb.preview_gen)`, and `spawn_wb_preview` gains `sheet: Option<String>` (parameter count stays under 8) and calls `raw_head(&path, cfg.limits, sheet.as_deref())`.

- [ ] **Step 6: Say the keys exist.** `HELP_KEYS` gains, after the PgUp/PgDn row: `("[ / ] (file / member)", "previous / next sheet of a workbook")`. In `draw_status`'s `Focus::Main` match, the `Context::File { raw, .. }` arm advertises the keys only when they do something:

```rust
            Context::File { raw, .. } if raw.sheets.len() > 1 => {
                "↑↓ scroll · [ ] sheet · Tab focus · ^Q quit"
            }
            Context::File { .. } => "↑↓ scroll · Tab focus · ^Q quit",
```

(The Member hint is already at the width budget; its `[ ]` lives in the `?` overlay only — deliberate.)

- [ ] **Step 7: Run the suite**

Run: `cargo test --workspace --lib --tests`
Expected: PASS.

- [ ] **Step 8: Commit**

```bash
git add -A tdy-tui
git commit -m "[ and ] page through a workbook's sheets, in the file view and a member's"
```

---

### Task 3: Pile scroll follows the selection

**Files:**
- Modify: `tdy-tui/src/workbench.rs` (fields ~line 135, `key_main` Pile Up/Down arms ~line 930, the `Payload::Fitted` selection-restore in `apply` ~line 455)
- Modify: `tdy-tui/src/wb_ui.rs` (new pure helper near `draw`, ~line 68)
- Modify: `tdy-tui/src/main.rs` (the `run_workbench` loop, right after `terminal.draw` ~line 433)
- Test: `tdy-tui/tests/workbench.rs`

**Interfaces:**
- Consumes: `draw_pile`'s layout (wb_ui.rs:554: one bold header line + one blank line, then one line per member — the selected member `i` is rendered line `2 + i`); `draw`'s vertical layout (wb_ui.rs:69: 1 header row + body + 1 status row) and `draw_right`'s (main pane = body minus `console_rows + 2`, minus 2 for the main block's borders).
- Produces: `Workbench.main_view_rows: usize` (pub, default 20), `Workbench::set_main_view_rows(&mut self, rows: usize)` (ignores 0), `wb_ui::main_inner_rows(height: u16, w: &Workbench) -> usize` (pure).

- [ ] **Step 1: Write the failing tests** — in `tdy-tui/tests/workbench.rs`, building a Pile context the way the file's existing pile tests do (a `PileReport` with ~20 members):

```rust
#[test]
fn pile_selection_drags_the_scroll_with_it() {
    let mut w = wb_with_pile(20); // the file's existing pile-builder helper shape
    w.focus = Focus::Main;
    w.set_main_view_rows(5);
    for _ in 0..10 {
        w.key(press_down());
    }
    // selected = 10 renders on line 2 + 10 = 12; with 5 visible rows the
    // scroll must have advanced so that line is on screen.
    let Context::Pile { selected, .. } = &w.context else { panic!("not a pile") };
    assert_eq!(*selected, 10);
    assert!(w.main_scroll + 5 > 12, "selected line scrolled off: scroll={}", w.main_scroll);
    assert!(w.main_scroll <= 12);
    // Coming back up to the top restores the header too.
    for _ in 0..10 {
        w.key(press_up());
    }
    assert_eq!(w.main_scroll, 0);
}
```

Adapt helper names to the file's own vocabulary; the arithmetic is the contract. Add a sibling asserting the same after a refit lands: apply a `Payload::Fitted` outcome (as the existing selection-preservation tests do) with the previously-selected member deep in the list and `main_view_rows = 5`, then assert the restored selection's line is within `[main_scroll, main_scroll + 5)`.

- [ ] **Step 2: Run to verify failure**

Run: `cargo test -p tdy-tui --test workbench drags_the_scroll`
Expected: FAIL — `set_main_view_rows` does not exist.

- [ ] **Step 3: Implement the follow rule.** In `workbench.rs`, new field + methods:

```rust
    /// Rows the main pane can actually show, told to us by the runtime
    /// (`wb_ui::main_inner_rows` after each draw) — the state machine does
    /// no terminal I/O, so this is how "keep the selection visible" learns
    /// what visible means. Default 20: close enough for the first frame,
    /// corrected before the first key can move a selection.
    pub main_view_rows: usize,

    pub fn set_main_view_rows(&mut self, rows: usize) {
        // 0 means "the main pane is not on screen" (console zoomed) — keep
        // the last real height rather than collapse the follow window.
        if rows > 0 {
            self.main_view_rows = rows;
        }
    }

    /// Keep the selected pile row visible: `draw_pile` renders member `i`
    /// on line `2 + i` (bold header + blank). Selection 0 goes back to a
    /// scroll of 0 so the header is visible again at the top.
    fn follow_pile_selection(&mut self) {
        let Context::Pile { selected, .. } = &self.context else { return };
        if *selected == 0 {
            self.main_scroll = 0;
            return;
        }
        let line = 2 + *selected;
        let rows = self.main_view_rows.max(1);
        if line < self.main_scroll {
            self.main_scroll = line;
        } else if line >= self.main_scroll + rows {
            self.main_scroll = line + 1 - rows;
        }
    }
```

Initialize `main_view_rows: 20` in `Workbench::new`. Call `self.follow_pile_selection()` at the end of the Pile `KeyCode::Up` and `KeyCode::Down` arms in `key_main`, and in `apply`'s `Payload::Fitted` arm immediately after the selection restore / `main_scroll` reset (the reset stays; the follow then moves the window onto the restored selection — this closes the slice-4 deferral "refit returns the pile to its top even when the preserved selection was deep").

- [ ] **Step 4: The pure viewport helper.** In `wb_ui.rs`, near `draw` (pub, pure — reads, never mutates):

```rust
/// How many rows of content the main pane can show at `height` terminal
/// rows — the same arithmetic `draw`/`draw_right` perform with Layout:
/// 1 header row + 1 status row around the body, `console_rows + 2` for the
/// console pane, 2 for the main block's own borders. 0 when the console is
/// zoomed (no main pane on screen) — `set_main_view_rows` ignores 0.
pub fn main_inner_rows(height: u16, w: &Workbench) -> usize {
    if w.zoom {
        return 0;
    }
    let body = height.saturating_sub(2);
    let main = body.saturating_sub(w.console_rows + 2);
    main.saturating_sub(2) as usize
}
```

In `run_workbench` (`tdy-tui/src/main.rs`), right after `terminal.draw(|f| wb_ui::draw(f, &mut wb))?;`:

```rust
        let size = terminal.size()?;
        wb.set_main_view_rows(wb_ui::main_inner_rows(size.height, &wb));
```

(Every loop iteration — covers terminal resize, `^Up`/`^Down` console resizes, and zoom toggles without any event plumbing.)

- [ ] **Step 5: Run the suite**

Run: `cargo test --workspace --lib --tests`
Expected: PASS, including the untouched slice-4 tests that pin `main_scroll = 0` on context *entry* (the follow only moves it on selection *moves* and post-restore).

- [ ] **Step 6: Commit**

```bash
git add -A tdy-tui
git commit -m "the pile's scroll follows its selection — down a long list, and back after a refit"
```

---

### Task 4: Honest single-line-target error, and the tidy sweep

**Files:**
- Modify: `tdy-tui/src/remedy.rs` (`column_line` ~line 258 + its `#[cfg(test)]` module)
- Modify: `tdy-tui/src/wb_ui.rs` (`draw_status` ~line 845)
- Modify: `tdy-tui/src/workbench.rs` (`record_target` doc comment; every assignment of `Context::Empty` that follows a scrolled context)
- Test: unit tests in `tdy-tui/src/remedy.rs`; `tdy-tui/tests/workbench.rs`

**Interfaces:**
- Consumes: `column_list_end(sql)`, `lines_with_endings`, `strip_end` (all exist in remedy.rs, used by `column_line` today).
- Produces: no new public API.

- [ ] **Step 1: Write the failing remedy test** — in remedy.rs's existing test module:

```rust
#[test]
fn single_line_target_gets_the_reformat_hint_not_a_lie() {
    let sql = "CREATE TABLE t (a TEXT, b BIGINT) WITH (format = 'csv');\n";
    let err = column_line(sql, "b").unwrap_err().to_string();
    assert!(err.contains("one column per line"), "{err}");
    // A column that is genuinely absent keeps the plain message.
    let err = column_line(sql, "zzz").unwrap_err().to_string();
    assert!(err.contains("no line in this target declares `zzz`"), "{err}");
    assert!(!err.contains("one column per line"), "{err}");
}
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test -p tdy-tui --lib single_line_target`
Expected: FAIL — both errors currently read "no line in this target declares".

- [ ] **Step 3: Implement.** Replace `column_line`'s final `bail!` with a diagnosis: the column *is* in the list region but shares its line with others → say what to do; otherwise the original message:

```rust
    // The loop above matches a line's FIRST identifier. Before declaring
    // the column absent, check whether it merely shares a line with other
    // declarations (a hand-minified target): remedies edit one column per
    // line by design (see the module doc — splicing inside a shared line
    // risks corrupting its neighbors), so the honest error names the
    // reformat, not a phantom missing column.
    let in_list = lines_with_endings(sql)
        .iter()
        .map(|l| strip_end(l))
        .take(end)
        .any(|line| {
            line.split(|c: char| !(c.is_alphanumeric() || c == '_' || c == '"'))
                .any(|w| w.trim_matches('"').eq_ignore_ascii_case(column))
        });
    if in_list {
        bail!(
            "`{column}` shares a line with other declarations — remedies edit one column \
             per line; reformat the target (one column per line, as `tdy draft` writes it) \
             or edit it by hand"
        );
    }
    bail!("no line in this target declares `{column}`")
```

(Adapt to the actual local names in scope; `end` is already computed at the top of the function.)

- [ ] **Step 4: The tidy sweep**, three independent one-liners:
  1. `wb_ui.rs:845`: `Constraint::Length(keys.len() as u16 + 2)` → `Constraint::Length(keys.chars().count() as u16 + 2)` — the hint strings carry multi-byte `↑↓·`, so `len()` over-reserves and pushes the left status text short.
  2. `workbench.rs`: `grep -n "Context::Empty" tdy-tui/src/workbench.rs` — every assignment that *leaves* a scrolled context for `Context::Empty` (e.g. Esc closing a pile) also sets `self.main_scroll = 0;`, matching the "a context CHANGE resets" rule the `Payload::Query` arm documents. Add a workbench test: scroll a pile (`main_scroll > 0` via PgDn), close it to Empty, assert `main_scroll == 0`. If every such site already resets, keep the test (it pins the rule) and touch nothing.
  3. `record_target`'s doc comment gains one sentence: "If a fit-family command ever grows a *valued* flag, the flag's value would be mistaken for the target here — extend the skip below when that happens."

- [ ] **Step 5: Run the suite**

Run: `cargo test --workspace --lib --tests`
Expected: PASS.

- [ ] **Step 6: Commit**

```bash
git add -A tdy-tui
git commit -m "remedies name the single-line-target limitation; status width counts chars; Empty resets scroll"
```

---

### Task 5: Docs — spec, README, CLAUDE.md

**Files:**
- Modify: `docs/design/2026-09-01-console-and-workbench.md` (§3/§7 `.show` rows ~line 67 and ~line 231, the §11 slice-4 note ~lines 373-377, plus a new slice-5 status note in §11)
- Modify: `README.md` (workbench key list and `.show` documentation)
- Modify: `CLAUDE.md` (slice status paragraph, test count)

**Interfaces:** none — prose only, but every claim must be verified against the code landed in Tasks 1-4 before it is written.

- [ ] **Step 1: Spec.** Update the `.show FILE` grammar row (~line 67) to `.show FILE [--sheet NAME]`; rewrite the §7 sentence at ~line 231 so the workbook raw view reads: sheet shapes, then the grid of one sheet — the first by default, any other via `[`/`]` (workbench) or `--sheet` (console) — "a tab per sheet" is DONE in this keyboard form, not tabs, and say why (tabs would add a second selection widget for what two keys already do). Update the §11 slice-4 note's "a tab per sheet remains future work" to point at the slice-5 note, and append the slice-5 note: sheet switching, scroll-follows-selection (and that it closes the refit-returns-to-top deferral), the single-line-target error, the status-width fix.

- [ ] **Step 2: README.** In the workbench section's key list add `[ / ]  previous / next sheet of a workbook`; where `.show` is documented add `--sheet NAME`. Verify the exact current wording with `grep -n "show\|\[ /" README.md` first and match the list's format.

- [ ] **Step 3: CLAUDE.md.** In the slice-status prose, replace "tab-per-sheet is future work" phrasing (grep for it) with one sentence on the slice-5 state, and update the test count: run `cargo test --workspace --lib --tests 2>&1 | grep -E "^test result" | awk '{s+=$4} END {print s}'` and write the real number.

- [ ] **Step 4: Verify claims.** Re-read each edited passage against the code (the grammar row against `parse.rs`, the key list against `key_main`, the counts against the suite output). No number or key name goes in unverified.

- [ ] **Step 5: Commit**

```bash
git add docs README.md CLAUDE.md
git commit -m "docs: sheets are pageable, the pile follows its selection, and the counts are true"
```

---

## Self-Review Notes

- **Spec coverage:** §7's tab-per-sheet deferral → Tasks 1-2 (keyboard form, ruled in T2's Step 3 comment and §11 note). Slice-4 parked items → T3 (refit scroll), T4 (record_target comment, status width). Slice-3 ticket remedy-single-line → T4. §3 grammar change → T1 + T5.
- **Deliberately not taken:** literal rendered *tabs* for sheets (two keys beat a widget; ruled in T5's spec note), wrapping `[`/`]` (clamping is visible honesty), sheet switching as a synthesized console line (a view change, like arrow-move preview — ruled in T2), splicing remedies into single-line targets (corruption risk; the error now says so).
- **Type consistency:** `raw_head(&Path, Limits, Option<&str>)` (T1) is what T2's `spawn_wb_preview` calls with `sheet.as_deref()`; `WbAction::PreviewFile { path, sheet }` (T2) is matched in T2's main.rs arm; `set_main_view_rows`/`main_inner_rows` names match between T3's workbench and wb_ui/main edits; `grid_sheet` is read by both renderers in T1 and by `switch_sheet` in T2.
- Line numbers are as of `d87e8f7` and are anchors, not gospel — every step names the function too.
