# The workbench frame (slice 2) — implementation plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** `tdy ui` with no argument opens the three-pane workbench — file browser left, main pane right, the slice-1 console at the bottom — on the current directory; the old target-centric review flow stays reachable behind a target argument, unchanged.

**Architecture:** The workbench is new state beside the old `App`, not a rewrite of it: `browser.rs` (tree state over `tdy::console::list_dir`), `workbench.rs` (a pure state machine — `crossterm` key events in, `WbAction` out — owning focus, the console scrollback, the embedded `LineEditor`, and the main-pane `Context`), `wb_ui.rs` (rendering only). A dedicated tokio task owns a `tdy::console::Session` and executes one line at a time from an mpsc channel; every action in the UI — typed line or browser shortcut — goes through that channel as a console line, so there is one code path and the scrollback is the audit trail. Main-pane contexts in this slice: `Empty` and the two File views (raw; raw + spec summary with the decisions list). Pile/Member/Evidence views migrate in slice 3.

**Tech Stack:** Rust ≥ 1.88, ratatui 0.30 + crossterm 0.29 (already tdy-tui deps), tokio, `tdy::console::{Session, Outcome, Payload, Entry, EntryStatus, Table, RawHead, SpecSummary, list_dir, raw_head, spec_summary, line::LineEditor, repl::{load_history, append_history}}`.

**Spec:** `docs/design/2026-09-01-console-and-workbench.md` — §6 (frame), §7 (Empty + File contexts), §10 (tests), §11 slice 2. One in-use revision binds this slice: bare `tdy` keeps opening the console (spec §5 revision note, 2026-09-02); the workbench's doors are `tdy ui` and `tdy-tui`, with no argument.

## Global Constraints

- `cargo test --workspace --lib --tests` green after every task (**`--workspace` is what builds tdy-tui**; plain `cargo test` has a known spurious doc-test failure on this machine).
- CI runs `cargo clippy --all-targets -- -D warnings`; clippy is NOT installed locally. No unused imports; `writeln!` never `write!(s, "...\n")`; `#[allow(dead_code)]` only with a comment naming the task that uses it.
- **One code path:** a browser shortcut NEVER calls a lib function directly — it synthesizes a console line and dispatches it like typed input. The scrollback records every action (spec §6: "There is no second code path").
- **The old flow must not regress:** `tdy-tui <target>` (and `tdy ui <target>`) runs today's `App`/`ui` screens byte-identically; `tdy-tui/tests/render.rs` and `preview.rs` stay green untouched.
- The workbench renders and mutates nothing in `wb_ui.rs`; everything that decides lives in `workbench.rs`/`browser.rs` with plain tests (the repo's load-bearing TUI rule).
- `Msg::Note` vs `Msg::Progress` discipline: a transient remark must never leave the UI busy (CLAUDE.md's standing rule).
- A session leaves behind exactly the files a CLI session would — no parallel state.
- Commit after every task; end every commit message with exactly:
  `Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>`
  `Claude-Session: https://claude.ai/code/session_01DmEku7uNkLUeyiNE38sho8`
- No `git stash` (shared stash stack across worktrees).
- Tests never need a network or model (`backend = none`).

## File structure

| file | responsibility |
|---|---|
| `tdy-tui/src/browser.rs` | tree state: root, current dir, entries (via `tdy::console::list_dir`), selection, enter/up/refresh — no rendering, no dispatch |
| `tdy-tui/src/workbench.rs` | the frame's state machine: `Focus`, `Context`, scrollback, `LineEditor` embed, key → `WbAction`; `apply(Outcome)`; busy/status fields |
| `tdy-tui/src/wb_ui.rs` | draws the three panes + status line from `&Workbench`; mutates nothing |
| `tdy-tui/src/main.rs` | entry split (no arg → workbench, arg → old flow), the workbench event loop, the console-worker task, `.edit` suspend |
| `tdy-tui/src/lib.rs` | `pub mod browser; pub mod workbench; pub mod wb_ui;` |
| `tdy-tui/tests/workbench.rs` | state-machine tests: focus cycling, browser keys, shortcut synthesis, context switching, audit-trail property |
| `tdy-tui/tests/wb_render.rs` | `TestBackend` render tests: frame at two sizes, focus borders, browser status column, Empty and File views, scrollback |
| `README.md`, `CLAUDE.md` | Task 7 |

The old `app.rs`, `ui.rs`, `remedy.rs`, `tests/render.rs`, `tests/preview.rs` are not modified in this slice.

---

### Task 1: `browser.rs` — the tree state

**Files:**
- Create: `tdy-tui/src/browser.rs`
- Modify: `tdy-tui/src/lib.rs` (add `pub mod browser;`)
- Test: `tdy-tui/src/browser.rs` `mod tests`

**Interfaces:**
- Consumes: `tdy::console::{list_dir, Entry, EntryKind, EntryStatus}` (`list_dir(dir: &Path) -> anyhow::Result<Vec<Entry>>`; `Entry { name: String, kind: EntryKind, status: EntryStatus }`, dirs first with trailing `/`, companions folded into status).
- Produces:

```rust
pub struct Browser {
    root: PathBuf,          // canonical; never left
    pub dir: PathBuf,       // current directory, under root
    pub entries: Vec<Entry>,
    pub selected: usize,
    pub error: Option<String>,   // list_dir failure, shown instead of entries
}
impl Browser {
    pub fn new(root: &Path) -> anyhow::Result<Browser>;   // canonicalizes, refreshes
    pub fn refresh(&mut self);                             // re-list dir; clamp selected; error -> self.error
    pub fn up(&mut self) -> bool;                          // false at root
    pub fn enter(&mut self) -> Option<PathBuf>;            // dir: descend+refresh, None; file/target: Some(abs path)
    pub fn move_sel(&mut self, delta: i32);
    pub fn selected_entry(&self) -> Option<&Entry>;
    /// Absolute path of the selection (None on empty dir or error).
    pub fn selected_path(&self) -> Option<PathBuf>;
    /// The selection as the console should spell it: relative to `dir`.
    pub fn selected_rel(&self) -> Option<String>;
    /// Root-relative label for the pane title ("." at root).
    pub fn title(&self) -> String;
}
```

- [ ] **Step 1: Write the failing tests** in `tdy-tui/src/browser.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use tdy::console::EntryKind;

    fn pile() -> tempfile::TempDir {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join("b.csv"), "A;B\n1;2\n").unwrap();
        std::fs::write(d.path().join("a.csv"), "A;B\n1;2\n").unwrap();
        std::fs::write(d.path().join("t.tdy.sql"), "CREATE TABLE t (a TEXT) WITH (files='*.csv');").unwrap();
        std::fs::write(d.path().join("a.csv.tdy.toml"), "junk").unwrap(); // companion: never an entry
        std::fs::create_dir(d.path().join("sub")).unwrap();
        std::fs::write(d.path().join("sub/c.csv"), "A\n1\n").unwrap();
        d
    }

    #[test]
    fn lists_dirs_first_companions_hidden_and_navigates() {
        let d = pile();
        let mut b = Browser::new(d.path()).unwrap();
        let names: Vec<&str> = b.entries.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, ["sub/", "a.csv", "b.csv", "t.tdy.sql"]);
        assert_eq!(b.title(), ".");

        // Enter the directory; selection resets; up() returns to root.
        assert_eq!(b.enter(), None);
        assert_eq!(b.title(), "sub");
        assert_eq!(b.entries.len(), 1);
        assert!(b.up());
        assert!(!b.up(), "cannot go above the root");

        // Enter on a file returns its absolute path.
        b.move_sel(1);
        assert_eq!(b.selected_rel().as_deref(), Some("a.csv"));
        let p = b.enter().unwrap();
        assert!(p.ends_with("a.csv") && p.is_absolute());
    }

    #[test]
    fn selection_clamps_and_survives_refresh() {
        let d = pile();
        let mut b = Browser::new(d.path()).unwrap();
        b.move_sel(100);
        assert_eq!(b.selected, b.entries.len() - 1);
        b.move_sel(-100);
        assert_eq!(b.selected, 0);
        std::fs::remove_file(d.path().join("t.tdy.sql")).unwrap();
        b.move_sel(100);
        b.refresh();
        assert!(b.selected < b.entries.len());
    }

    #[test]
    fn selected_rel_is_relative_to_the_current_dir() {
        let d = pile();
        let mut b = Browser::new(d.path()).unwrap();
        b.enter(); // into sub/
        assert_eq!(b.selected_rel().as_deref(), Some("c.csv"));
    }
}
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test -p tdy-tui --lib browser`
Expected: compile error (module missing).

- [ ] **Step 3: Implement** — `Browser::new` canonicalizes root, sets `dir = root`, calls `refresh()`. `refresh` calls `tdy::console::list_dir(&self.dir)`; on `Err(e)` set `self.error = Some(format!("{e:#}"))` and clear entries; clamp `selected` to `entries.len().saturating_sub(1)` (0 when empty). `enter` on `EntryKind::Dir` pushes the name (trailing `/` stripped) onto `dir`, refreshes, resets `selected = 0`, returns `None`; otherwise returns `selected_path()`. `up` pops one component unless `dir == root` (compare canonical paths), refreshes, returns whether it moved. `selected_rel` is the entry name with any trailing `/` stripped. `title` is `dir.strip_prefix(&root)` or `"."`. `move_sel` saturates at both ends.

- [ ] **Step 4: Run the tests**

Run: `cargo test -p tdy-tui --lib browser`
Expected: 3 pass. Then `cargo test --workspace --lib --tests` (full net).

- [ ] **Step 5: Commit**

```bash
git add tdy-tui/src/browser.rs tdy-tui/src/lib.rs
git commit -m "workbench browser: tree state over console::list_dir — dirs first, companions folded, root is the floor"
```

---

### Task 2: `workbench.rs` — the frame's state machine

**Files:**
- Create: `tdy-tui/src/workbench.rs`
- Modify: `tdy-tui/src/lib.rs` (add `pub mod workbench;`)
- Test: `tdy-tui/tests/workbench.rs`

**Interfaces:**
- Consumes: `browser::Browser`; `tdy::console::{Outcome, Payload, Table, RawHead, SpecSummary, line::{LineEditor, Edit}}`; `crossterm::event::{KeyCode, KeyEvent, KeyModifiers}`.
- Produces (Task 3 renders this; Task 4-5 drive it):

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Focus { Console, Browser, Main }

/// What the main pane shows. Slice 2: Empty and the File views; a completed
/// query's Table also lands here so SQL results are not scrollback-only.
#[derive(Debug, Clone, Default)]
pub enum Context {
    #[default]
    Empty,
    File { path: PathBuf, raw: RawHead, spec: Option<SpecSummary>, preview: Option<Table> },
    Query(Table),
}

/// One scrollback cell: the echoed line, then its text.
#[derive(Debug, Clone)]
pub struct Cell { pub echo: String, pub text: String, pub ok: bool }

/// What the UI wants the runtime to do. The runtime owns all I/O.
#[derive(Debug, Clone, PartialEq)]
pub enum WbAction {
    None,
    Quit,
    /// Send this line to the console worker (typed, or synthesized by a shortcut).
    Dispatch(String),
    /// Compute a preview of this file for the main pane (arrow-move preview).
    PreviewFile(PathBuf),
    /// Run $EDITOR on this path (comes back via Payload::Edit too).
    Edit(PathBuf),
}

pub struct Workbench {
    pub browser: Browser,
    pub focus: Focus,
    pub context: Context,
    pub scrollback: Vec<Cell>,
    pub scroll: usize,            // lines scrolled up from the bottom
    pub editor: LineEditor,
    pub console_rows: u16,        // default 8; Ctrl-Up/Down; min 3, max 30
    pub zoom: bool,               // Ctrl-L: console takes the whole right column
    pub busy: Option<String>,     // a command is running; what it said last
    pub status: String,           // transient note
    pub should_quit: bool,
}

impl Workbench {
    pub fn new(browser: Browser, history: Vec<String>) -> Workbench;
    /// One key in, one action out. Pure.
    pub fn key(&mut self, k: KeyEvent) -> WbAction;
    /// A dispatched line has started running (echo it, mark busy).
    pub fn begin(&mut self, line: &str);
    /// The worker finished a line: record it, update the context.
    pub fn apply(&mut self, o: Outcome) -> Option<WbAction>;
    /// Progress/note from the worker's sink.
    pub fn progress(&mut self, what: String);
    pub fn note(&mut self, what: String);
    pub fn prompt(&self) -> &'static str;   // "tdy> " or "   -> " (sql pending is tracked via Payload::Continue)
}
```

Key rules (spec §6), all to be implemented and tested:
- `Tab` cycles Console → Browser → Main → Console; `Esc` anywhere → Console. Focus `Console` feeds keys to `editor.key()`; `Edit::Submit(line)` → `WbAction::Dispatch(line)` (empty/whitespace line → `None`); `Edit::Interrupt` on empty prompt → `None` (quit is Ctrl-C twice? No — plain `Ctrl-C` with an empty editor sets a status hint "Ctrl-Q quits"; `Ctrl-Q` quits from anywhere, and `q` quits when focus is Browser or Main).
- While `busy.is_some()`: only `Ctrl-Q` (quit) and `Tab`/`Esc` (focus) act; everything else is swallowed — one command at a time, matching the console's one-Session serialization.
- Browser focus: `Up`/`Down` move and return `WbAction::PreviewFile(path)` for the newly selected data file (not for dirs/targets — targets preview in slice 3); `Enter` on a dir descends (`None`), on a file returns `PreviewFile`; `Backspace` goes up; `s` → `Dispatch(".sniff <rel>")`; `e` → `Dispatch(".edit <rel>")` (paths via `browser.selected_rel()`, quoted with the same rule the console's `quote_rel` uses: Debug-quote when it contains whitespace); if the browser's dir differs from the session's cwd, prefix the dispatch with a `.cd` — **no**: keep them in lockstep instead — entering a directory dispatches `Dispatch(".cd <rel>")` and `up()` dispatches `Dispatch(".cd ..")`, so the browser's dir IS the session's cwd and `selected_rel()` is always valid. (`enter()`/`up()` still mutate the browser immediately; the `.cd` outcome refreshes status only. This is the audit trail for navigation too.)
- Main focus: `Up`/`Down` scroll the File view (a `pub main_scroll: usize` field); other keys `None` in this slice.
- Console focus extras: `PgUp`/`PgDn` adjust `scroll` by 5 (clamped; any dispatch resets it to 0); `Ctrl-L` toggles `zoom`; `Ctrl-Up`/`Ctrl-Down` resize `console_rows` within [3, 30].
- `apply(o)`: push `Cell { echo: o.echo, text: o.text, ok: o.ok }` (skip pushing when both echo and text are empty — a buffered SQL line still echoes), clear `busy`, and update context from the payload: `Shown { path, raw, spec }` → `Context::File { path, raw, spec, preview: None }`; `Sniffed { path, spec, preview, .. }` → `Context::File` with `raw` recomputed? No — the state machine does no I/O: `Sniffed` sets `Context::File { path, raw: RawHead::default(), spec: Some(spec), preview: Some(preview) }` and returns `Some(WbAction::PreviewFile(path))` so the runtime fills the raw half in; `Query(t)` → `Context::Query(t)`; `Edit(p)` → return `Some(WbAction::Edit(p))`; `Quit` → `should_quit = true`; everything else leaves the context alone. (`RawHead` needs `#[derive(Default)]` — one-line change in `src/console/mod.rs` if it lacks it.)
- `begin(line)` sets `busy = Some(line.to_string())` and remembers the line in the editor's history via `editor.remember(line)`.

- [ ] **Step 1: Write the failing tests** — `tdy-tui/tests/workbench.rs`:

```rust
use std::path::PathBuf;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use tdy::console::{Outcome, Payload, RawHead, SpecSummary, Table};
use tdy_tui::browser::Browser;
use tdy_tui::workbench::{Context, Focus, WbAction, Workbench};

fn key(c: KeyCode) -> KeyEvent { KeyEvent::new(c, KeyModifiers::NONE) }
fn ctrl(c: char) -> KeyEvent { KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL) }
fn type_line(w: &mut Workbench, s: &str) -> WbAction {
    for ch in s.chars() { w.key(key(KeyCode::Char(ch))); }
    w.key(key(KeyCode::Enter))
}

fn pile() -> tempfile::TempDir {
    let d = tempfile::tempdir().unwrap();
    std::fs::write(d.path().join("a.csv"), "A;B\n1;2\n").unwrap();
    std::fs::write(d.path().join("b.csv"), "A;B\n3;4\n").unwrap();
    std::fs::create_dir(d.path().join("sub")).unwrap();
    std::fs::write(d.path().join("sub/c.csv"), "A\n1\n").unwrap();
    d
}
fn wb(d: &tempfile::TempDir) -> Workbench {
    Workbench::new(Browser::new(d.path()).unwrap(), vec![])
}
fn outcome(echo: &str, text: &str, payload: Payload) -> Outcome {
    Outcome { echo: echo.into(), text: text.into(), payload, ok: true }
}

#[test]
fn focus_cycles_and_esc_returns_to_console() {
    let d = pile();
    let mut w = wb(&d);
    assert_eq!(w.focus, Focus::Console);
    w.key(key(KeyCode::Tab));
    assert_eq!(w.focus, Focus::Browser);
    w.key(key(KeyCode::Tab));
    assert_eq!(w.focus, Focus::Main);
    w.key(key(KeyCode::Tab));
    assert_eq!(w.focus, Focus::Console);
    w.key(key(KeyCode::Tab));
    w.key(key(KeyCode::Esc));
    assert_eq!(w.focus, Focus::Console);
}

#[test]
fn typing_dispatches_and_browser_shortcut_is_the_same_line() {
    let d = pile();
    let mut w = wb(&d);
    // Typed.
    assert_eq!(type_line(&mut w, ".sniff a.csv"), WbAction::Dispatch(".sniff a.csv".into()));
    // Shortcut: select a.csv in the browser, press s.
    w.key(key(KeyCode::Tab)); // Browser
    w.key(key(KeyCode::Down)); // sub/ -> a.csv
    let act = w.key(key(KeyCode::Char('s')));
    assert_eq!(act, WbAction::Dispatch(".sniff a.csv".into())); // THE audit-trail property
}

#[test]
fn navigation_dispatches_cd_and_keeps_rel_paths_valid() {
    let d = pile();
    let mut w = wb(&d);
    w.key(key(KeyCode::Tab)); // Browser; sub/ is selected (dirs first)
    assert_eq!(w.key(key(KeyCode::Enter)), WbAction::Dispatch(".cd sub".into()));
    assert_eq!(w.browser.title(), "sub");
    let act = w.key(key(KeyCode::Char('s')));
    assert_eq!(act, WbAction::Dispatch(".sniff c.csv".into()));
    assert_eq!(w.key(key(KeyCode::Backspace)), WbAction::Dispatch(".cd ..".into()));
    assert_eq!(w.browser.title(), ".");
}

#[test]
fn arrow_move_previews_data_files_only() {
    let d = pile();
    let mut w = wb(&d);
    w.key(key(KeyCode::Tab));
    // From sub/ (dir: no preview) down to a.csv (preview).
    let act = w.key(key(KeyCode::Down));
    assert!(matches!(act, WbAction::PreviewFile(ref p) if p.ends_with("a.csv")), "{act:?}");
}

#[test]
fn busy_swallows_everything_but_quit_and_focus() {
    let d = pile();
    let mut w = wb(&d);
    w.begin(".fit t.tdy.sql");
    assert!(w.busy.is_some());
    assert_eq!(type_line(&mut w, ".ls"), WbAction::None);
    w.key(key(KeyCode::Tab)); // focus still moves
    assert_eq!(w.focus, Focus::Browser);
    assert_eq!(w.key(key(KeyCode::Char('s'))), WbAction::None);
    assert_eq!(w.key(ctrl('q')), WbAction::Quit);
}

#[test]
fn apply_updates_scrollback_and_context() {
    let d = pile();
    let mut w = wb(&d);
    w.begin(".show a.csv");
    let raw = RawHead { lines: vec!["A;B".into(), "1;2".into()], truncated: false, sheets: vec![] };
    let follow = w.apply(outcome(".show a.csv", "a.csv:\n  A;B\n", Payload::Shown {
        path: d.path().join("a.csv"), raw, spec: None,
    }));
    assert!(follow.is_none());
    assert!(w.busy.is_none());
    assert_eq!(w.scrollback.last().unwrap().echo, ".show a.csv");
    assert!(matches!(w.context, Context::File { ref path, .. } if path.ends_with("a.csv")));

    // A query result becomes the main pane's context.
    w.begin("SELECT 1;");
    let t = Table { columns: vec!["a".into()], types: vec![], rows: vec![vec!["1".into()]], total: 1, truncated: false };
    w.apply(outcome("SELECT 1;", "| a |\n", Payload::Query(t)));
    assert!(matches!(w.context, Context::Query(_)));

    // Edit comes back as a follow-up action.
    w.begin(".edit a.csv");
    let follow = w.apply(outcome(".edit a.csv", "", Payload::Edit(d.path().join("a.csv"))));
    assert!(matches!(follow, Some(WbAction::Edit(_))));
}

#[test]
fn zoom_resize_and_scroll_are_console_focus_keys() {
    let d = pile();
    let mut w = wb(&d);
    assert_eq!(w.console_rows, 8);
    w.key(KeyEvent::new(KeyCode::Up, KeyModifiers::CONTROL));
    assert_eq!(w.console_rows, 9);
    w.key(ctrl('l'));
    assert!(w.zoom);
    w.key(key(KeyCode::PageUp));
    assert_eq!(w.scroll, 5);
    type_line(&mut w, ".ls"); // any dispatch resets scroll
    assert_eq!(w.scroll, 0);
}
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test -p tdy-tui --test workbench`
Expected: compile error.

- [ ] **Step 3: Implement `workbench.rs`** per the Interfaces block and key rules above. Notes:
  - `key()` handles `KeyEventKind` — accept only `Press` (the runtime also filters, but the state machine must not double-act if handed repeats).
  - Quit keys: `Ctrl-Q` anywhere → `WbAction::Quit` + `should_quit`; `q` only when focus is Browser or Main.
  - The `.cd` synthesis in `enter()`/`up()` happens in `workbench.rs` (browser mutates, workbench dispatches): on `Enter` over a dir — capture `selected_rel()` BEFORE `browser.enter()`.
  - `Dispatch` lines quote a rel path containing whitespace with `format!("{rel:?}")` (matching the console tokenizer's double-quote handling).
  - `prompt()` returns `"   -> "` after an `apply` whose payload was `Payload::Continue`, `"tdy> "` otherwise (track with a `sql_pending: bool` field set/cleared in `apply`).

- [ ] **Step 4: Run the tests**

Run: `cargo test -p tdy-tui --test workbench` then `cargo test --workspace --lib --tests`
Expected: 7 pass; workspace green.

- [ ] **Step 5: Commit**

```bash
git add tdy-tui/src/workbench.rs tdy-tui/src/lib.rs tdy-tui/tests/workbench.rs src/console/mod.rs
git commit -m "workbench state machine: focus, shortcuts as console lines, contexts — pure, and the audit trail is a test"
```

---

### Task 3: `wb_ui.rs` — rendering the frame

**Files:**
- Create: `tdy-tui/src/wb_ui.rs`
- Modify: `tdy-tui/src/lib.rs` (add `pub mod wb_ui;`)
- Test: `tdy-tui/tests/wb_render.rs`

**Interfaces:**
- Consumes: `Workbench` (all pub fields), `tdy::console::{EntryStatus, EntryKind}`.
- Produces: `pub fn draw(f: &mut ratatui::Frame, w: &mut Workbench)`.

Layout (spec §6): vertical `[Length(1) header, Fill body, Length(1) status]`. Header: ` tdy — <browser.title()>`. Body: horizontal `[Length(26) browser, Fill right]` (browser hidden below 60 columns total: right takes everything); right column vertical `[Fill main, Length(console_rows+2) console]`, except `zoom` → console takes the whole right column. Focused pane gets a thick/colored border; titles ` files `, ` <context title> `, ` console `.

- Browser rows: `name` left, status right, exactly `render_listing`'s vocabulary — `sniffed 0.95 (heuristic)`, `stale`, `target, no lock`, `target, locked`, `target, drift (N)`, blank for none; selected row highlighted with `▸ `.
- Console pane: last `console_rows` lines of the flattened scrollback (each `Cell` renders as `tdy> {echo}` line — dim — followed by its text lines), honoring `scroll`; input line last: `{prompt}{editor.text()}` with the cursor column from `editor.cursor()` (set via `f.set_cursor_position` only when focus is Console).
- Main pane: `Context::Empty` → three orientation lines ("select a file on the left, or type `.help`", the root path, "`tdy ui <target>` opens the classic review flow"); `Context::File`/`Query` render in Task 4 (this task renders File minimally: the raw lines) — the full two-column File view is Task 4's.
- Status line: `busy` (spinner-less, just the text) or `status`, right-aligned key hints per focus (`Tab focus · ^L zoom · ^Q quit`).

- [ ] **Step 1: Write the failing render tests** — `tdy-tui/tests/wb_render.rs`, reusing `tests/render.rs`'s pattern:

```rust
use ratatui::backend::TestBackend;
use ratatui::Terminal;
use tdy_tui::browser::Browser;
use tdy_tui::workbench::Workbench;
use tdy_tui::wb_ui;

fn screen(w: &mut Workbench, cols: u16, rows: u16) -> Vec<String> {
    let mut t = Terminal::new(TestBackend::new(cols, rows)).unwrap();
    t.draw(|f| wb_ui::draw(f, w)).unwrap();
    let buf = t.backend().buffer().clone();
    (0..buf.area.height)
        .map(|y| (0..buf.area.width).map(|x| buf[(x, y)].symbol()).collect::<String>().trim_end().to_string())
        .collect()
}

fn pile() -> tempfile::TempDir {
    let d = tempfile::tempdir().unwrap();
    std::fs::write(d.path().join("a.csv"), "A;B\n1;2\n").unwrap();
    std::fs::write(d.path().join("t.tdy.sql"), "CREATE TABLE t (a TEXT) WITH (files='*.csv');").unwrap();
    d
}

#[test]
fn the_frame_shows_three_panes_and_the_status_vocabulary() {
    let d = pile();
    let mut w = Workbench::new(Browser::new(d.path()).unwrap(), vec![]);
    let text = screen(&mut w, 100, 30).join("\n");
    assert!(text.contains(" files "), "{text}");
    assert!(text.contains(" console "), "{text}");
    assert!(text.contains("a.csv"), "{text}");
    assert!(text.contains("target, no lock"), "{text}");
    assert!(text.contains("tdy>"), "{text}");
    assert!(text.contains("select a file"), "{text}");
}

#[test]
fn narrow_terminals_drop_the_browser_not_the_console() {
    let d = pile();
    let mut w = Workbench::new(Browser::new(d.path()).unwrap(), vec![]);
    let text = screen(&mut w, 50, 20).join("\n");
    assert!(!text.contains(" files "), "{text}");
    assert!(text.contains("tdy>"), "{text}");
}

#[test]
fn scrollback_shows_echo_then_text_and_busy_shows_in_status() {
    let d = pile();
    let mut w = Workbench::new(Browser::new(d.path()).unwrap(), vec![]);
    w.begin(".ls");
    let text = screen(&mut w, 100, 30).join("\n");
    assert!(text.contains(".ls"), "{text}");
    use tdy::console::{Outcome, Payload};
    w.apply(Outcome { echo: ".ls".into(), text: "a.csv  sniffed\n".into(), payload: Payload::Nothing, ok: true });
    w.progress("fitting a.csv (1 of 9)".into());
    let text = screen(&mut w, 100, 30).join("\n");
    assert!(text.contains("tdy> .ls"), "{text}");
    assert!(text.contains("a.csv  sniffed"), "{text}");
    assert!(text.contains("fitting a.csv (1 of 9)"), "{text}");
}
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test -p tdy-tui --test wb_render`
Expected: compile error.

- [ ] **Step 3: Implement `wb_ui.rs`** per the layout above. Rendering only; no `&mut` use beyond what ratatui's stateful widgets need. `progress()` in Task 2's Workbench sets `busy = Some(what)` — confirm that's how the test's status expectation is met.

- [ ] **Step 4: Run**

Run: `cargo test -p tdy-tui --test wb_render` then `cargo test --workspace --lib --tests`
Expected: green.

- [ ] **Step 5: Commit**

```bash
git add tdy-tui/src/wb_ui.rs tdy-tui/src/lib.rs tdy-tui/tests/wb_render.rs
git commit -m "workbench rendering: three panes, focus borders, the browser's status vocabulary, scrollback with echoes"
```

---

### Task 4: the File views — raw, and raw beside the spec with its decisions

**Files:**
- Modify: `tdy-tui/src/wb_ui.rs`, `tdy-tui/src/workbench.rs` (only if a field is missing)
- Test: `tdy-tui/tests/wb_render.rs`

**Interfaces:**
- Consumes: `Context::File { path, raw, spec, preview }`, `SpecSummary { method, confidence, extraction, transforms, columns: Vec<(String,String,String)>, notes }`, `RawHead { lines, truncated, sheets }`, `Table`.

Spec §7's two File views:
- **No sidecar** (`spec: None`): the raw head as-is (lines, or `sheet "Name": R row(s) x C col(s)` per sheet for workbooks), a facts footer (`not sniffed — press s`), and it "must never look as though tdy has an opinion yet" — no columns, no types.
- **With sidecar** (`spec: Some`): two columns — left the raw head (the file's own spelling), right the spec summary: `name ← "source" : TYPE` per column, the notes as a **decisions list** (each note prefixed `•`), confidence line colored red below 0.8 (`tdy::config` has no exported threshold — use a local `const ESCALATION: f32 = 0.8;` with a comment), and the preview table at the bottom when present.
- `Context::Query(t)`: the table with column headers, row count line `N row(s)` + `(truncated)` marker.

- [ ] **Step 1: Write the failing tests** (append to `wb_render.rs`):

```rust
#[test]
fn a_file_without_a_sidecar_shows_raw_only_and_no_opinion() {
    let d = pile();
    let mut w = Workbench::new(Browser::new(d.path()).unwrap(), vec![]);
    use tdy::console::{Outcome, Payload, RawHead};
    w.begin(".show a.csv");
    w.apply(Outcome {
        echo: ".show a.csv".into(), text: String::new(), ok: true,
        payload: Payload::Shown {
            path: d.path().join("a.csv"),
            raw: RawHead { lines: vec!["A;B".into(), "1;2".into()], truncated: true, sheets: vec![] },
            spec: None,
        },
    });
    let text = screen(&mut w, 100, 30).join("\n");
    assert!(text.contains("A;B") && text.contains("1;2"), "{text}");
    assert!(text.contains("…"), "truncation marker: {text}");
    assert!(text.contains("not sniffed"), "{text}");
    assert!(!text.contains("TEXT") && !text.contains("<-"), "no opinion yet: {text}");
}

#[test]
fn a_sniffed_file_shows_raw_beside_the_spec_and_its_decisions() {
    let d = pile();
    let mut w = Workbench::new(Browser::new(d.path()).unwrap(), vec![]);
    use tdy::console::{Outcome, Payload, RawHead, SpecSummary, Table};
    w.begin(".sniff a.csv");
    let spec = SpecSummary {
        method: "heuristic".into(), confidence: Some(0.6),
        extraction: r#"{"format":"delimited"}"#.into(),
        transforms: vec![r#"{"op":"promote_header"}"#.into()],
        columns: vec![("betrag".into(), "Betrag".into(), "DECIMAL(38,2)".into())],
        notes: vec!["ambiguous date order".into()],
    };
    let preview = Table { columns: vec!["betrag".into()], types: vec![], rows: vec![vec!["1.00".into()]], total: 1, truncated: false };
    let follow = w.apply(Outcome {
        echo: ".sniff a.csv".into(), text: String::new(), ok: true,
        payload: Payload::Sniffed { path: d.path().join("a.csv"), spec, preview, kept_existing: false },
    });
    assert!(follow.is_some(), "sniffed context asks the runtime for the raw half");
    let text = screen(&mut w, 110, 34).join("\n");
    assert!(text.contains("betrag") && text.contains("Betrag") && text.contains("DECIMAL(38,2)"), "{text}");
    assert!(text.contains("ambiguous date order"), "the decisions list: {text}");
    assert!(text.contains("0.60"), "confidence shown: {text}");
}

#[test]
fn a_query_context_shows_the_table_and_counts() {
    let d = pile();
    let mut w = Workbench::new(Browser::new(d.path()).unwrap(), vec![]);
    use tdy::console::{Outcome, Payload, Table};
    w.begin("SELECT 1;");
    let t = Table {
        columns: vec!["region".into(), "total".into()], types: vec![],
        rows: vec![vec!["Ost".into(), "14200.00".into()]], total: 500, truncated: true,
    };
    w.apply(Outcome { echo: "SELECT 1;".into(), text: String::new(), ok: true, payload: Payload::Query(t) });
    let text = screen(&mut w, 100, 30).join("\n");
    assert!(text.contains("region") && text.contains("14200.00"), "{text}");
    assert!(text.contains("500 row(s)") && text.contains("truncated"), "{text}");
}
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test -p tdy-tui --test wb_render`
Expected: new tests fail.

- [ ] **Step 3: Implement** the three context renderers in `wb_ui.rs` (two-column split `[Percentage(50), Percentage(50)]` for the sniffed view; `main_scroll` offsets the raw lines; workbook `sheets` render as their `sheet "N": R row(s) x C col(s)` lines in the raw half).

- [ ] **Step 4: Run**

Run: `cargo test -p tdy-tui --test wb_render` then the workspace suite.
Expected: green.

- [ ] **Step 5: Commit**

```bash
git add tdy-tui/src/wb_ui.rs tdy-tui/src/workbench.rs tdy-tui/tests/wb_render.rs
git commit -m "workbench File views: raw with no opinion, raw beside the spec's decisions, query tables in the main pane"
```

---

### Task 5: the runtime — console worker, event loop, entry split

**Files:**
- Modify: `tdy-tui/src/main.rs`
- Test: `tdy-tui/tests/workbench.rs` (one addition), plus the old suites as the regression net

**Interfaces:**
- Consumes: everything above; `tdy::console::{Session, repl::{load_history, append_history}}`; `tdy::progress`; the existing `run_editor`/`reenter`/panic-hook machinery in `main.rs`.
- Produces: `tdy-tui [PATH]` behaviour — no argument → the workbench on the current directory; a `.tdy.sql` argument (or a directory containing exactly one, via the existing `discover_target`) → the OLD review flow, unchanged; a data-file argument → the workbench with that file's directory as root and an initial `.show <file>` dispatched.

- [ ] **Step 1: Restructure `main()`**:

```rust
// In main(), replacing the unconditional target resolution:
let cli = Cli::parse();
enum Mode { Classic(PathBuf, String), Workbench { root: PathBuf, initial: Option<String> } }
let mode = match cli.target {
    Some(t) if t.to_string_lossy().ends_with(".tdy.sql") => {
        let sql = std::fs::read_to_string(&t)
            .with_context(|| format!("cannot read target {}", t.display()))?;
        tdy::target::Target::parse(&sql).with_context(|| format!("in {}", t.display()))?;
        Mode::Classic(t, sql)
    }
    Some(f) => {
        // A data file: open the workbench in its directory, showing it.
        let f = f.canonicalize().with_context(|| format!("cannot open {}", f.display()))?;
        let root = f.parent().map(Path::to_path_buf).unwrap_or_else(|| PathBuf::from("."));
        let name = f.file_name().unwrap().to_string_lossy().to_string();
        Mode::Workbench { root, initial: Some(format!(".show {name}")) }
    }
    None => match discover_target() {
        // Exactly one target here: the classic flow, today's behaviour.
        Ok(t) => {
            let sql = std::fs::read_to_string(&t)?;
            tdy::target::Target::parse(&sql).with_context(|| format!("in {}", t.display()))?;
            Mode::Classic(t, sql)
        }
        // No target, or several: the workbench is the answer now, not an error.
        Err(_) => Mode::Workbench { root: std::env::current_dir()?, initial: None },
    },
};
```

  (`discover_target`'s error cases stop being fatal — delete its "no .tdy.sql here" bail text's draft hint only if it is now unreachable; keep the function for the single-target case.)

- [ ] **Step 2: The workbench loop** (new `async fn run_workbench(terminal, root, initial, torn_down) -> Result<()>` mirroring the existing `run()`):

```rust
enum WbMsg {
    Started(String),          // worker began this line
    Done(Box<Outcome>),
    Progress(String),
    Note(String),
}

// The worker: owns the Session, runs one line at a time.
fn spawn_console_worker(
    root: PathBuf, cfg: Config, tx: mpsc::UnboundedSender<WbMsg>,
) -> mpsc::UnboundedSender<String> {
    let (line_tx, mut line_rx) = mpsc::unbounded_channel::<String>();
    tokio::spawn(async move {
        let mut session = match tdy::console::Session::new(&root, cfg) {
            Ok(s) => s,
            Err(e) => { let _ = tx.send(WbMsg::Note(format!("{e:#}"))); return; }
        };
        while let Some(line) = line_rx.recv().await {
            let _ = tx.send(WbMsg::Started(line.clone()));
            let sink_tx = tx.clone();
            let sink: tdy::progress::Sink = std::sync::Arc::new(move |e| {
                use tdy::progress::Event;
                let what = match e {
                    Event::MemberStarted { path, index, total } =>
                        format!("fitting {path} ({} of {total})", index + 1),
                    Event::MemberFinished { .. } => return,
                    Event::Consulting { path, backend, model, bytes } =>
                        format!("asking {model} via {backend} about {path} ({bytes} bytes sent)"),
                };
                let _ = sink_tx.send(WbMsg::Progress(what));
            });
            let o = session.run(&line, Some(&sink)).await;
            let quit = session.wants_quit();
            let _ = tx.send(WbMsg::Done(Box::new(o)));
            if quit { break; }
        }
    });
    line_tx
}
```

  The loop itself: same 60 ms poll structure as the existing `run()`; on `WbMsg::Started(l)` → `wb.begin(&l)` and `append_history(&l)` (single-line-normalized — reuse the console's behaviour by appending the echo AFTER `Done` instead if `begin`'s line differs from the echo; simplest correct rule: append on `Done` using `o.echo` when non-empty and the payload wasn't `Continue`); `Done(o)` → `wb.apply(*o)` and act on the returned `WbAction` (`Edit(p)` → the existing editor suspend/reenter dance, then dispatch `.ls`-free refresh: `wb.browser.refresh()`); `Progress`/`Note` → `wb.progress`/`wb.note`; key events → `wb.key(k)` and act: `Dispatch(line)` → `line_tx.send(line)`; `PreviewFile(p)` → `tokio::task::spawn_blocking` computing `tdy::console::raw_head(&p, cfg.limits)` + (if a fresh sidecar exists) `sidecar::load` → `spec_summary`, sent back as a `Done`-like `WbMsg` — add `WbMsg::Preview { path: PathBuf, raw: RawHead, spec: Option<SpecSummary> }` and a `Workbench::set_preview(path, raw, spec)` method (only applies if the context/selection still points at that path); `Quit` → break. `wb.should_quit` after any apply → break. Browser refresh after every `Done` whose `ok` (a sniff/fit/edit may have changed sidecar status) — `wb.browser.refresh()` is cheap (one `read_dir` + sidecar headers).
  Dispatch `initial` (if any) right after spawning the worker.

- [ ] **Step 3: The audit-trail integration test** (append to `tests/workbench.rs`) — the property that survives refactors:

```rust
#[test]
fn shortcut_and_typed_line_produce_identical_dispatches_after_cd() {
    let d = pile();
    let mut w1 = wb(&d);
    let mut w2 = wb(&d);
    // w1: navigate with the browser, sniff via shortcut.
    w1.key(key(KeyCode::Tab));
    w1.key(key(KeyCode::Enter));         // .cd sub
    let a1 = w1.key(key(KeyCode::Char('s')));
    // w2: type the same session.
    let _ = type_line(&mut w2, ".cd sub");
    let a2 = type_line(&mut w2, ".sniff c.csv");
    assert_eq!(a1, a2);
}
```

- [ ] **Step 4: Run everything**

Run: `cargo test --workspace --lib --tests`
Expected: green — including the untouched `tests/render.rs`/`preview.rs` (the classic flow) and slice 1's console/repl/mcp suites.

- [ ] **Step 5: Manual smoke note** — the sandbox has no PTY. Build release (`cargo build --release -p tdy-tui`) and verify the binary *starts* headlessly only in classic mode (`tdy-tui sales.tdy.sql </dev/null` fails cleanly about the terminal, same as today). Record in the report that the interactive three-pane frame needs the human smoke test; do not fake it.

- [ ] **Step 6: Commit**

```bash
git add tdy-tui/src/main.rs tdy-tui/tests/workbench.rs
git commit -m "the workbench runs: a console worker owns the Session, the UI dispatches lines, tdy-tui with no target opens on the directory"
```

---

### Task 6: `tdy ui` plumbing and the ambiguous-target path

**Files:**
- Modify: `src/main.rs` (the `Ui` arm's doc comment only, if stale), `tdy-tui/src/main.rs` (already done in Task 5 — this task verifies), `tests/repl.rs` or a new `tests/ui_dispatch.rs` if any assertion is feasible headlessly
- Test: headless assertions only

The behaviour Task 5 produced must hold at the `tdy ui` layer too (it execs `tdy-tui` with the same args, so this is mostly verification):

- [ ] **Step 1: Verify headlessly**: `cd testdata/drifting_exports && tdy-tui </dev/null` (two targets present) must now attempt the WORKBENCH (fails on no-TTY grounds — e.g. a terminal/Device-not-a-tty error — NOT the old "several targets here" error). `tdy-tui sales.tdy.sql </dev/null` must fail exactly as it does on main today (classic flow, terminal error). Run both against the release binary; capture the stderr in the report. If "several targets" still appears, Task 5's `discover_target` handling is wrong — fix there.
- [ ] **Step 2:** If `src/main.rs`'s `Ui` arm doc comment still describes the old error behaviour, update the sentence; no logic change.
- [ ] **Step 3:** Run `cargo test --workspace --lib --tests`; commit.

```bash
git add -A
git commit -m "tdy ui with several targets opens the workbench instead of refusing; classic flow untouched behind a named target"
```

---

### Task 7: docs

**Files:**
- Modify: `README.md` ("For humans: `tdy ui`" section), `CLAUDE.md` (the tdy-tui paragraph), `docs/design/2026-09-01-console-and-workbench.md` (§11 slice-2 status note)

- [ ] **Step 1: README** — rewrite the "For humans" section's opening: `tdy ui` with no target opens the three-pane workbench on the current directory (browser with sidecar status, main pane, the same console at the bottom — every keystroke shortcut is a console line, so the scrollback is the session's record); `tdy ui <target>` still opens the classic pile-review screens. Keep the existing review-gate paragraphs (they describe the classic flow, which remains). Do not fabricate terminal output; describe, don't transcribe.
- [ ] **Step 2: CLAUDE.md** — append to the tdy-tui paragraph: the workbench trio (`browser.rs` state / `workbench.rs` pure state machine, `Key` in `WbAction` out / `wb_ui.rs` renders and mutates nothing), the one-code-path rule (shortcuts synthesize console lines; the audit-trail equality test in `tests/workbench.rs`), the console-worker-owns-the-Session design, and that classic screens still hang off a target argument until slice 3 migrates them.
- [ ] **Step 3: Spec §11** — annotate slice 2's entry: *"Done 2026-09-02 with one scope change: bare `tdy` still opens the console (see §5 revision); the workbench's doors are `tdy ui`/`tdy-tui`. Pile/Member/Evidence contexts and the browser's `f`/`d`/`D` shortcuts move to slice 3."*
- [ ] **Step 4:** `cargo test --workspace --lib --tests`; commit.

```bash
git add README.md CLAUDE.md docs/design/2026-09-01-console-and-workbench.md
git commit -m "docs: the workbench — three panes over the console, classic screens behind a target until slice 3"
```

---

## Self-review against the spec

- **§6 frame:** panes/layout (T3), browser status vocabulary (T3, from `render_listing`'s exact strings), focus rules Tab/Esc (T2), console scrollback + zoom + resize + PgUp (T2/T3), shortcuts as dispatched lines with the echo in scrollback (T2, tested), status line Progress-vs-Note (T2 `progress`/`note`, T3 render), header (T3). Deferred to slice 3 per §11: `f`/`d`/`D` shortcuts, drift status needs no new work (comes from `EntryStatus`).
- **§7 contexts:** Empty (T3), File-no-sidecar "no opinion" (T4, asserted negatively), File-with-sidecar two-column + decisions list + red-confidence (T4), Query lands in main (T4 — a deliberate slight widening of §11's "Empty and the two File views," justified because the console's SQL output would otherwise be scrollback-only; ledger-ruling material for the controller). Pile/Member/Evidence: slice 3.
- **§5 entry points:** `tdy-tui [PATH]` handling (T5/T6) honors the 2026-09-02 revision; bare `tdy` untouched.
- **§10 tests:** state machine without terminal (T2), TestBackend renders at two sizes + status column + contexts (T3/T4), audit-trail property (T2 + T5's cd-equivalence test).
- **Type consistency:** `WbAction::{None,Quit,Dispatch(String),PreviewFile(PathBuf),Edit(PathBuf)}` used identically in T2/T5; `Context` variants in T2/T3/T4; `WbMsg` only in T5; `Cell { echo, text, ok }` in T2/T3.
- **Placeholders:** none — every step carries code or an exact command; T5's runtime step names each message flow explicitly.
