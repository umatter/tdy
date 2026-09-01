//! The application state, and every decision it makes — with no terminal in
//! sight.
//!
//! Keys arrive as [`Key`], not as crossterm events, and handling one returns
//! an [`Action`] the caller performs. So the whole behaviour of the UI —
//! which screen a key moves to, which remedy a number picks, what `a` does
//! on the accept screen and what it refuses to do anywhere else — is a pure
//! function that tests can drive without a pty.
//!
//! The rendering reads this state and never changes it.

use std::path::{Path, PathBuf};

use tdy::report::{MemberStatus, PileReport};

use crate::evidence::Evidence;
use crate::remedy::{self, Edit, Remedy};

/// The keys the UI understands, named for what they are rather than for the
/// bytes a terminal sends.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Key {
    Up,
    Down,
    Enter,
    Esc,
    Tab,
    Char(char),
    Backspace,
    /// Ctrl-C. Its own key, not a `Char('q')` in disguise: `q` means quit on
    /// some screens and nothing on others, and a TUI that traps Ctrl-C is one
    /// people learn to kill from another window.
    Interrupt,
}

/// What the UI wants done. Performed by the caller, which owns the I/O.
#[derive(Debug, Clone, PartialEq)]
pub enum Action {
    None,
    Quit,
    /// Re-fit the pile, accepting these members (relative paths).
    Refit { accept: Vec<String> },
    /// Write the target file, then re-fit.
    WriteTarget { text: String },
    /// Read the file behind the selected member and compute what accepting
    /// it would do.
    ComputeEvidence { member: String },
    /// Read the head of the file behind the selected member, as tdy sees it.
    ComputePreview { member: String },
    /// Run this SQL and show the result.
    RunQuery(String),
    /// Hand the terminal to `$EDITOR` for this file, then re-fit.
    OpenEditor(PathBuf),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Screen {
    Pile,
    Member,
    /// Showing what a judgement does, before it can be accepted.
    Accept,
    /// A diff awaiting yes/no.
    Confirm,
    Query,
    Help,
}

/// The file as tdy sees it after the frame's transforms. The answer to "no
/// column of this file binds" is nearly always to *look*, so the member
/// screen shows this beside the gap rather than making the reader go and open
/// the file themselves.
#[derive(Debug, Clone, Default)]
pub struct Preview {
    pub header: Vec<String>,
    pub rows: Vec<Vec<String>>,
}

/// A query result, kept as strings — the scratchpad shows values, it does not
/// compute with them.
#[derive(Debug, Clone, Default)]
pub struct QueryResult {
    pub columns: Vec<String>,
    pub rows: Vec<Vec<String>>,
    pub total: usize,
    pub truncated: bool,
}

pub struct App {
    pub target_path: PathBuf,
    /// The target's source text, as last read.
    pub target_sql: String,
    pub report: Option<PileReport>,
    pub screen: Screen,
    /// Index into `report.members`.
    pub selected: usize,
    /// Index into the selected member's remedy menu.
    pub remedy_selected: usize,
    /// The menu itself, computed when the member is opened rather than on
    /// every frame — building it re-serialises every `Problem` to JSON, and
    /// the draw loop runs many times a second.
    remedy_menu: Vec<Remedy>,
    /// One-line message under the frame.
    pub status: String,
    /// Set while a fit runs: what it is doing right now.
    pub busy: Option<String>,
    /// A fit is in flight. Separate from `busy`, which is a *display* field
    /// that several message kinds set and clear — using it as the guard let
    /// an unrelated note clear the way for a second concurrent fit racing
    /// the first one on the sidecars and the lock.
    fit_in_flight: bool,
    /// Evidence for the member on the accept screen — every judgement in it,
    /// not just the first, because the rest would otherwise be accepted
    /// unseen.
    pub evidence: Option<Vec<Evidence>>,
    /// The selected member's first rows, as tdy sees them.
    pub preview: Option<Preview>,
    /// A pending edit awaiting confirmation.
    pub pending: Option<(Remedy, Edit)>,
    pub query_input: String,
    pub query_result: Option<QueryResult>,
    pub query_history: Vec<String>,
    pub should_quit: bool,
}

impl App {
    pub fn new(target_path: PathBuf, target_sql: String) -> App {
        App {
            target_path,
            target_sql,
            report: None,
            screen: Screen::Pile,
            selected: 0,
            remedy_selected: 0,
            remedy_menu: Vec::new(),
            status: "fitting…".into(),
            busy: Some("starting".into()),
            fit_in_flight: true,
            evidence: None,
            preview: None,
            pending: None,
            query_input: String::new(),
            query_result: None,
            query_history: Vec::new(),
            should_quit: false,
        }
    }

    pub fn members(&self) -> &[tdy::report::MemberReport] {
        self.report.as_ref().map(|r| r.members.as_slice()).unwrap_or(&[])
    }

    pub fn selected_member(&self) -> Option<&tdy::report::MemberReport> {
        self.members().get(self.selected)
    }

    /// The remedies offered for the selected member, best first. Cached; see
    /// [`App::refresh_remedies`].
    pub fn remedies(&self) -> &[Remedy] {
        &self.remedy_menu
    }

    fn refresh_remedies(&mut self) {
        self.remedy_menu = self.compute_remedies();
        self.remedy_selected = 0;
    }

    /// The remedies offered for the selected member, best first.
    ///
    /// "Best" is not a guess: `tdy fit --propose` reports which of the file's
    /// columns could actually *produce* the declared type, and those come
    /// first. Offering the file's header in file order instead would put an
    /// arbitrary column at [1] — and a menu whose first entry is usually
    /// wrong is a menu that teaches people to stop reading it.
    fn compute_remedies(&self) -> Vec<Remedy> {
        let Some(m) = self.selected_member() else { return Vec::new() };
        let mut out: Vec<Remedy> = Vec::new();
        let push = |r: Remedy, out: &mut Vec<Remedy>| {
            if !out.contains(&r) {
                out.push(r);
            }
        };

        // Type-compatible candidates first, in the order the planner ranked
        // them, and only for columns that actually failed to bind.
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
            let v = serde_json::to_value(p).unwrap_or_default();
            for r in remedy::remedies_for(&v, &m.path) {
                push(r, &mut out);
            }
        }
        if out.is_empty() {
            out.push(Remedy::ExcludeFile { rel: m.path.clone() });
        }
        out
    }

    /// Is the highlighted remedy one that removes data from the dataset?
    ///
    /// A member with nothing wrong has only "exclude this file" to offer, and
    /// leaving that under the cursor makes Enter — the key that opened the
    /// screen — stage a removal. Nothing destructive is ever preselected.
    fn selection_is_destructive(&self) -> bool {
        matches!(self.remedy_menu.get(self.remedy_selected), Some(Remedy::ExcludeFile { .. }))
    }

    /// A finished fit replaces the report. The selection is kept where it can
    /// be — a user who was looking at the eleventh file wants to still be
    /// looking at it, and jumping to the top after every re-fit would make
    /// the loop feel like it forgot what they were doing.
    pub fn set_report(&mut self, report: PileReport) {
        self.preview = None;
        let keep = self.selected_member().map(|m| m.path.clone());
        self.selected = keep
            .and_then(|p| report.members.iter().position(|m| m.path == p))
            .unwrap_or(0);
        self.status = summary(&report);
        self.report = Some(report);
        self.busy = None;
        self.fit_in_flight = false;
        self.refresh_remedies();
    }

    pub fn set_error(&mut self, msg: String) {
        self.status = msg;
        self.busy = None;
        // An error ends whatever was running, including a fit.
        self.fit_in_flight = false;
    }

    /// A transient remark. Does not end a fit — the fit ends when its report
    /// or its error arrives, and nothing else may open the door to a second
    /// one running over the same sidecars.
    pub fn note(&mut self, msg: String) {
        self.status = msg;
        if !self.fit_in_flight {
            self.busy = None;
        }
    }

    pub fn handle(&mut self, key: Key) -> Action {
        // Ctrl-C leaves from anywhere, whatever is on screen.
        if key == Key::Interrupt {
            self.should_quit = true;
            return Action::Quit;
        }
        // A running fit owns the file system; the only key that means
        // anything is the one that leaves.
        if self.busy.is_some() || self.fit_in_flight {
            return match key {
                Key::Char('q') | Key::Char('Q') => {
                    self.should_quit = true;
                    Action::Quit
                }
                _ => Action::None,
            };
        }
        match self.screen {
            Screen::Pile => self.on_pile(key),
            Screen::Member => self.on_member(key),
            Screen::Accept => self.on_accept(key),
            Screen::Confirm => self.on_confirm(key),
            Screen::Query => self.on_query(key),
            Screen::Help => {
                self.screen = Screen::Pile;
                Action::None
            }
        }
    }

    fn on_pile(&mut self, key: Key) -> Action {
        match key {
            Key::Up => {
                self.selected = self.selected.saturating_sub(1);
                Action::None
            }
            Key::Down => {
                let n = self.members().len();
                if n > 0 {
                    self.selected = (self.selected + 1).min(n - 1);
                }
                Action::None
            }
            Key::Enter => match self.selected_member() {
                Some(m) => {
                    let member = m.path.clone();
                    self.refresh_remedies();
                    self.preview = None;
                    self.screen = Screen::Member;
                    Action::ComputePreview { member }
                }
                None => Action::None,
            },
            Key::Char('q') => {
                self.should_quit = true;
                Action::Quit
            }
            Key::Char('f') => {
                self.busy = Some("re-fitting".into());
                self.fit_in_flight = true;
                Action::Refit { accept: Vec::new() }
            }
            Key::Char('t') => Action::OpenEditor(self.target_path.clone()),
            Key::Char('?') => {
                self.screen = Screen::Help;
                Action::None
            }
            Key::Tab | Key::Char('/') => {
                self.screen = Screen::Query;
                Action::None
            }
            _ => Action::None,
        }
    }

    fn on_member(&mut self, key: Key) -> Action {
        match key {
            Key::Esc => {
                self.screen = Screen::Pile;
                Action::None
            }
            Key::Up => {
                self.remedy_selected = self.remedy_selected.saturating_sub(1);
                Action::None
            }
            Key::Down => {
                let n = self.remedy_menu.len();
                if n > 0 {
                    self.remedy_selected = (self.remedy_selected + 1).min(n - 1);
                }
                Action::None
            }
            // Applying a remedy never writes: it produces a diff to confirm.
            // Enter will not stage a removal, though — it is the key that
            // opened this screen, and a stray second press must not offer to
            // drop the file.
            Key::Enter if self.selection_is_destructive() => {
                self.status =
                    "excluding a file drops it from the dataset — press its number to                      stage that".into();
                Action::None
            }
            Key::Enter => self.stage_remedy(self.remedy_selected),
            Key::Char(c) if c.is_ascii_digit() && c != '0' => {
                self.stage_remedy(c as usize - '1' as usize)
            }
            Key::Char('e') => match self.selected_member() {
                Some(m) => {
                    let data = self.target_dir().join(&m.path);
                    Action::OpenEditor(tdy::sidecar::sidecar_path(&data))
                }
                None => Action::None,
            },
            // The accept screen is the ONLY way to accept, and it is reached
            // only from a member that actually needs a judgement.
            Key::Char('a') => match self.selected_member() {
                Some(m) if m.status == MemberStatus::NeedsReview => {
                    let member = m.path.clone();
                    self.evidence = None;
                    self.screen = Screen::Accept;
                    self.busy = Some(format!("reading {member}"));
                    Action::ComputeEvidence { member }
                }
                Some(_) => {
                    self.status = "nothing to accept: this member needs no judgement".into();
                    Action::None
                }
                None => Action::None,
            },
            _ => Action::None,
        }
    }

    fn stage_remedy(&mut self, index: usize) -> Action {
        let Some(r) = self.remedy_menu.get(index).cloned() else { return Action::None };
        match remedy::apply(&self.target_sql, &r) {
            Ok(edit) => {
                self.pending = Some((r, edit));
                self.screen = Screen::Confirm;
            }
            Err(e) => self.status = format!("{e:#}"),
        }
        Action::None
    }

    fn on_confirm(&mut self, key: Key) -> Action {
        match key {
            Key::Char('y') | Key::Enter => {
                let Some((_, edit)) = self.pending.take() else {
                    self.screen = Screen::Pile;
                    return Action::None;
                };
                // The in-memory copy is NOT updated here: the caller owns
                // the write, and only a write that succeeded makes this text
                // the file's text. Updating it first left the next diff
                // quoting a "before" line the file does not contain.
                self.screen = Screen::Pile;
                self.busy = Some("re-fitting".into());
                self.fit_in_flight = true;
                Action::WriteTarget { text: edit.new_text }
            }
            _ => {
                self.pending = None;
                self.screen = Screen::Member;
                Action::None
            }
        }
    }

    fn on_accept(&mut self, key: Key) -> Action {
        match key {
            // Deliberately not `y`: accepting is not a yes/no prompt, it is a
            // distinct act on a screen that shows what it does. And there is
            // no accept-all anywhere, at any depth.
            //
            // No evidence, no acceptance. While it is being read the busy
            // guard already blocks every key — but if the read *fails*, busy
            // clears and the screen still says "reading the file…", and
            // accepting there would be accepting against a blank panel. The
            // gate's entire value is that a human saw the consequence.
            Key::Char('a') if self.evidence.is_none() => {
                self.status =
                    "nothing to accept against yet — the consequence could not be read".into();
                Action::None
            }
            Key::Char('a') => {
                let Some(m) = self.selected_member() else { return Action::None };
                let member = m.path.clone();
                self.screen = Screen::Pile;
                self.evidence = None;
                self.busy = Some(format!("accepting {member}"));
                self.fit_in_flight = true;
                Action::Refit { accept: vec![member] }
            }
            Key::Esc | Key::Char('q') => {
                self.screen = Screen::Member;
                self.evidence = None;
                Action::None
            }
            _ => Action::None,
        }
    }

    fn on_query(&mut self, key: Key) -> Action {
        match key {
            Key::Esc => {
                self.screen = Screen::Pile;
                Action::None
            }
            Key::Enter => {
                let sql = self.query_input.trim().to_string();
                if sql.is_empty() {
                    return Action::None;
                }
                self.query_history.push(sql.clone());
                self.busy = Some("querying".into());
                Action::RunQuery(sql)
            }
            Key::Backspace => {
                self.query_input.pop();
                Action::None
            }
            Key::Up => {
                if let Some(last) = self.query_history.last() {
                    self.query_input = last.clone();
                }
                Action::None
            }
            Key::Char(c) => {
                self.query_input.push(c);
                Action::None
            }
            _ => Action::None,
        }
    }

    pub fn target_dir(&self) -> PathBuf {
        self.target_path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from("."))
    }

    /// The default query for the scratchpad: whatever the user is most likely
    /// to want to type first.
    pub fn default_query(&self) -> String {
        format!("SELECT * FROM dataset('{}') LIMIT 20", self.target_path.display())
    }
}

/// The counts the whole UI uses, so the header and the status line cannot
/// disagree about the word "fit". `PileReport::fitted` counts members that
/// *reached* the target, which includes the ones still waiting on a human —
/// so a screen that prints it beside a separate "review" count is printing
/// them twice.
pub fn counts(r: &PileReport) -> (usize, usize, usize) {
    (r.fitted.saturating_sub(r.needs_review), r.needs_review, r.failed)
}

fn summary(r: &PileReport) -> String {
    let (fits, _, _) = counts(r);
    let mut s = format!("{} of {} fit", fits, r.members.len());
    if r.needs_review > 0 {
        s.push_str(&format!(" · {} awaiting a human", r.needs_review));
    }
    if r.failed > 0 {
        s.push_str(&format!(" · {} refused", r.failed));
    }
    match &r.lock_written {
        Some(_) => s.push_str(" · lock written"),
        None if r.failed > 0 => s.push_str(" · no lock (a member does not fit)"),
        None => {}
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use tdy::report::{MemberReport, Problem};

    fn member(path: &str, status: MemberStatus) -> MemberReport {
        MemberReport {
            path: path.into(),
            status,
            via: Some("heuristic".into()),
            sources: vec![],
            review: (status == MemberStatus::NeedsReview)
                .then(|| "`amount_chf` applies decimal_shift = -2".to_string()),
            accepted: false,
            notes: vec![],
            problems: vec![],
            proposals: vec![],
        }
    }

    fn report(members: Vec<MemberReport>) -> PileReport {
        let failed = members.iter().filter(|m| m.status == MemberStatus::Gaps).count();
        let needs_review =
            members.iter().filter(|m| m.status == MemberStatus::NeedsReview).count();
        PileReport {
            target: "sales".into(),
            target_file: "sales.tdy.sql".into(),
            declared_columns: 3,
            fitted: members.len() - failed,
            failed,
            needs_review,
            members,
            lock_written: None,
            dry_run: false,
        }
    }

    fn app() -> App {
        let mut a = App::new("sales.tdy.sql".into(), String::new());
        a.set_report(report(vec![
            member("2025-01.csv", MemberStatus::Fits),
            member("2025-07.csv", MemberStatus::NeedsReview),
            member("2025-11.csv", MemberStatus::Gaps),
        ]));
        a
    }

    /// The rule the whole accept design rests on: acceptance happens only
    /// from the evidence screen, one member at a time. `a` from the pile does
    /// nothing, and no key anywhere accepts more than one member.
    #[test]
    fn acceptance_is_reachable_only_through_the_evidence_screen() {
        let mut a = app();
        a.selected = 1; // the member that needs review

        // From the pile, `a` is not an accept.
        assert_eq!(a.handle(Key::Char('a')), Action::None);
        assert_eq!(a.screen, Screen::Pile);

        // Into the member (which also asks for a preview of the file), then
        // `a` only *asks for the evidence*.
        assert_eq!(
            a.handle(Key::Enter),
            Action::ComputePreview { member: "2025-07.csv".into() }
        );
        assert_eq!(a.screen, Screen::Member);
        let act = a.handle(Key::Char('a'));
        assert_eq!(act, Action::ComputeEvidence { member: "2025-07.csv".into() });
        assert_eq!(a.screen, Screen::Accept);

        // Only once the evidence is actually on the screen does `a` accept —
        // and exactly one member.
        a.busy = None;
        a.evidence = Some(vec![Evidence::Unillustrated { reason: "x".into() }]);
        let act = a.handle(Key::Char('a'));
        assert_eq!(act, Action::Refit { accept: vec!["2025-07.csv".into()] });
        assert_eq!(a.screen, Screen::Pile);
    }

    /// A member that needs no judgement cannot be accepted at all — there is
    /// nothing to say yes to, and offering the screen would teach the gesture
    /// on files where it means nothing.
    #[test]
    fn a_member_that_needs_no_judgement_has_no_accept_screen() {
        let mut a = app();
        a.selected = 0; // fits
        a.handle(Key::Enter);
        let act = a.handle(Key::Char('a'));
        assert_eq!(act, Action::None);
        assert_eq!(a.screen, Screen::Member);
        assert!(a.status.contains("nothing to accept"), "{}", a.status);
    }

    /// The gate's whole value is that a human saw the consequence, so there
    /// is no accepting a screen that has none. While the evidence is being
    /// read the busy guard blocks every key; if the read *fails*, busy
    /// clears — and this is what stops `a` from accepting against a panel
    /// that still says "reading the file…".
    #[test]
    fn there_is_no_accepting_without_evidence_on_the_screen() {
        let mut a = app();
        a.selected = 1;
        a.handle(Key::Enter);
        a.handle(Key::Char('a'));
        assert_eq!(a.screen, Screen::Accept);

        // While it reads: busy, so nothing at all happens.
        assert!(a.busy.is_some());
        assert_eq!(a.handle(Key::Char('a')), Action::None);

        // The read failed. Not busy any more, still no evidence — and still
        // no acceptance.
        a.set_error("could not read the file".into());
        assert_eq!(a.handle(Key::Char('a')), Action::None);
        assert!(a.status.contains("nothing to accept against"), "{}", a.status);

        // With the evidence on screen, it accepts.
        a.evidence = Some(vec![Evidence::Unillustrated { reason: "x".into() }]);
        assert_eq!(
            a.handle(Key::Char('a')),
            Action::Refit { accept: vec!["2025-07.csv".into()] }
        );
    }

    /// Escaping the accept screen accepts nothing and drops the evidence, so
    /// the next visit re-reads rather than showing a stale panel.
    #[test]
    fn leaving_the_accept_screen_accepts_nothing() {
        let mut a = app();
        a.selected = 1;
        a.handle(Key::Enter);
        a.handle(Key::Char('a'));
        a.busy = None;
        a.evidence = Some(vec![Evidence::Unillustrated { reason: "x".into() }]);
        let act = a.handle(Key::Esc);
        assert_eq!(act, Action::None);
        assert_eq!(a.screen, Screen::Member);
        assert!(a.evidence.is_none());
    }

    /// A remedy is staged as a diff, never written on the keystroke that
    /// chose it. `y` confirms; anything else backs out with the target
    /// untouched.
    #[test]
    fn a_remedy_is_confirmed_as_a_diff_before_anything_is_written() {
        let sql = "CREATE TABLE t (\n  region TEXT NOT NULL\n)\nWITH (files = '*.csv');\n";
        let mut a = App::new("t.tdy.sql".into(), sql.into());
        let mut m = member("2025-11.csv", MemberStatus::Gaps);
        m.problems = vec![Problem {
            kind: "no_candidate".into(),
            column: Some("region".into()),
            message: "…".into(),
            want: Some("TEXT".into()),
            tried: vec!["region".into()],
            header: vec!["Datum".into(), "Kanton".into()],
            choices: vec![],
            field: None,
        }];
        a.set_report(report(vec![m]));
        a.handle(Key::Enter);
        assert_eq!(a.screen, Screen::Member);

        // Pick "teach `region` the spelling Kanton" (the second header).
        let remedies = a.remedies();
        let pick = remedies
            .iter()
            .position(|r| matches!(r, Remedy::AddMatch { spelling, .. } if spelling == "Kanton"))
            .expect("the file's own headers are offered as spellings");
        let act = a.handle(Key::Char(char::from_digit(pick as u32 + 1, 10).unwrap()));
        assert_eq!(act, Action::None, "choosing a remedy must not write");
        assert_eq!(a.screen, Screen::Confirm);
        let (_, edit) = a.pending.clone().expect("a diff is staged");
        assert!(edit.diff().contains("Kanton"), "{}", edit.diff());

        // Backing out leaves the target exactly as it was.
        let mut b = a.clone_for_test();
        assert_eq!(b.handle(Key::Esc), Action::None);
        assert_eq!(b.target_sql, sql);
        assert!(b.pending.is_none());

        // Confirming writes — and the write is an Action, so the caller does
        // it and the state machine stays pure.
        let act = a.handle(Key::Char('y'));
        match act {
            Action::WriteTarget { text } => assert!(text.contains("Kanton")),
            other => panic!("expected a write, got {other:?}"),
        }
    }

    /// While a fit is running the UI takes no orders but `q`: the file system
    /// is being written and a second fit on top of it is not a thing to allow
    /// by holding a key down.
    #[test]
    fn a_running_fit_ignores_every_key_but_quit() {
        let mut a = app();
        a.busy = Some("re-fitting".into());
        assert_eq!(a.handle(Key::Enter), Action::None);
        assert_eq!(a.handle(Key::Char('f')), Action::None);
        assert_eq!(a.handle(Key::Char('a')), Action::None);
        assert_eq!(a.screen, Screen::Pile);
        assert_eq!(a.handle(Key::Char('q')), Action::Quit);
    }

    /// Ctrl-C leaves from any screen, including ones where `q` means nothing
    /// and including mid-fit. A TUI that traps it is one people learn to kill
    /// from another window.
    #[test]
    fn ctrl_c_quits_from_anywhere() {
        for screen in [Screen::Pile, Screen::Member, Screen::Accept, Screen::Confirm, Screen::Query]
        {
            let mut a = app();
            a.screen = screen;
            assert_eq!(a.handle(Key::Interrupt), Action::Quit, "stuck on {screen:?}");
            assert!(a.should_quit);
        }
        // Mid-fit too.
        let mut a = app();
        a.busy = Some("fitting".into());
        assert_eq!(a.handle(Key::Interrupt), Action::Quit);
    }

    /// A transient note must not open the door to a second fit running over
    /// the sidecars and the lock of the first. `busy` is a display field that
    /// several message kinds touch; the single-flight guard is its own.
    #[test]
    fn a_note_during_a_fit_does_not_release_the_single_flight_guard() {
        let mut a = app();
        a.busy = Some("re-fitting".into());
        a.fit_in_flight = true;

        // A failed preview arrives and says so…
        a.note("preview unavailable: no such file".into());
        assert!(a.status.contains("preview unavailable"));
        // …and the fit is still in flight, so `f` starts nothing.
        assert_eq!(a.handle(Key::Char('f')), Action::None);

        // Only the fit's own outcome releases it.
        a.set_report(report(vec![member("2025-01.csv", MemberStatus::Fits)]));
        assert_eq!(a.handle(Key::Char('f')), Action::Refit { accept: vec![] });
    }

    /// A re-fit keeps the user looking at the file they were looking at.
    #[test]
    fn the_selection_survives_a_refit() {
        let mut a = app();
        a.selected = 2;
        let was = a.selected_member().unwrap().path.clone();
        a.set_report(report(vec![
            member("2025-01.csv", MemberStatus::Fits),
            member("2025-07.csv", MemberStatus::NeedsReview),
            member("2025-11.csv", MemberStatus::Fits),
        ]));
        assert_eq!(a.selected_member().unwrap().path, was);

        // …and falls back to the top when that file is gone (excluded).
        a.set_report(report(vec![member("2025-01.csv", MemberStatus::Fits)]));
        assert_eq!(a.selected, 0);
    }

    impl App {
        fn clone_for_test(&self) -> App {
            let mut a = App::new(self.target_path.clone(), self.target_sql.clone());
            a.screen = self.screen;
            a.selected = self.selected;
            a.pending = self.pending.clone();
            a.busy = None;
            a.fit_in_flight = false;
            a
        }
    }
}
