//! The frame's state machine: a key in, a [`WbAction`] out.
//!
//! `Workbench` decides only — no printing, no terminal calls, and the one
//! piece of I/O it does is the browser's own directory listing (a
//! `refresh`, when a `Done` moves the session's cwd). The
//! runtime (Task 4/5) owns the console worker and the terminal; this module
//! only decides what should happen, so its whole behaviour is a unit test.
//! `ui.rs` (Task 3) reads its state and never changes it, the way `app.rs`
//! already keeps rendering and deciding apart for the evidence screen.

use std::path::{Path, PathBuf};

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

use tdy::console::line::{Edit, LineEditor};
use tdy::console::{EntryKind, Outcome, Payload, RawHead, SpecSummary, Table};
use tdy::report::{MemberReport, PileReport};

use crate::browser::Browser;
use crate::remedy::{self, Remedy};

/// Which pane keys are routed to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Focus {
    Console,
    Browser,
    Main,
}

/// What the main pane shows. Slice 2: Empty and the File views; a completed
/// query's Table also lands here so SQL results are not scrollback-only.
/// Slice 3 Task 2 adds `Pile` (a `.fit`/`.check`/`.accept` report) and the
/// shell of `Member` (one member of it, opened with `Enter`) — Task 3 fills
/// in the Member view's own rendering and remedy flow.
#[derive(Debug, Default)]
pub enum Context {
    #[default]
    Empty,
    File { path: PathBuf, raw: RawHead, spec: Option<SpecSummary>, preview: Option<Table> },
    Query(Table),
    Pile { target: PathBuf, report: PileReport, selected: usize },
    Member {
        target: PathBuf,
        report: PileReport,
        member: usize,
        /// Filled in by the runtime's `PreviewFile` follow-up, the way
        /// `File`'s raw head is — the state machine does no I/O.
        raw: Option<RawHead>,
        /// Which remedy is highlighted in the ranked remedy menu.
        remedy_selected: usize,
    },
}

/// One scrollback cell: the echoed line, then its text.
#[derive(Debug, Clone)]
pub struct Cell {
    pub echo: String,
    pub text: String,
    pub ok: bool,
}

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
    /// Lines scrolled up from the bottom of the console pane.
    pub scroll: usize,
    pub editor: LineEditor,
    /// Console pane height in rows; default 8, resized by Ctrl-Up/Ctrl-Down
    /// within [3, 30].
    pub console_rows: u16,
    /// Ctrl-L: console takes the whole right column.
    pub zoom: bool,
    /// A command is running; what it said last.
    pub busy: Option<String>,
    /// A transient note (e.g. "Ctrl-Q quits").
    pub status: String,
    pub should_quit: bool,
    /// Scroll position of the File view in the main pane.
    pub main_scroll: usize,
    /// `?` (from Browser or Main focus) opens the key-help overlay; while
    /// set, the *next* key just closes it again instead of acting normally
    /// — see `key()`. Console focus never sets this, since `?` there is an
    /// ordinary character for the line editor.
    pub help: bool,
    /// Set when the last `apply`d outcome's payload was `Payload::Continue`;
    /// drives `prompt()`.
    sql_pending: bool,
    /// The target `.fit`/`.accept`/`.check` last named, resolved against
    /// `browser.dir` at the moment the line was dispatched — `Payload::Fitted`
    /// carries no path of its own, so this is how it knows which file it
    /// fitted. Set in `begin()`; every other command leaves it alone.
    /// Deliberately fragile (see `record_target`'s doc comment).
    pub last_target: Option<PathBuf>,
}

impl Workbench {
    pub fn new(browser: Browser, history: Vec<String>) -> Workbench {
        Workbench {
            browser,
            focus: Focus::Console,
            context: Context::default(),
            scrollback: Vec::new(),
            scroll: 0,
            editor: LineEditor::new(history),
            console_rows: 8,
            zoom: false,
            busy: None,
            status: String::new(),
            should_quit: false,
            main_scroll: 0,
            help: false,
            sql_pending: false,
            last_target: None,
        }
    }

    /// One key in, one action out. Pure.
    pub fn key(&mut self, k: KeyEvent) -> WbAction {
        if k.kind != KeyEventKind::Press {
            // A held key repeats or a release; the runtime filters these
            // too, but this module must not double-act if handed one.
            return WbAction::None;
        }
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        if ctrl && k.code == KeyCode::Char('q') {
            self.should_quit = true;
            return WbAction::Quit;
        }
        // The help overlay swallows exactly the next key, whatever it is,
        // to close itself — that is the whole overlay contract, and it
        // takes priority even over the busy gate below (help can only ever
        // have been opened while not busy, but closing it must never
        // depend on that).
        if self.help {
            self.help = false;
            return WbAction::None;
        }
        // One command at a time, matching the console's one-Session
        // serialization: while busy, only quit and focus movement act.
        if self.busy.is_some() && !matches!(k.code, KeyCode::Tab | KeyCode::Esc) {
            return WbAction::None;
        }
        match k.code {
            KeyCode::Tab => {
                self.cycle_focus();
                return WbAction::None;
            }
            KeyCode::Esc => {
                // Main focus over a Pile: Esc backs out of the report to
                // Empty (the same key a person would use to escape any
                // other overlay) rather than jumping focus to the console —
                // that would leave the report on screen but unreachable
                // without retyping `.fit`. Over a Member, Esc backs out one
                // level only, to the Pile row this member came from — the
                // same report, moved rather than cloned, with `selected`
                // pointing back at the member just examined.
                if self.focus == Focus::Main {
                    match &self.context {
                        Context::Pile { .. } => {
                            self.context = Context::Empty;
                            return WbAction::None;
                        }
                        Context::Member { .. } => {
                            self.leave_member();
                            return WbAction::None;
                        }
                        _ => {}
                    }
                }
                self.focus = Focus::Console;
                return WbAction::None;
            }
            KeyCode::Char('q') if !ctrl && matches!(self.focus, Focus::Browser | Focus::Main) => {
                self.should_quit = true;
                return WbAction::Quit;
            }
            // Console focus needs `?` as an ordinary character (falls
            // through to `key_console` below); Browser/Main have no use for
            // a literal `?`, so there it opens the key-help overlay.
            KeyCode::Char('?') if !ctrl && matches!(self.focus, Focus::Browser | Focus::Main) => {
                self.help = true;
                return WbAction::None;
            }
            _ => {}
        }
        match self.focus {
            Focus::Console => self.key_console(k, ctrl),
            Focus::Browser => self.key_browser(k),
            Focus::Main => self.key_main(k),
        }
    }

    /// A dispatched line has started running (echo it, mark busy).
    ///
    /// Idempotent while already busy: the runtime calls this synchronously
    /// the moment a line is dispatched (before it ever reaches the worker,
    /// so a fast key burst cannot slip a second `Dispatch` past `key()`'s
    /// busy gate — see the runtime's `act_on_wb`), and the worker's own
    /// `Started` message for that same line calls this again once it
    /// round-trips. Without the guard that second call would remember the
    /// line twice; with it, it is a no-op, and a line dispatched the one
    /// other way — straight to the worker, bypassing this synchronous call
    /// entirely (the workbench's initial `.show`, sent before any key can
    /// reach the runtime) — still gets marked busy and remembered exactly
    /// once, by its own `Started` round trip, because `busy` is `None` when
    /// that arrives.
    pub fn begin(&mut self, line: &str) {
        if self.busy.is_some() {
            return;
        }
        self.busy = Some(line.to_string());
        self.editor.remember(line);
        self.record_target(line);
    }

    /// `.fit`, `.accept` and `.check` all name a target as their first
    /// argument; remember it (resolved against the browser's current
    /// directory) so a later `Payload::Fitted` — which carries no path of
    /// its own — knows which file produced it. Any other line leaves
    /// `last_target` alone, so `Payload::Fitted` from an `.accept` after a
    /// `.fit` (or a re-`.fit` after browsing elsewhere) still resolves.
    ///
    /// Deliberately tiny and fragile, per the design ledger: this is a
    /// whitespace split plus trimming a matching pair of `"` off the second
    /// token, not the console's own tokenizer — a target path containing a
    /// space, or spelled with single quotes, is a known gap, not a bug to
    /// chase here.
    fn record_target(&mut self, line: &str) {
        let mut tokens = line.trim().split_whitespace();
        let Some(cmd) = tokens.next() else { return };
        if matches!(cmd, ".fit" | ".accept" | ".check") {
            if let Some(tok) = tokens.next() {
                let tok = tok.trim_matches('"');
                self.last_target = Some(self.browser.dir.join(tok));
            }
        }
    }

    /// The console worker is gone (the `Session` failed to build, or the
    /// task ended): clear busy and say so. Without this a dispatch whose
    /// send fails leaves the UI busy forever — the busy text covering the
    /// very error that explains it, and every key but Ctrl-Q swallowed.
    pub fn worker_died(&mut self, note: &str) {
        self.busy = None;
        self.status = note.to_string();
    }

    /// The worker finished a line: record it, update the context, and
    /// re-root the browser on the session's `cwd`.
    ///
    /// The cwd rides on every `Done` because `.cd` is ordinary typed
    /// grammar: the browser moves only when navigation went *through* it,
    /// so after a typed `.cd sub` the browser would still list the root and
    /// its `s` shortcut would synthesize `.sniff jan.csv` for a file the
    /// session resolves in `sub/` — a different file than the highlighted
    /// one. Taking the session as the source of truth heals that and the
    /// reverse (a browser descent whose `.cd` the session refused: that
    /// `Done` carries the unchanged cwd and rolls the browser back).
    pub fn apply(&mut self, o: Outcome, cwd: &Path) -> Option<WbAction> {
        self.busy = None;
        self.browser.sync_dir(cwd);
        self.sql_pending = matches!(&o.payload, Payload::Continue);
        let Outcome { echo, text, payload, ok } = o;
        // A buffered SQL line still echoes; only skip a cell that is
        // entirely empty.
        if !(echo.is_empty() && text.is_empty()) {
            self.scrollback.push(Cell { echo, text, ok });
        }
        match payload {
            Payload::Shown { path, raw, spec } => {
                self.show_file(path, raw, spec, None);
                None
            }
            Payload::Sniffed { path, spec, preview, .. } => {
                // The state machine does no I/O: the raw half is filled in
                // by the runtime's own PreviewFile action.
                self.show_file(path.clone(), RawHead::default(), Some(spec), Some(preview));
                Some(WbAction::PreviewFile(path))
            }
            Payload::Query(t) => {
                self.context = Context::Query(t);
                None
            }
            Payload::Fitted(r) => {
                // `Payload::Fitted` names no path of its own: prefer the
                // target `begin()` just recorded, and fall back to the
                // target a Pile already on screen was fitted from (a re-fit
                // that, for whatever reason, `record_target` missed). If
                // neither exists — should not happen for a real `.fit`, but
                // this must still be total — leave the context as it is and
                // say so rather than guess a path.
                let target = self.last_target.clone().or_else(|| match &self.context {
                    Context::Pile { target, .. } => Some(target.clone()),
                    _ => None,
                });
                match target {
                    Some(target) => self.context = Context::Pile { target, report: r, selected: 0 },
                    None => self.status = "fitted, but no target known".to_string(),
                }
                None
            }
            Payload::Edit(p) => Some(WbAction::Edit(p)),
            Payload::Quit => {
                self.should_quit = true;
                None
            }
            _ => None,
        }
    }

    /// Progress from the worker's sink.
    pub fn progress(&mut self, what: String) {
        self.busy = Some(what);
    }

    /// A transient note from the worker's sink.
    pub fn note(&mut self, what: String) {
        self.status = what;
    }

    /// A `PreviewFile` action's result arrived (computed off the UI
    /// thread). Applies only when the context or the browser's current
    /// selection still points at `path` — an arrow key can move on, or
    /// another command can replace the context, before a spawned preview
    /// finishes, and a stale result must be dropped rather than clobbering
    /// whatever is now shown.
    pub fn set_preview(&mut self, path: PathBuf, raw: RawHead, spec: Option<SpecSummary>) {
        // A Member context's preview is keyed on the one member being
        // examined, not on the browser's selection (which is elsewhere,
        // showing the pile's directory) — match or drop, and never fall
        // through to the File-view logic below.
        if let Context::Member { target, report, member, raw: r, .. } = &mut self.context {
            let expected = report.members.get(*member).map(|m| member_preview_path(target, &m.path));
            if expected.as_deref() == Some(path.as_path()) {
                *r = Some(raw);
            }
            return;
        }
        let context_matches = matches!(&self.context, Context::File { path: p, .. } if *p == path);
        let selection_matches = self.browser.selected_path().as_deref() == Some(path.as_path());
        if !context_matches && !selection_matches {
            return;
        }
        // Preserve whatever query preview a prior `.sniff` already put here
        // for this same path (this action's own job is only to fill in the
        // raw half — see `apply`'s `Payload::Sniffed` arm).
        let preview = match &self.context {
            Context::File { path: p, preview, .. } if *p == path => preview.clone(),
            _ => None,
        };
        self.show_file(path, raw, spec, preview);
    }

    /// Point the main pane at a file, resetting its scroll only when the
    /// file actually changes. Arrowing to the next file must open it at the
    /// top — carrying the previous file's offset renders a short file as a
    /// blank pane, which reads as an empty file — while an update for the
    /// *same* path (a `.sniff`'s raw fill-in landing after the fact) must
    /// keep the scroll the user set.
    fn show_file(&mut self, path: PathBuf, raw: RawHead, spec: Option<SpecSummary>, preview: Option<Table>) {
        let same = matches!(&self.context, Context::File { path: p, .. } if *p == path);
        if !same {
            self.main_scroll = 0;
        }
        self.context = Context::File { path, raw, spec, preview };
    }

    /// The member currently under the cursor — in a `Pile` the row at
    /// `selected`, in a `Member` the one being examined. `None` in every
    /// other context, or if the index is somehow out of range (a stale
    /// selection surviving a report replaced by a shorter one).
    pub fn pile_selected_member(&self) -> Option<&MemberReport> {
        match &self.context {
            Context::Pile { report, selected, .. } => report.members.get(*selected),
            Context::Member { report, member, .. } => report.members.get(*member),
            _ => None,
        }
    }

    /// Every remedy every problem of the current Member offers, in order,
    /// deduplicated by first occurrence — `remedy::remedies_for` is fed each
    /// problem's own JSON (the same shape the old classic app's
    /// `refresh_remedies` builds via `serde_json::to_value`), so a remedy
    /// this module invents and one `remedy.rs` actually knows how to apply
    /// can never drift apart. Empty outside a `Member` context, or for a
    /// member with no problems (nothing to fit is nothing to remedy).
    pub fn member_remedies(&self) -> Vec<Remedy> {
        let Context::Member { report, member, .. } = &self.context else { return Vec::new() };
        let Some(m) = report.members.get(*member) else { return Vec::new() };
        let mut out: Vec<Remedy> = Vec::new();
        for p in &m.problems {
            let value = serde_json::to_value(p).unwrap_or_default();
            for r in remedy::remedies_for(&value, &m.path) {
                if !out.contains(&r) {
                    out.push(r);
                }
            }
        }
        out
    }

    pub fn prompt(&self) -> &'static str {
        if self.sql_pending {
            "   -> "
        } else {
            "tdy> "
        }
    }

    fn cycle_focus(&mut self) {
        self.focus = match self.focus {
            Focus::Console => Focus::Browser,
            Focus::Browser => Focus::Main,
            Focus::Main => Focus::Console,
        };
    }

    fn key_console(&mut self, k: KeyEvent, ctrl: bool) -> WbAction {
        match (k.code, ctrl) {
            (KeyCode::Up, true) => {
                self.console_rows = (self.console_rows + 1).min(30);
                WbAction::None
            }
            (KeyCode::Down, true) => {
                self.console_rows = self.console_rows.saturating_sub(1).max(3);
                WbAction::None
            }
            (KeyCode::Char('l'), true) => {
                self.zoom = !self.zoom;
                WbAction::None
            }
            (KeyCode::PageUp, _) => {
                self.scroll = self.scroll.saturating_add(5);
                WbAction::None
            }
            (KeyCode::PageDown, _) => {
                self.scroll = self.scroll.saturating_sub(5);
                WbAction::None
            }
            _ => match self.editor.key(k) {
                Edit::Submit(line) => {
                    if line.trim().is_empty() {
                        return WbAction::None;
                    }
                    self.scroll = 0;
                    WbAction::Dispatch(line)
                }
                Edit::Interrupt => {
                    // Plain Ctrl-C on an empty prompt: hint, don't quit.
                    self.status = "Ctrl-Q quits".to_string();
                    WbAction::None
                }
                Edit::Redraw | Edit::Cleared | Edit::Eof | Edit::Nothing => WbAction::None,
            },
        }
    }

    fn key_browser(&mut self, k: KeyEvent) -> WbAction {
        match k.code {
            KeyCode::Up => {
                self.browser.move_sel(-1);
                self.preview_selected()
            }
            KeyCode::Down => {
                self.browser.move_sel(1);
                self.preview_selected()
            }
            KeyCode::Enter => self.enter_browser(),
            KeyCode::Backspace => {
                // The browser's dir IS the session's cwd (see the design
                // note on `.cd` in the task brief): keep them in lockstep,
                // so `up()`'s own move is followed by the same `.cd ..`
                // dispatch that `Enter` on a directory issues. At the
                // browser root, up() does nothing and nothing dispatches.
                if self.browser.up() {
                    WbAction::Dispatch(".cd ..".into())
                } else {
                    WbAction::None
                }
            }
            KeyCode::Char('s') => self.shortcut(".sniff"),
            KeyCode::Char('e') => self.shortcut(".edit"),
            // `f` fits a target — only meaningful on a `*.tdy.sql` entry; a
            // data file's shortcut stays `s`.
            KeyCode::Char('f') => match self.browser.selected_entry() {
                Some(e) if e.kind == EntryKind::Target => self.shortcut(".fit"),
                _ => WbAction::None,
            },
            _ => WbAction::None,
        }
    }

    fn key_main(&mut self, k: KeyEvent) -> WbAction {
        if let Context::Pile { report, selected, .. } = &mut self.context {
            let len = report.members.len();
            return match k.code {
                KeyCode::Up => {
                    *selected = selected.saturating_sub(1);
                    WbAction::None
                }
                KeyCode::Down => {
                    if len > 0 {
                        *selected = (*selected + 1).min(len - 1);
                    }
                    WbAction::None
                }
                KeyCode::Enter => self.enter_pile_member(),
                KeyCode::Char('f') => self.refit_pile(),
                _ => WbAction::None,
            };
        }
        if matches!(&self.context, Context::Member { .. }) {
            return self.key_member(k);
        }
        match k.code {
            KeyCode::Up => {
                self.main_scroll = self.main_scroll.saturating_sub(1);
                WbAction::None
            }
            KeyCode::Down => {
                self.main_scroll = self.main_scroll.saturating_add(1);
                WbAction::None
            }
            _ => WbAction::None,
        }
    }

    /// `Enter` on the selected Pile row: open it as a `Member` and ask the
    /// runtime to preview the file (the raw half is filled in by that
    /// action's own result, same as `File`'s). Moves the report out of
    /// `Pile` rather than cloning it — `PileReport` carries no `Clone`, on
    /// purpose, so a "current" and a "stale copy" version can never quietly
    /// disagree.
    fn enter_pile_member(&mut self) -> WbAction {
        let ctx = std::mem::take(&mut self.context);
        let Context::Pile { target, report, selected } = ctx else {
            self.context = ctx;
            return WbAction::None;
        };
        let Some(member_path) = report.members.get(selected).map(|m| m.path.clone()) else {
            self.context = Context::Pile { target, report, selected };
            return WbAction::None;
        };
        let preview_path = member_preview_path(&target, &member_path);
        self.context =
            Context::Member { target, report, member: selected, raw: None, remedy_selected: 0 };
        WbAction::PreviewFile(preview_path)
    }

    /// Esc from a Member: back to the Pile it came from, `selected` on the
    /// same member — moving the report out with `mem::take` rather than
    /// cloning it, exactly as `enter_pile_member` moves it the other way.
    fn leave_member(&mut self) {
        let ctx = std::mem::take(&mut self.context);
        let Context::Member { target, report, member, .. } = ctx else {
            self.context = ctx;
            return;
        };
        self.context = Context::Pile { target, report, selected: member };
    }

    /// Keys over a Member context (Main focus): Up/Down pick a remedy,
    /// clamped to the current menu; `e` edits the file itself. `a` and the
    /// digit shortcuts land in later tasks.
    fn key_member(&mut self, k: KeyEvent) -> WbAction {
        match k.code {
            KeyCode::Up => {
                self.move_remedy_selection(-1);
                WbAction::None
            }
            KeyCode::Down => {
                self.move_remedy_selection(1);
                WbAction::None
            }
            KeyCode::Char('e') => self.edit_member(),
            _ => WbAction::None,
        }
    }

    /// Moves `remedy_selected` by `delta`, clamped to `[0, len - 1]` (`0`
    /// when the menu is empty — an accepted or otherwise remedy-less member
    /// has nothing to select).
    fn move_remedy_selection(&mut self, delta: i32) {
        let len = self.member_remedies().len();
        let Context::Member { remedy_selected, .. } = &mut self.context else { return };
        if len == 0 {
            *remedy_selected = 0;
            return;
        }
        let next = (*remedy_selected as i32 + delta).clamp(0, len as i32 - 1);
        *remedy_selected = next as usize;
    }

    /// `e` over a Member: the same `.edit <rel>` a browser shortcut would
    /// dispatch, spelled relative to `browser.dir` — the member's own file,
    /// not the target.
    fn edit_member(&self) -> WbAction {
        let Context::Member { target, report, member, .. } = &self.context else {
            return WbAction::None;
        };
        let Some(m) = report.members.get(*member) else { return WbAction::None };
        let path = member_preview_path(target, &m.path);
        WbAction::Dispatch(format!(".edit {}", quote_rel(&self.rel_spelling(&path))))
    }

    /// `f` from the Pile context: re-dispatch the same `.fit` that produced
    /// it, spelled relative to the browser's current directory when the
    /// target lives under it (the common case) and as its full path
    /// otherwise — the dispatched line must still resolve from the
    /// session's cwd even after a `.cd` has moved the browser elsewhere.
    fn refit_pile(&self) -> WbAction {
        let Context::Pile { target, .. } = &self.context else { return WbAction::None };
        WbAction::Dispatch(format!(".fit {}", quote_rel(&self.rel_spelling(target))))
    }

    fn rel_spelling(&self, path: &Path) -> String {
        match path.strip_prefix(&self.browser.dir) {
            Ok(rel) => rel.display().to_string(),
            Err(_) => path.display().to_string(),
        }
    }

    /// After an arrow move, preview the newly selected entry — but only a
    /// data file: a directory has nothing to preview, and a target previews
    /// in slice 3.
    fn preview_selected(&self) -> WbAction {
        match self.browser.selected_entry() {
            Some(e) if e.kind == EntryKind::File => match self.browser.selected_path() {
                Some(p) => WbAction::PreviewFile(p),
                None => WbAction::None,
            },
            _ => WbAction::None,
        }
    }

    /// `Enter`: a directory descends and dispatches `.cd <rel>` (the rel
    /// path is captured before `browser.enter()` mutates the dir — after
    /// the move, `selected_rel()` would answer from the wrong directory); a
    /// file returns its path to preview.
    fn enter_browser(&mut self) -> WbAction {
        let Some(kind) = self.browser.selected_entry().map(|e| e.kind) else {
            return WbAction::None;
        };
        if kind == EntryKind::Dir {
            let rel = self.browser.selected_rel();
            self.browser.enter();
            match rel {
                Some(r) => WbAction::Dispatch(format!(".cd {}", quote_rel(&r))),
                None => WbAction::None,
            }
        } else {
            let p = self.browser.enter();
            match (kind, p) {
                (EntryKind::File, Some(path)) => WbAction::PreviewFile(path),
                _ => WbAction::None,
            }
        }
    }

    /// `s`/`e`: dispatch the same line a human would type, over the
    /// currently selected entry — the audit trail is that a shortcut and
    /// the equivalent typed command are indistinguishable in the console's
    /// history.
    fn shortcut(&self, cmd: &str) -> WbAction {
        match self.browser.selected_rel() {
            Some(rel) => WbAction::Dispatch(format!("{cmd} {}", quote_rel(&rel))),
            None => WbAction::None,
        }
    }
}

/// A member's path, spelled how the target sees it (relative to its own
/// declaration), resolved to an absolute path a preview or an edit can open.
/// Shared by `enter_pile_member`, `set_preview` and `edit_member` so all
/// three agree on what "the member's file" means.
fn member_preview_path(target: &Path, member_rel: &str) -> PathBuf {
    let target_dir = target.parent().map(Path::to_path_buf).unwrap_or_else(|| PathBuf::from("."));
    target_dir.join(member_rel)
}

/// Quote a rel path the way the console's own tokenizer expects to read it
/// back — Debug-quote (the console's `quote_rel` rule) only when it
/// contains whitespace.
fn quote_rel(s: &str) -> String {
    if s.chars().any(char::is_whitespace) {
        format!("{s:?}")
    } else {
        s.to_string()
    }
}
