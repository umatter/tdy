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

use tdy::console::line::{Edit as LineEdit, LineEditor};
use tdy::console::{EntryKind, Outcome, Payload, RawHead, SpecSummary, Table, quote_rel};
use tdy::evidence::Evidence;
use tdy::report::{MemberReport, PileReport};

use crate::browser::Browser;
use crate::remedy::{self, Edit, Remedy};

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
    File {
        path: PathBuf,
        raw: RawHead,
        spec: Option<SpecSummary>,
        preview: Option<Table>,
        /// The sidecar's fingerprint no longer matches the file
        /// (`SidecarStatus::Stale`) — the footer names `--force` instead of
        /// the plain "not sniffed" hint, which would send someone to re-run
        /// a command that will just report the same staleness back.
        stale: bool,
    },
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
    /// `.accept`'s step one: what accepting this member's judgement(s) would
    /// actually do — never anything written yet. `line` is the exact
    /// `.accept TARGET MEMBER` line that produced this, echoed back
    /// verbatim by `a` here so the *session's* `pending_accept` (not this
    /// module — see the module doc) can recognise step two as the same
    /// line repeated. Arriving here necessarily replaces whatever `Member`
    /// context sent the `a` that produced it — see `key_evidence` and
    /// `Esc`'s handling below for the consequence.
    ///
    /// A typed `.cd` (or any other line) between this screen appearing and
    /// `a` being pressed is a deliberate degradation, not a bug this module
    /// needs to guard against: `Session::run`'s own rule clears
    /// `pending_accept` on any line that is not that exact `.accept`
    /// repeated, `.cd` included, so redispatching `line` unchanged after
    /// one just gets step one again — a fresh evidence render, nothing
    /// written — rather than silently accepting against whatever the
    /// session now considers current. This context still holds the same
    /// `target`/`member`/`rows` it was built with (`apply`'s `sync_dir`
    /// call updates `browser.dir` on a real cwd move, never the context),
    /// so the screen itself does not go stale or need to be cleared; only
    /// the *session's* answer to a second `a` does, and that is tested at
    /// the session's own layer (`console::mod`), not here. See
    /// `evidence_survives_a_cd_between_steps_and_still_redispatches_the_line`
    /// in `tests/workbench.rs` for the pin: this module's job is only to
    /// not panic and not wrongly clear the context.
    Evidence { target: PathBuf, member: String, rows: Vec<Evidence>, line: String },
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
    /// Compute a preview of this file for the main pane (arrow-move
    /// preview, or `[`/`]` picking another sheet of a workbook — a view
    /// change like the arrow-move preview, so it deliberately does NOT
    /// synthesize a console line; `.show FILE --sheet NAME` is the
    /// console's own spelling of the same thing).
    PreviewFile { path: PathBuf, sheet: Option<String> },
    /// Run $EDITOR on this path (comes back via Payload::Edit too).
    Edit(PathBuf),
    /// Write a confirmed remedy edit to the target, guarded by `expected`
    /// (the text it was staged against — see `write_target`), then dispatch
    /// `refit` (a `.fit <target>` line) through the normal Dispatch path so
    /// the refit lands in the scrollback like any other command.
    WriteTarget { path: PathBuf, expected: String, new_text: String, refit: String },
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
    /// The target's source text, as last read after a `Fitted` — see the
    /// runtime's `Done` handling. What a Member's remedy menu edits; `None`
    /// until the first fit lands, so a digit pressed before then is a status
    /// note rather than an edit against stale or absent text.
    pub target_sql: Option<String>,
    /// A staged edit awaiting `y`/`Esc` confirmation: the remedy that
    /// produced it, the edit itself, the target it would be written to, and
    /// the text it was staged against (what `write_target`'s guard compares
    /// the file to before writing — see `WbAction::WriteTarget`). Its mere
    /// presence makes `key()` modal — see the top of that function.
    pub pending_edit: Option<(Remedy, Edit, PathBuf, String)>,
    /// Browser rows marked with `d` (rel paths, relative to `browser.dir`);
    /// `D` drafts them. Cleared on any directory move — see `enter_browser`,
    /// `key_browser`'s `Backspace` arm and `apply`'s `sync_dir` call — since
    /// a rel path only means something inside the directory it was marked
    /// in.
    pub marked: Vec<String>,
    /// Below this confidence a red confidence number / browser `✓` glyph.
    /// `wb_ui` cannot reach `Config` on its own, so the runtime threads the
    /// real value through here instead of the module inventing one — see
    /// `spec.rs`'s escalation threshold, which this mirrors.
    pub confidence_threshold: f32,
    /// Bumped every time `key()`/`apply()` RETURNS a fresh `WbAction::
    /// PreviewFile` — never on receipt. `WbMsg::Preview` carries the value
    /// it was spawned for, and `set_preview` drops anything that does not
    /// match the current counter: an arrow key can move on (or another
    /// preview can be requested) before a spawned preview finishes, and
    /// without this a slow, stale result landing after a fresher one would
    /// silently overwrite it — the exact race the slice 2 review flagged.
    pub preview_gen: u64,
    /// Rows the main pane can actually show, told to us by the runtime
    /// (`wb_ui::main_inner_rows` after each draw) — the state machine does
    /// no terminal I/O, so this is how "keep the selection visible" learns
    /// what visible means. Default 20: close enough for the first frame,
    /// corrected before the first key can move a selection.
    pub main_view_rows: usize,
}

impl Workbench {
    pub fn new(browser: Browser, history: Vec<String>, confidence_threshold: f32) -> Workbench {
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
            target_sql: None,
            pending_edit: None,
            marked: Vec::new(),
            confidence_threshold,
            preview_gen: 0,
            main_view_rows: 20,
        }
    }

    /// Told by the runtime, once per draw, how many rows the main pane
    /// actually has (`wb_ui::main_inner_rows`). 0 means the main pane is
    /// not on screen (console zoomed) — ignored, so the follow window
    /// keeps the last real height rather than collapse to nothing.
    pub fn set_main_view_rows(&mut self, rows: usize) {
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
        // A staged edit is modal: every key but Ctrl-Q (handled above), `y`
        // and Esc/`n` is swallowed — no Tab, no busy gate to check, nothing
        // falls through to the ordinary focus dispatch below. `y` takes the
        // staged edit and turns it into the one write this module can ask
        // for; the runtime supplies the guard and does the actual I/O.
        if self.pending_edit.is_some() {
            return match k.code {
                KeyCode::Char('y') => {
                    let (_, edit, target, expected) = self.pending_edit.take().unwrap();
                    // --propose, like every other fit the workbench dispatches:
                    // the proposals are what ranks the next member's remedy
                    // menu, and the post-write refit is the fit most likely to
                    // land the user on one.
                    let refit =
                        format!(".fit {} --propose", quote_rel(&self.rel_spelling(&target)));
                    WbAction::WriteTarget { path: target, expected, new_text: edit.new_text, refit }
                }
                KeyCode::Esc | KeyCode::Char('n') => {
                    self.pending_edit = None;
                    self.status = "edit cancelled".to_string();
                    WbAction::None
                }
                _ => WbAction::None,
            };
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
                            // A context change resets (see the
                            // `Payload::Query` arm in `apply`): a
                            // paged-down Pile must not leave a stale offset
                            // behind for whatever opens in Main next.
                            self.main_scroll = 0;
                            return WbAction::None;
                        }
                        Context::Member { .. } => {
                            self.leave_member();
                            return WbAction::None;
                        }
                        Context::Evidence { .. } => {
                            // The Member context this evidence replaced is
                            // gone (Evidence cannot carry a `PileReport`
                            // alongside it without cloning one, which the
                            // design deliberately avoids everywhere else —
                            // see `enter_pile_member`/`leave_member`), so
                            // there is no report to hand back here. Empty,
                            // plus a note pointing at `f`, which still works
                            // (`last_target` survives) to bring the pile back.
                            self.context = Context::Empty;
                            // Context change resets, matching the Pile arm
                            // above and the `Payload::Query` arm.
                            self.main_scroll = 0;
                            self.status =
                                "evidence closed — press f to re-open the pile".to_string();
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
            // Zoom is global, not Console-only: `Ctrl-L` from Browser/Main
            // must still be able to turn it on (and, since zoom skips Main
            // in the Tab cycle — see `cycle_focus` — toggling it on while
            // Main is focused has nowhere left for Main to be, so focus
            // moves to Console, the same place `Esc` would put it).
            KeyCode::Char('l') if ctrl => {
                self.zoom = !self.zoom;
                if self.zoom && self.focus == Focus::Main {
                    self.focus = Focus::Console;
                }
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
    /// Uses the console's own tokenizer (`tdy::console::parse::tokenize`),
    /// so a quoted target with spaces, or a flag typed before the
    /// positional (`.fit --dry-run t.tdy.sql`), both resolve correctly —
    /// the whitespace-split this replaced got both wrong. An unterminated
    /// quote is the tokenizer's own error and is not this function's to
    /// recover from: it records nothing, leaving `last_target` at whatever
    /// it already was, the same as any other line that isn't `.fit`/
    /// `.accept`/`.check`.
    ///
    /// If a fit-family command ever grows a *valued* flag, the flag's value
    /// would be mistaken for the target here — extend the skip below when
    /// that happens.
    fn record_target(&mut self, line: &str) {
        let Ok(tokens) = tdy::console::parse::tokenize(line.trim()) else { return };
        let mut it = tokens.iter();
        let Some(cmd) = it.next() else { return };
        if matches!(cmd.as_str(), ".fit" | ".accept" | ".check") {
            if let Some(tok) = it.find(|t| !t.starts_with("--")) {
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
        // A real move — including the rollback branch, which moves the
        // browser right back to where marks were made and so is just as
        // stale — invalidates any rel paths in `marked` (see its doc
        // comment); `enter_browser`'s own directory branch covers the other
        // way a directory move happens.
        if self.browser.sync_dir(cwd) {
            self.marked.clear();
        }
        self.sql_pending = matches!(&o.payload, Payload::Continue);
        let Outcome { echo, text, payload, ok } = o;
        // `Payload::Evidence` wants the exact line that produced it (see
        // `Context::Evidence`'s doc) — captured here, before the scrollback
        // push below moves `echo` into a `Cell`.
        let line = echo.clone();
        // A buffered SQL line still echoes; only skip a cell that is
        // entirely empty.
        if !(echo.is_empty() && text.is_empty()) {
            self.scrollback.push(Cell { echo, text, ok });
        }
        match payload {
            Payload::Shown { path, raw, spec, stale } => {
                // `.show` now names its own staleness (`Command::Show`
                // distinguishes `Fresh`/`Stale`/`Absent` and carries the
                // flag through `Payload::Shown`) — passed straight through,
                // so a typed `.show` on a stale file gets the same
                // `.sniff --force` footer an arrow-key preview would give
                // it, not the generic "not sniffed" hint.
                self.show_file(path, raw, spec, None, stale);
                None
            }
            Payload::Sniffed { path, spec, preview, .. } => {
                // The state machine does no I/O: the raw half is filled in
                // by the runtime's own PreviewFile action. A spec just
                // produced by `.sniff` is fresh by definition.
                self.show_file(path.clone(), RawHead::default(), Some(spec), Some(preview), false);
                Some(self.preview_action(path, None))
            }
            Payload::Query(t) => {
                self.context = Context::Query(t);
                // A fresh result set starts at its first row. Every
                // context now *consumes* `main_scroll`, so an offset left
                // over from a long Pile would open a short answer scrolled
                // past its last row — a blank pane, which reads as "no
                // rows". The rule: a context CHANGE resets, a same-context
                // update (`show_file` on the same path) preserves.
                self.main_scroll = 0;
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
                    Some(target) => {
                        // Selection survives a refit: when the outgoing
                        // context was a Pile or a Member over the SAME
                        // target, remember which member's *path* (not
                        // index — a refit can insert/remove members ahead
                        // of it) was selected, before the report it points
                        // into is replaced below. A different target, or no
                        // prior selection, has nothing to preserve.
                        let prev_member_path: Option<String> = match &self.context {
                            Context::Pile { target: t, report, selected } if *t == target => {
                                report.members.get(*selected).map(|m| m.path.clone())
                            }
                            Context::Member { target: t, report, member, .. } if *t == target => {
                                report.members.get(*member).map(|m| m.path.clone())
                            }
                            // Evidence is the outgoing context of `.accept`
                            // step two, whose `Done` carries the refit —
                            // and it already names the member by its
                            // report-relative path, the very key the
                            // lookup below wants. Without this arm the
                            // member you just accepted is the one member
                            // the new Pile does not have selected.
                            Context::Evidence { target: t, member, .. } if *t == target => {
                                Some(member.clone())
                            }
                            _ => None,
                        };
                        let selected = prev_member_path
                            .and_then(|p| r.members.iter().position(|m| m.path == p))
                            .unwrap_or(0);
                        self.context = Context::Pile { target, report: r, selected };
                        // A Pile drawn from a fresh report starts at its
                        // first row (see the `Payload::Query` arm) — then
                        // the follow moves the window onto the restored
                        // selection when it landed somewhere deep.
                        self.main_scroll = 0;
                        self.follow_pile_selection();
                    }
                    None => self.status = "fitted, but no target known".to_string(),
                }
                None
            }
            Payload::Evidence { target, member, rows } => {
                self.context = Context::Evidence { target, member, rows, line };
                // Evidence is what a judgement rests on: it opens at its
                // first line, never at wherever the Pile behind it was
                // scrolled to (see the `Payload::Query` arm).
                self.main_scroll = 0;
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
    /// thread). `gen` is checked first, ahead of any path match: it is the
    /// counter `preview_gen` held at the moment the request was spawned, and
    /// a mismatch means something newer has been asked for since — another
    /// arrow key, a fresh `.sniff` follow-up — so a slow result landing
    /// late must never overwrite what is now on screen, even if it happens
    /// to name the very same path. Past that, applies only when the context
    /// or the browser's current selection still points at `path` — the
    /// original staleness check this augments, not replaces.
    pub fn set_preview(
        &mut self,
        gen: u64,
        path: PathBuf,
        raw: RawHead,
        spec: Option<SpecSummary>,
        stale: bool,
    ) {
        if gen != self.preview_gen {
            return;
        }
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
        self.show_file(path, raw, spec, preview, stale);
    }

    /// A `PreviewFile` action FAILED (computed off the UI thread). Same
    /// gen+path staleness rules as `set_preview` — see its doc comment for
    /// why `gen` is checked first — but instead of new content this fills
    /// the matching File/Member context's raw with the reason, so the pane
    /// says why rather than showing "loading…" forever.
    pub fn preview_failed(&mut self, gen: u64, path: PathBuf, msg: String) {
        if gen != self.preview_gen {
            return;
        }
        let raw = RawHead {
            lines: vec![format!("cannot read: {msg}")],
            truncated: false,
            sheets: vec![],
            grid: vec![],
            grid_sheet: None,
        };
        if let Context::Member { target, report, member, raw: r, .. } = &mut self.context {
            let expected = report.members.get(*member).map(|m| member_preview_path(target, &m.path));
            if expected.as_deref() == Some(path.as_path()) {
                *r = Some(raw);
                self.status = format!("preview unavailable: {msg}");
            }
            return;
        }
        let context_matches = matches!(&self.context, Context::File { path: p, .. } if *p == path);
        let selection_matches = self.browser.selected_path().as_deref() == Some(path.as_path());
        if !context_matches && !selection_matches {
            return;
        }
        // Preserve whatever spec/preview a prior successful sniff already
        // put here for this same path — a raw-head read failing later must
        // not wipe an opinion that is still valid.
        let (spec, preview, stale) = match &self.context {
            Context::File { path: p, spec, preview, stale, .. } if *p == path => {
                (spec.clone(), preview.clone(), *stale)
            }
            _ => (None, None, false),
        };
        self.status = format!("preview unavailable: {msg}");
        self.show_file(path, raw, spec, preview, stale);
    }

    /// Point the main pane at a file, resetting its scroll only when the
    /// file actually changes. Arrowing to the next file must open it at the
    /// top — carrying the previous file's offset renders a short file as a
    /// blank pane, which reads as an empty file — while an update for the
    /// *same* path (a `.sniff`'s raw fill-in landing after the fact) must
    /// keep the scroll the user set.
    fn show_file(
        &mut self,
        path: PathBuf,
        raw: RawHead,
        spec: Option<SpecSummary>,
        preview: Option<Table>,
        stale: bool,
    ) {
        let same = matches!(&self.context, Context::File { path: p, .. } if *p == path);
        if !same {
            self.main_scroll = 0;
        }
        self.context = Context::File { path, raw, spec, preview, stale };
    }

    /// The one place `WbAction::PreviewFile` is built: bumps `preview_gen`
    /// every time one is handed back to the runtime, so the result — tagged
    /// with the counter it read here — can be told apart from any later
    /// request's, in `set_preview`.
    fn preview_action(&mut self, path: PathBuf, sheet: Option<String>) -> WbAction {
        self.preview_gen += 1;
        WbAction::PreviewFile { path, sheet }
    }

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

    /// The remedies offered for the current Member, **best first**.
    ///
    /// "Best" is not a guess: `.fit --propose` reports which of the file's
    /// columns could actually *produce* the declared type, and those come
    /// first, in the order the planner ranked them. Offering the file's
    /// header in file order instead would put an arbitrary column at [1] —
    /// and a menu whose first entry is usually wrong is a menu that teaches
    /// people to stop reading it. (Ported from the classic app's
    /// `App::compute_remedies`, deleted with `app.rs` in Task 7; the
    /// workbench's `.fit` dispatches carry `--propose` so `proposals` is
    /// populated — see `refit_pile` and `main::dry_run_target_mode`.)
    ///
    /// Everything the problems offer follows, deduplicated by first
    /// occurrence — `remedy::remedies_for` is fed each problem's own JSON
    /// (the same shape `compute_remedies` built via `serde_json::to_value`),
    /// so a remedy this module invents and one `remedy.rs` actually knows
    /// how to apply can never drift apart. Empty outside a `Member`
    /// context, or for a member with neither proposals nor problems
    /// (nothing to fit is nothing to remedy).
    pub fn member_remedies(&self) -> Vec<Remedy> {
        let Context::Member { report, member, .. } = &self.context else { return Vec::new() };
        let Some(m) = report.members.get(*member) else { return Vec::new() };
        let mut out: Vec<Remedy> = Vec::new();
        let push = |r: Remedy, out: &mut Vec<Remedy>| {
            if !out.contains(&r) {
                out.push(r);
            }
        };
        // Type-compatible candidates first, in the planner's own order, and
        // only for the columns that actually failed to bind.
        for p in &m.proposals {
            for (spelling, _why) in &p.candidates {
                push(
                    Remedy::AddMatch { column: p.column.clone(), spelling: spelling.clone() },
                    &mut out,
                );
            }
        }
        // Then everything else the file offers, and the structural remedies.
        for p in &m.problems {
            let value = serde_json::to_value(p).unwrap_or_default();
            for r in remedy::remedies_for(&value, &m.path) {
                push(r, &mut out);
            }
        }
        // The classic floor: a member waiting on a judgement or carrying a
        // problem always has *something* to remedy, even when neither
        // `proposals` nor `problems` gave the loops above anything to offer
        // (a review-only member: `review: Some(_)`, `problems` empty) — the
        // exclude-this-file remedy, so the menu is never blank for a member
        // that plainly needs one.
        if out.is_empty() && (m.review.is_some() || !m.problems.is_empty()) {
            out.push(Remedy::ExcludeFile { rel: m.path.clone() });
        }
        out
    }

    /// The target's absolute path, from the current `Pile` or `Member`
    /// context — `None` outside either. Lets the runtime re-read the
    /// target's text after a `Fitted` `Done` without reaching into the
    /// `Context` enum itself.
    pub fn pile_target(&self) -> Option<&Path> {
        match &self.context {
            Context::Pile { target, .. } | Context::Member { target, .. } => Some(target),
            _ => None,
        }
    }

    /// Record the target's source text, freshly read — what a Member's
    /// remedy menu edits from here on. Called by the runtime after every
    /// `Fitted` `Done` (see `pile_target`), so the menu always edits the
    /// text the last fit actually saw.
    pub fn set_target_sql(&mut self, text: String) {
        self.target_sql = Some(text);
    }

    pub fn prompt(&self) -> &'static str {
        if self.sql_pending {
            "   -> "
        } else {
            "tdy> "
        }
    }

    /// `zoom` removes Main from the cycle entirely — with the console taking
    /// the whole right column there is nowhere for Main to draw, so Browser
    /// hands focus straight back to Console rather than through a pane that
    /// is not on screen. (`key()`'s `Ctrl-L` arm handles the other half:
    /// zoom turning on while Main already has focus.)
    fn cycle_focus(&mut self) {
        self.focus = match self.focus {
            Focus::Console => Focus::Browser,
            Focus::Browser if self.zoom => Focus::Console,
            Focus::Browser => Focus::Main,
            Focus::Main => Focus::Console,
        };
    }

    /// The console scrollback's total rendered line count, flattened exactly
    /// as `draw_console` lays it out: each cell contributes its echo's line
    /// count (a multi-line echo prints one row per `\n`-split part, `tdy>`
    /// then `   -> ` continuations) plus its text's line count. What
    /// `key_console`'s `PageUp` clamps `scroll` against, so it cannot run
    /// past real content into blank space.
    pub fn scrollback_lines(&self) -> usize {
        self.scrollback
            .iter()
            .map(|c| c.echo.split('\n').count() + c.text.lines().count())
            .sum()
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
            (KeyCode::PageUp, _) => {
                // Clamped to the flattened scrollback length: without this,
                // holding PgUp on a short session scrolls the console into
                // blank space that no key ever brings back into view (there
                // is nothing to `PageDown` back onto except more blank).
                self.scroll = (self.scroll + 5).min(self.scrollback_lines());
                WbAction::None
            }
            (KeyCode::PageDown, _) => {
                self.scroll = self.scroll.saturating_sub(5);
                WbAction::None
            }
            _ => match self.editor.key(k) {
                LineEdit::Submit(line) => {
                    if line.trim().is_empty() {
                        return WbAction::None;
                    }
                    self.scroll = 0;
                    WbAction::Dispatch(line)
                }
                LineEdit::Interrupt => {
                    // Ctrl-C on an empty prompt (the editor only reports
                    // `Interrupt` when its buffer is empty — see
                    // `LineEditor::key`): if a SQL statement is buffered,
                    // this is exactly what `.abort` is for — dispatch it
                    // through the ordinary console path, so it lands in the
                    // scrollback and history like any typed line. Otherwise
                    // there is nothing to abort; hint at the real quit key.
                    if self.sql_pending {
                        WbAction::Dispatch(".abort".to_string())
                    } else {
                        self.status = "Ctrl-Q quits".to_string();
                        WbAction::None
                    }
                }
                LineEdit::Redraw | LineEdit::Cleared | LineEdit::Eof | LineEdit::Nothing => WbAction::None,
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
                    self.marked.clear();
                    WbAction::Dispatch(".cd ..".into())
                } else {
                    WbAction::None
                }
            }
            KeyCode::Char('s') => self.shortcut(".sniff"),
            KeyCode::Char('e') => self.shortcut(".edit"),
            // `f` fits a target — only meaningful on a `*.tdy.sql` entry; a
            // data file's shortcut stays `s`. `--propose` for the same
            // reason `refit_pile` asks for it: the Pile this produces is
            // the one whose members' remedy menus need ranking.
            KeyCode::Char('f') => match self.browser.selected_entry() {
                Some(e) if e.kind == EntryKind::Target => {
                    match self.browser.selected_rel() {
                        Some(rel) => {
                            WbAction::Dispatch(format!(".fit {} --propose", quote_rel(&rel)))
                        }
                        None => WbAction::None,
                    }
                }
                _ => WbAction::None,
            },
            KeyCode::Char('d') => {
                self.toggle_mark();
                WbAction::None
            }
            KeyCode::Char('D') => self.draft_marked(),
            _ => WbAction::None,
        }
    }

    /// `d`: mark or unmark the selected entry — only a data file (a
    /// directory or a target has no business in a `.draft` file list, so
    /// selecting one is a no-op, not an error).
    fn toggle_mark(&mut self) {
        let Some(e) = self.browser.selected_entry() else { return };
        if e.kind != EntryKind::File {
            return;
        }
        let Some(rel) = self.browser.selected_rel() else { return };
        match self.marked.iter().position(|m| *m == rel) {
            Some(i) => {
                self.marked.remove(i);
            }
            None => self.marked.push(rel),
        }
    }

    /// `D`: dispatch `.draft` over every marked file, space-joined and each
    /// quoted the way the console's own tokenizer reads it back — the same
    /// `quote_rel` every other synthesized line uses. Clears the marks: a
    /// `.draft` run over them is the whole point of marking, and stale
    /// marks pointing at a directory the browser has since left would be
    /// worse than none. No marks is a status note, not a silent no-op.
    fn draft_marked(&mut self) -> WbAction {
        if self.marked.is_empty() {
            self.status = "no files marked — d marks a file, D drafts the marked files".to_string();
            return WbAction::None;
        }
        let files = std::mem::take(&mut self.marked);
        let line = files.iter().map(|f| quote_rel(f)).collect::<Vec<_>>().join(" ");
        WbAction::Dispatch(format!(".draft {line}"))
    }

    fn key_main(&mut self, k: KeyEvent) -> WbAction {
        // PgUp/PgDn scroll the main pane in EVERY context, ahead of the
        // context-specific dispatch below — Up/Down keep meaning selection
        // in Pile/Member, so scrolling needs keys those contexts do not
        // already claim. Checked first so it can never be shadowed by a
        // context's own `_ => WbAction::None` arm. `main_scroll` is an
        // offset from the TOP that grows as you move further into the
        // content (see the fallback Up/Down arm below: `Down` adds, `Up`
        // subtracts) — the opposite sense of the console's own `scroll`
        // field (lines scrolled up FROM THE BOTTOM) — so PgDn is the one
        // that adds here, mirroring `Down`, and PgUp subtracts, mirroring
        // `Up`.
        match k.code {
            KeyCode::PageDown => {
                self.main_scroll = (self.main_scroll + 5).min(self.main_scroll_bound());
                return WbAction::None;
            }
            KeyCode::PageUp => {
                self.main_scroll = self.main_scroll.saturating_sub(5);
                return WbAction::None;
            }
            _ => {}
        }
        // `[`/`]` page through a workbook's sheets — checked here, ahead of
        // the File/Member context dispatch below, so both get it without
        // duplicating the key in each context's own match.
        match k.code {
            KeyCode::Char('[') => return self.switch_sheet(-1),
            KeyCode::Char(']') => return self.switch_sheet(1),
            _ => {}
        }
        if let Context::Pile { report, selected, .. } = &mut self.context {
            let len = report.members.len();
            return match k.code {
                KeyCode::Up => {
                    *selected = selected.saturating_sub(1);
                    self.follow_pile_selection();
                    WbAction::None
                }
                KeyCode::Down => {
                    if len > 0 {
                        *selected = (*selected + 1).min(len - 1);
                    }
                    self.follow_pile_selection();
                    WbAction::None
                }
                KeyCode::Enter => self.enter_pile_member(),
                KeyCode::Char('f') => self.refit_pile(),
                KeyCode::Char('t') => self.edit_target(),
                _ => WbAction::None,
            };
        }
        if matches!(&self.context, Context::Member { .. }) {
            return self.key_member(k);
        }
        if matches!(&self.context, Context::Evidence { .. }) {
            return self.key_evidence(k);
        }
        match k.code {
            KeyCode::Up => {
                self.main_scroll = self.main_scroll.saturating_sub(1);
                WbAction::None
            }
            KeyCode::Down => {
                self.main_scroll = (self.main_scroll + 1).min(self.main_scroll_bound());
                WbAction::None
            }
            _ => WbAction::None,
        }
    }

    /// A generous upper bound for `main_scroll`, from what the current
    /// context actually has to show — not an exact fit (a wrapped line, a
    /// multi-row sheet header are not counted), just enough that scrolling
    /// cannot run away into blank space forever. Every arm here now names a
    /// context whose renderer actually reads `main_scroll` (`wb_ui`'s
    /// `draw_file_no_spec`/`draw_file_with_spec`, `draw_pile`, `draw_member`
    /// (left column only — see its own doc comment), `draw_evidence`, and
    /// the `Query` arm of `draw_main`) — `key_main`'s PgUp/PgDn read this in
    /// every context, and the fallback Up/Down arm (File/Query/Empty) does
    /// too; Pile and Member answer plain Up/Down with selection instead of
    /// scroll (see `key_main`/`key_member`), and Evidence's own Up/Down
    /// (see `key_evidence`) reads this too.
    ///
    /// The Evidence multiplier is 9, not a round number: `draw_evidence`
    /// prints, per row, one bold headline, up to 5 `head` lines, an optional
    /// `smallest` and an optional `largest`, then a blank separator —
    /// 1 + 5 + 1 + 1 + 1 = 9 in the worst case (a `Shift` with both extremes
    /// present); `Frame` and the illustration-less variants print fewer, so
    /// 9 stays an upper, not exact, bound.
    ///
    /// Pile's `+ 16` slack covers `draw_pile`'s own 2-line header (bold
    /// summary + a blank), which scrolls along with the member rows — no
    /// separate term needed, 16 is generous enough on its own. Member's
    /// bound is sized off the raw head alone (`lines` + `sheets` + `grid`
    /// rows), matching exactly what `draw_member`'s left column — the only
    /// half of that view `main_scroll` offsets — actually prints; the right
    /// column (status/review/remedies) is unscrolled, the same trade
    /// `draw_file_with_spec` already makes for its own two columns. Query's
    /// bound is sized off the table's row count (`table_lines` also prints
    /// a header and a count line, covered by the `+ 16` slack).
    fn main_scroll_bound(&self) -> usize {
        match &self.context {
            Context::File { raw, preview, .. } => {
                raw.lines.len()
                    + raw.sheets.len()
                    + preview.as_ref().map(|t| t.rows.len()).unwrap_or(0)
                    + 16
            }
            Context::Evidence { rows, .. } => rows.len() * 9 + 16,
            Context::Pile { report, .. } => report.members.len() + 16,
            Context::Member { raw, .. } => {
                raw.as_ref()
                    .map(|r| r.lines.len() + r.sheets.len() + r.grid.len())
                    .unwrap_or(0)
                    + 16
            }
            Context::Query(t) => t.rows.len() + 16,
            _ => 16,
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
        // A member's raw column opens at its first line. The Pile this came
        // from consumes the same `main_scroll`, so a paged-down pile would
        // otherwise open a short raw head scrolled past its end — a blank
        // pane, indistinguishable from an empty file.
        self.main_scroll = 0;
        self.preview_action(preview_path, None)
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
        // Back to a list of members, at its top: the offset on screen was
        // the member's raw column's, and means nothing here (see
        // `enter_pile_member`).
        self.main_scroll = 0;
    }

    /// Keys over a Member context (Main focus): Up/Down pick a remedy,
    /// clamped to the current menu; `e` edits the file itself; `1`-`9` stage
    /// the corresponding remedy from `member_remedies()` for confirmation
    /// (see `stage_remedy`); `Enter` stages whichever remedy `▸` currently
    /// marks — the same call a digit makes, just reading the index off
    /// `remedy_selected` instead of the key itself, so the marker stops
    /// being decorative; `a` dispatches `.accept` step one (see
    /// `accept_member`).
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
            KeyCode::Enter => {
                let Context::Member { remedy_selected, .. } = &self.context else {
                    return WbAction::None;
                };
                let idx = *remedy_selected;
                self.stage_remedy(idx)
            }
            KeyCode::Char('e') => self.edit_member(),
            KeyCode::Char('t') => self.edit_target(),
            KeyCode::Char(c @ '1'..='9') => self.stage_remedy((c as u8 - b'1') as usize),
            KeyCode::Char('a') => self.accept_member(),
            _ => WbAction::None,
        }
    }

    /// `a` over a Member with a live judgement waiting on review (`review:
    /// Some(_)` and not yet `accepted`) dispatches `.accept TARGET MEMBER` —
    /// the console's own step one, which returns evidence and remembers the
    /// line (see the session's `pending_accept`); any other member (fits, a
    /// gap, or already accepted) has nothing to accept, so the key is
    /// swallowed with a status note rather than dispatching a line the
    /// console would itself reject with "nothing to accept".
    fn accept_member(&mut self) -> WbAction {
        let (target_rel, member_rel, reviewable) = match &self.context {
            Context::Member { target, report, member, .. } => match report.members.get(*member) {
                Some(m) => (self.rel_spelling(target), m.path.clone(), m.review.is_some() && !m.accepted),
                None => return WbAction::None,
            },
            _ => return WbAction::None,
        };
        if reviewable {
            WbAction::Dispatch(format!(".accept {} {}", quote_rel(&target_rel), quote_rel(&member_rel)))
        } else {
            self.status = "nothing to accept for this member".to_string();
            WbAction::None
        }
    }

    /// Keys over an Evidence context (Main focus): `a` re-dispatches the
    /// exact `.accept` line that produced this evidence, verbatim — the
    /// session recognises the repeat as step two (see the module doc on
    /// `Context::Evidence`). Evidence has no selection (unlike Pile/Member),
    /// so Up/Down scroll `main_scroll` here rather than sit idle — the same
    /// meaning File's fallback arm gives them. `Esc` is handled earlier in
    /// `key()`, ahead of the focus dispatch, because it needs to leave
    /// `Focus::Main` in place the same way Pile/Member's `Esc` does.
    fn key_evidence(&mut self, k: KeyEvent) -> WbAction {
        match k.code {
            KeyCode::Char('a') => match &self.context {
                Context::Evidence { line, .. } => WbAction::Dispatch(line.clone()),
                _ => WbAction::None,
            },
            KeyCode::Up => {
                self.main_scroll = self.main_scroll.saturating_sub(1);
                WbAction::None
            }
            KeyCode::Down => {
                self.main_scroll = (self.main_scroll + 1).min(self.main_scroll_bound());
                WbAction::None
            }
            _ => WbAction::None,
        }
    }

    /// Stage `member_remedies()[idx]` as a `pending_edit` for the confirm
    /// overlay — the one place both a digit key and `Enter` on the `▸`
    /// marker end up (see `key_member`). Out of range and "no target text
    /// loaded yet" both end in a status note rather than an edit — the
    /// second is real (a fit that never landed a `Fitted`, or one whose
    /// target could not be resolved) and must not panic or silently do
    /// nothing unexplained. `remedy::apply`'s own errors (a column the
    /// declaration does not have any more, an edit that would not parse or
    /// would change more than it says) surface the same way.
    fn stage_remedy(&mut self, idx: usize) -> WbAction {
        let remedies = self.member_remedies();
        let Some(remedy) = remedies.get(idx).cloned() else {
            self.status = format!("no remedy {}", idx + 1);
            return WbAction::None;
        };
        let Some(sql) = self.target_sql.clone() else {
            self.status = "target text not loaded yet".to_string();
            return WbAction::None;
        };
        let Context::Member { target, .. } = &self.context else { return WbAction::None };
        let target = target.clone();
        match remedy::apply(&sql, &remedy) {
            Ok(edit) => {
                self.pending_edit = Some((remedy, edit, target, sql));
                WbAction::None
            }
            Err(e) => {
                self.status = format!("{e:#}");
                WbAction::None
            }
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

    /// `t` over a Pile or Member (Main focus): open the target itself in
    /// $EDITOR — `.edit <target rel>`, spelled the same way `f`/`e` already
    /// spell their own paths. Both contexts carry `target`, so this reads
    /// straight off whichever one is current rather than duplicating the
    /// match per caller.
    fn edit_target(&self) -> WbAction {
        let target = match &self.context {
            Context::Pile { target, .. } => target,
            Context::Member { target, .. } => target,
            _ => return WbAction::None,
        };
        WbAction::Dispatch(format!(".edit {}", quote_rel(&self.rel_spelling(target))))
    }

    /// `f` from the Pile context: re-dispatch the same `.fit` that produced
    /// it, spelled relative to the browser's current directory when the
    /// target lives under it (the common case) and as its full path
    /// otherwise — the dispatched line must still resolve from the
    /// session's cwd even after a `.cd` has moved the browser elsewhere.
    ///
    /// `--propose` rides along for the same reason the launch line carries
    /// it (`main::dry_run_target_mode`): the proposals ARE the remedy
    /// menu's ranking (see `member_remedies`), and a refit that dropped
    /// them would leave the next member's menu in file order. Unlike the
    /// launch line this one is *not* a dry run — `f` is the key that writes
    /// the lock for real.
    fn refit_pile(&self) -> WbAction {
        let Context::Pile { target, .. } = &self.context else { return WbAction::None };
        WbAction::Dispatch(format!(".fit {} --propose", quote_rel(&self.rel_spelling(target))))
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
    fn preview_selected(&mut self) -> WbAction {
        match self.browser.selected_entry().map(|e| e.kind) {
            Some(EntryKind::File) => match self.browser.selected_path() {
                Some(p) => self.preview_action(p, None),
                None => WbAction::None,
            },
            _ => WbAction::None,
        }
    }

    /// `Enter`: a directory descends and dispatches `.cd <rel>` (the rel
    /// path is captured before `browser.enter()` mutates the dir — after
    /// the move, `selected_rel()` would answer from the wrong directory); a
    /// file returns its path to preview. Marks are cleared here too — see
    /// `marked`'s doc comment — since this is the one path a directory move
    /// takes without ever reaching `apply`'s `sync_dir` call (the browser
    /// moves synchronously, ahead of the session's own confirmation).
    fn enter_browser(&mut self) -> WbAction {
        let Some(kind) = self.browser.selected_entry().map(|e| e.kind) else {
            return WbAction::None;
        };
        if kind == EntryKind::Dir {
            let rel = self.browser.selected_rel();
            self.browser.enter();
            self.marked.clear();
            match rel {
                Some(r) => WbAction::Dispatch(format!(".cd {}", quote_rel(&r))),
                None => WbAction::None,
            }
        } else {
            let p = self.browser.enter();
            match (kind, p) {
                (EntryKind::File, Some(path)) => self.preview_action(path, None),
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

