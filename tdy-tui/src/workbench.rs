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

use crate::browser::Browser;

/// Which pane keys are routed to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Focus {
    Console,
    Browser,
    Main,
}

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
            _ => WbAction::None,
        }
    }

    fn key_main(&mut self, k: KeyEvent) -> WbAction {
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
