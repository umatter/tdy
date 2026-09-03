use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use tdy::console::{Outcome, Payload, RawHead, Table};
use tdy::report::{
    MemberReport, MemberStatus, PileReport, Problem, ProposalReport, SourceBinding,
};
use tdy_tui::browser::Browser;
use tdy_tui::remedy::Remedy;
use tdy_tui::workbench::{Context, Focus, WbAction, Workbench};

/// From a Pile with the given members, focus Main and press Enter on
/// `selected` — the state shared by the Member tests below.
fn pile_and_enter(d: &tempfile::TempDir, members: Vec<MemberReport>, selected: usize) -> (Workbench, WbAction) {
    let mut w = wb(d);
    w.begin(".fit sales.tdy.sql");
    let report = pile_report("sales.tdy.sql", members);
    w.apply(outcome(".fit sales.tdy.sql", "", Payload::Fitted(report)), d.path());
    if let Context::Pile { selected: sel, .. } = &mut w.context {
        *sel = selected;
    }
    w.key(key(KeyCode::Tab)); // Browser
    w.key(key(KeyCode::Tab)); // Main
    let act = w.key(key(KeyCode::Enter));
    (w, act)
}

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
    Workbench::new(Browser::new(d.path()).unwrap(), vec![], 0.8)
}
fn outcome(echo: &str, text: &str, payload: Payload) -> Outcome {
    Outcome { echo: echo.into(), text: text.into(), payload, ok: true }
}

// Copied from the old `tests/render.rs:40-75`, now deleted (Task 7) — the
// member-builder pattern a synthetic `PileReport` needs.
fn member(path: &str, status: MemberStatus) -> MemberReport {
    MemberReport {
        path: path.into(),
        status,
        via: Some("heuristic".into()),
        sources: vec![SourceBinding { column: "month".into(), source: "Datum".into() }],
        review: (status == MemberStatus::NeedsReview).then(|| {
            "`amount_chf` applies decimal_shift = -2, which changes every value".into()
        }),
        accepted: false,
        notes: vec![],
        problems: vec![],
        proposals: vec![],
    }
}

fn gap_member(path: &str) -> MemberReport {
    let mut m = member(path, MemberStatus::Gaps);
    m.problems = vec![Problem {
        kind: "no_candidate".into(),
        column: Some("region".into()),
        message: "`region` (TEXT): no column of this file binds\n    looked for \"region\""
            .into(),
        want: Some("TEXT".into()),
        tried: vec!["region".into()],
        header: vec!["Datum".into(), "Kanton".into()],
        choices: vec![],
        field: None,
    }];
    m
}

fn pile_report(target_file: &str, members: Vec<MemberReport>) -> PileReport {
    let failed = members.iter().filter(|m| m.status == MemberStatus::Gaps).count();
    let needs_review = members.iter().filter(|m| m.status == MemberStatus::NeedsReview).count();
    PileReport {
        target: "sales".into(),
        target_file: target_file.into(),
        declared_columns: 3,
        fitted: members.len() - failed,
        failed,
        needs_review,
        members,
        lock_written: None,
        dry_run: false,
    }
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
    let raw = RawHead { lines: vec!["A;B".into(), "1;2".into()], truncated: false, sheets: vec![], grid: vec![], grid_sheet: None };
    let follow = w.apply(outcome(".show a.csv", "a.csv:\n  A;B\n", Payload::Shown {
        path: d.path().join("a.csv"), raw, spec: None, stale: false,
    }), d.path());
    assert!(follow.is_none());
    assert!(w.busy.is_none());
    assert_eq!(w.scrollback.last().unwrap().echo, ".show a.csv");
    assert!(matches!(w.context, Context::File { ref path, .. } if path.ends_with("a.csv")));

    // A query result becomes the main pane's context.
    w.begin("SELECT 1;");
    let t = Table { columns: vec!["a".into()], types: vec![], rows: vec![vec!["1".into()]], total: 1, truncated: false };
    w.apply(outcome("SELECT 1;", "| a |\n", Payload::Query(t)), d.path());
    assert!(matches!(w.context, Context::Query(_)));

    // Edit comes back as a follow-up action.
    w.begin(".edit a.csv");
    let follow = w.apply(outcome(".edit a.csv", "", Payload::Edit(d.path().join("a.csv"))), d.path());
    assert!(matches!(follow, Some(WbAction::Edit(_))));
}

/// The audit-trail property that survives refactors: a browser shortcut and
/// the equivalent typed line must dispatch identically, even after a `.cd`
/// has moved the session's directory.
#[test]
fn shortcut_and_typed_line_produce_identical_dispatches_after_cd() {
    let d = pile();
    let mut w1 = wb(&d);
    let mut w2 = wb(&d);
    // w1: navigate with the browser, sniff via shortcut.
    w1.key(key(KeyCode::Tab));
    w1.key(key(KeyCode::Enter)); // .cd sub
    let a1 = w1.key(key(KeyCode::Char('s')));
    // w2: type the same session.
    let _ = type_line(&mut w2, ".cd sub");
    let a2 = type_line(&mut w2, ".sniff c.csv");
    assert_eq!(a1, a2);
}

/// The runtime now calls `begin` synchronously the moment a `Dispatch`
/// action is produced — before the line ever reaches the worker — so that
/// `key()`'s busy gate is up immediately, not a whole `Started` round trip
/// later. A key arriving right after that synchronous `begin` (long before
/// any worker message could possibly come back) must already be swallowed.
#[test]
fn begin_called_synchronously_after_dispatch_blocks_the_next_key() {
    let d = pile();
    let mut w = wb(&d);
    let action = type_line(&mut w, ".sniff a.csv");
    let WbAction::Dispatch(line) = action else { panic!("expected Dispatch, got {action:?}") };
    // What the runtime's `act_on_wb` now does before sending on `line_tx`.
    w.begin(&line);
    assert!(w.busy.is_some());
    assert_eq!(w.key(key(KeyCode::Char('x'))), WbAction::None);
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

    // PageUp now clamps to `scrollback_lines()` — give it real content to
    // scroll into first, or an empty scrollback would clamp it straight
    // back to 0.
    w.begin(".ls");
    w.apply(outcome(".ls", "a\nb\nc\nd\ne\nf\ng\nh", Payload::Nothing), d.path());
    assert!(w.scrollback_lines() >= 5);

    w.key(key(KeyCode::PageUp));
    assert_eq!(w.scroll, 5);
    type_line(&mut w, ".ls"); // any dispatch resets scroll
    assert_eq!(w.scroll, 0);
}

/// CRITICAL: a typed `.cd` moves the session but never the browser, so the
/// browser would keep listing the old directory while `s` on the
/// highlighted file synthesized a name the session resolves elsewhere — a
/// different file than the one shown (monthly trees repeat names, so this
/// is not hypothetical). Every `Done` carries the session's cwd and the
/// browser re-roots on it.
#[test]
fn a_done_carrying_a_different_cwd_re_roots_the_browser() {
    let d = pile();
    let mut w = wb(&d);
    assert_eq!(w.browser.title(), ".");

    // Typed — the browser is never asked to move.
    let act = type_line(&mut w, ".cd sub");
    assert_eq!(act, WbAction::Dispatch(".cd sub".into()));
    w.begin(".cd sub");
    w.apply(outcome(".cd sub", "sub\n", Payload::Nothing), &d.path().join("sub"));
    assert_eq!(w.browser.title(), "sub", "the browser follows the session");

    // And the shortcut now names the file the browser is actually showing.
    w.key(key(KeyCode::Tab)); // Browser
    assert_eq!(w.key(key(KeyCode::Char('s'))), WbAction::Dispatch(".sniff c.csv".into()));

    // The reverse direction: a browser descent whose `.cd` the session
    // refused (a symlink out of the root) comes back as a `Done` carrying
    // the unchanged cwd, and the browser rolls back.
    w.begin(".cd nowhere");
    w.apply(outcome(".cd nowhere", "outside the root\n", Payload::Nothing), d.path());
    assert_eq!(w.browser.title(), ".");
}

/// IMPORTANT: if the console worker is gone, the dispatch never runs — busy
/// must not stay set, or the UI is wedged with the explanation hidden
/// behind the busy text and only Ctrl-Q left.
#[test]
fn a_dead_worker_clears_busy_instead_of_wedging_the_ui() {
    let d = pile();
    let mut w = wb(&d);
    let action = type_line(&mut w, ".sniff a.csv");
    let WbAction::Dispatch(line) = action else { panic!("expected Dispatch, got {action:?}") };
    w.begin(&line);
    assert!(w.busy.is_some());
    // What the runtime does when `line_tx.send` reports a closed channel.
    w.worker_died("the console worker is gone — restart the workbench");
    assert!(w.busy.is_none());
    assert!(w.status.contains("worker is gone"), "{}", w.status);
    // And keys act again rather than being swallowed by the busy gate.
    assert_eq!(type_line(&mut w, ".ls"), WbAction::Dispatch(".ls".into()));
}

/// IMPORTANT: the main pane's scroll belongs to the file being shown.
/// Arrowing to the next file must open it at the top — a short file
/// rendered at the previous file's offset is a blank pane, which reads as
/// an empty file — while a same-path update (the `.sniff` follow-up filling
/// in the raw half) must keep the scroll the user set.
/// `?` from Browser (or Main) focus opens the help overlay instead of
/// acting as an ordinary key; whatever key comes next just closes it again
/// rather than being dispatched — the overlay swallows one keystroke.
#[test]
fn question_mark_opens_help_from_browser_and_any_key_closes_it() {
    let d = pile();
    let mut w = wb(&d);
    w.key(key(KeyCode::Tab)); // Browser
    assert_eq!(w.focus, Focus::Browser);
    assert!(!w.help);
    assert_eq!(w.key(key(KeyCode::Char('?'))), WbAction::None);
    assert!(w.help);
    assert_eq!(w.key(key(KeyCode::Char('x'))), WbAction::None);
    assert!(!w.help, "any key closes the overlay");
}

/// In Console focus `?` is just a character for the line editor — the
/// overlay would otherwise make it impossible to type a literal `?`.
#[test]
fn question_mark_in_console_is_just_a_character() {
    let d = pile();
    let mut w = wb(&d);
    assert_eq!(w.focus, Focus::Console);
    w.key(key(KeyCode::Char('?')));
    assert!(!w.help);
    assert!(w.editor.text().contains('?'), "{}", w.editor.text());
}

/// `.fit`'s report lands in the main pane as `Context::Pile`, targeted at
/// the file `begin()` remembered from the dispatched line.
#[test]
fn a_fit_report_becomes_the_pile_context() {
    let d = pile();
    let mut w = wb(&d);
    w.begin(".fit sales.tdy.sql");
    let report = pile_report("sales.tdy.sql", vec![member("2025-01.csv", MemberStatus::Fits)]);
    let follow = w.apply(outcome(".fit sales.tdy.sql", "", Payload::Fitted(report)), d.path());
    assert!(follow.is_none());
    match &w.context {
        Context::Pile { target, report, selected } => {
            assert!(target.ends_with("sales.tdy.sql"), "{target:?}");
            assert_eq!(report.members.len(), 1);
            assert_eq!(*selected, 0);
        }
        other => panic!("expected Pile, got {other:?}"),
    }
}

/// Up/Down move the selection (clamped), Enter opens the selected member
/// (returning a `PreviewFile` for it), and Esc from a Pile drops back to
/// Empty.
#[test]
fn pile_navigation_and_enter_opens_a_member() {
    let d = pile();
    let mut w = wb(&d);
    w.begin(".fit sales.tdy.sql");
    let report = pile_report(
        "sales.tdy.sql",
        vec![member("2025-01.csv", MemberStatus::Fits), gap_member("2025-02.csv")],
    );
    w.apply(outcome(".fit sales.tdy.sql", "", Payload::Fitted(report)), d.path());
    w.key(key(KeyCode::Tab)); // Browser
    w.key(key(KeyCode::Tab)); // Main
    assert_eq!(w.focus, Focus::Main);

    // Up from 0 clamps at 0.
    w.key(key(KeyCode::Up));
    assert!(matches!(&w.context, Context::Pile { selected: 0, .. }));
    // Down moves to 1; a further Down clamps at the last member.
    w.key(key(KeyCode::Down));
    assert!(matches!(&w.context, Context::Pile { selected: 1, .. }));
    w.key(key(KeyCode::Down));
    assert!(matches!(&w.context, Context::Pile { selected: 1, .. }));

    let act = w.key(key(KeyCode::Enter));
    assert!(matches!(&act, WbAction::PreviewFile(p) if p.ends_with("2025-02.csv")), "{act:?}");
    match &w.context {
        Context::Member { member, .. } => assert_eq!(*member, 1),
        other => panic!("expected Member, got {other:?}"),
    }

    // Esc from a Pile drops back to Empty (re-derive a fresh Pile context
    // rather than reuse the Member one above).
    w.begin(".fit sales.tdy.sql");
    let report2 =
        pile_report("sales.tdy.sql", vec![member("2025-01.csv", MemberStatus::Fits)]);
    w.apply(outcome(".fit sales.tdy.sql", "", Payload::Fitted(report2)), d.path());
    assert!(matches!(&w.context, Context::Pile { .. }));
    w.key(key(KeyCode::Esc));
    assert!(matches!(&w.context, Context::Empty), "{:?}", w.context);
}

/// A refit against the SAME target keeps the cursor on the member the user
/// was looking at — matched by `path`, not index, since a refit can insert
/// or remove members ahead of the one selected. When that member is gone
/// from the new report, the selection falls back to 0 rather than an
/// out-of-range or arbitrary index.
#[test]
fn selection_survives_a_refit_by_member_path() {
    let d = pile();
    let mut w = wb(&d);
    w.begin(".fit sales.tdy.sql");
    let report = pile_report(
        "sales.tdy.sql",
        vec![
            member("2025-01.csv", MemberStatus::Fits),
            member("2025-02.csv", MemberStatus::Fits),
            gap_member("2025-03.csv"),
        ],
    );
    w.apply(outcome(".fit sales.tdy.sql", "", Payload::Fitted(report)), d.path());
    let Context::Pile { selected, .. } = &mut w.context else { panic!("expected Pile") };
    *selected = 2;

    // Same members, same target, re-fitted: 2025-03.csv is still at index 2,
    // but the selection must be recovered by path, not merely survive
    // because the index happens to match.
    w.begin(".fit sales.tdy.sql");
    let report_same = pile_report(
        "sales.tdy.sql",
        vec![
            gap_member("2025-03.csv"),
            member("2025-01.csv", MemberStatus::Fits),
            member("2025-02.csv", MemberStatus::Fits),
        ],
    );
    w.apply(outcome(".fit sales.tdy.sql", "", Payload::Fitted(report_same)), d.path());
    match &w.context {
        Context::Pile { selected, report, .. } => {
            assert_eq!(*selected, 0, "2025-03.csv moved to index 0 in the new report");
            assert_eq!(report.members[*selected].path, "2025-03.csv");
        }
        other => panic!("expected Pile, got {other:?}"),
    }

    // A refit missing that member falls back to 0.
    w.begin(".fit sales.tdy.sql");
    let report_missing = pile_report(
        "sales.tdy.sql",
        vec![member("2025-01.csv", MemberStatus::Fits), member("2025-02.csv", MemberStatus::Fits)],
    );
    w.apply(outcome(".fit sales.tdy.sql", "", Payload::Fitted(report_missing)), d.path());
    assert!(matches!(&w.context, Context::Pile { selected: 0, .. }), "{:?}", w.context);
}

/// `f` re-dispatches a fit: from the Pile context (Main focus) it repeats
/// the target that produced it; from the Browser, only on a `*.tdy.sql`
/// entry — a data file keeps `s`.
#[test]
fn f_dispatches_a_refit_from_pile_and_from_a_browser_target() {
    let d = pile();
    std::fs::write(d.path().join("t.tdy.sql"), "CREATE TABLE t (a TEXT) WITH (files='*.csv');")
        .unwrap();
    let mut w = wb(&d);

    // From Pile + Main focus.
    w.begin(".fit sales.tdy.sql");
    let report = pile_report("sales.tdy.sql", vec![member("2025-01.csv", MemberStatus::Fits)]);
    w.apply(outcome(".fit sales.tdy.sql", "", Payload::Fitted(report)), d.path());
    w.key(key(KeyCode::Tab)); // Browser
    w.key(key(KeyCode::Tab)); // Main
    let act = w.key(key(KeyCode::Char('f')));
    let WbAction::Dispatch(line) = act else { panic!("expected Dispatch, got {act:?}") };
    assert!(line.contains(".fit"), "{line}");
    assert!(line.contains("sales.tdy.sql"), "{line}");
    // The refit must keep asking for proposals: they are what ranks the
    // remedy menu of every member the new report refuses.
    assert!(line.contains("--propose"), "{line}");
    // …and it is NOT a dry run — `f` is the key that writes for real.
    assert!(!line.contains("--dry-run"), "{line}");

    // From Browser, on a target entry.
    let mut w2 = wb(&d);
    w2.key(key(KeyCode::Tab)); // Browser; entries sorted dirs-first then files/targets
    while w2.browser.selected_entry().map(|e| e.name.as_str()) != Some("t.tdy.sql") {
        w2.key(key(KeyCode::Down));
    }
    assert_eq!(
        w2.key(key(KeyCode::Char('f'))),
        WbAction::Dispatch(".fit t.tdy.sql --propose".into())
    );

    // `f` on a data file does nothing; `s` still sniffs.
    w2.key(key(KeyCode::Up));
    while w2.browser.selected_entry().map(|e| e.kind) != Some(tdy::console::EntryKind::File) {
        w2.key(key(KeyCode::Down));
    }
    assert_eq!(w2.key(key(KeyCode::Char('f'))), WbAction::None);
}

#[test]
fn main_scroll_resets_on_a_new_file_and_survives_a_same_path_update() {
    let d = pile();
    let mut w = wb(&d);
    let raw = || RawHead { lines: vec!["A;B".into(), "1;2".into()], truncated: false, sheets: vec![], grid: vec![], grid_sheet: None };
    w.begin(".show a.csv");
    w.apply(outcome(".show a.csv", "", Payload::Shown { path: d.path().join("a.csv"), raw: raw(), spec: None, stale: false }), d.path());

    // Scroll the raw view down.
    w.key(key(KeyCode::Tab));
    w.key(key(KeyCode::Tab)); // Main
    w.key(key(KeyCode::Down));
    w.key(key(KeyCode::Down));
    assert_eq!(w.main_scroll, 2);

    // A different file: back to the top.
    w.begin(".show b.csv");
    w.apply(outcome(".show b.csv", "", Payload::Shown { path: d.path().join("b.csv"), raw: raw(), spec: None, stale: false }), d.path());
    assert_eq!(w.main_scroll, 0);

    // The same file again (the preview's raw fill-in): the scroll stands.
    w.key(key(KeyCode::Down));
    assert_eq!(w.main_scroll, 1);
    w.set_preview(w.preview_gen, d.path().join("b.csv"), raw(), None, false);
    assert_eq!(w.main_scroll, 1, "a same-path update must not throw away the user's scroll");
}

/// `main_scroll` is ONE offset shared by every context that scrolls — and
/// since slice 4 that is all of them (Pile, a Member's raw column,
/// Evidence, Query). So it must reset on every context CHANGE, not only in
/// `show_file`: a paged-down Pile followed by `Enter` used to open the
/// member's raw column already scrolled past a short raw head, drawing a
/// blank pane that reads as an empty file. Same leak Pile → Evidence, back
/// out again, and into a fresh query result.
#[test]
fn main_scroll_resets_on_every_context_change() {
    let d = pile();
    let mut w = wb(&d);
    w.begin(".fit sales.tdy.sql");
    let report = pile_report(
        "sales.tdy.sql",
        vec![
            member("2025-01.csv", MemberStatus::Fits),
            member("2025-02.csv", MemberStatus::NeedsReview),
        ],
    );
    w.apply(outcome(".fit sales.tdy.sql", "", Payload::Fitted(report)), d.path());
    w.key(key(KeyCode::Tab)); // Browser
    w.key(key(KeyCode::Tab)); // Main

    // Pile → Member.
    w.key(key(KeyCode::PageDown));
    w.key(key(KeyCode::PageDown));
    assert!(w.main_scroll > 0, "PageDown must scroll the Pile");
    w.key(key(KeyCode::Enter));
    assert!(matches!(w.context, Context::Member { .. }), "{:?}", w.context);
    assert_eq!(w.main_scroll, 0, "a member's raw column opens at its first line");

    // Member → Pile.
    w.key(key(KeyCode::PageDown));
    assert!(w.main_scroll > 0);
    w.key(key(KeyCode::Esc));
    assert!(matches!(w.context, Context::Pile { .. }), "{:?}", w.context);
    assert_eq!(w.main_scroll, 0, "back at a list of members, at its top");

    // Pile → Evidence.
    w.key(key(KeyCode::PageDown));
    assert!(w.main_scroll > 0);
    w.begin(".accept sales.tdy.sql 2025-02.csv");
    let rows = vec![tdy::evidence::Evidence::Unillustrated { reason: "x".into() }];
    w.apply(
        outcome(
            ".accept sales.tdy.sql 2025-02.csv",
            "",
            Payload::Evidence {
                target: d.path().join("sales.tdy.sql"),
                member: "2025-02.csv".into(),
                rows,
            },
        ),
        d.path(),
    );
    assert_eq!(w.main_scroll, 0, "evidence opens at its first line");

    // Evidence → Query.
    w.key(key(KeyCode::Down));
    w.key(key(KeyCode::Down));
    assert!(w.main_scroll > 0);
    w.begin("SELECT 1;");
    let t = Table {
        columns: vec!["a".into()],
        types: vec![],
        rows: vec![vec!["1".into()]],
        total: 1,
        truncated: false,
    };
    w.apply(outcome("SELECT 1;", "| a |\n", Payload::Query(t)), d.path());
    assert_eq!(w.main_scroll, 0, "a fresh result set starts at its first row");
}

/// `.accept` is two steps, and the second one's `Done` is a refit: the
/// selection-preservation match must therefore know `Evidence` as an
/// outgoing context too. Without that arm the member you just accepted is
/// the one member the new Pile does not have selected — the cursor jumps to
/// row 0 exactly when you want to see what your judgement did.
#[test]
fn the_accepted_member_stays_selected_after_step_two() {
    let d = pile();
    let members = || {
        vec![
            member("2025-01.csv", MemberStatus::Fits),
            member("2025-02.csv", MemberStatus::Fits),
            member("2025-03.csv", MemberStatus::NeedsReview),
        ]
    };
    // Pile → Enter on the reviewable member → `a` (step one).
    let (mut w, _) = pile_and_enter(&d, members(), 2);
    let act = w.key(key(KeyCode::Char('a')));
    let WbAction::Dispatch(line) = act else { panic!("expected Dispatch, got {act:?}") };
    w.begin(&line);
    let rows = vec![tdy::evidence::Evidence::Unillustrated { reason: "x".into() }];
    w.apply(
        outcome(
            &line,
            "",
            Payload::Evidence {
                target: d.path().join("sales.tdy.sql"),
                member: "2025-03.csv".into(),
                rows,
            },
        ),
        d.path(),
    );

    // Step two: the same line again, whose Done is the refit.
    let act = w.key(key(KeyCode::Char('a')));
    assert_eq!(act, WbAction::Dispatch(line.clone()));
    w.begin(&line);
    let mut refitted = members();
    refitted[2].accepted = true;
    w.apply(outcome(&line, "", Payload::Fitted(pile_report("sales.tdy.sql", refitted))), d.path());

    match &w.context {
        Context::Pile { selected, report, .. } => {
            assert_eq!(*selected, 2, "the accepted member must still be selected");
            assert_eq!(report.members[*selected].path, "2025-03.csv");
            assert!(report.members[*selected].accepted);
        }
        other => panic!("expected Pile, got {other:?}"),
    }
}

/// `member_remedies()` reads the current Member's problems through
/// `remedy::remedies_for`: a gap member (declared `region`, the file's own
/// header is `Datum`/`Kanton`) offers at least the two headers as
/// candidates, first-label-first; a member that fits has nothing to offer.
#[test]
fn member_remedies_come_from_the_problems() {
    let d = pile();
    let (w, act) = pile_and_enter(
        &d,
        vec![member("2025-01.csv", MemberStatus::Fits), gap_member("2025-02.csv")],
        1,
    );
    assert!(matches!(act, WbAction::PreviewFile(_)), "{act:?}");
    let remedies = w.member_remedies();
    assert!(!remedies.is_empty(), "a gap member must offer remedies");
    let first = remedies[0].label();
    assert!(
        first.contains("region") || first.contains("Datum") || first.contains("Kanton"),
        "{first}"
    );

    // A fits-member (Enter on index 0) offers nothing.
    let (w2, _) = pile_and_enter(&d, vec![member("2025-01.csv", MemberStatus::Fits)], 0);
    assert!(w2.member_remedies().is_empty());
}

/// A review-only member (`review: Some(_)`, no `problems`, no `proposals` —
/// `member(.., MemberStatus::NeedsReview)` builds exactly this) has nothing
/// for either loop in `member_remedies` to offer, but it plainly needs a
/// remedy: the classic floor kicks in and the menu is exactly the one
/// exclude-this-file entry, never empty.
#[test]
fn a_review_only_member_floors_to_exclude_file() {
    let d = pile();
    let (w, act) = pile_and_enter(&d, vec![member("2025-05.csv", MemberStatus::NeedsReview)], 0);
    assert!(matches!(act, WbAction::PreviewFile(_)), "{act:?}");
    let remedies = w.member_remedies();
    assert_eq!(
        remedies,
        vec![Remedy::ExcludeFile { rel: "2025-05.csv".into() }],
        "{remedies:?}"
    );
}

/// The menu is **ranked by `--propose`**, not listed in file order (design
/// §7, and CLAUDE.md's "ranked by `--propose` … rather than listed in file
/// order"). The member's `Problem` names the file's whole header —
/// `Datum` first, `Kanton` second — but `--propose` found that only
/// `Kanton` can actually produce a TEXT `region`; a date cannot. So the
/// first entry must be the `Kanton` match, and `Datum` must come after it.
///
/// This is the assertion the deleted `tests/render.rs` carried as "the
/// type-compatible candidate must be [1]" (`[1]` being the menu's
/// one-based label for `remedies()[0]`); it moves here now that ranking
/// lives in `Workbench::member_remedies`.
#[test]
fn the_remedy_menu_is_ranked_by_propose_not_by_file_order() {
    let d = pile();
    let mut m = gap_member("2025-11.csv");
    // What `--propose` found: `Kanton` can produce TEXT, `Datum` cannot.
    m.proposals = vec![ProposalReport {
        column: "region".into(),
        want: "TEXT".into(),
        candidates: vec![("Kanton".into(), "all 4 sampled value(s) parse as TEXT".into())],
        message: "region TEXT OPTIONS(matches = 'Kanton')".into(),
    }];
    let (w, _) = pile_and_enter(&d, vec![m], 0);
    let remedies = w.member_remedies();
    assert!(
        matches!(&remedies[0], Remedy::AddMatch { column, spelling }
                 if column == "region" && spelling == "Kanton"),
        "the type-compatible candidate must be [1], got {remedies:?}"
    );
    // The header's other column is still reachable — ranking demotes it,
    // it does not hide it; a proposal is a ranking, not a proof.
    assert!(
        remedies.iter().any(|r| matches!(r, Remedy::AddMatch { spelling, .. } if spelling == "Datum")),
        "the file's other column must still be offered, lower down: {remedies:?}"
    );
    // …and it is offered exactly once, though both the proposal and the
    // problem's header name `Kanton`.
    let kantons = remedies
        .iter()
        .filter(|r| matches!(r, Remedy::AddMatch { spelling, .. } if spelling == "Kanton"))
        .count();
    assert_eq!(kantons, 1, "the proposal and the problem must dedupe: {remedies:?}");
}

/// Up/Down move `remedy_selected`, clamped to the remedy count; Esc returns
/// to `Context::Pile` with `selected` pointing at the member just examined —
/// the very report, moved rather than cloned.
#[test]
fn member_navigation_and_escape_preserve_the_pile() {
    let d = pile();
    let (mut w, _) = pile_and_enter(
        &d,
        vec![member("2025-01.csv", MemberStatus::Fits), gap_member("2025-02.csv")],
        1,
    );
    let n = w.member_remedies().len();
    assert!(n > 0);

    // Down repeatedly clamps at n - 1.
    for _ in 0..(n + 3) {
        w.key(key(KeyCode::Down));
    }
    assert!(matches!(&w.context, Context::Member { remedy_selected, .. } if *remedy_selected == n - 1));
    // Up repeatedly clamps at 0.
    for _ in 0..(n + 3) {
        w.key(key(KeyCode::Up));
    }
    assert!(matches!(&w.context, Context::Member { remedy_selected: 0, .. }));

    w.key(key(KeyCode::Esc));
    match &w.context {
        Context::Pile { selected, report, .. } => {
            assert_eq!(*selected, 1);
            assert_eq!(report.members.len(), 2, "the same report, not a fresh one");
        }
        other => panic!("expected Pile, got {other:?}"),
    }
}

/// The `PreviewFile` path Enter asked for is exactly what `set_preview` must
/// match to fill `Context::Member.raw`; any other path is dropped silently.
#[test]
fn a_member_preview_fills_raw_and_stale_paths_are_dropped() {
    let d = pile();
    let (mut w, act) =
        pile_and_enter(&d, vec![member("2025-01.csv", MemberStatus::Fits), gap_member("2025-02.csv")], 1);
    let WbAction::PreviewFile(path) = act else { panic!("expected PreviewFile, got {act:?}") };

    let raw = RawHead { lines: vec!["Datum;Kanton".into()], truncated: false, sheets: vec![], grid: vec![], grid_sheet: None };
    // A different, unrelated path must be dropped.
    w.set_preview(w.preview_gen, d.path().join("a.csv"), raw.clone(), None, false);
    assert!(matches!(&w.context, Context::Member { raw: None, .. }), "stale path must not fill raw");

    // The actual path Enter asked for fills it.
    w.set_preview(w.preview_gen, path, raw, None, false);
    assert!(matches!(&w.context, Context::Member { raw: Some(_), .. }));
}

/// `preview_failed` follows the same gen+path staleness rules as
/// `set_preview` (stale generation dropped before the path is even
/// checked), but on a match it fills `Context::Member.raw` with a message
/// explaining why, so the pane stops reading "loading…" forever.
#[test]
fn preview_failed_fills_member_raw_and_drops_a_stale_generation() {
    let d = pile();
    let (mut w, act) =
        pile_and_enter(&d, vec![member("2025-01.csv", MemberStatus::Fits), gap_member("2025-02.csv")], 1);
    let WbAction::PreviewFile(path) = act else { panic!("expected PreviewFile, got {act:?}") };
    let gen = w.preview_gen;

    // A stale generation is dropped before the path is even checked.
    w.preview_failed(gen - 1, path.clone(), "permission denied".into());
    assert!(matches!(&w.context, Context::Member { raw: None, .. }), "stale gen must not fill raw");

    w.preview_failed(gen, path, "permission denied".into());
    match &w.context {
        Context::Member { raw: Some(r), .. } => {
            assert!(
                r.lines.iter().any(|l| l.contains("permission denied")),
                "{:?}",
                r.lines
            );
        }
        other => panic!("expected Member with raw filled, got {other:?}"),
    }
}

/// `e` in Member context dispatches `.edit <member rel>`.
#[test]
fn e_dispatches_edit_for_the_member() {
    let d = pile();
    let (mut w, _) =
        pile_and_enter(&d, vec![member("2025-01.csv", MemberStatus::Fits), gap_member("2025-02.csv")], 1);
    let act = w.key(key(KeyCode::Char('e')));
    let WbAction::Dispatch(line) = act else { panic!("expected Dispatch, got {act:?}") };
    assert!(line.starts_with(".edit"), "{line}");
    assert!(line.contains("2025-02.csv"), "{line}");
}

/// `t` in Pile or Member context (Main focus) opens the target itself in
/// $EDITOR — the same `.edit <rel>` shape `f`/`e` already dispatch, but
/// naming the declaration rather than a member. Reachable from either
/// context, since both carry the target path.
#[test]
fn t_dispatches_edit_for_the_target_from_pile_and_member() {
    let d = pile();
    let (mut w, _) = pile_and_enter(&d, vec![member("2025-01.csv", MemberStatus::Fits)], 0);
    assert!(matches!(&w.context, Context::Member { .. }), "{:?}", w.context);
    assert_eq!(
        w.key(key(KeyCode::Char('t'))),
        WbAction::Dispatch(".edit sales.tdy.sql".into())
    );

    w.key(key(KeyCode::Esc)); // back to Pile
    assert!(matches!(&w.context, Context::Pile { .. }), "{:?}", w.context);
    assert_eq!(
        w.key(key(KeyCode::Char('t'))),
        WbAction::Dispatch(".edit sales.tdy.sql".into())
    );
}

/// The target text a Member's remedy menu edits — written for real, so the
/// staged edit can be re-parsed as a target and the diff read back.
fn target_sql() -> &'static str {
    "CREATE TABLE t (\n  region TEXT NOT NULL OPTIONS(matches = 'Region')\n) WITH (files='*.csv');\n"
}

/// From a Pile targeted at `t.tdy.sql` (rather than `pile_and_enter`'s
/// hard-coded `sales.tdy.sql`), enter the given member — the remedy tests
/// below need the dispatched `.fit` and the real target file on disk to
/// name the same path.
fn t_pile_and_enter(d: &tempfile::TempDir, members: Vec<MemberReport>, selected: usize) -> (Workbench, WbAction) {
    let mut w = wb(d);
    w.begin(".fit t.tdy.sql");
    let report = pile_report("t.tdy.sql", members);
    w.apply(outcome(".fit t.tdy.sql", "", Payload::Fitted(report)), d.path());
    if let Context::Pile { selected: sel, .. } = &mut w.context {
        *sel = selected;
    }
    w.key(key(KeyCode::Tab)); // Browser
    w.key(key(KeyCode::Tab)); // Main
    let act = w.key(key(KeyCode::Enter));
    (w, act)
}

/// Digit `1` over a gap member's remedy menu stages an edit — `pending_edit`
/// is set, its diff carries a `+` line — and `y` turns it into a
/// `WriteTarget` action whose `new_text` still parses as a target, whose
/// `expected` is the text `set_target_sql` was given, and whose `refit`
/// re-dispatches `.fit` at the same target.
#[test]
fn a_digit_stages_an_edit_and_y_writes_then_refits() {
    let d = pile();
    let target_path = d.path().join("t.tdy.sql");
    std::fs::write(&target_path, target_sql()).unwrap();
    let (mut w, act) = t_pile_and_enter(&d, vec![gap_member("2025-02.csv")], 0);
    assert!(matches!(act, WbAction::PreviewFile(_)), "{act:?}");
    w.set_target_sql(target_sql().to_string());

    let act = w.key(key(KeyCode::Char('1')));
    assert_eq!(act, WbAction::None);
    let (_, edit, staged_target, expected) =
        w.pending_edit.as_ref().expect("digit 1 should stage an edit");
    assert!(staged_target.ends_with("t.tdy.sql"), "{staged_target:?}");
    assert_eq!(expected, target_sql());
    assert!(edit.diff().lines().any(|l| l.contains('+')), "{}", edit.diff());

    let act = w.key(key(KeyCode::Char('y')));
    assert!(w.pending_edit.is_none(), "y clears the pending edit");
    let WbAction::WriteTarget { path, expected, new_text, refit } = act else {
        panic!("expected WriteTarget, got {act:?}")
    };
    assert!(path.ends_with("t.tdy.sql"), "{path:?}");
    assert_eq!(expected, target_sql());
    assert!(tdy::target::Target::parse(&new_text).is_ok(), "{new_text}");
    assert!(refit.starts_with(".fit "), "{refit}");
    assert!(refit.contains("t.tdy.sql"), "{refit}");
    // The post-write refit must keep asking for proposals — they rank the
    // next member's remedy menu (the fourth dispatch site the final review's
    // re-review caught missing).
    assert!(refit.contains("--propose"), "{refit}");
}

/// `Enter` stages whichever remedy `▸` currently marks — the same effect as
/// pressing the digit for `remedy_selected + 1`, just reading the index off
/// the cursor instead of the key. Move down to the second remedy, then
/// press Enter: `pending_edit` must be staged from `member_remedies()[1]`,
/// not `[0]`.
#[test]
fn enter_stages_the_selected_remedy() {
    let d = pile();
    std::fs::write(d.path().join("t.tdy.sql"), target_sql()).unwrap();
    let (mut w, act) = t_pile_and_enter(&d, vec![gap_member("2025-02.csv")], 0);
    assert!(matches!(act, WbAction::PreviewFile(_)), "{act:?}");
    w.set_target_sql(target_sql().to_string());

    let remedies = w.member_remedies();
    assert!(remedies.len() >= 2, "{remedies:?}");
    let second = remedies[1].clone();

    w.key(key(KeyCode::Down)); // remedy_selected: 0 -> 1
    assert!(matches!(&w.context, Context::Member { remedy_selected: 1, .. }), "{:?}", w.context);

    let act = w.key(key(KeyCode::Enter));
    assert_eq!(act, WbAction::None);
    let (staged_remedy, ..) = w.pending_edit.as_ref().expect("Enter should stage an edit");
    assert_eq!(staged_remedy, &second, "Enter must stage the remedy the ▸ marker points at");
}

/// After `$EDITOR` returns, the runtime re-reads the target and hands the
/// new text to `set_target_sql` (`main::after_editing`) — so the *next*
/// remedy digit stages against what the human just wrote, not against the
/// text from before the edit. This is the half of spec §8's `.edit` return
/// path that is testable here: `set_target_sql` is a `Workbench` method
/// with an observable consequence, while the accompanying "target edited;
/// lock is stale — `.fit` to re-prove" note is emitted by the runtime after
/// the editor process exits, which no test in this crate can reach without
/// a terminal and an `$EDITOR`.
///
/// Concretely: the pre-edit declaration matches on `Region`, the post-edit
/// one on `Bezirk`. Staging after the re-read must produce an edit whose
/// `expected` is the *new* text — otherwise `write_target`'s guard would
/// refuse the very next write, and the remedy menu would be permanently
/// dead after any use of `t`.
#[test]
fn re_reading_the_target_after_an_edit_restages_against_the_new_text() {
    let d = pile();
    std::fs::write(d.path().join("t.tdy.sql"), target_sql()).unwrap();
    let (mut w, _) = t_pile_and_enter(&d, vec![gap_member("2025-02.csv")], 0);
    w.set_target_sql(target_sql().to_string());

    // …the human presses `t`, edits, and the runtime re-reads.
    let edited =
        target_sql().replace("matches = 'Region'", "matches = 'Bezirk'");
    assert_ne!(edited, target_sql());
    std::fs::write(d.path().join("t.tdy.sql"), &edited).unwrap();
    w.set_target_sql(edited.clone());

    let act = w.key(key(KeyCode::Char('1')));
    assert_eq!(act, WbAction::None);
    let (_, _, _, expected) = w.pending_edit.as_ref().expect("digit 1 should stage an edit");
    assert_eq!(
        expected, &edited,
        "the staged edit must be against the post-edit text, or the guard refuses the write"
    );

    // And `y`'s WriteTarget carries that same `expected`, which is what
    // `write_target` compares the file against.
    let WbAction::WriteTarget { expected, new_text, .. } = w.key(key(KeyCode::Char('y'))) else {
        panic!("expected WriteTarget")
    };
    assert_eq!(expected, edited);
    assert_eq!(expected, std::fs::read_to_string(d.path().join("t.tdy.sql")).unwrap());
    assert!(tdy::target::Target::parse(&new_text).is_ok(), "{new_text}");
}

/// The confirm overlay is modal: once an edit is staged, arbitrary keys
/// (typing, Tab) are swallowed and change nothing; Esc cancels and clears
/// `pending_edit`. Pressing a digit with no `target_sql` loaded leaves
/// `pending_edit` unset and sets a status note instead of panicking.
#[test]
fn the_overlay_is_modal_and_esc_cancels() {
    let d = pile();
    std::fs::write(d.path().join("t.tdy.sql"), target_sql()).unwrap();
    let (mut w, _) = t_pile_and_enter(&d, vec![gap_member("2025-02.csv")], 0);

    // No target text loaded yet: the digit does nothing but note why.
    let act = w.key(key(KeyCode::Char('1')));
    assert_eq!(act, WbAction::None);
    assert!(w.pending_edit.is_none());
    assert!(!w.status.is_empty(), "a status note should explain the no-op");

    w.set_target_sql(target_sql().to_string());
    let act = w.key(key(KeyCode::Char('1')));
    assert_eq!(act, WbAction::None);
    assert!(w.pending_edit.is_some());

    let before_focus = w.focus;
    assert_eq!(w.key(key(KeyCode::Char('s'))), WbAction::None);
    assert_eq!(w.key(key(KeyCode::Tab)), WbAction::None);
    assert_eq!(w.focus, before_focus, "Tab must not move focus while modal");
    assert!(w.pending_edit.is_some(), "an unrelated key must not clear the pending edit");

    assert_eq!(w.key(key(KeyCode::Esc)), WbAction::None);
    assert!(w.pending_edit.is_none(), "Esc cancels");
    assert!(w.status.contains("cancelled"), "{}", w.status);
}

/// `a` over a Member with a live judgement (`review: Some(_)`, not yet
/// accepted) dispatches `.accept TARGET MEMBER`, naming both the target and
/// the member; `a` over a member with nothing to review (fits, no review
/// pending) is swallowed with a status note instead.
#[test]
fn a_dispatches_accept_only_for_reviewable_members() {
    let d = pile();
    let (mut w, act) =
        pile_and_enter(&d, vec![member("2025-01.csv", MemberStatus::NeedsReview)], 0);
    assert!(matches!(act, WbAction::PreviewFile(_)), "{act:?}");

    let act = w.key(key(KeyCode::Char('a')));
    let WbAction::Dispatch(line) = act else { panic!("expected Dispatch, got {act:?}") };
    assert!(line.starts_with(".accept "), "{line}");
    assert!(line.contains("sales.tdy.sql"), "{line}");
    assert!(line.contains("2025-01.csv"), "{line}");

    let (mut w2, _) = pile_and_enter(&d, vec![member("2025-01.csv", MemberStatus::Fits)], 0);
    let act2 = w2.key(key(KeyCode::Char('a')));
    assert_eq!(act2, WbAction::None);
    assert!(w2.status.contains("nothing to accept"), "{}", w2.status);
}

/// `Payload::Evidence` lands as `Context::Evidence`, carrying the exact
/// `.accept` line that produced it; `a` there re-dispatches that identical
/// line (the session treats the repeat as step two); `Esc` backs out to
/// `Context::Empty` with a note pointing at `f` to bring the pile back.
#[test]
fn evidence_arrives_and_a_repeats_the_exact_line() {
    let d = pile();
    let mut w = wb(&d);
    w.begin(".accept t.tdy.sql m.csv");
    let rows = vec![tdy::evidence::Evidence::Unillustrated { reason: "x".into() }];
    w.apply(
        outcome(
            ".accept t.tdy.sql m.csv",
            "evidence for m.csv (nothing written):\n",
            Payload::Evidence { target: d.path().join("t.tdy.sql"), member: "m.csv".into(), rows },
        ),
        d.path(),
    );
    match &w.context {
        Context::Evidence { line, member, .. } => {
            assert_eq!(line, ".accept t.tdy.sql m.csv");
            assert_eq!(member, "m.csv");
        }
        other => panic!("expected Evidence context, got {other:?}"),
    }

    w.key(key(KeyCode::Tab)); // Browser
    w.key(key(KeyCode::Tab)); // Main
    let act = w.key(key(KeyCode::Char('a')));
    assert_eq!(act, WbAction::Dispatch(".accept t.tdy.sql m.csv".to_string()));

    let act = w.key(key(KeyCode::Esc));
    assert_eq!(act, WbAction::None);
    assert!(matches!(w.context, Context::Empty), "{:?}", w.context);
    assert!(w.status.contains("evidence closed"), "{}", w.status);
}

/// A `.cd` between step one landing and `a` being pressed is a deliberate
/// degradation — see `Context::Evidence`'s doc comment. `apply`'s
/// `sync_dir` call (triggered by the `.cd` Done's different cwd) must not
/// clear or otherwise disturb the Evidence context, and `a` afterward must
/// still dispatch the exact stored line without panicking; whether the
/// session then treats that as step two or degrades to a fresh step one is
/// tested at the session's own layer, not here.
#[test]
fn evidence_survives_a_cd_between_steps_and_still_redispatches_the_line() {
    let d = pile();
    let mut w = wb(&d);
    w.begin(".accept t.tdy.sql m.csv");
    let rows = vec![tdy::evidence::Evidence::Unillustrated { reason: "x".into() }];
    w.apply(
        outcome(
            ".accept t.tdy.sql m.csv",
            "evidence for m.csv (nothing written):\n",
            Payload::Evidence { target: d.path().join("t.tdy.sql"), member: "m.csv".into(), rows },
        ),
        d.path(),
    );
    assert!(matches!(w.context, Context::Evidence { .. }), "{:?}", w.context);

    // A `.cd sub` Done: a real cwd move, `Payload::Nothing` (the same
    // payload `Command::Cd` actually returns), landing while Evidence is
    // still on screen.
    w.apply(outcome(".cd sub", "sub\n", Payload::Nothing), &d.path().join("sub"));

    match &w.context {
        Context::Evidence { line, member, .. } => {
            assert_eq!(line, ".accept t.tdy.sql m.csv");
            assert_eq!(member, "m.csv");
        }
        other => panic!("a `.cd` Done must not clear or replace the Evidence context, got {other:?}"),
    }

    w.key(key(KeyCode::Tab)); // Browser
    w.key(key(KeyCode::Tab)); // Main
    let act = w.key(key(KeyCode::Char('a')));
    assert_eq!(act, WbAction::Dispatch(".accept t.tdy.sql m.csv".to_string()));
}

/// `d` marks/unmarks the selected DATA file only (a directory is a no-op);
/// `D` dispatches `.draft` over every mark, space-joined, and clears them.
#[test]
fn d_marks_files_and_upper_d_dispatches_draft_then_clears_marks() {
    let d = pile();
    let mut w = wb(&d);
    w.key(key(KeyCode::Tab)); // Browser; entries are ["sub/", "a.csv", "b.csv"]

    // `d` on the directory currently selected does nothing.
    w.key(key(KeyCode::Char('d')));
    assert!(w.marked.is_empty());

    w.key(key(KeyCode::Down));
    assert_eq!(w.browser.selected_rel().as_deref(), Some("a.csv"));
    w.key(key(KeyCode::Char('d')));
    assert_eq!(w.marked, vec!["a.csv".to_string()]);

    w.key(key(KeyCode::Down));
    assert_eq!(w.browser.selected_rel().as_deref(), Some("b.csv"));
    w.key(key(KeyCode::Char('d')));
    assert_eq!(w.marked, vec!["a.csv".to_string(), "b.csv".to_string()]);

    // Marking a file already marked unmarks it.
    w.key(key(KeyCode::Up));
    w.key(key(KeyCode::Char('d')));
    assert_eq!(w.marked, vec!["b.csv".to_string()]);
    w.key(key(KeyCode::Char('d')));
    assert_eq!(w.marked, vec!["b.csv".to_string(), "a.csv".to_string()]);

    let act = w.key(key(KeyCode::Char('D')));
    assert_eq!(act, WbAction::Dispatch(".draft b.csv a.csv".into()));
    assert!(w.marked.is_empty(), "D clears the marks");
}

#[test]
fn upper_d_with_no_marks_is_a_status_note_not_a_dispatch() {
    let d = pile();
    let mut w = wb(&d);
    w.key(key(KeyCode::Tab)); // Browser
    let act = w.key(key(KeyCode::Char('D')));
    assert_eq!(act, WbAction::None);
    assert!(w.status.contains("no files marked"), "{}", w.status);
}

/// A rel path only means something inside the directory it was marked in —
/// both ways a directory move happens (the browser's own `Enter`, and a
/// typed `.cd` re-rooting the browser via `apply`'s `sync_dir`) must clear
/// stale marks.
#[test]
fn marks_clear_on_any_directory_move() {
    let d = pile();
    let mut w = wb(&d);
    w.key(key(KeyCode::Tab)); // Browser
    w.key(key(KeyCode::Down)); // a.csv
    w.key(key(KeyCode::Char('d')));
    assert_eq!(w.marked.len(), 1);
    w.key(key(KeyCode::Up)); // back to sub/
    w.key(key(KeyCode::Enter)); // descend into sub/ — dispatches .cd sub
    assert!(w.marked.is_empty(), "entering a directory must clear stale marks");

    w.key(key(KeyCode::Char('d'))); // mark c.csv, the only file in sub/
    assert_eq!(w.marked.len(), 1);
    w.begin(".cd ..");
    w.apply(outcome(".cd ..", ".\n", Payload::Nothing), d.path());
    assert!(w.marked.is_empty(), "a typed `.cd` re-rooting via sync_dir must clear marks too");
}

/// `zoom` removes Main from the Tab cycle entirely (Browser hands focus
/// straight back to Console); turning zoom on while Main already has focus
/// moves focus to Console, the same place it would end up cycling to.
#[test]
fn zoom_skips_main_in_the_tab_cycle() {
    let d = pile();
    let mut w = wb(&d);
    w.key(ctrl('l')); // zoom on, from the default Console focus
    assert!(w.zoom);
    assert_eq!(w.focus, Focus::Console);
    w.key(key(KeyCode::Tab)); // -> Browser
    assert_eq!(w.focus, Focus::Browser);
    w.key(key(KeyCode::Tab)); // zoomed: Main is skipped -> Console
    assert_eq!(w.focus, Focus::Console);

    // Focus Main first (zoom off), then turn zoom on: focus must move.
    w.key(ctrl('l')); // zoom off
    assert!(!w.zoom);
    w.key(key(KeyCode::Tab)); // Browser
    w.key(key(KeyCode::Tab)); // Main
    assert_eq!(w.focus, Focus::Main);
    w.key(ctrl('l')); // zoom on while Main is focused
    assert!(w.zoom);
    assert_eq!(w.focus, Focus::Console, "zoom must move Main's focus to Console");
}

/// A SQL statement buffered (`Payload::Continue`, shown by the `   -> `
/// continuation prompt) and Ctrl-C on the empty editor dispatches `.abort`
/// — the console-only command that discards it — rather than the plain
/// "Ctrl-Q quits" hint a Ctrl-C with nothing pending still gives.
#[test]
fn ctrl_c_aborts_pending_sql_from_the_console() {
    let d = pile();
    let mut w = wb(&d);
    let act = type_line(&mut w, "SELECT 1");
    assert_eq!(act, WbAction::Dispatch("SELECT 1".into()));
    w.begin("SELECT 1");
    w.apply(outcome("SELECT 1", "", Payload::Continue), d.path());
    assert_eq!(w.prompt(), "   -> ", "the buffered statement changes the prompt");

    let act = w.key(ctrl('c'));
    assert_eq!(act, WbAction::Dispatch(".abort".into()));
}

#[test]
fn ctrl_c_with_nothing_pending_only_hints_at_ctrl_q() {
    let d = pile();
    let mut w = wb(&d);
    assert_eq!(w.prompt(), "tdy> ");
    let act = w.key(ctrl('c'));
    assert_eq!(act, WbAction::None);
    assert!(w.status.contains("Ctrl-Q quits"), "{}", w.status);
}

/// `Down` in a File context cannot scroll past a generous bound computed
/// from what that context actually has to show.
#[test]
fn main_scroll_is_clamped_to_a_generous_bound() {
    let d = pile();
    let mut w = wb(&d);
    let raw = RawHead { lines: vec!["a".into(), "b".into(), "c".into()], truncated: false, sheets: vec![], grid: vec![], grid_sheet: None };
    w.begin(".show a.csv");
    w.apply(
        outcome(".show a.csv", "", Payload::Shown { path: d.path().join("a.csv"), raw, spec: None, stale: false }),
        d.path(),
    );
    w.key(key(KeyCode::Tab)); // Browser
    w.key(key(KeyCode::Tab)); // Main
    for _ in 0..500 {
        w.key(key(KeyCode::Down));
    }
    // 3 raw lines, no sheets, no preview: 3 + 0 + 0 + 16.
    assert!(w.main_scroll <= 3 + 0 + 16, "{}", w.main_scroll);
}

/// `set_preview` drops a result tagged with an older generation before it
/// ever checks the path — the fix for the stale-overwrite race the slice 2
/// review flagged.
#[test]
fn set_preview_drops_a_stale_generation_before_the_path_check() {
    let d = pile();
    let mut w = wb(&d);
    w.begin(".show a.csv");
    w.apply(
        outcome(
            ".show a.csv",
            "",
            Payload::Shown { path: d.path().join("a.csv"), raw: RawHead::default(), spec: None, stale: false },
        ),
        d.path(),
    );
    assert_eq!(w.preview_gen, 0);

    // Two arrow-key previews bump the counter to 2.
    w.key(key(KeyCode::Tab)); // Browser
    w.key(key(KeyCode::Down)); // preview a.csv -> gen 1
    w.key(key(KeyCode::Down)); // preview b.csv -> gen 2
    assert_eq!(w.preview_gen, 2);

    // A result for generation 1, even naming the path the context still
    // shows, must be dropped — a newer request has since been made.
    let raw = RawHead { lines: vec!["stale".into()], truncated: false, sheets: vec![], grid: vec![], grid_sheet: None };
    w.set_preview(1, d.path().join("a.csv"), raw, None, false);
    assert!(
        matches!(&w.context, Context::File { raw, .. } if raw.lines.is_empty()),
        "a stale generation must not overwrite the context: {:?}",
        w.context
    );

    // The current generation still applies.
    let raw = RawHead { lines: vec!["fresh".into()], truncated: false, sheets: vec![], grid: vec![], grid_sheet: None };
    w.set_preview(2, d.path().join("a.csv"), raw, None, false);
    assert!(matches!(&w.context, Context::File { raw, .. } if raw.lines == ["fresh".to_string()]));
}

/// The console's `PageUp` used to be unbounded — holding it could scroll
/// past every real line into blank space with no way back except `Home`,
/// which does not exist. It must now clamp to `scrollback_lines()`, the
/// flattened line count `draw_console` actually has to show, no matter how
/// many times it is pressed.
#[test]
fn console_page_up_clamps_to_the_scrollback_length() {
    let d = pile();
    let mut w = wb(&d);
    for i in 0..5 {
        w.begin(&format!(".show a.csv {i}"));
        w.apply(outcome(&format!(".show a.csv {i}"), "line one\nline two\nline three", Payload::Nothing), d.path());
    }
    assert!(w.scrollback_lines() > 0);

    for _ in 0..100 {
        w.key(key(KeyCode::PageUp));
    }
    assert!(
        w.scroll <= w.scrollback_lines(),
        "scroll {} must not exceed scrollback_lines() {}",
        w.scroll,
        w.scrollback_lines()
    );
}

/// PgUp/PgDn scroll `main_scroll` in a Pile context too, without disturbing
/// `selected` — Up/Down still mean member selection there (see `key_main`'s
/// Pile arm), so PgDn is the only way to move the pane's own scroll.
#[test]
fn pile_page_down_scrolls_without_moving_the_selection() {
    let d = pile();
    let mut w = wb(&d);
    w.begin(".fit sales.tdy.sql");
    let report = pile_report(
        "sales.tdy.sql",
        (0..30).map(|i| member(&format!("2025-{i:02}.csv"), MemberStatus::Fits)).collect(),
    );
    w.apply(outcome(".fit sales.tdy.sql", "", Payload::Fitted(report)), d.path());
    let Context::Pile { selected, .. } = &mut w.context else { panic!("expected Pile") };
    *selected = 3;
    w.key(key(KeyCode::Tab)); // Browser
    w.key(key(KeyCode::Tab)); // Main
    assert_eq!(w.main_scroll, 0);

    w.key(key(KeyCode::PageDown));
    let after_down = w.main_scroll;
    assert!(after_down > 0, "PageDown must advance main_scroll in a Pile context");
    w.key(key(KeyCode::PageUp));
    assert!(w.main_scroll < after_down, "PageUp must move main_scroll back up");

    assert!(matches!(&w.context, Context::Pile { selected: 3, .. }), "{:?}", w.context);
}

/// `record_target` now goes through the console's own tokenizer, so a
/// quoted target with a space in it resolves to the whole quoted string,
/// not just the text before the first space.
#[test]
fn record_target_resolves_a_quoted_target_with_a_space() {
    let d = pile();
    let mut w = wb(&d);
    w.begin(".fit \"my target.tdy.sql\" --dry-run");
    let last = w.last_target.expect("last_target should be set");
    assert_eq!(last.file_name().unwrap(), "my target.tdy.sql");
}

/// A flag typed *before* the positional target must not be mistaken for
/// it — `record_target` skips tokens starting with `--` when looking for
/// the target.
#[test]
fn record_target_skips_a_leading_flag_to_find_the_positional() {
    let d = pile();
    let mut w = wb(&d);
    w.begin(".fit --dry-run t.tdy.sql");
    let last = w.last_target.expect("last_target should be set");
    assert_eq!(last.file_name().unwrap(), "t.tdy.sql");
}

/// An unterminated quote is the tokenizer's own error; `record_target`
/// records nothing rather than falling back to a partial parse, leaving
/// `last_target` exactly as it was.
#[test]
fn record_target_records_nothing_on_an_unterminated_quote() {
    let d = pile();
    let mut w = wb(&d);
    w.begin(".fit sales.tdy.sql"); // establishes a baseline last_target
    let before = w.last_target.clone();
    w.busy = None; // begin() is idempotent while busy; reset so the next call runs
    w.begin(".fit \"broken");
    assert_eq!(w.last_target, before, "an unterminated quote must not change last_target");
}
