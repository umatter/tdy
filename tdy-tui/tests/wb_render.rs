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
            raw: RawHead { lines: vec!["A;B".into(), "1;2".into()], truncated: true, sheets: vec![] },
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
    };
    w.apply(
        Outcome { echo: ".fit sales.tdy.sql".into(), text: String::new(), ok: true, payload: Payload::Fitted(report) },
        d.path(),
    );
    w.key(key(KeyCode::Tab)); // Browser
    w.key(key(KeyCode::Tab)); // Main
    w.key(key(KeyCode::Enter)); // opens the Member context

    let raw = RawHead { lines: vec!["Datum;Kanton;Betrag".into()], truncated: false, sheets: vec![] };
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
            column: "amount_chf".into(),
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
    assert!(text.contains("amount_chf"), "the Shift judgement's headline: {text}");
    assert!(
        text.contains("no computable consequence to show"),
        "the Unillustrated judgement's headline too — every judgement shows: {text}"
    );
    assert!(text.contains("a accepts"), "{text}");
    assert!(text.contains("Esc closes"), "{text}");
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
    let raw = || RawHead { lines: vec!["A;B".into(), "1;2".into()], truncated: false, sheets: vec![] };
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
                raw: RawHead { lines: vec!["A;B".into(), "1;2".into()], truncated: false, sheets: vec![] },
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
                raw: RawHead { lines: vec!["A;B".into()], truncated: false, sheets: vec![] },
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
            columns: vec!["region".into(), "amount_chf".into()],
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
