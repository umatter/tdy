//! What the workbench frame actually shows, rendered into a headless buffer.
//!
//! Mirrors `tests/render.rs`'s approach: draw into a `TestBackend`, read the
//! text back, assert on what a person would see.

use ratatui::backend::TestBackend;
use ratatui::Terminal;
use tdy_tui::browser::Browser;
use tdy_tui::wb_ui;
use tdy_tui::workbench::Workbench;

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

#[test]
fn the_frame_shows_three_panes_and_the_status_vocabulary() {
    let d = pile();
    let mut w = Workbench::new(Browser::new(d.path()).unwrap(), vec![]);
    let text = screen(&mut w, 100, 30).join("\n");
    assert!(text.contains(" files "), "{text}");
    assert!(text.contains(" console "), "{text}");
    assert!(text.contains("a.csv"), "{text}");
    assert!(text.contains("target, no lock"), "{text}");
    assert!(text.contains("tdy>"), "{text}");
    assert!(text.contains("select a file"), "{text}");
}

#[test]
fn narrow_terminals_drop_the_browser_not_the_console() {
    let d = pile();
    let mut w = Workbench::new(Browser::new(d.path()).unwrap(), vec![]);
    let text = screen(&mut w, 50, 20).join("\n");
    assert!(!text.contains(" files "), "{text}");
    assert!(text.contains("tdy>"), "{text}");
}

#[test]
fn scrollback_shows_echo_then_text_and_busy_shows_in_status() {
    let d = pile();
    let mut w = Workbench::new(Browser::new(d.path()).unwrap(), vec![]);
    w.begin(".ls");
    let text = screen(&mut w, 100, 30).join("\n");
    assert!(text.contains(".ls"), "{text}");
    use tdy::console::{Outcome, Payload};
    w.apply(Outcome { echo: ".ls".into(), text: "a.csv  sniffed\n".into(), payload: Payload::Nothing, ok: true });
    w.progress("fitting a.csv (1 of 9)".into());
    let text = screen(&mut w, 100, 30).join("\n");
    assert!(text.contains("tdy> .ls"), "{text}");
    assert!(text.contains("a.csv  sniffed"), "{text}");
    assert!(text.contains("fitting a.csv (1 of 9)"), "{text}");
}
