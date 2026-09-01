//! What the screens actually say, rendered into a headless buffer.
//!
//! A TUI's contract is what a person sees, so these tests read the drawn
//! text back and assert on it. The one that matters most is the accept
//! screen: the review gate's whole value is that a human reads the
//! consequence before saying yes, and a screen that showed the reason but
//! not the numbers would quietly turn that into a keystroke.

use ratatui::backend::TestBackend;
use ratatui::Terminal;

use tdy::report::{
    MemberReport, MemberStatus, PileReport, Problem, ProposalReport, SourceBinding,
};
use tdy_tui::app::{App, Key, Preview, Screen};
use tdy_tui::evidence::{Evidence, Pair};
use tdy_tui::remedy::Remedy;
use tdy_tui::ui;

/// The visible text of each row, styles stripped.
fn screen(app: &mut App, w: u16, h: u16) -> Vec<String> {
    let mut terminal = Terminal::new(TestBackend::new(w, h)).unwrap();
    terminal.draw(|f| ui::draw(f, app)).unwrap();
    let buf = terminal.backend().buffer().clone();
    (0..buf.area.height)
        .map(|y| {
            (0..buf.area.width)
                .map(|x| buf[(x, y)].symbol())
                .collect::<String>()
                .trim_end()
                .to_string()
        })
        .collect()
}

fn text(app: &mut App) -> String {
    screen(app, 110, 34).join("\n")
}

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

fn app_with(members: Vec<MemberReport>) -> App {
    let failed = members.iter().filter(|m| m.status == MemberStatus::Gaps).count();
    let needs_review = members.iter().filter(|m| m.status == MemberStatus::NeedsReview).count();
    let mut a = App::new(
        "sales.tdy.sql".into(),
        "CREATE TABLE sales (\n  region TEXT NOT NULL\n)\nWITH (files = '*.csv');\n".into(),
    );
    a.set_report(PileReport {
        target: "sales".into(),
        target_file: "sales.tdy.sql".into(),
        declared_columns: 3,
        fitted: members.len() - failed,
        failed,
        needs_review,
        members,
        lock_written: None,
        dry_run: false,
    });
    a
}

/// The pile view names every member, its status, and — crucially — the
/// *reason*, so a refused row tells you something without being opened.
#[test]
fn the_pile_view_shows_a_reason_not_just_a_status() {
    let mut a = app_with(vec![member("2025-01.csv", MemberStatus::Fits), gap_member("2025-11.csv")]);
    let s = text(&mut a);

    assert!(s.contains("2025-01.csv"), "{s}");
    assert!(s.contains("fits"), "{s}");
    assert!(s.contains("2025-11.csv"), "{s}");
    assert!(s.contains("REFUSED"), "{s}");
    assert!(s.contains("no column of this file binds"), "the reason is on the row:\n{s}");
    // The header counts what it says it counts.
    assert!(s.contains("1 fit"), "{s}");
    assert!(s.contains("1 refused"), "{s}");
}

/// The member screen puts the gap beside the file's own rows: "no column of
/// this file binds" is answered by looking, so looking must not require
/// leaving.
#[test]
fn the_member_screen_shows_the_gap_beside_the_file() {
    let mut a = app_with(vec![gap_member("2025-11.csv")]);
    a.handle(Key::Enter);
    a.preview = Some(Preview {
        header: vec!["Datum".into(), "Kanton".into()],
        rows: vec![vec!["30.11.2025".into(), "Ticino".into()]],
    });
    let s = text(&mut a);

    assert!(s.contains("no column of this file binds"), "{s}");
    assert!(s.contains("what tdy sees"), "{s}");
    assert!(s.contains("Kanton"), "the file's own header is shown:\n{s}");
    assert!(s.contains("30.11.2025"), "the file's own rows are shown:\n{s}");
    // The remedies are offered as numbered, one-key edits.
    assert!(s.contains("remedies"), "{s}");
    assert!(s.contains("[1]"), "{s}");
}

/// THE screen the whole gate rests on. It must show the raw values beside
/// what they become, and the extremes over the whole file — a shift applied
/// the wrong way is invisible in the head and unmissable at the ends.
#[test]
fn the_accept_screen_shows_the_consequence_not_just_the_reason() {
    let mut a = app_with(vec![member("2025-07.csv", MemberStatus::NeedsReview)]);
    a.handle(Key::Enter);
    a.busy = None;
    a.handle(Key::Char('a'));
    a.busy = None;
    a.evidence = Some(vec![Evidence::Shift {
        column: "amount_chf".into(),
        source: "Betrag Rp.".into(),
        shift: -2,
        head: vec![Pair { row: 1, raw: "170000".into(), parsed: "1700.00".into() }],
        smallest: Some(Pair { row: 1, raw: "170000".into(), parsed: "1700.00".into() }),
        largest: Some(Pair { row: 4, raw: "173000".into(), parsed: "1730.00".into() }),
        rows: 4,
    }]);
    let s = text(&mut a);

    assert_eq!(a.screen, Screen::Accept);
    // The reason…
    assert!(s.contains("decimal_shift = -2"), "{s}");
    // …and the consequence, in numbers.
    assert!(s.contains("170000"), "the raw value is shown:\n{s}");
    assert!(s.contains("1700.00"), "the parsed value is shown:\n{s}");
    assert!(s.contains("173000") && s.contains("1730.00"), "the extremes are shown:\n{s}");
    assert!(s.contains("every row of the file"), "{s}");
    // …and what accepting means.
    assert!(s.contains("retracts it"), "the consequence of acceptance is stated:\n{s}");
    assert!(s.contains("[a]"), "{s}");
    // There is no bulk accept anywhere on this screen.
    assert!(!s.to_lowercase().contains("accept all"), "{s}");
}

/// An asserted constant reads as an assertion, with the row count it reaches
/// and the sentence saying the file does not contain it.
#[test]
fn the_accept_screen_says_a_constant_is_being_asserted() {
    let mut a = app_with(vec![member("2025-11.csv", MemberStatus::NeedsReview)]);
    a.handle(Key::Enter);
    a.busy = None;
    a.handle(Key::Char('a'));
    a.busy = None;
    a.evidence =
        Some(vec![Evidence::Constant { column: "region".into(), value: "Ticino".into(), rows: 4 }]);
    let s = text(&mut a);
    assert!(s.contains("Ticino"), "{s}");
    assert!(s.contains("4 row(s)"), "{s}");
    assert!(s.contains("You are asserting it"), "{s}");
}

/// An edit is shown as a diff of the declaration before anything is written,
/// with both the old line and the new one.
#[test]
fn the_confirm_screen_shows_the_diff_before_writing() {
    let mut m = gap_member("2025-11.csv");
    // What `--propose` found: `Kanton` can produce a TEXT column, `Datum`
    // (a date) cannot. The menu must rank by that, not by file order.
    m.proposals = vec![ProposalReport {
        column: "region".into(),
        want: "TEXT".into(),
        candidates: vec![("Kanton".into(), "all 4 sampled value(s) parse as TEXT".into())],
        message: "region TEXT OPTIONS(matches = 'Kanton')".into(),
    }];
    let mut a = app_with(vec![m]);
    a.handle(Key::Enter);
    assert!(
        matches!(&a.remedies()[0], Remedy::AddMatch { spelling, .. } if spelling == "Kanton"),
        "the type-compatible candidate must be [1], got {:?}",
        a.remedies()
    );
    a.handle(Key::Char('1'));
    let s = text(&mut a);

    assert_eq!(a.screen, Screen::Confirm);
    assert!(s.contains("edit the declaration"), "{s}");
    assert!(s.contains("- "), "the old line is shown:\n{s}");
    assert!(s.contains("+ "), "the new line is shown:\n{s}");
    assert!(s.contains("Kanton"), "{s}");
    assert!(s.contains("[y]"), "{s}");
}

/// While work runs, the status line says what is happening — the whole point
/// of the progress channel.
#[test]
fn the_status_line_narrates_running_work() {
    let mut a = app_with(vec![member("2025-01.csv", MemberStatus::Fits)]);
    a.busy = Some("asking gemini via openrouter about 2025-01.csv".into());
    let s = text(&mut a);
    assert!(s.contains("asking gemini"), "{s}");
}

/// Rendering must not panic at hostile sizes: a narrow or short terminal is
/// a resize away, and a panic there takes the user's terminal with it.
#[test]
fn every_screen_renders_at_hostile_sizes() {
    let screens =
        [Screen::Pile, Screen::Member, Screen::Accept, Screen::Confirm, Screen::Query, Screen::Help];
    for (w, h) in [(20u16, 5u16), (40, 10), (200, 60), (10, 3), (1, 1)] {
        for sc in screens {
            let mut a = app_with(vec![
                gap_member("2025-11.csv"),
                member("2025-07.csv", MemberStatus::NeedsReview),
            ]);
            // Reach Confirm through the app so `pending` is genuinely set.
            if sc == Screen::Confirm {
                a.handle(Key::Enter);
                a.handle(Key::Char('1'));
            } else {
                a.screen = sc;
            }
            a.evidence = Some(vec![Evidence::Unillustrated { reason: "x".into() }]);
            a.preview = Some(Preview {
                header: vec!["a".into(), "b".into()],
                rows: vec![vec!["1".into(), "2".into()]],
            });
            let _ = screen(&mut a, w, h);
        }
    }
}
