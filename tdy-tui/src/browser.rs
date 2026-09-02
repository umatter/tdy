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

    /// Re-root the listing on `dir` — the *session's* working directory.
    ///
    /// The session, not the browser, is the source of truth for where we
    /// are: `.cd` is ordinary typed grammar, so the session can move
    /// without the browser ever being asked, and a browser descent whose
    /// `.cd` the session refused (a symlink out of the root, which
    /// `list_dir` follows and `confine` does not) must roll back. Both are
    /// the same bug — a shortcut synthesizing `.sniff jan.csv` for the
    /// highlighted file while the session resolves that name somewhere
    /// else — and this heals both directions.
    ///
    /// A dir outside the root is refused and the browser keeps what it has:
    /// the browser is confined to its root, and leaving it would be a worse
    /// answer than a stale one. Returns true if the directory moved.
    pub fn sync_dir(&mut self, dir: &Path) -> bool {
        // The session's cwd is canonical (`Session::new` and `.cd` both
        // canonicalise); canonicalise here too so a caller handing over a
        // symlinked-but-equivalent path is not read as a move.
        let dir = std::fs::canonicalize(dir).unwrap_or_else(|_| dir.to_path_buf());
        if dir == self.dir || !dir.starts_with(&self.root) {
            return false;
        }
        self.dir = dir;
        // A different directory means a different list: start at the top,
        // exactly as `enter()` does, rather than carrying an index that
        // pointed at some other file.
        self.selected = 0;
        self.refresh();
        true
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

    /// The canonical root the browser is confined to (the Empty context's
    /// orientation line names it, since `title()` only ever gives a
    /// root-relative path).
    pub fn root(&self) -> &Path {
        &self.root
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
    fn sync_dir_follows_the_session_under_the_root_and_refuses_to_leave_it() {
        let d = pile();
        let mut b = Browser::new(d.path()).unwrap();
        let root = b.root().to_path_buf();

        // The session `.cd`d into sub/ without the browser being asked
        // (a typed `.cd`): the browser follows.
        assert!(b.sync_dir(&root.join("sub")));
        assert_eq!(b.title(), "sub");
        let names: Vec<&str> = b.entries.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, ["c.csv", "d.csv"], "the listing is refreshed, not stale");
        assert_eq!(b.selected, 0);

        // The same dir again is not a move.
        assert!(!b.sync_dir(&root.join("sub")));

        // Anything outside the root is refused, and the browser keeps what
        // it has rather than leaving its root.
        let outside = tempfile::tempdir().unwrap();
        assert!(!b.sync_dir(outside.path()));
        assert_eq!(b.title(), "sub");
        assert!(b.dir.starts_with(&root));

        // Back to the root (a `.cd ..`, or a refused descent rolling back).
        assert!(b.sync_dir(&root));
        assert_eq!(b.title(), ".");
        assert_eq!(b.entries.len(), 4);
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
