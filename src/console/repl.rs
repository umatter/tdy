//! The plain console: `tdy>` on a TTY, a batch runner on a pipe.

use std::io::{BufRead, IsTerminal, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use crossterm::event::{self, Event};
use crossterm::terminal;

use super::line::{Edit, LineEditor};
use super::{Payload, Session};

const HISTORY_LIMIT: usize = 1000;

pub fn history_path() -> Option<PathBuf> {
    dirs::data_dir().map(|d| d.join("tdy").join("history"))
}

/// The last `limit` lines of the history file, oldest first. Any failure to
/// read (no file yet, permissions, ...) is silently treated as "no history"
/// — a console must not die over its history file. A `\n` sequence left
/// over from an older tdy that escaped newlines instead of collapsing them
/// (see `append_history`'s doc comment) is treated the same way a real
/// embedded newline now is: folded to a space, so an old history file does
/// not resurrect the garbled-redraw bug this replaced.
pub fn load_history(limit: usize) -> Vec<String> {
    let Some(p) = history_path() else { return vec![] };
    let Ok(text) = std::fs::read_to_string(p) else { return vec![] };
    let lines: Vec<String> = text.lines().map(|l| l.replace("\\n", " ")).collect();
    let skip = lines.len().saturating_sub(limit);
    lines.into_iter().skip(skip).collect()
}

/// Append one remembered line to the history file, creating its directory
/// if needed. Failures are silently ignored, for the same reason
/// `load_history` ignores them.
///
/// A multi-line SQL statement is collapsed to one line first
/// (`one_line_for_history`): SQL is whitespace-insensitive, so the
/// recalled line is still a valid statement, and it is now genuinely
/// editable — a raw-mode single-line editor's redraw has no way to clear
/// or address an embedded newline (see the module doc's history section).
pub fn append_history(line: &str) {
    let Some(p) = history_path() else { return };
    if let Some(dir) = p.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&p) {
        let _ = writeln!(f, "{}", one_line_for_history(line));
    }
}

/// Collapse a (possibly multi-line) remembered line to one line: `\n` and
/// `\r` become a single space each. Used before both `append_history` and
/// `LineEditor::remember`, so the file on disk and the in-memory recall
/// buffer agree — an entry recalled with Up-arrow is always one line the
/// raw-mode editor can redraw and address a cursor within.
fn one_line_for_history(line: &str) -> String {
    line.chars().map(|c| if c == '\n' || c == '\r' { ' ' } else { c }).collect()
}

/// Read lines from `input` to EOF, run each, write `text` to `out`.
/// Returns the exit code: 0, or 1 at the FIRST failing outcome (stops
/// there). `.quit` stops cleanly with exit 0; a `.edit` payload in batch
/// mode is not runnable (there is no terminal to hand to an editor), so it
/// prints a message explaining that and exits 1.
///
/// Input that ends mid-statement — no `;` before EOF — is an error, not a
/// no-op: `printf 'SELECT 1 AS one' | tdy` used to exit 0 having printed
/// nothing, which is a script that looks like it ran and produced no rows.
pub async fn run_batch(session: &mut Session, input: impl BufRead, out: &mut impl Write) -> Result<i32> {
    for line in input.lines() {
        let line = line.context("reading input")?;
        let o = session.run(&line, None).await;
        write!(out, "{}", o.text)?;
        out.flush()?;
        if let Payload::Edit(p) = &o.payload {
            writeln!(out, "Error: no editor in batch mode; edit {} yourself", p.display())?;
            return Ok(1);
        }
        if !o.ok {
            return Ok(1);
        }
        if session.wants_quit() {
            break;
        }
    }
    if let Some(buf) = session.discard_pending() {
        writeln!(out, "Error: incomplete statement at end of input: {}", first_line(&buf))?;
        out.flush()?;
        return Ok(1);
    }
    Ok(0)
}

/// The TTY loop: raw-mode prompt, history file, `$EDITOR` for `.edit`.
pub async fn run_interactive(session: &mut Session) -> Result<()> {
    let mut stdout = std::io::stdout();
    let mut ed = LineEditor::new(load_history(HISTORY_LIMIT));
    let sink = crate::progress::stderr_sink();
    loop {
        let prompt = if session.sql_pending() { "   -> " } else { "tdy> " };
        let line = match read_line(&mut ed, prompt, &mut stdout)? {
            Read::Line(l) => l,
            Read::Interrupt => {
                // Ctrl-C on an empty prompt abandons a pending statement.
                if let Some(buf) = session.discard_pending() {
                    writeln!(stdout, "note: discarded incomplete statement: {}", first_line(&buf))?;
                }
                continue;
            }
            Read::Eof => {
                // Ctrl-D at the continuation prompt: say what was dropped,
                // for the same reason a dot-command does.
                if let Some(buf) = session.discard_pending() {
                    writeln!(stdout, "note: discarded incomplete statement: {}", first_line(&buf))?;
                }
                break;
            }
        };
        let o = session.run(&line, Some(&sink)).await;
        if !o.echo.trim().is_empty() && !matches!(o.payload, Payload::Continue) {
            // Collapsed to one line before either recall buffer sees it —
            // see `one_line_for_history`'s doc comment.
            let remembered = one_line_for_history(&o.echo);
            ed.remember(&remembered);
            append_history(&remembered);
        }
        print!("{}", o.text);
        stdout.flush()?;
        if let Payload::Edit(p) = &o.payload {
            run_editor(p)?;
        }
        if session.wants_quit() {
            break;
        }
    }
    Ok(())
}

/// The first line of a multi-line buffer, for the discarded-statement note
/// (naming the whole thing would dump arbitrarily long SQL into one line).
fn first_line(s: &str) -> &str {
    s.lines().next().unwrap_or("")
}

enum Read {
    Line(String),
    Interrupt,
    Eof,
}

/// One line in raw mode, redrawing on every edit. Restores cooked mode on
/// every exit path, including `?`.
fn read_line(ed: &mut LineEditor, prompt: &str, out: &mut std::io::Stdout) -> Result<Read> {
    struct Raw;
    impl Drop for Raw {
        fn drop(&mut self) {
            let _ = terminal::disable_raw_mode();
        }
    }
    terminal::enable_raw_mode()?;
    let _guard = Raw;
    let redraw = |ed: &LineEditor, out: &mut std::io::Stdout| -> Result<()> {
        // \r, clear line, prompt + text, then move the cursor back.
        let text = ed.text();
        let back = text.chars().count().saturating_sub(ed.cursor());
        write!(out, "\r\x1b[2K{prompt}{text}")?;
        if back > 0 {
            write!(out, "\x1b[{back}D")?;
        }
        out.flush()?;
        Ok(())
    };
    redraw(ed, out)?;
    loop {
        if let Event::Key(k) = event::read()? {
            if k.kind != event::KeyEventKind::Press {
                continue;
            }
            match ed.key(k) {
                Edit::Redraw | Edit::Nothing => redraw(ed, out)?,
                Edit::Cleared => {
                    write!(out, "^C\r\n")?;
                    redraw(ed, out)?;
                }
                Edit::Interrupt => {
                    write!(out, "^C\r\n")?;
                    out.flush()?;
                    return Ok(Read::Interrupt);
                }
                Edit::Eof => {
                    write!(out, "\r\n")?;
                    out.flush()?;
                    return Ok(Read::Eof);
                }
                Edit::Submit(l) => {
                    write!(out, "\r\n")?;
                    out.flush()?;
                    return Ok(Read::Line(l));
                }
            }
        }
    }
}

fn run_editor(path: &Path) -> Result<()> {
    let editor = std::env::var("VISUAL").or_else(|_| std::env::var("EDITOR")).unwrap_or_else(|_| "vi".into());
    let status = std::process::Command::new(&editor)
        .arg(path)
        .status()
        .with_context(|| format!("cannot run editor {editor}"))?;
    if !status.success() {
        anyhow::bail!("{editor} exited with {status}");
    }
    println!("edited {}", path.display());
    Ok(())
}

/// Whether both ends are terminals — the interactive console's precondition.
pub fn stdio_is_tty() -> bool {
    std::io::stdin().is_terminal() && std::io::stdout().is_terminal()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_line_for_history_folds_newlines_and_crlf_to_spaces() {
        assert_eq!(one_line_for_history("SELECT 1\nFROM t;"), "SELECT 1 FROM t;");
        assert_eq!(one_line_for_history("a\r\nb\nc"), "a  b c");
        assert_eq!(one_line_for_history(".ls"), ".ls");
    }

    /// A multi-line statement round-trips through `append_history` /
    /// `load_history` as one space-joined line — never staircasing raw
    /// terminal output on Up-arrow recall, and never containing a real
    /// `\n` a single-line redraw could not clear or address a cursor
    /// within. `history_path` is not overridable directly, so this points
    /// `dirs::data_dir` (via `XDG_DATA_HOME`) at a scratch directory; this
    /// is the only test in the binary that touches that variable.
    #[test]
    fn history_round_trip_collapses_a_multiline_statement_to_one_line() {
        let dir = std::env::temp_dir().join(format!("tdy_history_test_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let prev = std::env::var_os("XDG_DATA_HOME");
        std::env::set_var("XDG_DATA_HOME", &dir);

        append_history("SELECT 1\nFROM t;");
        append_history(".ls");
        let got = load_history(1000);

        match prev {
            Some(v) => std::env::set_var("XDG_DATA_HOME", v),
            None => std::env::remove_var("XDG_DATA_HOME"),
        }
        let _ = std::fs::remove_dir_all(&dir);

        assert_eq!(got, vec!["SELECT 1 FROM t;".to_string(), ".ls".to_string()]);
    }
}
