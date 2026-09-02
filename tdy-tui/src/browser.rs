use std::path::{Path, PathBuf};
use anyhow::Result;
use tdy::console::{list_dir, Entry};

/// A tree-navigation state over a directory hierarchy, rooted at a canonical path.
/// The selection is always clamped to the current entries; navigation is confined to the root.
pub struct Browser {
    root: PathBuf,
    /// Current directory, always under root.
    pub dir: PathBuf,
    /// The entries in the current directory (dirs first, then files/targets, companions hidden).
    pub entries: Vec<Entry>,
    /// Index into entries, or 0 if empty.
    pub selected: usize,
    /// If list_dir failed, an error message; entries is empty.
    pub error: Option<String>,
}

impl Browser {
    /// Create a new Browser rooted at the given path (canonicalized).
    /// Fails if the path cannot be canonicalized or list_dir fails.
    pub fn new(root: &Path) -> Result<Self> {
        let root = std::fs::canonicalize(root)?;
        let mut browser = Browser {
            root: root.clone(),
            dir: root,
            entries: Vec::new(),
            selected: 0,
            error: None,
        };
        browser.refresh();
        Ok(browser)
    }

    /// Re-list the current directory.
    /// On error, set self.error and clear entries.
    /// Clamp selected to the number of entries.
    pub fn refresh(&mut self) {
        match list_dir(&self.dir) {
            Ok(entries) => {
                self.entries = entries;
                self.error = None;
                self.selected = self.selected.min(self.entries.len().saturating_sub(1));
            }
            Err(e) => {
                self.error = Some(format!("{e:#}"));
                self.entries.clear();
                self.selected = 0;
            }
        }
    }

    /// Navigate up one directory, unless already at root.
    /// Returns true if moved, false if already at root.
    /// Selection is clamped by refresh(), not reset.
    pub fn up(&mut self) -> bool {
        if self.dir == self.root {
            return false;
        }
        self.dir.pop();
        self.refresh();
        true
    }

    /// Enter the selected directory (if it is a directory), or return the path to the selected file/target.
    /// For directories: descend, refresh, reset selection, return None.
    /// For files/targets: return absolute path.
    pub fn enter(&mut self) -> Option<PathBuf> {
        let is_dir = self.selected_entry()?.kind == tdy::console::EntryKind::Dir;
        let entry_name = self.selected_entry()?.name.clone();

        if is_dir {
            let dirname = entry_name.strip_suffix('/').unwrap_or(&entry_name);
            self.dir.push(dirname);
            self.refresh();
            self.selected = 0;
            None
        } else {
            self.selected_path()
        }
    }

    /// Move selection up or down by delta, clamping to [0, entries.len()).
    pub fn move_sel(&mut self, delta: i32) {
        let current = self.selected as i32;
        let max = self.entries.len().max(1) as i32;
        self.selected = ((current + delta).max(0).min(max - 1)) as usize;
    }

    /// Return the currently selected entry, or None if no entries.
    pub fn selected_entry(&self) -> Option<&Entry> {
        self.entries.get(self.selected)
    }

    /// Return the absolute path of the selected entry, or None if no entry selected or error.
    pub fn selected_path(&self) -> Option<PathBuf> {
        self.selected_entry().map(|e| {
            let name = e.name.strip_suffix('/').unwrap_or(&e.name);
            self.dir.join(name)
        })
    }

    /// Return the selection as a relative path to the current directory,
    /// suitable for console display (with any trailing `/` stripped).
    pub fn selected_rel(&self) -> Option<String> {
        self.selected_entry().map(|e| {
            e.name.strip_suffix('/').unwrap_or(&e.name).to_string()
        })
    }

    /// Return a title for the pane: root-relative path, or "." if at root.
    pub fn title(&self) -> String {
        match self.dir.strip_prefix(&self.root) {
            Ok(rel) if rel.as_os_str().is_empty() => ".".to_string(),
            Ok(rel) => rel.display().to_string(),
            Err(_) => ".".to_string(), // Should never happen if dir is under root
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pile() -> tempfile::TempDir {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join("b.csv"), "A;B\n1;2\n").unwrap();
        std::fs::write(d.path().join("a.csv"), "A;B\n1;2\n").unwrap();
        std::fs::write(d.path().join("t.tdy.sql"), "CREATE TABLE t (a TEXT) WITH (files='*.csv');").unwrap();
        std::fs::write(d.path().join("a.csv.tdy.toml"), "junk").unwrap(); // companion: never an entry
        std::fs::create_dir(d.path().join("sub")).unwrap();
        std::fs::write(d.path().join("sub/c.csv"), "A\n1\n").unwrap();
        std::fs::write(d.path().join("sub/d.csv"), "A\n2\n").unwrap();
        d
    }

    #[test]
    fn lists_dirs_first_companions_hidden_and_navigates() {
        let d = pile();
        let mut b = Browser::new(d.path()).unwrap();
        let names: Vec<&str> = b.entries.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, ["sub/", "a.csv", "b.csv", "t.tdy.sql"]);
        assert_eq!(b.title(), ".");

        // Enter the directory; selection resets; up() returns to root.
        assert_eq!(b.enter(), None);
        assert_eq!(b.title(), "sub");
        assert_eq!(b.entries.len(), 2);
        assert!(b.up());
        assert!(!b.up(), "cannot go above the root");

        // Enter on a file returns its absolute path.
        b.move_sel(1);
        assert_eq!(b.selected_rel().as_deref(), Some("a.csv"));
        let p = b.enter().unwrap();
        assert!(p.ends_with("a.csv") && p.is_absolute());
    }

    #[test]
    fn selection_clamps_and_survives_refresh() {
        let d = pile();
        let mut b = Browser::new(d.path()).unwrap();
        b.move_sel(100);
        assert_eq!(b.selected, b.entries.len() - 1);
        b.move_sel(-100);
        assert_eq!(b.selected, 0);
        std::fs::remove_file(d.path().join("t.tdy.sql")).unwrap();
        b.move_sel(100);
        b.refresh();
        assert!(b.selected < b.entries.len());
    }

    #[test]
    fn selected_rel_is_relative_to_the_current_dir() {
        let d = pile();
        let mut b = Browser::new(d.path()).unwrap();
        b.enter(); // into sub/
        assert_eq!(b.selected_rel().as_deref(), Some("c.csv"));
    }

    #[test]
    fn up_clamps_selection_without_resetting() {
        let d = pile();
        let mut b = Browser::new(d.path()).unwrap();
        // Root has 4 entries: ["sub/", "a.csv", "b.csv", "t.tdy.sql"]
        assert_eq!(b.entries.len(), 4);
        // Enter sub/ at index 0 (resets selection to 0)
        assert_eq!(b.enter(), None);
        assert_eq!(b.selected, 0);
        // sub/ now has 2 entries: ["c.csv", "d.csv"]
        assert_eq!(b.entries.len(), 2);
        // Move selection to 1 within sub/
        b.move_sel(1);
        assert_eq!(b.selected, 1);
        // Go back up to root (4 entries)
        assert!(b.up());
        assert_eq!(b.entries.len(), 4);
        // Selection should be clamped but not reset to 0; it was 1, still valid
        assert_eq!(b.selected, 1);
    }
}
