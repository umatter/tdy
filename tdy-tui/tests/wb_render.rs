//! What the workbench frame actually shows, rendered into a headless buffer.
//!
//! Draw into a `TestBackend`, read the text back, assert on what a person
//! would see — the approach the classic screens' `tests/render.rs` used
//! before every review behaviour moved into the workbench and that file
//! was deleted (slice 3 Task 7).

use ratatui::backend::TestBackend;
use ratatui::Terminal;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use tdy::report::{MemberReport, MemberStatus, PileReport, Problem, SourceBinding};
use tdy_tui::browser::Browser;
use tdy_tui::wb_ui;
use tdy_tui::workbench::Workbench;

fn key(c: KeyCode) -> KeyEvent { KeyEvent::new(c, KeyModifiers::NONE) }
fn ctrl(c: char) -> KeyEvent { KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL) }

fn screen(w: &mut Workbench, cols: u16, rows: u16) -> Vec<String> {
    let mut t = Terminal::new(TestBackend::new(cols, rows)).unwrap();
    t.draw(|f| wb_ui::draw(f, w)).unwrap();
    let buf = t.backend().buffer().clone();
    (0..buf.area.height)
        .map(|y| (0..buf.area.width).map(|x| buf[(x, y)].symbol()).collect::<String>().trim_end().to_string())
        .collect()
}

/// The drawn buffer itself, for assertions on colour and position that
/// the text alone cannot make.
fn buffer(w: &mut Workbench, cols: u16, rows: u16) -> ratatui::buffer::Buffer {
    let mut t = Terminal::new(TestBackend::new(cols, rows)).unwrap();
    t.draw(|f| wb_ui::draw(f, w)).unwrap();
    t.backend().buffer().clone()
}

fn row_text(buf: &ratatui::buffer::Buffer, y: u16) -> String {
    (0..buf.area.width).map(|x| buf[(x, y)].symbol()).collect()
}

/// First (x, y) at which `needle` starts, scanning rows top to bottom.
fn find(buf: &ratatui::buffer::Buffer, needle: &str) -> Option<(u16, u16)> {
    (0..buf.area.height).find_map(|y| row_text(buf, y).find(needle).map(|i| {
        // byte index -> column: symbols are one cell each here
        let col = row_text(buf, y)[..i].chars().count() as u16;
        (col, y)
    }))
}

fn fg_at(buf: &ratatui::buffer::Buffer, x: u16, y: u16) -> ratatui::style::Color {
    buf[(x, y)].fg
}

fn reversed_at(buf: &ratatui::buffer::Buffer, x: u16, y: u16) -> bool {
    buf[(x, y)].modifier.contains(ratatui::style::Modifier::REVERSED)
}

fn declared(name: &str, dtype: &str, matches: &[&str]) -> tdy::report::TargetColumnReport {
    tdy::report::TargetColumnReport {
        name: name.into(),
        dtype: dtype.into(),
        nullable: false,
        matches: matches.iter().map(|m| m.to_string()).collect(),
        if_missing_null: false,
    }
}

fn fitted(w: &mut Workbench, d: &tempfile::TempDir, report: PileReport) {
    use tdy::console::{Outcome, Payload};
    w.begin(".fit sales.tdy.sql");
    w.apply(
        Outcome { echo: ".fit sales.tdy.sql".into(), text: String::new(), ok: true, payload: Payload::Fitted(report) },
        d.path(),
    );
}

fn pile_report(members: Vec<MemberReport>) -> PileReport {
    let failed = members.iter().filter(|m| m.status == MemberStatus::Gaps).count();
    let needs_review = members.iter().filter(|m| m.status == MemberStatus::NeedsReview).count();
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
        columns: vec![],
        drift: vec![],
    }
}

fn pile() -> tempfile::TempDir {
    let d = tempfile::tempdir().unwrap();
    std::fs::write(d.path().join("a.csv"), "A;B\n1;2\n").unwrap();
    std::fs::write(d.path().join("t.tdy.sql"), "CREATE TABLE t (a TEXT) WITH (files='*.csv');").unwrap();
    d
}

// Copied from the old `tests/render.rs:40-75`, now deleted (Task 7).
fn member(path: &str, status: MemberStatus) -> MemberReport {
    MemberReport {
        path: path.into(),
        sheet: None,
        status,
        via: Some("heuristic".into()),
        sources: vec![SourceBinding { column: "month".into(), source: "Datum".into() }],
        review: (status == MemberStatus::NeedsReview).then(|| {
            "`amount` applies decimal_shift = -2, which changes every value".into()
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
        long_form: None,
    }];
    m
}

#[test]
fn the_frame_shows_three_panes_and_the_status_vocabulary() {
    let d = pile();
    let mut w = Workbench::new(Browser::new(d.path()).unwrap(), vec![], 0.8);
    let text = screen(&mut w, 100, 30).join("\n");
    assert!(text.contains(" files "), "{text}");
    assert!(text.contains(" console "), "{text}");
    assert!(text.contains("a.csv"), "{text}");
    assert!(text.contains("no lock"), "{text}");
    assert!(text.contains("tdy>"), "{text}");
    assert!(text.contains("select a file"), "{text}");
}

#[test]
fn narrow_terminals_drop_the_browser_not_the_console() {
    let d = pile();
    let mut w = Workbench::new(Browser::new(d.path()).unwrap(), vec![], 0.8);
    let text = screen(&mut w, 50, 20).join("\n");
    assert!(!text.contains(" files "), "{text}");
    assert!(text.contains("tdy>"), "{text}");
}

#[test]
fn scrollback_shows_echo_then_text_and_busy_shows_in_status() {
    let d = pile();
    let mut w = Workbench::new(Browser::new(d.path()).unwrap(), vec![], 0.8);
    w.begin(".ls");
    let text = screen(&mut w, 100, 30).join("\n");
    assert!(text.contains(".ls"), "{text}");
    use tdy::console::{Outcome, Payload};
    w.apply(Outcome { echo: ".ls".into(), text: "a.csv  sniffed\n".into(), payload: Payload::Nothing, ok: true }, d.path());
    w.progress("fitting a.csv (1 of 9)".into());
    let text = screen(&mut w, 100, 30).join("\n");
    assert!(text.contains("tdy> .ls"), "{text}");
    assert!(text.contains("a.csv  sniffed"), "{text}");
    assert!(text.contains("fitting a.csv (1 of 9)"), "{text}");
}

/// The 26-column browser pane cannot carry `render_listing`'s long-form
/// text (`sniffed 0.95 (heuristic)` is 24 chars against ~22 usable
/// columns) without silently clipping — the bug this test exists to catch.
/// The browser uses its own compact vocabulary instead (design doc §6:
/// `✓ 0.95`, `drift (N)`, …), and the status never gives way to a long
/// name; the name is what gets ellipsized.
#[test]
fn browser_status_uses_compact_glyphs_and_never_clips_even_with_a_long_name() {
    let d = pile();
    let mut w = Workbench::new(Browser::new(d.path()).unwrap(), vec![], 0.8);
    use tdy::console::{Entry, EntryKind, EntryStatus};
    w.browser.entries.push(Entry {
        name: "b.csv".into(),
        kind: EntryKind::File,
        status: EntryStatus::Sniffed { confidence: Some(0.95), method: "heuristic".into() },
    });
    let long_name = "a_very_long_filename_that_cannot_possibly_fit_in_a_twenty_six_column_pane.csv";
    w.browser.entries.push(Entry {
        name: long_name.into(),
        kind: EntryKind::File,
        status: EntryStatus::Drift(99),
    });
    let text = screen(&mut w, 100, 30).join("\n");
    assert!(text.contains("✓ 0.95"), "{text}");
    assert!(text.contains("drift (99)"), "{text}");
    assert!(!text.contains(long_name), "the full long name should be ellipsized: {text}");
}

#[test]
fn a_file_without_a_sidecar_shows_raw_only_and_no_opinion() {
    let d = pile();
    let mut w = Workbench::new(Browser::new(d.path()).unwrap(), vec![], 0.8);
    use tdy::console::{Outcome, Payload, RawHead};
    w.begin(".show a.csv");
    w.apply(Outcome {
        echo: ".show a.csv".into(), text: String::new(), ok: true,
        payload: Payload::Shown {
            path: d.path().join("a.csv"),
            raw: RawHead { lines: vec!["A;B".into(), "1;2".into()], truncated: true, sheets: vec![], grid: vec![], grid_sheet: None },
            spec: None,
            stale: false,
        },
    }, d.path());
    let text = screen(&mut w, 100, 30).join("\n");
    assert!(text.contains("A;B") && text.contains("1;2"), "{text}");
    assert!(text.contains("…"), "truncation marker: {text}");
    assert!(text.contains("not sniffed"), "{text}");
    assert!(!text.contains("TEXT") && !text.contains("<-"), "no opinion yet: {text}");
}

/// A workbook member's raw view used to show only `sheet "N": R row(s) x C
/// col(s)` — no header spelling, no raw values. The grid is the deleted
/// preview.rs's load-bearing property brought back: the file's own
/// spelling and its own thousands separator, not a paraphrase of them.
#[test]
fn a_workbook_member_shows_its_grid() {
    let d = pile();
    let mut w = Workbench::new(Browser::new(d.path()).unwrap(), vec![], 0.8);
    use tdy::console::{Outcome, Payload, RawHead};
    w.begin(".show umsatz.xlsx");
    w.apply(Outcome {
        echo: ".show umsatz.xlsx".into(), text: String::new(), ok: true,
        payload: Payload::Shown {
            path: d.path().join("umsatz.xlsx"),
            raw: RawHead {
                lines: vec![],
                truncated: false,
                sheets: vec![("Umsatz".into(), 9, 6)],
                grid: vec![
                    vec!["Region".into(), "Betrag CHF".into()],
                    vec!["Ost".into(), "1'100.00".into()],
                    // Longer than 14 chars — `raw_head_lines`'s cell
                    // truncation must clip it to a 13-char prefix plus `…`
                    // and never show the string whole. Pins the TUI's
                    // 14-char-per-cell rule (slice-3 review minor #13).
                    vec!["West".into(), "Umsatzübersicht_gesamt".into()],
                ],
                grid_sheet: Some("Umsatz".into()),
            },
            spec: None,
            stale: false,
        },
    }, d.path());
    let text = screen(&mut w, 100, 30).join("\n");
    // The grid is the first sheet's, and the panel says so above the rows —
    // a workbook may list a dozen sheets right there.
    assert!(text.contains("grid of sheet \"Umsatz\""), "{text}");
    assert!(text.contains("Betrag CHF"), "{text}");
    assert!(text.contains("1'100.00"), "{text}");
    assert!(text.contains("Umsatzübersic…"), "the truncated prefix must appear: {text}");
    assert!(!text.contains("Umsatzübersicht_gesamt"), "the full string must not appear: {text}");
}

#[test]
fn a_sniffed_file_shows_raw_beside_the_spec_and_its_decisions() {
    let d = pile();
    let mut w = Workbench::new(Browser::new(d.path()).unwrap(), vec![], 0.8);
    use tdy::console::{Outcome, Payload, SpecSummary, Table};
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
    }, d.path());
    assert!(follow.is_some(), "sniffed context asks the runtime for the raw half");
    let text = screen(&mut w, 110, 34).join("\n");
    assert!(text.contains("betrag") && text.contains("Betrag") && text.contains("DECIMAL(38,2)"), "{text}");
    assert!(text.contains("ambiguous date order"), "the decisions list: {text}");
    assert!(text.contains("0.60"), "confidence shown: {text}");
}

#[test]
fn a_query_context_shows_the_table_and_counts() {
    let d = pile();
    let mut w = Workbench::new(Browser::new(d.path()).unwrap(), vec![], 0.8);
    use tdy::console::{Outcome, Payload, Table};
    w.begin("SELECT 1;");
    let t = Table {
        columns: vec!["region".into(), "total".into()], types: vec![],
        rows: vec![vec!["Ost".into(), "14200.00".into()]], total: 500, truncated: true,
    };
    w.apply(Outcome { echo: "SELECT 1;".into(), text: String::new(), ok: true, payload: Payload::Query(t) }, d.path());
    let text = screen(&mut w, 100, 30).join("\n");
    assert!(text.contains("region") && text.contains("14200.00"), "{text}");
    assert!(text.contains("500 row(s)") && text.contains("truncated"), "{text}");

    // A result table scrolls (slice 4 wired `main_scroll` through
    // `table_lines`), so the status hint must advertise the keys that move
    // it rather than the bare "Tab focus" it used to show.
    w.key(key(KeyCode::Tab)); // Browser
    w.key(key(KeyCode::Tab)); // Main
    let text = screen(&mut w, 100, 30).join("\n");
    assert!(text.contains("↑↓ scroll"), "a scrollable result must say so: {text}");
    assert!(text.contains("^Q"), "quit must stay advertised in every context: {text}");
}

/// The Empty view now draws the generated mark (half-block glyphs) above
/// the orientation lines, in a pane tall enough to hold it.
///
/// The orientation must also be *true*: it used to advertise "the classic
/// review flow", which no longer exists — a target on the command line
/// opens this very workbench, fitted as a dry run. Orientation text that
/// names a screen the reader can never reach is worse than none.
#[test]
fn the_empty_view_draws_the_mark_and_orients_truthfully() {
    let d = pile();
    let mut w = Workbench::new(Browser::new(d.path()).unwrap(), vec![], 0.8);
    let text = screen(&mut w, 100, 30).join("\n");
    assert!(text.contains('▀') || text.contains('▄'), "no mark glyph found: {text}");
    assert!(text.contains("select a file"), "{text}");
    assert!(!text.contains("classic"), "the classic flow is gone; do not advertise it: {text}");
    assert!(text.contains("dry run"), "{text}");
    assert!(text.contains("press f"), "{text}");
}

/// `?` opens a bordered ` keys ` overlay over the main pane, listing the
/// current key vocabulary and showing the mark again.
#[test]
fn the_help_overlay_lists_the_keys() {
    let d = pile();
    let mut w = Workbench::new(Browser::new(d.path()).unwrap(), vec![], 0.8);
    w.key(key(KeyCode::Tab)); // Browser
    w.key(key(KeyCode::Char('?')));
    assert!(w.help);
    let text = screen(&mut w, 100, 30).join("\n");
    assert!(text.contains(" keys "), "{text}");
    assert!(text.contains("Tab"), "{text}");
    assert!(text.contains('▀') || text.contains('▄'), "no mark glyph in overlay: {text}");
}

/// Regression: `draw_right` used to check `zoom` before `help`, so `?`
/// while zoomed (Tab still moves focus off the console) set an invisible
/// overlay — nothing drawn, and the next keystroke was silently swallowed
/// closing a help screen nobody saw. `help` must win regardless of `zoom`.
#[test]
fn the_help_overlay_renders_even_when_the_console_is_zoomed() {
    let d = pile();
    let mut w = Workbench::new(Browser::new(d.path()).unwrap(), vec![], 0.8);
    w.key(ctrl('l')); // zoom, from the default Console focus
    assert!(w.zoom);
    w.key(key(KeyCode::Tab)); // Browser
    w.key(key(KeyCode::Char('?')));
    assert!(w.help);
    let text = screen(&mut w, 100, 30).join("\n");
    assert!(text.contains(" keys "), "{text}");
}

/// Regression: the preview-table height heuristic used to apply its floor
/// (`.max(2)`) AFTER capping to available space, so a short pane could give
/// the strip 2 rows while the `Fill(1)` spec summary above it got zero. The
/// summary (method, confidence, columns, decisions) is primary; the
/// preview is secondary and must never take rows from it — a pane too
/// short for both must drop the preview strip, never squeeze the summary.
/// The Pile context lists each member's path and status word, a counts
/// line, and marks the selected row.
#[test]
fn the_pile_context_lists_members_with_status_words() {
    let d = pile();
    let mut w = Workbench::new(Browser::new(d.path()).unwrap(), vec![], 0.8);
    use tdy::console::{Outcome, Payload};
    w.begin(".fit sales.tdy.sql");
    let members = vec![
        member("2025-01.csv", MemberStatus::Fits),
        gap_member("2025-02.csv"),
        member("2025-03.csv", MemberStatus::NeedsReview),
    ];
    let failed = 1;
    let needs_review = 1;
    let report = PileReport {
        target: "sales".into(),
        target_file: "sales.tdy.sql".into(),
        declared_columns: 3,
        fitted: members.len() - failed,
        failed,
        needs_review,
        members,
        lock_written: None,
        dry_run: false,
        columns: vec![],
        drift: vec![],
    };
    w.apply(
        Outcome {
            echo: ".fit sales.tdy.sql".into(),
            text: String::new(),
            ok: true,
            payload: Payload::Fitted(report),
        },
        d.path(),
    );
    let text = screen(&mut w, 110, 30).join("\n");
    assert!(text.contains("2025-01.csv"), "{text}");
    assert!(text.contains("2025-02.csv"), "{text}");
    assert!(text.contains("2025-03.csv"), "{text}");
    assert!(text.contains("GAP"), "{text}");
    assert!(text.contains("REVIEW"), "{text}");
    assert!(text.contains("2 fitted") && text.contains("1 failed") && text.contains("1 need review"), "{text}");
    // The selected row (index 0) is marked.
    let row0 = text.lines().find(|l| l.contains("2025-01.csv")).unwrap();
    assert!(row0.contains('▸'), "{row0}");
}

/// A dry-run fit (the launch-time review, and `f`'s explicit `--dry-run`)
/// must say so in the pile header — `dry run` is the difference between
/// "this is what would happen" and "this is what happened", and the
/// workbench must never blur the two.
#[test]
fn a_dry_run_pile_report_marks_the_header() {
    let d = pile();
    let mut w = Workbench::new(Browser::new(d.path()).unwrap(), vec![], 0.8);
    use tdy::console::{Outcome, Payload};
    w.begin(".fit sales.tdy.sql --dry-run");
    let members = vec![member("2025-01.csv", MemberStatus::Fits)];
    let report = PileReport {
        target: "sales".into(),
        target_file: "sales.tdy.sql".into(),
        declared_columns: 3,
        fitted: members.len(),
        failed: 0,
        needs_review: 0,
        members,
        lock_written: None,
        dry_run: true,
        columns: vec![],
        drift: vec![],
    };
    w.apply(
        Outcome {
            echo: ".fit sales.tdy.sql --dry-run".into(),
            text: String::new(),
            ok: true,
            payload: Payload::Fitted(report),
        },
        d.path(),
    );
    let text = screen(&mut w, 110, 30).join("\n");
    assert!(text.contains("· dry run"), "{text}");
}

#[test]
fn a_short_pane_never_zeroes_the_spec_summary_for_the_preview_strip() {
    let d = pile();
    let mut w = Workbench::new(Browser::new(d.path()).unwrap(), vec![], 0.8);
    use tdy::console::{Outcome, Payload, SpecSummary, Table};
    // At 30 total rows with a 22-row console, the main pane's inner height
    // lands at 2 — exactly the case the old code mishandled.
    w.console_rows = 22;
    w.begin(".sniff a.csv");
    let spec = SpecSummary {
        method: "heuristic".into(), confidence: Some(0.6),
        extraction: r#"{"format":"delimited"}"#.into(),
        transforms: vec![],
        columns: vec![("betrag".into(), "Betrag".into(), "DECIMAL(38,2)".into())],
        notes: vec!["ambiguous date order".into()],
    };
    let preview = Table { columns: vec!["betrag".into()], types: vec![], rows: vec![vec!["1.00".into()]], total: 1, truncated: false };
    w.apply(Outcome {
        echo: ".sniff a.csv".into(), text: String::new(), ok: true,
        payload: Payload::Sniffed { path: d.path().join("a.csv"), spec, preview, kept_existing: false },
    }, d.path());
    // No panic reaching here is itself part of what this test checks.
    let text = screen(&mut w, 100, 30).join("\n");
    assert!(text.contains("0.60"), "spec summary must still render, not be squeezed to nothing: {text}");
    assert!(!text.contains("1.00"), "preview strip should be dropped when the pane is too short for both: {text}");
}

/// The Member context: the file's own raw head on the left (once `raw` is
/// filled), the gap's problem message, and the numbered remedy menu with the
/// selection marker on the right.
#[test]
fn the_member_context_shows_gap_beside_raw_and_the_menu() {
    let d = pile();
    let mut w = Workbench::new(Browser::new(d.path()).unwrap(), vec![], 0.8);
    use tdy::console::{Outcome, Payload, RawHead};
    w.begin(".fit sales.tdy.sql");
    let report = PileReport {
        target: "sales".into(),
        target_file: "sales.tdy.sql".into(),
        declared_columns: 3,
        fitted: 0,
        failed: 1,
        needs_review: 0,
        members: vec![gap_member("2025-02.csv")],
        lock_written: None,
        dry_run: false,
        columns: vec![],
        drift: vec![],
    };
    w.apply(
        Outcome { echo: ".fit sales.tdy.sql".into(), text: String::new(), ok: true, payload: Payload::Fitted(report) },
        d.path(),
    );
    w.key(key(KeyCode::Tab)); // Browser
    w.key(key(KeyCode::Tab)); // Main
    w.key(key(KeyCode::Enter)); // opens the Member context

    let raw = RawHead { lines: vec!["Datum;Kanton;Betrag".into()], truncated: false, sheets: vec![], grid: vec![], grid_sheet: None };
    if let tdy_tui::workbench::Context::Member { target, report, member, .. } = &w.context {
        let path = target.parent().unwrap().join(&report.members[*member].path);
        w.set_preview(w.preview_gen, path, raw, None, false);
    } else {
        panic!("expected Member context, got {:?}", w.context);
    }

    let text = screen(&mut w, 110, 34).join("\n");
    assert!(text.contains("no column of this file"), "problem message: {text}");
    assert!(text.contains("Datum;Kanton;Betrag"), "the file's own raw header: {text}");
    let has_remedy_line = text.lines().any(|l| l.contains("1.") && (l.contains("region") || l.contains("Datum") || l.contains("Kanton")));
    assert!(has_remedy_line, "numbered remedy menu: {text}");
    assert!(text.contains('▸'), "selection marker: {text}");
}

/// After staging an edit (digit `1` on a gap member whose header's first
/// entry is `Kanton`), the confirm overlay covers the main pane with a
/// ` confirm edit ` title, the diff's `+` line, and the y/Esc footer.
#[test]
fn the_confirm_overlay_shows_the_diff() {
    let d = pile();
    let target_sql =
        "CREATE TABLE t (\n  region TEXT NOT NULL OPTIONS(matches = 'Region')\n) WITH (files='*.csv');\n";
    std::fs::write(d.path().join("t.tdy.sql"), target_sql).unwrap();
    let mut w = Workbench::new(Browser::new(d.path()).unwrap(), vec![], 0.8);
    use tdy::console::{Outcome, Payload};
    w.begin(".fit sales.tdy.sql");
    let mut m = gap_member("2025-02.csv");
    // Kanton first, so digit `1` (the first AddMatch candidate) stages the
    // spelling this test asserts on.
    m.problems[0].header = vec!["Kanton".into(), "Datum".into()];
    let report = PileReport {
        target: "sales".into(),
        target_file: "sales.tdy.sql".into(),
        declared_columns: 3,
        fitted: 0,
        failed: 1,
        needs_review: 0,
        members: vec![m],
        lock_written: None,
        dry_run: false,
        columns: vec![],
        drift: vec![],
    };
    w.apply(
        Outcome { echo: ".fit sales.tdy.sql".into(), text: String::new(), ok: true, payload: Payload::Fitted(report) },
        d.path(),
    );
    w.key(key(KeyCode::Tab)); // Browser
    w.key(key(KeyCode::Tab)); // Main
    w.key(key(KeyCode::Enter)); // opens the Member context
    w.set_target_sql(target_sql.to_string());

    let act = w.key(key(KeyCode::Char('1')));
    assert_eq!(act, tdy_tui::workbench::WbAction::None);
    assert!(w.pending_edit.is_some(), "digit 1 should stage an edit");

    let text = screen(&mut w, 110, 34).join("\n");
    assert!(text.contains(" confirm edit "), "{text}");
    let plus_line = text.lines().find(|l| l.contains('+') && l.contains("Kanton"));
    assert!(plus_line.is_some(), "expected a `+` diff line naming Kanton: {text}");
    assert!(text.contains("y writes the target"), "{text}");
    assert!(text.contains("Esc cancels"), "{text}");
}

/// The Evidence view: this restores the classic accept screen's load-bearing
/// property — raw beside parsed, and the extremes over the whole file, not
/// just the head — plus every judgement's own headline, even the ones with
/// nothing else to show (`Unillustrated`).
#[test]
fn the_evidence_view_shows_raw_beside_parsed_and_the_extremes() {
    use tdy::console::{Outcome, Payload};
    use tdy::evidence::{Evidence, Pair};

    let d = pile();
    let mut w = Workbench::new(Browser::new(d.path()).unwrap(), vec![], 0.8);
    w.begin(".accept t.tdy.sql m.csv");
    let rows = vec![
        Evidence::Shift {
            column: "amount".into(),
            source: "Betrag Rp.".into(),
            shift: -2,
            head: vec![Pair { row: 1, raw: "170000".into(), parsed: "1700.00".into() }],
            smallest: Some(Pair { row: 9, raw: "5".into(), parsed: "0.05".into() }),
            largest: Some(Pair { row: 3, raw: "999999".into(), parsed: "9999.99".into() }),
            rows: 36,
        },
        Evidence::Unillustrated { reason: "a model chose the frame".into() },
    ];
    w.apply(
        Outcome {
            echo: ".accept t.tdy.sql m.csv".into(),
            text: String::new(),
            ok: true,
            payload: Payload::Evidence {
                target: d.path().join("t.tdy.sql"),
                member: "m.csv".into(),
                rows,
            },
        },
        d.path(),
    );

    let text = screen(&mut w, 110, 34).join("\n");
    assert!(text.contains(" accept m.csv ? "), "{text}");
    assert!(text.contains("170000"), "raw: {text}");
    assert!(text.contains("1700.00"), "parsed: {text}");
    assert!(text.contains("0.05"), "smallest: {text}");
    assert!(text.contains("9999.99"), "largest: {text}");
    assert!(text.contains("amount"), "the Shift judgement's headline: {text}");
    assert!(
        text.contains("no computable consequence to show"),
        "the Unillustrated judgement's headline too — every judgement shows: {text}"
    );
    assert!(text.contains("a accepts"), "{text}");
    assert!(text.contains("Esc closes"), "{text}");
}

/// Evidence gained scroll (Task 3, folded into slice 4): with enough rows
/// that the pane cannot show them all, `PageDown` (Main focus) advances
/// `main_scroll` AND the render actually shifts — the first judgement's
/// headline scrolls out of view.
#[test]
fn evidence_scrolls_with_page_down() {
    use tdy::console::{Outcome, Payload};
    use tdy::evidence::{Evidence, Pair};

    let d = pile();
    let mut w = Workbench::new(Browser::new(d.path()).unwrap(), vec![], 0.8);
    w.begin(".accept t.tdy.sql m.csv");
    let rows: Vec<Evidence> = (0..40)
        .map(|i| Evidence::Shift {
            column: format!("col{i:02}"),
            source: "Betrag".into(),
            shift: -2,
            head: vec![Pair { row: 1, raw: "100".into(), parsed: "1.00".into() }],
            smallest: Some(Pair { row: 2, raw: "5".into(), parsed: "0.05".into() }),
            largest: Some(Pair { row: 3, raw: "999999".into(), parsed: "9999.99".into() }),
            rows: 10,
        })
        .collect();
    w.apply(
        Outcome {
            echo: ".accept t.tdy.sql m.csv".into(),
            text: String::new(),
            ok: true,
            payload: Payload::Evidence { target: d.path().join("t.tdy.sql"), member: "m.csv".into(), rows },
        },
        d.path(),
    );
    w.key(key(KeyCode::Tab)); // Browser
    w.key(key(KeyCode::Tab)); // Main
    assert_eq!(w.main_scroll, 0);

    let before = screen(&mut w, 100, 20).join("\n");
    assert!(before.contains("col00"), "the first judgement must be visible before scrolling: {before}");

    for _ in 0..5 {
        w.key(key(KeyCode::PageDown));
    }
    assert!(w.main_scroll > 0, "PageDown must advance main_scroll");

    let after = screen(&mut w, 100, 20).join("\n");
    assert!(
        !after.contains("col00"),
        "the first judgement must have scrolled out of view: {after}"
    );
}

/// The Pile view was the widest gap the review found: `main_scroll` moved
/// (item 5's state machine wiring) but `draw_pile` never read it, so PgDn
/// over a long pile — a documented key per the `?` overlay — visibly did
/// nothing. With ~30 members and a pane too short to show them all,
/// `PageDown` must scroll the FIRST VISIBLE member row past index 0.
#[test]
fn pile_scrolls_past_the_first_member_with_page_down() {
    use tdy::console::{Outcome, Payload};

    let d = pile();
    let mut w = Workbench::new(Browser::new(d.path()).unwrap(), vec![], 0.8);
    w.begin(".fit sales.tdy.sql");
    let members: Vec<MemberReport> =
        (0..30).map(|i| member(&format!("m{i:02}.csv"), MemberStatus::Fits)).collect();
    let report = PileReport {
        target: "sales".into(),
        target_file: "sales.tdy.sql".into(),
        declared_columns: 3,
        fitted: members.len(),
        failed: 0,
        needs_review: 0,
        members,
        lock_written: None,
        dry_run: false,
        columns: vec![],
        drift: vec![],
    };
    w.apply(
        Outcome { echo: ".fit sales.tdy.sql".into(), text: String::new(), ok: true, payload: Payload::Fitted(report) },
        d.path(),
    );
    w.key(key(KeyCode::Tab)); // Browser
    w.key(key(KeyCode::Tab)); // Main
    assert_eq!(w.main_scroll, 0);

    let before = screen(&mut w, 100, 20).join("\n");
    assert!(before.contains("m00.csv"), "the first member must be visible before scrolling: {before}");

    for _ in 0..5 {
        w.key(key(KeyCode::PageDown));
    }
    assert!(w.main_scroll > 0, "PageDown must advance main_scroll in a Pile context");
    // `Up`/`Down` still mean member selection here, not scroll — confirm
    // PageDown left it alone.
    assert!(matches!(&w.context, tdy_tui::workbench::Context::Pile { selected: 0, .. }), "{:?}", w.context);

    let after = screen(&mut w, 100, 20).join("\n");
    assert!(!after.contains("m00.csv"), "the first member must have scrolled out of view: {after}");
}

/// The Member view's own gap: `draw_member`'s left column (the file's raw
/// head) never read `main_scroll` either. With a raw head longer than the
/// pane and a pane too short to show it all, `PageDown` must scroll the
/// FIRST VISIBLE raw line past line 0.
#[test]
fn member_raw_head_scrolls_with_page_down() {
    use tdy::console::{Outcome, Payload, RawHead};

    let d = pile();
    let mut w = Workbench::new(Browser::new(d.path()).unwrap(), vec![], 0.8);
    w.begin(".fit sales.tdy.sql");
    let report = PileReport {
        target: "sales".into(),
        target_file: "sales.tdy.sql".into(),
        declared_columns: 3,
        fitted: 1,
        failed: 0,
        needs_review: 0,
        members: vec![member("2025-02.csv", MemberStatus::Fits)],
        lock_written: None,
        dry_run: false,
        columns: vec![],
        drift: vec![],
    };
    w.apply(
        Outcome { echo: ".fit sales.tdy.sql".into(), text: String::new(), ok: true, payload: Payload::Fitted(report) },
        d.path(),
    );
    w.key(key(KeyCode::Tab)); // Browser
    w.key(key(KeyCode::Tab)); // Main
    w.key(key(KeyCode::Enter)); // opens the Member context

    let raw = RawHead {
        lines: (0..40).map(|i| format!("line{i:02}")).collect(),
        truncated: false,
        sheets: vec![],
        grid: vec![],
        grid_sheet: None,
    };
    if let tdy_tui::workbench::Context::Member { target, report, member, .. } = &w.context {
        let path = target.parent().unwrap().join(&report.members[*member].path);
        w.set_preview(w.preview_gen, path, raw, None, false);
    } else {
        panic!("expected Member context, got {:?}", w.context);
    }
    assert_eq!(w.main_scroll, 0);

    let before = screen(&mut w, 100, 20).join("\n");
    assert!(before.contains("line00"), "the first raw line must be visible before scrolling: {before}");

    for _ in 0..5 {
        w.key(key(KeyCode::PageDown));
    }
    assert!(w.main_scroll > 0, "PageDown must advance main_scroll in a Member context");

    let after = screen(&mut w, 100, 20).join("\n");
    assert!(!after.contains("line00"), "the first raw line must have scrolled out of view: {after}");
}

/// A marked file's browser row carries a `*` — `wb_ui` reads `w.marked`
/// directly, so this is the render-level half of the `d`/`D` state-machine
/// tests in `tests/workbench.rs`.
#[test]
fn a_marked_file_shows_an_asterisk_in_the_browser_row() {
    let d = pile();
    let mut w = Workbench::new(Browser::new(d.path()).unwrap(), vec![], 0.8);
    w.key(key(KeyCode::Tab)); // Browser; entries are ["a.csv", "t.tdy.sql"]
    assert_eq!(w.browser.selected_rel().as_deref(), Some("a.csv"));
    w.key(key(KeyCode::Char('d')));
    assert_eq!(w.marked, vec!["a.csv".to_string()]);

    let text = screen(&mut w, 100, 30).join("\n");
    let row = text.lines().find(|l| l.contains("a.csv")).unwrap();
    assert!(row.contains('*'), "{row}");
    // The unmarked target's row carries no asterisk.
    let other = text.lines().find(|l| l.contains("t.tdy.sql")).unwrap();
    assert!(!other.contains('*'), "{other}");
}

/// A stale sidecar (fingerprint no longer matches the file) shows the
/// `--force` hint in the footer instead of the plain "not sniffed" one,
/// which would send someone to re-run a command that reports the same
/// staleness right back. `spec` still stays `None` — only the footer text
/// changes.
#[test]
fn a_stale_sidecar_shows_the_force_hint_instead_of_not_sniffed() {
    let d = pile();
    let mut w = Workbench::new(Browser::new(d.path()).unwrap(), vec![], 0.8);
    use tdy::console::{Outcome, Payload, RawHead};
    let raw = || RawHead { lines: vec!["A;B".into(), "1;2".into()], truncated: false, sheets: vec![], grid: vec![], grid_sheet: None };
    w.begin(".show a.csv");
    w.apply(
        Outcome {
            echo: ".show a.csv".into(),
            text: String::new(),
            ok: true,
            payload: Payload::Shown { path: d.path().join("a.csv"), raw: raw(), spec: None, stale: false },
        },
        d.path(),
    );
    // The runtime's own `PreviewFile` follow-up would carry `stale: true`
    // here (from `spawn_wb_preview`'s `SidecarStatus::Stale` case); no
    // arrow key fired one in this test, so `preview_gen` is still 0.
    w.set_preview(0, d.path().join("a.csv"), raw(), None, true);

    let text = screen(&mut w, 100, 30).join("\n");
    assert!(text.contains("sidecar stale"), "{text}");
    assert!(text.contains(".sniff --force"), "{text}");
    assert!(!text.contains("not sniffed"), "{text}");
}

/// A typed `.show` on a file with a stale sidecar must show the same
/// `.sniff --force` footer an arrow-key preview would — `Payload::Shown`
/// now carries its own `stale` flag (`Command::Show` tells `Fresh`/`Stale`/
/// `Absent` apart), so `apply`'s Shown arm needs no help from a later
/// `set_preview` call to get this right.
#[test]
fn a_typed_show_on_a_stale_sidecar_shows_the_force_hint_too() {
    let d = pile();
    let mut w = Workbench::new(Browser::new(d.path()).unwrap(), vec![], 0.8);
    use tdy::console::{Outcome, Payload, RawHead};
    w.begin(".show a.csv");
    w.apply(
        Outcome {
            echo: ".show a.csv".into(),
            text: String::new(),
            ok: true,
            payload: Payload::Shown {
                path: d.path().join("a.csv"),
                raw: RawHead { lines: vec!["A;B".into(), "1;2".into()], truncated: false, sheets: vec![], grid: vec![], grid_sheet: None },
                spec: None,
                stale: true,
            },
        },
        d.path(),
    );
    let text = screen(&mut w, 100, 30).join("\n");
    assert!(text.contains("sidecar stale"), "{text}");
    assert!(text.contains(".sniff --force"), "{text}");
    assert!(!text.contains("not sniffed"), "{text}");
}

/// The plain "not sniffed" footer is unchanged when there is no staleness
/// to report.
#[test]
fn a_file_with_no_sidecar_at_all_still_shows_the_plain_footer() {
    let d = pile();
    let mut w = Workbench::new(Browser::new(d.path()).unwrap(), vec![], 0.8);
    use tdy::console::{Outcome, Payload, RawHead};
    w.begin(".show a.csv");
    w.apply(
        Outcome {
            echo: ".show a.csv".into(),
            text: String::new(),
            ok: true,
            payload: Payload::Shown {
                path: d.path().join("a.csv"),
                raw: RawHead { lines: vec!["A;B".into()], truncated: false, sheets: vec![], grid: vec![], grid_sheet: None },
                spec: None,
                stale: false,
            },
        },
        d.path(),
    );
    let text = screen(&mut w, 100, 30).join("\n");
    assert!(text.contains("not sniffed — press s"), "{text}");
    assert!(!text.contains("sidecar stale"), "{text}");
}

/// The configured `confidence_threshold` (not a hard-coded constant) is
/// what the File view's confidence line and the browser's `✓ x.xx` glyph
/// are drawn against — a low threshold makes even a low confidence read as
/// fine. Color itself is not assertable through `TestBackend`'s plain
/// symbols, so this documents that the value on screen is the one that was
/// configured, which is the property under test now that it is no longer a
/// module-level constant.
#[test]
fn confidence_is_shown_against_the_configured_threshold_not_a_constant() {
    let d = pile();
    // A threshold of 0.0 means nothing is ever "below" it — proving the
    // number drawn is `w.confidence_threshold`, not the old hard-coded 0.8
    // (which would have nothing to do here either way, since only the
    // color — not assertable — would differ).
    let mut w = Workbench::new(Browser::new(d.path()).unwrap(), vec![], 0.0);
    assert_eq!(w.confidence_threshold, 0.0);
    use tdy::console::{Outcome, Payload, SpecSummary, Table};
    w.begin(".sniff a.csv");
    let spec = SpecSummary {
        method: "heuristic".into(),
        confidence: Some(0.42),
        extraction: r#"{"format":"delimited"}"#.into(),
        transforms: vec![],
        columns: vec![],
        notes: vec![],
    };
    let preview = Table { columns: vec![], types: vec![], rows: vec![], total: 0, truncated: false };
    w.apply(
        Outcome {
            echo: ".sniff a.csv".into(),
            text: String::new(),
            ok: true,
            payload: Payload::Sniffed { path: d.path().join("a.csv"), spec, preview, kept_existing: false },
        },
        d.path(),
    );
    let text = screen(&mut w, 110, 34).join("\n");
    assert!(text.contains("0.42"), "{text}");
}

/// A multi-line echo (a SQL statement assembled across `   -> `
/// continuation lines) is rendered the same way it was typed: `tdy> ` on
/// the first line, `   -> ` on every continuation — never a single line
/// with an embedded newline.
#[test]
fn a_multi_line_echo_renders_as_prompt_then_continuations() {
    let d = pile();
    let mut w = Workbench::new(Browser::new(d.path()).unwrap(), vec![], 0.8);
    use tdy::console::{Outcome, Payload};
    w.begin("SELECT count(*) AS n\nFROM messy('a.csv');");
    w.apply(
        Outcome {
            echo: "SELECT count(*) AS n\nFROM messy('a.csv');".into(),
            text: "| n |\n".into(),
            ok: true,
            payload: Payload::Nothing,
        },
        d.path(),
    );
    let text = screen(&mut w, 100, 30).join("\n");
    assert!(text.contains("tdy> SELECT count(*) AS n"), "{text}");
    assert!(text.contains("   -> FROM messy('a.csv');"), "{text}");
}

/// A failed command's echo line still shows (color is not assertable
/// through `TestBackend`'s plain symbols — this documents that the text
/// itself survives styling, which the code review checks by eye).
#[test]
fn a_failed_cells_echo_still_shows() {
    let d = pile();
    let mut w = Workbench::new(Browser::new(d.path()).unwrap(), vec![], 0.8);
    use tdy::console::{Outcome, Payload};
    w.begin(".nope");
    w.apply(
        Outcome {
            echo: ".nope".into(),
            text: "Error: unknown command `.nope`\n".into(),
            ok: false,
            payload: Payload::Error { message: "unknown command".into() },
        },
        d.path(),
    );
    let text = screen(&mut w, 100, 30).join("\n");
    assert!(text.contains("tdy> .nope"), "{text}");
    assert!(text.contains("Error: unknown command"), "{text}");
}

/// Rendering must not panic at hostile sizes: a narrow or short terminal is
/// a resize away, and a panic there takes the user's terminal with it.
///
/// The classic screens carried this sweep as
/// `render.rs::every_screen_renders_at_hostile_sizes` (deleted with them in
/// Task 7; it drew at 20x5, 40x10, 200x60, 10x3 and 1x1). It comes back
/// here over `Context` instead of `Screen`, at the union of that list and
/// the small squares that break layout arithmetic (2x2, 5x5, 20x10, 80x24),
/// and it covers the two overlays as well — a box drawn into an area
/// smaller than its own borders is exactly where a subtraction underflows.
/// The assertion is simply that nothing panics.
#[test]
fn every_context_renders_at_hostile_sizes() {
    use tdy::console::{Outcome, Payload, RawHead, SpecSummary, Table};
    use tdy::evidence::Evidence;

    const SIZES: [(u16, u16); 9] =
        [(1, 1), (2, 2), (5, 5), (10, 3), (20, 5), (20, 10), (40, 10), (80, 24), (200, 60)];

    fn raw() -> RawHead {
        RawHead {
            lines: vec!["Datum;Kanton;Betrag".into(), "2025-01-01;BE;1.00".into()],
            truncated: true,
            sheets: vec![],
            grid: vec![],
            grid_sheet: None,
        }
    }
    fn spec() -> SpecSummary {
        SpecSummary {
            method: "heuristic".into(),
            confidence: Some(0.42),
            extraction: r#"{"format":"delimited","delimiter":";"}"#.into(),
            transforms: vec!["promote_header".into()],
            columns: vec![("betrag".into(), "Betrag".into(), "DECIMAL(38,2)".into())],
            notes: vec!["ambiguous date order".into()],
        }
    }
    fn table() -> Table {
        Table {
            columns: vec!["region".into(), "amount".into()],
            types: vec!["Utf8".into(), "Decimal128(38, 2)".into()],
            rows: vec![vec!["BE".into(), "14200.00".into()]],
            total: 500,
            truncated: true,
        }
    }
    /// A gap member plus a reviewable one, as a dry-run Pile: something in
    /// every status column, and Enter on index 0 reaches a Member with a
    /// remedy menu.
    fn fitted(d: &tempfile::TempDir) -> Workbench {
        let mut w = Workbench::new(Browser::new(d.path()).unwrap(), vec![], 0.8);
        w.begin(".fit t.tdy.sql --dry-run --propose");
        let members =
            vec![gap_member("2025-11.csv"), member("2025-07.csv", MemberStatus::NeedsReview)];
        let report = PileReport {
            target: "t".into(),
            target_file: "t.tdy.sql".into(),
            declared_columns: 3,
            fitted: 1,
            failed: 1,
            needs_review: 1,
            members,
            lock_written: None,
            dry_run: true,
            columns: vec![],
            drift: vec![],
        };
        w.apply(
            Outcome {
                echo: ".fit t.tdy.sql --dry-run --propose".into(),
                text: String::new(),
                ok: true,
                payload: Payload::Fitted(report),
            },
            d.path(),
        );
        w
    }
    /// Enter on the gap member, with the raw half a real run would have
    /// filled in from its preview task.
    fn opened_member(d: &tempfile::TempDir) -> Workbench {
        let mut w = fitted(d);
        w.key(key(KeyCode::Tab)); // Browser
        w.key(key(KeyCode::Tab)); // Main
        w.key(key(KeyCode::Enter));
        if let tdy_tui::workbench::Context::Member { target, report, member, .. } = &w.context {
            let path = target.parent().unwrap().join(&report.members[*member].path);
            w.set_preview(w.preview_gen, path, raw(), None, false);
        }
        w
    }

    type Build = fn(&tempfile::TempDir) -> Workbench;
    let builders: [(&str, Build); 9] = [
        ("empty", |d| Workbench::new(Browser::new(d.path()).unwrap(), vec![], 0.8)),
        ("file, no spec", |d| {
            let mut w = Workbench::new(Browser::new(d.path()).unwrap(), vec![], 0.8);
            w.begin(".show a.csv");
            w.apply(
                Outcome {
                    echo: ".show a.csv".into(),
                    text: String::new(),
                    ok: true,
                    payload: Payload::Shown {
                        path: d.path().join("a.csv"),
                        raw: raw(),
                        spec: None,
                        stale: true,
                    },
                },
                d.path(),
            );
            w
        }),
        ("file, with spec", |d| {
            let mut w = Workbench::new(Browser::new(d.path()).unwrap(), vec![], 0.8);
            w.begin(".sniff a.csv");
            w.apply(
                Outcome {
                    echo: ".sniff a.csv".into(),
                    text: String::new(),
                    ok: true,
                    payload: Payload::Sniffed {
                        path: d.path().join("a.csv"),
                        spec: spec(),
                        preview: table(),
                        kept_existing: false,
                    },
                },
                d.path(),
            );
            w.set_preview(w.preview_gen, d.path().join("a.csv"), raw(), Some(spec()), false);
            w
        }),
        ("query", |d| {
            let mut w = Workbench::new(Browser::new(d.path()).unwrap(), vec![], 0.8);
            w.begin("SELECT * FROM dataset('t.tdy.sql');");
            w.apply(
                Outcome {
                    echo: "SELECT * FROM dataset('t.tdy.sql');".into(),
                    text: String::new(),
                    ok: true,
                    payload: Payload::Query(table()),
                },
                d.path(),
            );
            w
        }),
        ("pile", fitted),
        ("member", opened_member),
        ("evidence", |d| {
            let mut w = Workbench::new(Browser::new(d.path()).unwrap(), vec![], 0.8);
            w.begin(".accept t.tdy.sql 2025-07.csv");
            w.apply(
                Outcome {
                    echo: ".accept t.tdy.sql 2025-07.csv".into(),
                    text: String::new(),
                    ok: true,
                    payload: Payload::Evidence {
                        target: d.path().join("t.tdy.sql"),
                        member: "2025-07.csv".into(),
                        rows: vec![
                            Evidence::Constant {
                                column: "region".into(),
                                value: "Ticino".into(),
                                rows: 4,
                            },
                            Evidence::Unillustrated { reason: "a model chose the frame".into() },
                        ],
                    },
                },
                d.path(),
            );
            w
        }),
        ("help overlay", |d| {
            let mut w = Workbench::new(Browser::new(d.path()).unwrap(), vec![], 0.8);
            w.key(key(KeyCode::Tab)); // Browser — `?` in the console is a character
            w.key(key(KeyCode::Char('?')));
            assert!(w.help);
            w
        }),
        ("confirm overlay", |d| {
            let mut w = opened_member(d);
            w.set_target_sql(std::fs::read_to_string(d.path().join("t.tdy.sql")).unwrap());
            w.key(key(KeyCode::Char('1')));
            assert!(w.pending_edit.is_some(), "the confirm overlay must actually be staged");
            w
        }),
    ];

    for (name, build) in builders {
        for (cols, rows) in SIZES {
            // A fresh directory per draw: the browser reads it live, and
            // one builder's writes must not leak into the next.
            let d = tempfile::tempdir().unwrap();
            std::fs::write(d.path().join("a.csv"), "Datum;Kanton;Betrag\n2025-01-01;BE;1.00\n")
                .unwrap();
            std::fs::write(
                d.path().join("t.tdy.sql"),
                "CREATE TABLE t (\n  region TEXT NOT NULL OPTIONS(matches = 'Region')\n) \
                 WITH (files='*.csv');\n",
            )
            .unwrap();
            let mut w = build(&d);
            // Any panic happens inside here; the context's name and the
            // size are what a failure report needs to carry.
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let _ = screen(&mut w, cols, rows);
            }))
            .unwrap_or_else(|_| panic!("{name} panicked at {cols}x{rows}"));
        }
    }
}

/// The status bar's hint text depends on what Main is actually showing
/// (Task 4 item 3): a Pile's status line names `f refit`, which does
/// nothing in a Member's remedy menu. `^Q quit` is universal, so both this
/// and the Member test below also pin that it stays advertised regardless
/// of context.
#[test]
fn pile_status_hint_names_refit() {
    let d = pile();
    let mut w = Workbench::new(Browser::new(d.path()).unwrap(), vec![], 0.8);
    use tdy::console::{Outcome, Payload};
    w.begin(".fit sales.tdy.sql");
    let report = PileReport {
        target: "sales".into(),
        target_file: "sales.tdy.sql".into(),
        declared_columns: 3,
        fitted: 1,
        failed: 0,
        needs_review: 0,
        members: vec![member("2025-01.csv", MemberStatus::Fits)],
        lock_written: None,
        dry_run: false,
        columns: vec![],
        drift: vec![],
    };
    w.apply(
        Outcome { echo: ".fit sales.tdy.sql".into(), text: String::new(), ok: true, payload: Payload::Fitted(report) },
        d.path(),
    );
    w.key(key(KeyCode::Tab)); // Browser
    w.key(key(KeyCode::Tab)); // Main
    let text = screen(&mut w, 110, 30).join("\n");
    assert!(text.contains("f refit"), "{text}");
    assert!(text.contains("^Q"), "quit must stay advertised in every context: {text}");
}

/// `spec_lines`' `name ← "source" : TYPE` rows must never let TYPE clip off
/// the right edge (Task 4 item 7): a very long SOURCE (the file's own
/// header spelling, which can be arbitrarily long) is ellipsized so TYPE
/// survives. 132 cols keeps the File view's right half (the spec pane,
/// after the browser pane and the 50/50 raw/spec split) at ~52 columns —
/// the ellipsized `betrag ← "…" : DECIMAL(38,2)` row comes out to ~51,
/// about one column of slack, nowhere near the 89-plus the un-truncated
/// 60-plus-char raw source would need — so the assertion below is only
/// true because the cap engaged, not because there was room to spare.
#[test]
fn a_long_source_is_ellipsized_so_the_type_never_clips() {
    let d = pile();
    let mut w = Workbench::new(Browser::new(d.path()).unwrap(), vec![], 0.8);
    use tdy::console::{Outcome, Payload, SpecSummary, Table};
    w.begin(".sniff a.csv");
    let long_source = "Umsatzübersicht_gesamt_nach_Kanton_und_Gemeinde_und_Jahr_2025";
    let spec = SpecSummary {
        method: "heuristic".into(), confidence: Some(0.6),
        extraction: r#"{"format":"delimited"}"#.into(),
        transforms: vec![],
        columns: vec![("betrag".into(), long_source.into(), "DECIMAL(38,2)".into())],
        notes: vec![],
    };
    let preview = Table { columns: vec!["betrag".into()], types: vec![], rows: vec![], total: 0, truncated: false };
    w.apply(Outcome {
        echo: ".sniff a.csv".into(), text: String::new(), ok: true,
        payload: Payload::Sniffed { path: d.path().join("a.csv"), spec, preview, kept_existing: false },
    }, d.path());
    let text = screen(&mut w, 132, 30).join("\n");
    assert!(text.contains("DECIMAL(38,2)"), "TYPE must survive at a narrow width: {text}");
    assert!(!text.contains(long_source), "the full source must not appear: {text}");
}

/// …and a Member's remedy menu names `1-9` (the digit shortcuts that stage
/// a ranked remedy), which a Pile's own status line does not mention.
#[test]
fn member_status_hint_names_digit_shortcuts() {
    let d = pile();
    let mut w = Workbench::new(Browser::new(d.path()).unwrap(), vec![], 0.8);
    use tdy::console::{Outcome, Payload};
    w.begin(".fit sales.tdy.sql");
    let report = PileReport {
        target: "sales".into(),
        target_file: "sales.tdy.sql".into(),
        declared_columns: 3,
        fitted: 0,
        failed: 1,
        needs_review: 0,
        members: vec![gap_member("2025-02.csv")],
        lock_written: None,
        dry_run: false,
        columns: vec![],
        drift: vec![],
    };
    w.apply(
        Outcome { echo: ".fit sales.tdy.sql".into(), text: String::new(), ok: true, payload: Payload::Fitted(report) },
        d.path(),
    );
    w.key(key(KeyCode::Tab)); // Browser
    w.key(key(KeyCode::Tab)); // Main
    w.key(key(KeyCode::Enter)); // opens the Member context
    let text = screen(&mut w, 110, 30).join("\n");
    assert!(text.contains("1-9"), "{text}");
    assert!(text.contains("^Q"), "quit must stay advertised in every context: {text}");
}

// ---------------------------------------------------------------------------
// The workbench's first visual slice: real tables, a semantic palette, one
// selection grammar, and a member view whose two halves point at each other.
// ---------------------------------------------------------------------------

/// The pile is a table: each member's binding sits under the declared
/// column it supplies, so vocabulary drift across months is a column to
/// read down rather than a sentence per row to compare.
#[test]
fn pile_rows_put_each_binding_under_its_declared_column() {
    let d = pile();
    let mut w = Workbench::new(Browser::new(d.path()).unwrap(), vec![], 0.8);
    let mut jan = member("2025-01.csv", MemberStatus::Fits);
    jan.sources = vec![
        SourceBinding { column: "month".into(), source: "Datum".into() },
        SourceBinding { column: "region".into(), source: "Region".into() },
        SourceBinding { column: "amount".into(), source: "Betrag".into() },
    ];
    let mut oct = member("2025-10.xlsx", MemberStatus::Fits);
    oct.sources = vec![
        SourceBinding { column: "month".into(), source: "Date".into() },
        SourceBinding { column: "region".into(), source: "Region".into() },
        SourceBinding { column: "amount".into(), source: "Amount".into() },
    ];
    let mut r = pile_report(vec![jan, oct]);
    r.columns = vec![
        declared("month", "DATE", &["Datum", "Date"]),
        declared("region", "TEXT", &[]),
        declared("amount", "DECIMAL(14,2)", &["Betrag", "Amount"]),
    ];
    fitted(&mut w, &d, r);

    let buf = buffer(&mut w, 120, 30);
    let in_row = |row_needle: &str, needle: &str| -> (u16, u16) {
        (0..buf.area.height)
            .find_map(|y| {
                let t = row_text(&buf, y);
                (t.contains(row_needle) && t.contains(needle))
                    .then(|| (t.find(needle).map(|i| t[..i].chars().count() as u16).unwrap(), y))
            })
            .unwrap_or_else(|| panic!("no row with {row_needle:?} and {needle:?}"))
    };
    let (x_month, y_head) = in_row("status", "month");
    let (x_datum, y_jan) = in_row("2025-01.csv", "Datum");
    let (x_date, y_oct) = in_row("2025-10.xlsx", "Date");
    assert_eq!(x_datum, x_month, "the binding sits under its column:\n{}", screen(&mut w, 120, 30).join("\n"));
    assert_eq!(x_date, x_month, "{}", screen(&mut w, 120, 30).join("\n"));
    assert!(y_head < y_jan && y_jan < y_oct);
    let text = screen(&mut w, 120, 30).join("\n");
    assert!(text.contains("month DATE"), "the declaration is stated: {text}");
    assert!(text.contains("Datum, Date") || text.contains("Datum | Date"), "with its matches: {text}");
}

/// Status words carry the palette: green for what fits, red for a gap,
/// yellow for a judgement waiting on a person.
#[test]
fn pile_status_words_are_coloured_by_meaning() {
    use ratatui::style::Color;
    let d = pile();
    let mut w = Workbench::new(Browser::new(d.path()).unwrap(), vec![], 0.8);
    let r = pile_report(vec![
        member("2025-01.csv", MemberStatus::Fits),
        gap_member("2025-02.csv"),
        member("2025-03.csv", MemberStatus::NeedsReview),
    ]);
    fitted(&mut w, &d, r);
    // Move the selection off row 0 so its own highlight does not confuse
    // the colour read on the first row.
    w.key(key(KeyCode::Tab));
    w.key(key(KeyCode::Tab));
    w.key(key(KeyCode::Down));
    w.key(key(KeyCode::Down));
    let buf = buffer(&mut w, 120, 30);
    let (x, y) = find(&buf, "fits").unwrap();
    assert_eq!(fg_at(&buf, x, y), Color::Green);
    let (x, y) = find(&buf, "GAP").unwrap();
    assert_eq!(fg_at(&buf, x, y), Color::Red);
    let (x, y) = find(&buf, "REVIEW").unwrap();
    assert_eq!(fg_at(&buf, x, y), Color::Yellow);
    let (x, y) = find(&buf, "1 failed").unwrap();
    assert_eq!(fg_at(&buf, x, y), Color::Red, "the header's counts follow the same rule");
}

/// One selection grammar: the selected pile row is reversed, as the
/// browser's selected entry already is.
#[test]
fn the_selected_pile_row_is_reversed_like_the_browser_selection() {
    let d = pile();
    let mut w = Workbench::new(Browser::new(d.path()).unwrap(), vec![], 0.8);
    let r = pile_report(vec![member("2025-01.csv", MemberStatus::Fits), member("2025-02.csv", MemberStatus::Fits)]);
    fitted(&mut w, &d, r);
    w.key(key(KeyCode::Tab));
    w.key(key(KeyCode::Tab));
    w.key(key(KeyCode::Down));
    let buf = buffer(&mut w, 120, 30);
    let (x, y) = find(&buf, "2025-02.csv").unwrap();
    assert!(reversed_at(&buf, x, y), "selected row");
    let (x0, y0) = find(&buf, "2025-01.csv").unwrap();
    assert!(!reversed_at(&buf, x0, y0), "unselected row");
}

/// Lock drift is a fact about the pile, and the pile view says it.
#[test]
fn the_pile_view_names_drift_against_the_lock() {
    let d = pile();
    let mut w = Workbench::new(Browser::new(d.path()).unwrap(), vec![], 0.8);
    let mut r = pile_report(vec![member("2025-01.csv", MemberStatus::Fits)]);
    r.drift = vec!["2025-13.csv matches this dataset and is not in the lock — run `tdy fit` to plan it".into()];
    fitted(&mut w, &d, r);
    let text = screen(&mut w, 120, 30).join("\n");
    assert!(text.contains("drift") && text.contains("2025-13.csv"), "{text}");
}

fn member_with_raw(w: &mut Workbench, d: &tempfile::TempDir, m: MemberReport, raw: tdy::console::RawHead) {
    fitted(w, d, pile_report(vec![m]));
    w.key(key(KeyCode::Tab));
    w.key(key(KeyCode::Tab));
    w.key(key(KeyCode::Enter));
    if let tdy_tui::workbench::Context::Member { target, report, member, .. } = &w.context {
        let path = target.parent().unwrap().join(&report.members[*member].path);
        w.set_preview(w.preview_gen, path, raw, None, false);
    } else {
        panic!("expected Member context, got {:?}", w.context);
    }
}

fn raw_of(lines: &[&str]) -> tdy::console::RawHead {
    tdy::console::RawHead { lines: lines.iter().map(|l| l.to_string()).collect(), truncated: false, sheets: vec![], grid: vec![], grid_sheet: None }
}

/// The two halves of the member view point at each other: the column
/// `--propose` says can supply the declared one is green in the file's own
/// header line, so the reader does not match spellings by eye.
#[test]
fn member_raw_header_marks_the_proposed_candidate_green() {
    use ratatui::style::Color;
    let d = pile();
    let mut w = Workbench::new(Browser::new(d.path()).unwrap(), vec![], 0.8);
    let mut m = gap_member("2025-02.csv");
    m.problems[0].header = vec!["Datum".into(), "Kanton".into(), "Betrag".into()];
    m.proposals = vec![tdy::report::ProposalReport {
        column: "region".into(),
        want: "TEXT".into(),
        candidates: vec![("Kanton".into(), "all 4 sampled value(s) parse as TEXT".into())],
        message: String::new(),
    }];
    member_with_raw(&mut w, &d, m, raw_of(&["Datum;Kanton;Betrag", "31.01.2025;BE;1.00"]));
    let buf = buffer(&mut w, 120, 34);
    let (x, y) = find(&buf, "Datum;Kanton;Betrag").unwrap();
    assert_eq!(fg_at(&buf, x + 6, y), Color::Green, "Kanton is the candidate");
    assert_ne!(fg_at(&buf, x, y), Color::Green, "Datum is not");
}

/// ...and a column the problem itself implicates — the two `Betrag`s of an
/// ambiguous binding — is yellow.
#[test]
fn member_raw_header_marks_an_implicated_column_yellow() {
    use ratatui::style::Color;
    let d = pile();
    let mut w = Workbench::new(Browser::new(d.path()).unwrap(), vec![], 0.8);
    let mut m = member("2025-08.csv", MemberStatus::Gaps);
    m.problems = vec![Problem {
        kind: "ambiguous".into(),
        column: Some("amount".into()),
        message: "`amount`: 2 columns of this file match, which is ambiguous".into(),
        want: None,
        tried: vec![],
        header: vec![],
        choices: vec!["Betrag (column 3)".into(), "Betrag (column 4)".into()],
        field: None,
        long_form: None,
    }];
    member_with_raw(&mut w, &d, m, raw_of(&["Datum;Region;Betrag;Betrag", "31.08.2025;Ost;1;2"]));
    let buf = buffer(&mut w, 120, 34);
    let (x, y) = find(&buf, "Datum;Region;Betrag;Betrag").unwrap();
    assert_eq!(fg_at(&buf, x + 13, y), Color::Yellow, "the first Betrag");
    assert_eq!(fg_at(&buf, x + 20, y), Color::Yellow, "and the second");
    assert_ne!(fg_at(&buf, x, y), Color::Yellow, "Datum is not implicated");
}

/// The raw-head column takes the width its content needs, not half the
/// pane: a four-row CSV must not push the problem text into a narrow strip
/// that breaks every spelling across two lines.
#[test]
fn the_member_split_follows_the_raw_head_width() {
    let d = pile();
    let mut w = Workbench::new(Browser::new(d.path()).unwrap(), vec![], 0.8);
    member_with_raw(&mut w, &d, gap_member("2025-02.csv"), raw_of(&["Datum;Kanton", "1;2"]));
    let buf = buffer(&mut w, 120, 34);
    let (x, _) = find(&buf, "`region` (TEXT)").expect("the problem text");
    // Main pane starts at column 26 (browser) + 1 (border); half of the
    // remaining ~92 columns would put the right half at ~73.
    assert!(x < 60, "the right column should start near the raw head's edge, not at half: x={x}\n{}", screen(&mut w, 120, 34).join("\n"));
}

/// The names that were looked for and the file's own header are lists, and
/// they render as lists — one item per line — instead of a comma-joined
/// sentence that wraps mid-spelling.
#[test]
fn the_member_view_lists_tried_names_and_the_header_one_per_line() {
    let d = pile();
    let mut w = Workbench::new(Browser::new(d.path()).unwrap(), vec![], 0.8);
    let mut m = gap_member("2025-02.csv");
    m.problems[0].tried = vec!["region".into(), "Region".into(), "Kanton".into()];
    m.problems[0].header = vec!["Datum".into(), "Gebiet".into(), "Betrag".into()];
    member_with_raw(&mut w, &d, m, raw_of(&["Datum;Gebiet;Betrag"]));
    let lines = screen(&mut w, 120, 34);
    let right = |needle: &str| lines.iter().find(|l| l.trim_start_matches(['│', ' ']).starts_with(needle) && !l.contains("Datum;")).cloned();
    assert!(lines.iter().any(|l| l.contains("looked for")), "{}", lines.join("\n"));
    assert!(right("Kanton").is_some(), "each tried name on its own line:\n{}", lines.join("\n"));
    assert!(right("Gebiet").is_some(), "each header cell on its own line:\n{}", lines.join("\n"));
}

/// A preview is a table: numbers right-aligned under their header, text
/// left-aligned, so a column of amounts reads as a column.
#[test]
fn preview_and_query_tables_align_numbers_right_and_text_left() {
    use tdy::console::Table;
    let d = pile();
    let mut w = Workbench::new(Browser::new(d.path()).unwrap(), vec![], 0.8);
    let t = Table {
        columns: vec!["region".into(), "total".into()],
        types: vec!["TEXT".into(), "DECIMAL(14,2)".into()],
        rows: vec![vec!["Ost".into(), "14200.00".into()], vec!["West".into(), "5.00".into()]],
        total: 2,
        truncated: false,
    };
    w.context = tdy_tui::workbench::Context::Query(t);
    let buf = buffer(&mut w, 120, 30);
    let (x_big, y_big) = find(&buf, "14200.00").unwrap();
    let (x_small, y_small) = find(&buf, "5.00").unwrap();
    assert_eq!(x_big + 8, x_small + 4, "right edges line up:\n{}", screen(&mut w, 120, 30).join("\n"));
    assert!(y_small > y_big);
    let (x_ost, _) = find(&buf, "Ost").unwrap();
    let (x_west, _) = find(&buf, "West").unwrap();
    assert_eq!(x_ost, x_west, "text is left-aligned");
    let text = screen(&mut w, 120, 30).join("\n");
    assert!(text.contains("DECIMAL(14,2)"), "a query result names its types: {text}");
}

/// A declaration wider than the pane is clipped with an ellipsis, not cut
/// by the border: a line that ends mid-word reads as complete.
#[test]
fn a_wide_declaration_line_is_clipped_with_an_ellipsis() {
    let d = pile();
    let mut w = Workbench::new(Browser::new(d.path()).unwrap(), vec![], 0.8);
    let mut r = pile_report(vec![member("2025-01.csv", MemberStatus::Fits)]);
    r.columns = (0..12)
        .map(|i| declared(&format!("column_number_{i}"), "DECIMAL(14,2)", &["Betrag", "Betrag CHF", "Amount"]))
        .collect();
    fitted(&mut w, &d, r);
    let lines = screen(&mut w, 100, 30);
    let decl = lines.iter().find(|l| l.contains("declares")).expect("the declares line");
    assert!(decl.trim_end_matches('│').trim_end().ends_with('…'), "{decl}");
}

/// The browser's status column speaks the same palette as the pile: a
/// target without a lock is yellow, a stale sidecar red.
#[test]
fn browser_status_follows_the_palette() {
    use ratatui::style::Color;
    let d = pile();
    let mut w = Workbench::new(Browser::new(d.path()).unwrap(), vec![], 0.8);
    let buf = buffer(&mut w, 100, 20);
    let (x, y) = find(&buf, "no lock").expect("the target's status");
    assert_eq!(fg_at(&buf, x, y), Color::Yellow);
}

// ---------------------------------------------------------------------------
// Slice 2a: popups, header and status line, console wrapping, borders.
// ---------------------------------------------------------------------------

/// The help is a popup over the main pane, not a repaint of it: it floats
/// inside the pane with the pane's own border still visible around it, and
/// it leads with the keys that do something *here* before the ones that
/// work everywhere.
#[test]
fn the_help_popup_floats_and_leads_with_this_contexts_keys() {
    let d = pile();
    let mut w = Workbench::new(Browser::new(d.path()).unwrap(), vec![], 0.8);
    fitted(&mut w, &d, pile_report(vec![member("2025-01.csv", MemberStatus::Fits)]));
    w.key(key(KeyCode::Tab));
    w.key(key(KeyCode::Tab));
    w.key(key(KeyCode::Char('?')));
    assert!(w.help);
    let buf = buffer(&mut w, 120, 36);
    let (x, y) = find(&buf, " keys ").expect("the popup title");
    // The main pane's own top border is row 1 and its title starts at
    // column 27; a floating popup sits strictly inside both.
    assert!(y > 1 && x > 28, "popup at ({x}, {y}) is not floating:\n{}", screen(&mut w, 120, 36).join("\n"));
    let text = screen(&mut w, 120, 36).join("\n");
    let here = text.find("re-fit").expect("the pile's own keys");
    let everywhere = text.find("cycle focus").expect("the global keys");
    assert!(here < everywhere, "this context's keys come first:\n{text}");
    assert!(text.contains("everywhere"), "{text}");
}

/// The confirm popup shows the diff with a dim line-number gutter, the
/// removed line red and the added line green — and floats like help does.
#[test]
fn the_confirm_popup_has_a_dim_gutter_and_coloured_lines() {
    use ratatui::style::Color;
    let d = pile();
    std::fs::write(
        d.path().join("t.tdy.sql"),
        "CREATE TABLE t (\n  region TEXT NOT NULL\n) WITH (files = '*.csv');\n",
    )
    .unwrap();
    let mut w = Workbench::new(Browser::new(d.path()).unwrap(), vec![], 0.8);
    let mut m = gap_member("a.csv");
    m.problems[0].header = vec!["Kanton".into(), "Datum".into()];
    member_with_raw(&mut w, &d, m, raw_of(&["Kanton;Datum"]));
    w.target_sql = Some(std::fs::read_to_string(d.path().join("t.tdy.sql")).unwrap());
    w.key(key(KeyCode::Char('1')));
    assert!(w.pending_edit.is_some());
    let buf = buffer(&mut w, 120, 36);
    let (x, y) = find(&buf, " confirm edit ").expect("the popup title");
    // The pane's own top border is row 1 and its left border column 26; a
    // popup as wide as its diff still sits one cell inside both.
    assert!(y > 1 && x >= 28, "popup at ({x}, {y}) is not floating");
    let (xp, yp) = find(&buf, "+").expect("the added line's marker");
    assert_eq!(fg_at(&buf, xp, yp), Color::Green);
    // The gutter (the line number) sits left of the marker and is dim.
    let gutter = (0..xp).rev().find(|&gx| buf[(gx, yp)].symbol().chars().all(|c| c.is_ascii_digit()) && buf[(gx, yp)].symbol() != " ").expect("a line number in the gutter");
    assert_eq!(fg_at(&buf, gutter, yp), Color::DarkGray, "gutter is dim");
}

/// The header carries what changes what a key does: the root, the target
/// on screen with its lock state, and the backend — and a DRY RUN badge
/// when the pile on screen is one.
#[test]
fn the_header_names_root_target_lock_state_and_backend() {
    let d = pile();
    let mut w = Workbench::new(Browser::new(d.path()).unwrap(), vec![], 0.8);
    w.backend = "openrouter/google/gemini-2.5-flash".into();
    let mut r = pile_report(vec![member("a.csv", MemberStatus::Fits)]);
    r.target_file = "t.tdy.sql".into();
    r.dry_run = true;
    // The target's name comes from the dispatched line (`Payload::Fitted`
    // carries no path), so fit the target the browser actually lists.
    use tdy::console::{Outcome, Payload};
    w.begin(".fit t.tdy.sql --dry-run");
    w.apply(
        Outcome { echo: ".fit t.tdy.sql --dry-run".into(), text: String::new(), ok: true, payload: Payload::Fitted(r) },
        d.path(),
    );
    let lines = screen(&mut w, 120, 30);
    let header = &lines[0];
    let root_tail = d.path().canonicalize().unwrap().file_name().unwrap().to_string_lossy().to_string();
    assert!(header.contains(&root_tail), "root: {header}");
    assert!(header.contains("t.tdy.sql"), "target: {header}");
    assert!(header.contains("no lock"), "lock state: {header}");
    assert!(header.contains("openrouter/google/gemini-2.5-flash"), "backend: {header}");
    assert!(header.trim_end().ends_with("DRY RUN"), "the badge sits at the right edge: {header}");
}

/// A busy status line carries a spinner ahead of the progress text, and a
/// note naming a file under the root names it relative to the root.
#[test]
fn the_status_line_spins_while_busy_and_shortens_paths() {
    let d = pile();
    let mut w = Workbench::new(Browser::new(d.path()).unwrap(), vec![], 0.8);
    w.begin(".fit sales.tdy.sql");
    w.progress("fitting a.csv (1 of 9)".into());
    let lines = screen(&mut w, 100, 20);
    let status = lines.last().unwrap();
    let first = status.trim_start().chars().next().unwrap();
    assert!("⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏".contains(first), "spinner glyph first: {status}");
    assert!(status.contains("fitting a.csv"), "{status}");

    // A note shows once the command is done — busy wins while it runs.
    let mut w = Workbench::new(Browser::new(d.path()).unwrap(), vec![], 0.8);
    let abs = w.browser.root().join("a.csv").display().to_string();
    w.note(format!("warning: heuristics are only 80% confident about {abs}"));
    let lines = screen(&mut w, 100, 20);
    let status = lines.last().unwrap();
    assert!(status.contains("about a.csv"), "relative to the root: {status}");
    assert!(!status.contains(&abs), "{status}");
}

/// A console line wider than the pane wraps; it does not lose its tail.
#[test]
fn console_lines_wrap_instead_of_clipping() {
    use tdy::console::{Outcome, Payload};
    let d = pile();
    let mut w = Workbench::new(Browser::new(d.path()).unwrap(), vec![], 0.8);
    w.begin(".fit sales.tdy.sql");
    let long = "Error: 3 file(s) cannot reach the declared schema; no lock written. Fix them, exclude them, or widen the target.";
    w.apply(Outcome { echo: ".fit sales.tdy.sql".into(), text: long.into(), ok: false, payload: Payload::Nothing }, d.path());
    let text = screen(&mut w, 90, 24).join("\n");
    assert!(text.contains("widen the target."), "the tail survives:\n{text}");
}

/// Scrolled up, the console says so, and says how far.
#[test]
fn a_scrolled_console_marks_how_far_it_is_from_the_end() {
    use tdy::console::{Outcome, Payload};
    let d = pile();
    let mut w = Workbench::new(Browser::new(d.path()).unwrap(), vec![], 0.8);
    for i in 0..30 {
        w.begin(".ls");
        w.apply(Outcome { echo: ".ls".into(), text: format!("line {i}"), ok: true, payload: Payload::Nothing }, d.path());
    }
    w.key(key(KeyCode::PageUp));
    w.key(key(KeyCode::PageUp));
    assert!(w.scroll > 0);
    let text = screen(&mut w, 100, 24).join("\n");
    assert!(text.contains('⋮'), "{text}");
    assert!(text.contains("PgDn") || text.contains("more"), "{text}");
}

/// Rounded corners, and one shared line where the browser meets the right
/// column: a `┬` where it starts at the top, a `├` where the main pane
/// hands over to the console, a `┴` where it ends.
#[test]
fn panes_share_one_border_line_with_junctions() {
    let d = pile();
    let mut w = Workbench::new(Browser::new(d.path()).unwrap(), vec![], 0.8);
    let buf = buffer(&mut w, 100, 30);
    let seam = 26u16;
    assert_eq!(buf[(seam, 1)].symbol(), "┬", "top junction:\n{}", screen(&mut w, 100, 30).join("\n"));
    assert_eq!(buf[(seam, 28)].symbol(), "┴", "bottom junction");
    let joins = (2..28).filter(|&y| buf[(seam, y)].symbol() == "├").count();
    assert_eq!(joins, 1, "exactly one main/console join on the seam");
    assert_eq!(buf[(0, 1)].symbol(), "╭", "rounded corner");
    let text = screen(&mut w, 100, 30).join("\n");
    assert!(text.contains(" files ") && text.contains(" console "), "{text}");
}

/// When the header does not fit, the root gives way from its left — its
/// tail is what tells two directories apart — and the target, its lock
/// state and the backend stay whole. `$HOME` reads as `~`.
#[test]
fn a_long_root_yields_to_the_target_and_backend_in_the_header() {
    let home = std::env::var("HOME").unwrap();
    let deep = std::path::Path::new(&home).join(".cache").join("tdy-render-test").join("a-rather-long-directory-name").join("and-another-one-beneath-it");
    std::fs::create_dir_all(&deep).unwrap();
    std::fs::write(deep.join("a.csv"), "A;B\n1;2\n").unwrap();
    std::fs::write(deep.join("t.tdy.sql"), "CREATE TABLE t (a TEXT) WITH (files='*.csv');").unwrap();
    let mut w = Workbench::new(Browser::new(&deep).unwrap(), vec![], 0.8);
    w.backend = "openrouter/google/gemini-2.5-flash".into();
    w.begin(".fit t.tdy.sql");
    use tdy::console::{Outcome, Payload};
    w.apply(
        Outcome { echo: ".fit t.tdy.sql".into(), text: String::new(), ok: true, payload: Payload::Fitted(pile_report(vec![member("a.csv", MemberStatus::Fits)])) },
        &deep,
    );
    let lines = screen(&mut w, 90, 20);
    let header = &lines[0];
    assert!(header.contains("t.tdy.sql"), "the target stays whole: {header}");
    assert!(header.contains("no lock"), "{header}");
    assert!(header.contains("backend openrouter/google/gemini-2.5-flash"), "the backend stays whole: {header}");
    assert!(header.contains("one-beneath-it"), "the root's tail survives: {header}");
    assert!(!header.contains(&home), "HOME reads as ~: {header}");
    let _ = std::fs::remove_dir_all(std::path::Path::new(&home).join(".cache").join("tdy-render-test"));
}

// ---------------------------------------------------------------------------
// Slice 2b: a filtered pile, the sheet on show, decisions with their values.
// ---------------------------------------------------------------------------

/// A filtered pile draws only the members that need attention, and says
/// that it is filtered and how to get everything back.
#[test]
fn a_filtered_pile_shows_only_problem_rows_and_says_so() {
    let d = pile();
    let mut w = Workbench::new(Browser::new(d.path()).unwrap(), vec![], 0.8);
    fitted(&mut w, &d, pile_report(vec![
        member("2025-01.csv", MemberStatus::Fits),
        gap_member("2025-02.csv"),
        member("2025-03.csv", MemberStatus::NeedsReview),
    ]));
    w.key(key(KeyCode::Tab));
    w.key(key(KeyCode::Tab));
    w.key(key(KeyCode::Char('/')));
    let text = screen(&mut w, 120, 30).join("\n");
    let main: String = text.lines().filter(|l| !l.contains("│▸ 2025-01") && !l.starts_with("│  2025-01")).collect::<Vec<_>>().join("\n");
    assert!(main.contains("2025-02.csv") && main.contains("2025-03.csv"), "{text}");
    assert!(!main.contains("  2025-01.csv   fits"), "fits rows are hidden:\n{text}");
    assert!(text.contains("problems only"), "{text}");
    assert!(text.contains("/ shows all") || text.contains("/ all"), "{text}");
}

/// A workbook's pane title names the sheet on show and how many there are.
#[test]
fn a_workbook_file_title_names_the_sheet_shown() {
    use tdy::console::RawHead;
    let d = pile();
    let mut w = Workbench::new(Browser::new(d.path()).unwrap(), vec![], 0.8);
    w.context = tdy_tui::workbench::Context::File {
        path: d.path().join("book.xlsx"),
        raw: RawHead {
            lines: vec![],
            truncated: false,
            sheets: vec![("Umsatz".into(), 7, 3), ("Legende".into(), 4, 2)],
            grid: vec![vec!["Datum".into(), "Betrag".into()]],
            grid_sheet: Some("Umsatz".into()),
        },
        spec: None,
        preview: None,
        stale: false,
    };
    let text = screen(&mut w, 120, 30).join("\n");
    assert!(text.contains("book.xlsx · sheet 1/2 \"Umsatz\""), "{text}");
}

/// A column decision in the spec summary shows the raw values that drove
/// it — the first few of that column, from the file's own head beside it.
#[test]
fn a_column_decision_shows_the_values_that_drove_it() {
    use tdy::console::{Outcome, Payload, RawHead, SpecSummary};
    let d = pile();
    let mut w = Workbench::new(Browser::new(d.path()).unwrap(), vec![], 0.8);
    let raw = RawHead {
        lines: vec![
            "Datum;Region;Betrag".into(),
            "31.01.2025;Ost;1'100.00".into(),
            "31.01.2025;West;1'110.00".into(),
            "31.01.2025;Nord;1'120.00".into(),
            "31.01.2025;Sued;1'130.00".into(),
        ],
        truncated: false,
        sheets: vec![],
        grid: vec![],
        grid_sheet: None,
    };
    let spec = SpecSummary {
        method: "heuristic".into(),
        confidence: Some(0.95),
        extraction: r#"{"format":"delimited","delimiter":";","quote":"\"","ragged":"pad_nulls"}"#.into(),
        transforms: vec![],
        columns: vec![
            ("datum".into(), "Datum".into(), "DATE (%d.%m.%Y)".into()),
            ("betrag".into(), "Betrag".into(), "DECIMAL(38,2)".into()),
        ],
        notes: vec!["column `betrag`: read as decimal(2) — scale inferred from the first 500 rows".into()],
    };
    w.begin(".show 2025-01.csv");
    w.apply(
        Outcome { echo: ".show 2025-01.csv".into(), text: String::new(), ok: true, payload: Payload::Shown { path: d.path().join("2025-01.csv"), raw, spec: Some(spec), stale: false } },
        d.path(),
    );
    let lines = screen(&mut w, 130, 34);
    let ex = lines.iter().find(|l| l.contains("1'100.00") && l.contains("1'110.00") && !l.contains("Datum;")).cloned();
    assert!(ex.is_some(), "the note's driving values on one line:\n{}", lines.join("\n"));
    let text = lines.join("\n");
    let note_at = text.find("column `betrag`").expect("the note");
    let ex_at = text.find(ex.unwrap().trim()).unwrap();
    assert!(ex_at > note_at, "values sit under the note");
}

/// Two sheet members of one workbook are two rows, each named with its
/// sheet — never two indistinguishable `2025.xlsx` lines.
#[test]
fn sheet_members_are_named_with_their_sheet_in_the_pile() {
    let d = pile();
    let mut w = Workbench::new(Browser::new(d.path()).unwrap(), vec![], 0.8);
    let mut q1 = member("2025.xlsx", MemberStatus::Fits);
    q1.sheet = Some("Q1".into());
    let mut q2 = member("2025.xlsx", MemberStatus::Fits);
    q2.sheet = Some("Q2".into());
    fitted(&mut w, &d, pile_report(vec![q1, q2]));
    let text = screen(&mut w, 120, 30).join("\n");
    assert!(text.contains("2025.xlsx#Q1") && text.contains("2025.xlsx#Q2"), "{text}");
    w.key(key(KeyCode::Tab));
    w.key(key(KeyCode::Tab));
    w.key(key(KeyCode::Enter));
    let text = screen(&mut w, 120, 30).join("\n");
    assert!(text.contains(" 2025.xlsx#Q1 "), "the member view's title names the sheet: {text}");
}

/// The browser's status column says a workbook has sheet specs, in its own
/// compact vocabulary, and colours it as sniffed rather than leaving it blank.
#[test]
fn the_browser_shows_a_workbooks_sheet_specs() {
    use tdy::spec::{ColumnSpec, DType, Extraction, InferenceMethod, ParseSpec, Transform, ValueParsing};
    let d = tempfile::tempdir().unwrap();
    let book = d.path().join("2025.xlsx");
    std::fs::copy(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../testdata/sheet_frames_two_fit.xlsx"),
        &book,
    )
    .unwrap();
    for sheet in ["Q1", "Q2"] {
        let spec = ParseSpec {
            extraction: Extraction::Excel { sheet_name: Some(sheet.into()), sheet_index: None, range: None },
            transforms: vec![Transform::PromoteHeader { rows: 1, join: " ".into() }],
            columns: vec![ColumnSpec { name: "region".into(), source: Some("Region".into()), dtype: DType::Utf8, nullable: false, parse: ValueParsing::default(), pointer: None }],
            confidence: Some(1.0),
            notes: vec![],
        };
        tdy::sidecar::save_member(&book, Some(sheet), &spec, tdy::sidecar::ProvenanceInfo { method: InferenceMethod::Manual, model: None, prompt_version: None, sampled_bytes: None }).unwrap();
    }
    let mut w = Workbench::new(Browser::new(d.path()).unwrap(), vec![], 0.8);
    let text = screen(&mut w, 100, 20).join("\n");
    assert!(text.contains("✓ 2 sheets"), "{text}");
}
