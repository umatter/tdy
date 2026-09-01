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

    fn browse(&mut self, dir: i32) {
        if self.history.is_empty() {
            return;
        }
        let next = match (self.pos, dir) {
            (None, -1) => {
                self.stash = self.buf.clone();
                Some(self.history.len() - 1)
            }
            (None, _) => None,
            (Some(0), -1) => Some(0),
            (Some(i), -1) => Some(i - 1),
            (Some(i), _) if i + 1 >= self.history.len() => None,
            (Some(i), _) => Some(i + 1),
        };
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
        type_str(&mut ed, "draft");
        ed.key(k(KeyCode::Up));
        assert_eq!(ed.text().as_str(), "second");
        ed.key(k(KeyCode::Up));
        assert_eq!(ed.text().as_str(), "first");
        ed.key(k(KeyCode::Up)); // past the oldest: stays
        assert_eq!(ed.text().as_str(), "first");
        ed.key(k(KeyCode::Down));
        ed.key(k(KeyCode::Down));
        assert_eq!(ed.text().as_str(), "draft"); // the draft comes back
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
