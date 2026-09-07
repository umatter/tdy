//! The prompt's line editor, as a state machine: a key in, an [`Edit`] out.
//! No terminal in here, so every behaviour is a unit test. Deliberately
//! small — insert, delete, move, history — because history recall is the
//! feature that matters and a readline crate is a dependency tree.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

pub enum Edit {
    /// Redraw the line: (text, cursor position in chars).
    Redraw,
    /// Enter: the line is complete.
    Submit(String),
    /// Ctrl-C on a non-empty line: cleared. On an empty line: Interrupt.
    Cleared,
    Interrupt,
    /// Ctrl-D on an empty line.
    Eof,
    Nothing,
}

pub struct LineEditor {
    buf: Vec<char>,
    cursor: usize,
    history: Vec<String>,
    /// Index into history while browsing; None = editing the draft.
    pos: Option<usize>,
    /// The draft, stashed while browsing history.
    stash: Vec<char>,
}

impl LineEditor {
    pub fn new(history: Vec<String>) -> LineEditor {
        LineEditor { buf: vec![], cursor: 0, history, pos: None, stash: vec![] }
    }

    pub fn text(&self) -> String {
        self.buf.iter().collect()
    }

    pub fn cursor(&self) -> usize {
        self.cursor
    }

    pub fn history(&self) -> &[String] {
        &self.history
    }

    /// Record a submitted line (skips empty and consecutive duplicates).
    pub fn remember(&mut self, line: &str) {
        if line.trim().is_empty() || self.history.last().map(String::as_str) == Some(line) {
            return;
        }
        self.history.push(line.to_string());
    }

    pub fn key(&mut self, k: KeyEvent) -> Edit {
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        match (k.code, ctrl) {
            (KeyCode::Char('c'), true) => {
                if self.buf.is_empty() {
                    return Edit::Interrupt;
                }
                self.reset();
                Edit::Cleared
            }
            (KeyCode::Char('d'), true) => {
                if self.buf.is_empty() {
                    Edit::Eof
                } else {
                    Edit::Nothing
                }
            }
            (KeyCode::Char('u'), true) => {
                self.reset();
                Edit::Redraw
            }
            (KeyCode::Char('a'), true) | (KeyCode::Home, _) => {
                self.cursor = 0;
                Edit::Redraw
            }
            (KeyCode::Char('e'), true) | (KeyCode::End, _) => {
                self.cursor = self.buf.len();
                Edit::Redraw
            }
            (KeyCode::Char(c), false) => {
                self.buf.insert(self.cursor, c);
                self.cursor += 1;
                Edit::Redraw
            }
            (KeyCode::Backspace, _) => {
                if self.cursor > 0 {
                    self.cursor -= 1;
                    self.buf.remove(self.cursor);
                }
                Edit::Redraw
            }
            (KeyCode::Delete, _) => {
                if self.cursor < self.buf.len() {
                    self.buf.remove(self.cursor);
                }
                Edit::Redraw
            }
            (KeyCode::Left, _) => {
                self.cursor = self.cursor.saturating_sub(1);
                Edit::Redraw
            }
            (KeyCode::Right, _) => {
                self.cursor = (self.cursor + 1).min(self.buf.len());
                Edit::Redraw
            }
            (KeyCode::Up, _) => {
                self.browse(-1);
                Edit::Redraw
            }
            (KeyCode::Down, _) => {
                self.browse(1);
                Edit::Redraw
            }
            (KeyCode::Enter, _) => {
                let line: String = self.buf.iter().collect();
                self.reset();
                Edit::Submit(line)
            }
            _ => Edit::Nothing,
        }
    }

    fn reset(&mut self) {
        self.buf.clear();
        self.cursor = 0;
        self.pos = None;
        self.stash.clear();
    }

    /// Walk the history by one step. With something typed, only the lines
    /// that start with it are visited (fish's and zsh's Up), so `.sn` then
    /// Up finds the last `.sniff` past every `.fit`; a prefix nothing starts
    /// with leaves the draft alone. `pos` indexes the full history either
    /// way — the filter decides which indices are stops.
    fn browse(&mut self, dir: i32) {
        if self.history.is_empty() {
            return;
        }
        if self.pos.is_none() && dir < 0 {
            self.stash = self.buf.clone();
        }
        let prefix: String = self.stash.iter().collect();
        let stops = |i: usize| self.history[i].starts_with(&prefix);
        let next = match (self.pos, dir) {
            (None, -1) => (0..self.history.len()).rev().find(|&i| stops(i)),
            (None, _) => None,
            (Some(i), -1) => (0..i).rev().find(|&j| stops(j)).or(Some(i)),
            (Some(i), _) => (i + 1..self.history.len()).find(|&j| stops(j)),
        };
        // A first Up that finds nothing is not a browse: the draft stays
        // and the stash is dropped so a later Up starts afresh.
        if self.pos.is_none() && next.is_none() {
            self.stash.clear();
            return;
        }
        self.pos = next;
        self.buf = match next {
            Some(i) => self.history[i].chars().collect(),
            None => self.stash.clone(),
        };
        self.cursor = self.buf.len();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    fn k(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn ctrl(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL)
    }

    fn type_str(ed: &mut LineEditor, s: &str) {
        for c in s.chars() {
            ed.key(k(KeyCode::Char(c)));
        }
    }

    #[test]
    fn typing_editing_and_submit() {
        let mut ed = LineEditor::new(vec![]);
        type_str(&mut ed, ".sniff a.csv");
        assert_eq!((ed.text().as_str(), ed.cursor()), (".sniff a.csv", 12));
        ed.key(k(KeyCode::Left));
        ed.key(k(KeyCode::Left));
        ed.key(k(KeyCode::Backspace));
        assert_eq!(ed.text().as_str(), ".sniff a.sv");
        ed.key(k(KeyCode::Home));
        ed.key(k(KeyCode::Delete));
        assert_eq!(ed.text().as_str(), "sniff a.sv");
        ed.key(k(KeyCode::End));
        type_str(&mut ed, "!");
        assert!(matches!(ed.key(k(KeyCode::Enter)), Edit::Submit(s) if s == "sniff a.sv!"));
        assert_eq!(ed.text().as_str(), "");
    }

    #[test]
    fn history_recall_keeps_the_draft() {
        let mut ed = LineEditor::new(vec!["first".into(), "second".into()]);
        ed.key(k(KeyCode::Up));
        assert_eq!(ed.text().as_str(), "second");
        ed.key(k(KeyCode::Up));
        assert_eq!(ed.text().as_str(), "first");
        ed.key(k(KeyCode::Up)); // past the oldest: stays
        assert_eq!(ed.text().as_str(), "first");
        ed.key(k(KeyCode::Down));
        ed.key(k(KeyCode::Down));
        assert_eq!(ed.text().as_str(), ""); // the (empty) draft comes back
    }

    /// Up with something typed recalls only the lines that start with it,
    /// the way fish and zsh do: `.sn` then Up finds the last `.sniff`,
    /// skipping every `.fit` in between, and a prefix nothing starts with
    /// leaves the draft alone rather than replacing it with an unrelated
    /// line. Down walks back the same way and ends on the draft.
    #[test]
    fn up_with_a_draft_recalls_by_prefix() {
        let mut ed = LineEditor::new(vec![
            ".sniff a.csv".into(),
            ".fit t.tdy.sql".into(),
            ".sniff b.csv".into(),
            ".fit t.tdy.sql --dry-run".into(),
        ]);
        type_str(&mut ed, ".sn");
        ed.key(k(KeyCode::Up));
        assert_eq!(ed.text().as_str(), ".sniff b.csv");
        ed.key(k(KeyCode::Up));
        assert_eq!(ed.text().as_str(), ".sniff a.csv");
        ed.key(k(KeyCode::Up)); // no older `.sn…`: stays
        assert_eq!(ed.text().as_str(), ".sniff a.csv");
        ed.key(k(KeyCode::Down));
        assert_eq!(ed.text().as_str(), ".sniff b.csv");
        ed.key(k(KeyCode::Down));
        assert_eq!(ed.text().as_str(), ".sn"); // the draft comes back
        assert_eq!(ed.cursor(), 3);

        let mut ed = LineEditor::new(vec![".ls".into()]);
        type_str(&mut ed, "SELECT");
        ed.key(k(KeyCode::Up));
        assert_eq!(ed.text().as_str(), "SELECT", "nothing starts with it: the draft stays");
    }

    #[test]
    fn remember_skips_empty_and_duplicates() {
        let mut ed = LineEditor::new(vec![]);
        ed.remember(".ls");
        ed.remember(".ls");
        ed.remember("");
        ed.remember(".help");
        assert_eq!(ed.history(), [".ls", ".help"]);
    }

    #[test]
    fn control_keys() {
        let mut ed = LineEditor::new(vec![]);
        assert!(matches!(ed.key(ctrl('d')), Edit::Eof));
        assert!(matches!(ed.key(ctrl('c')), Edit::Interrupt));
        type_str(&mut ed, "abc");
        assert!(matches!(ed.key(ctrl('c')), Edit::Cleared));
        assert_eq!(ed.text().as_str(), "");
        type_str(&mut ed, "abc");
        assert!(matches!(ed.key(ctrl('d')), Edit::Nothing)); // not EOF mid-line
        assert!(matches!(ed.key(ctrl('u')), Edit::Redraw));
        assert_eq!(ed.text().as_str(), "");
    }
}
