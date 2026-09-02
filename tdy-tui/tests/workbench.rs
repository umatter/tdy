use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use tdy::console::{Outcome, Payload, RawHead, Table};
use tdy::report::{MemberReport, MemberStatus, PileReport, Problem, SourceBinding};
use tdy_tui::browser::Browser;
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
    Workbench::new(Browser::new(d.path()).unwrap(), vec![])
}
fn outcome(echo: &str, text: &str, payload: Payload) -> Outcome {
    Outcome { echo: echo.into(), text: text.into(), payload, ok: true }
}

// Copied from the old `tests/render.rs:40-75` (that file dies in Task 7) —
// the member-builder pattern a synthetic `PileReport` needs.
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
    let raw = RawHead { lines: vec!["A;B".into(), "1;2".into()], truncated: false, sheets: vec![] };
    let follow = w.apply(outcome(".show a.csv", "a.csv:\n  A;B\n", Payload::Shown {
        path: d.path().join("a.csv"), raw, spec: None,
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

    // From Browser, on a target entry.
    let mut w2 = wb(&d);
    w2.key(key(KeyCode::Tab)); // Browser; entries sorted dirs-first then files/targets
    while w2.browser.selected_entry().map(|e| e.name.as_str()) != Some("t.tdy.sql") {
        w2.key(key(KeyCode::Down));
    }
    assert_eq!(w2.key(key(KeyCode::Char('f'))), WbAction::Dispatch(".fit t.tdy.sql".into()));

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
    let raw = || RawHead { lines: vec!["A;B".into(), "1;2".into()], truncated: false, sheets: vec![] };
    w.begin(".show a.csv");
    w.apply(outcome(".show a.csv", "", Payload::Shown { path: d.path().join("a.csv"), raw: raw(), spec: None }), d.path());

    // Scroll the raw view down.
    w.key(key(KeyCode::Tab));
    w.key(key(KeyCode::Tab)); // Main
    w.key(key(KeyCode::Down));
    w.key(key(KeyCode::Down));
    assert_eq!(w.main_scroll, 2);

    // A different file: back to the top.
    w.begin(".show b.csv");
    w.apply(outcome(".show b.csv", "", Payload::Shown { path: d.path().join("b.csv"), raw: raw(), spec: None }), d.path());
    assert_eq!(w.main_scroll, 0);

    // The same file again (the preview's raw fill-in): the scroll stands.
    w.key(key(KeyCode::Down));
    assert_eq!(w.main_scroll, 1);
    w.set_preview(d.path().join("b.csv"), raw(), None);
    assert_eq!(w.main_scroll, 1, "a same-path update must not throw away the user's scroll");
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

    let raw = RawHead { lines: vec!["Datum;Kanton".into()], truncated: false, sheets: vec![] };
    // A different, unrelated path must be dropped.
    w.set_preview(d.path().join("a.csv"), raw.clone(), None);
    assert!(matches!(&w.context, Context::Member { raw: None, .. }), "stale path must not fill raw");

    // The actual path Enter asked for fills it.
    w.set_preview(path, raw, None);
    assert!(matches!(&w.context, Context::Member { raw: Some(_), .. }));
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
