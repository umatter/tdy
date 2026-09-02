use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use tdy::console::{Outcome, Payload, RawHead, Table};
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
