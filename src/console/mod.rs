//! The console: one grammar for the plain REPL, the batch runner and the
//! workbench. See docs/design/2026-09-01-console-and-workbench.md.
//!
//! `Session` is the whole of the console's state and behaviour: it owns the
//! working directory, runs one line at a time through [`parse`] and its own
//! `dispatch`, and returns an [`Outcome`] rather than printing — nothing in
//! this module writes to stdout or stderr, so the same session drives the
//! plain REPL, a batch runner and (eventually) the TUI workbench identically.
//!
//! This task lands the skeleton: `.help`, `.quit`, `.cd`, `.ls`, and the
//! error path every other dot-command falls into until its own task lands.
//! Every command that names a path — implemented or not — is confined to
//! the session's root before anything else happens to it, because
//! confinement is a safety property of the session, not a feature that
//! ships with each command.

pub mod parse;

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};

use crate::config::Config;
use crate::progress;
pub use parse::{parse, Command, ParseError};

/// The result of running one line: what to show, and what it means.
#[derive(Debug)]
pub struct Outcome {
    /// The line as the frontend should echo it (empty when the caller
    /// already knows what it typed, e.g. a REPL that echoed as it read).
    pub echo: String,
    /// Human-readable text — what the CLI would have printed.
    pub text: String,
    pub payload: Payload,
    pub ok: bool,
}

#[derive(Debug)]
pub enum Payload {
    Nothing,
    /// An incomplete SQL statement was buffered; nothing ran.
    // constructed from Task 8 (multi-line SQL assembly)
    Continue,
    Quit,
    Listing(Vec<Entry>),
    // constructed from Task 7 (.show)
    Shown { path: PathBuf, raw: RawHead, spec: Option<SpecSummary> },
    // constructed from Task 6 (.sniff)
    Sniffed { path: PathBuf, spec: SpecSummary, preview: Table, kept_existing: bool },
    // constructed from Task 7 (.draft)
    Drafted { ddl: String, wrote: Option<PathBuf> },
    // constructed from Task 6 (.fit / .check)
    Fitted(crate::report::PileReport),
    // constructed from Task 9 (.accept)
    Evidence { target: PathBuf, member: String, rows: Vec<crate::evidence::Evidence> },
    // constructed from Task 8 (bare SQL)
    Query(Table),
    /// The frontend runs `$EDITOR` on this path (the session cannot own the
    /// terminal).
    // constructed from Task 7 (.edit)
    Edit(PathBuf),
    Error { message: String },
}

/// One entry in a `.ls` listing.
// `Eq` is dropped even though the brief's sketch derives it: `EntryStatus`
// carries an `Option<f32>` (confidence), and `f32` is not `Eq`.
#[derive(Debug, Clone, PartialEq)]
pub struct Entry {
    /// Relative to the listed directory; directories end with `/`.
    pub name: String,
    pub kind: EntryKind,
    pub status: EntryStatus,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryKind {
    Dir,
    File,
    Target,
}

#[derive(Debug, Clone, PartialEq)]
pub enum EntryStatus {
    /// A directory, or a file with no sidecar.
    None,
    Sniffed { confidence: Option<f32>, method: String },
    Stale,
    /// A target with no lock.
    NoLock,
    /// A target with a lock and no drift.
    Locked,
    /// A target whose lock disagrees with it in N places.
    Drift(usize),
}

/// A table of results, as text — what `.sniff`'s preview, a `.fit` dry run
/// or a bare SQL query prints.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Table {
    pub columns: Vec<String>,
    pub types: Vec<String>,
    pub rows: Vec<Vec<String>>,
    pub total: usize,
    pub truncated: bool,
}

/// The raw head of a file, for `.show` — what tdy sees before any spec is
/// applied.
#[derive(Debug, Clone, PartialEq)]
pub struct RawHead {
    pub lines: Vec<String>,
    pub truncated: bool,
    /// Excel/ODS only: (sheet name, rows, cols) per sheet.
    pub sheets: Vec<(String, usize, usize)>,
}

/// A rendered `ParseSpec`, for `.sniff` and `.show`.
#[derive(Debug, Clone, PartialEq)]
pub struct SpecSummary {
    pub method: String,
    pub confidence: Option<f32>,
    /// Compact JSON of `spec.extraction`.
    pub extraction: String,
    /// Compact JSON per transform, in spec order.
    pub transforms: Vec<String>,
    /// (name, source, dtype as `describe_dtype`).
    pub columns: Vec<(String, String, String)>,
    pub notes: Vec<String>,
}

pub struct Session {
    root: PathBuf,
    cwd: PathBuf,
    cfg: Config,
    quit: bool,
    #[allow(dead_code)] // written/read from Task 8 (multi-line SQL assembly)
    sql_buffer: String,
    #[allow(dead_code)] // written by Task 8 (`.output`), read once a query result is produced
    output: Option<(PathBuf, crate::provider::OutputFormat)>,
    #[allow(dead_code)] // written/read by Task 9 (`.accept`, the review gate)
    pending_accept: Option<(PathBuf, String)>,
}

impl Session {
    /// `root` is canonicalised; `cwd` starts at root. `cfg` is what the
    /// commands run with (backend, limits).
    pub fn new(root: &Path, cfg: Config) -> Result<Session> {
        let root = root
            .canonicalize()
            .with_context(|| format!("cannot open root {}", root.display()))?;
        Ok(Session {
            cwd: root.clone(),
            root,
            cfg,
            quit: false,
            sql_buffer: String::new(),
            output: None,
            pending_accept: None,
        })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }
    pub fn cwd(&self) -> &Path {
        &self.cwd
    }
    #[allow(dead_code)] // no command reads the config back yet; kept for Task 6+ (`.sniff --hint`, `.fit`, ...)
    pub fn cfg(&self) -> &Config {
        &self.cfg
    }
    /// Set by `.quit`; frontends check it after each `run`.
    pub fn wants_quit(&self) -> bool {
        self.quit
    }

    /// Resolve a user-supplied path against cwd and confine it to root.
    ///
    /// `fileio::confine` already distinguishes the two ways this can fail —
    /// the path does not exist at all, or it exists but resolves outside
    /// `root` — and its own message says which (the escape case's message
    /// contains "outside"; the missing-file case's says "does not exist").
    /// That distinction matters to a person typing at a prompt (a typo is
    /// not an escape attempt), so it is preserved here, not collapsed.
    pub fn resolve(&self, p: &str) -> Result<PathBuf> {
        let joined = self.cwd.join(p);
        crate::fileio::confine(&joined, &self.root).with_context(|| p.to_string())
    }

    /// Globs expanded against cwd, each result confined. Errors if a glob
    /// matched nothing.
    pub fn expand(&self, patterns: &[String]) -> Result<Vec<PathBuf>> {
        let mut out = Vec::new();
        for pat in patterns {
            let hits = crate::lockfile::expand_glob(&self.cwd, pat)?;
            if hits.is_empty() {
                bail!("{pat}: no file matches");
            }
            for h in hits {
                // Literal (non-glob) patterns pass through expand_glob
                // unchecked, so a hit here can still be a plain typo, not
                // an escape — see resolve()'s doc comment.
                out.push(crate::fileio::confine(&h, &self.root).with_context(|| h.display().to_string())?);
            }
        }
        Ok(out)
    }

    /// One line in, one outcome out. Never panics on input; never prints.
    pub async fn run(&mut self, line: &str, progress: Option<&progress::Sink>) -> Outcome {
        let trimmed = line.trim();
        // SQL assembly (Task 8) goes here; for now every non-dot line is
        // an error so the tests for this task do not depend on it.
        let cmd = match parse(trimmed) {
            Ok(c) => c,
            Err(e) => return Outcome::error(trimmed, e.to_string()),
        };
        let echo = trimmed.to_string();
        match self.dispatch(cmd, progress).await {
            Ok(mut o) => {
                if o.echo.is_empty() {
                    o.echo = echo;
                }
                o
            }
            Err(e) => Outcome::error(&echo, format!("{e:#}")),
        }
    }

    async fn dispatch(&mut self, cmd: Command, _progress: Option<&progress::Sink>) -> Result<Outcome> {
        // Confinement applies to every path a command names, whether or not
        // that command is implemented yet: a `.sniff` outside the root must
        // fail as "outside", not as "not implemented".
        self.confine_command_paths(&cmd)?;
        Ok(match cmd {
            Command::Help { command } => Outcome::ok(help_text(command.as_deref()), Payload::Nothing),
            Command::Quit => {
                self.quit = true;
                Outcome::ok(String::new(), Payload::Quit)
            }
            Command::Cd { dir } => {
                let p = self.resolve(&dir)?;
                if !p.is_dir() {
                    bail!("{dir}: not a directory");
                }
                self.cwd = p.canonicalize()?;
                // Relative paths inside SQL (`messy('2025-01.csv')`) resolve
                // against the process's directory; a console is one session
                // per process, so moving the process is the honest thing.
                std::env::set_current_dir(&self.cwd)?;
                Outcome::ok(format!("{}\n", self.display_rel(&self.cwd)), Payload::Nothing)
            }
            Command::Ls { dir } => {
                let p = match dir {
                    Some(d) => self.resolve(&d)?,
                    None => self.cwd.clone(),
                };
                let entries = list_dir(&p)?;
                Outcome::ok(render_listing(&entries), Payload::Listing(entries))
            }
            other => bail!("`{}` is not implemented yet", describe_command(&other)),
        })
    }

    /// Resolve (or expand) every path-shaped argument a command carries,
    /// discarding nothing — this is called before dispatch decides whether
    /// the command is even implemented, so a path outside the root is
    /// always refused as "outside" rather than swallowed by "not
    /// implemented yet". Output-only paths (`.draft --to`, `.output`) are
    /// not checked here: they need not exist yet, and the command that
    /// writes them is responsible for confining the directory it writes
    /// into.
    fn confine_command_paths(&self, cmd: &Command) -> Result<()> {
        match cmd {
            Command::Sniff { file, .. }
            | Command::Validate { file, .. }
            | Command::Show { file }
            | Command::Edit { file } => {
                self.resolve(file)?;
            }
            Command::Draft { files, .. } => {
                self.expand(files)?;
            }
            Command::Fit { target, file, .. } => {
                self.resolve(target)?;
                if let Some(f) = file {
                    self.resolve(f)?;
                }
            }
            Command::Check { target, against } => {
                self.resolve(target)?;
                if !against.is_empty() {
                    self.expand(against)?;
                }
            }
            Command::Accept { target, .. } => {
                self.resolve(target)?;
            }
            Command::Cd { .. }
            | Command::Ls { .. }
            | Command::Output { .. }
            | Command::Schema
            | Command::ConfigInit
            | Command::Help { .. }
            | Command::Quit
            | Command::Sql(_) => {}
        }
        Ok(())
    }

    /// Root-relative display of a path, "." for the root itself.
    fn display_rel(&self, p: &Path) -> String {
        match p.strip_prefix(&self.root) {
            Ok(r) if r.as_os_str().is_empty() => ".".into(),
            Ok(r) => r.display().to_string(),
            Err(_) => p.display().to_string(),
        }
    }
}

impl Outcome {
    fn ok(text: String, payload: Payload) -> Outcome {
        Outcome { echo: String::new(), text, payload, ok: true }
    }
    fn error(echo: &str, message: String) -> Outcome {
        Outcome {
            echo: echo.to_string(),
            text: format!("Error: {message}\n"),
            payload: Payload::Error { message },
            ok: false,
        }
    }
}

fn describe_command(c: &Command) -> String {
    format!("{c:?}")
}

/// Which files the browser and `.ls` treat as data (by extension).
pub fn is_data_file(name: &str) -> bool {
    let ext = Path::new(name).extension().and_then(|e| e.to_str()).unwrap_or("").to_ascii_lowercase();
    matches!(
        ext.as_str(),
        "csv" | "tsv" | "txt" | "log" | "json" | "ndjson" | "jsonl" | "xlsx" | "xlsm" | "xls" | "xlsb" | "ods"
    )
}
pub fn is_target(name: &str) -> bool {
    name.ends_with(".tdy.sql")
}

/// Directory listing the way the browser shows it: dirs first, then files,
/// each sorted; companions folded into their owner's status.
pub fn list_dir(dir: &Path) -> Result<Vec<Entry>> {
    let mut dirs = Vec::new();
    let mut files = Vec::new();
    for e in std::fs::read_dir(dir).with_context(|| format!("cannot read {}", dir.display()))?.flatten() {
        let name = e.file_name().to_string_lossy().to_string();
        if name.starts_with('.') {
            continue;
        }
        let path = e.path();
        if path.is_dir() {
            dirs.push(Entry { name: format!("{name}/"), kind: EntryKind::Dir, status: EntryStatus::None });
        } else if is_target(&name) {
            files.push(Entry { name, kind: EntryKind::Target, status: target_status(&path) });
        } else if is_data_file(&name) && !name.ends_with(".tdy.toml") {
            files.push(Entry { name, kind: EntryKind::File, status: file_status(&path) });
        }
    }
    dirs.sort_by(|a, b| a.name.cmp(&b.name));
    files.sort_by(|a, b| a.name.cmp(&b.name));
    dirs.extend(files);
    Ok(dirs)
}

fn file_status(path: &Path) -> EntryStatus {
    use crate::sidecar::SidecarStatus;
    match crate::sidecar::load(path) {
        Ok(SidecarStatus::Fresh(sc)) => {
            EntryStatus::Sniffed { confidence: sc.spec.confidence, method: method_label(&sc.provenance.method) }
        }
        Ok(SidecarStatus::Stale(_)) => EntryStatus::Stale,
        Ok(SidecarStatus::Absent) => EntryStatus::None,
        Err(_) => EntryStatus::Stale, // unreadable sidecar: not something a query would use
    }
}

/// The sidecar's `InferenceMethod` as the lowercase word its TOML uses
/// (`heuristic`/`llm`/`manual`) — `Debug` would print the Rust identifier
/// casing instead.
fn method_label(m: &crate::spec::InferenceMethod) -> String {
    serde_json::to_string(m).unwrap_or_default().trim_matches('"').to_string()
}

fn target_status(path: &Path) -> EntryStatus {
    let Ok(target) = crate::target::Target::load(path) else { return EntryStatus::NoLock };
    match crate::lockfile::Lock::load(path) {
        Ok(Some(lock)) => match crate::lockfile::drift(&lock, &target, path) {
            Ok(d) if d.is_empty() => EntryStatus::Locked,
            Ok(d) => EntryStatus::Drift(d.len()),
            Err(_) => EntryStatus::Drift(1),
        },
        _ => EntryStatus::NoLock,
    }
}

/// The `.ls` text: `"name  status"` per line, aligned.
pub fn render_listing(entries: &[Entry]) -> String {
    let width = entries.iter().map(|e| e.name.chars().count()).max().unwrap_or(0);
    let mut s = String::new();
    for e in entries {
        let status = match &e.status {
            EntryStatus::None => String::new(),
            EntryStatus::Sniffed { confidence: Some(c), method } => format!("sniffed {c:.2} ({method})"),
            EntryStatus::Sniffed { confidence: None, method } => format!("sniffed ({method})"),
            EntryStatus::Stale => "stale".into(),
            EntryStatus::NoLock => "target, no lock".into(),
            EntryStatus::Locked => "target, locked".into(),
            EntryStatus::Drift(n) => format!("target, drift ({n})"),
        };
        let _ = writeln!(s, "{:<width$}  {status}", e.name, width = width);
    }
    s
}

fn help_text(command: Option<&str>) -> String {
    const ALL: &str = "\
SQL runs as typed; end a statement with `;` (it may span lines).
Everything else is a dot-command:

  .sniff FILE [--quick] [--force] [--no-llm] [--hint \"…\"]   infer the sidecar for one file
  .validate FILE [--stamp]                                  check a sidecar against its file
  .draft FILES… [--to NAME.tdy.sql]                         draft a target from a pile
  .fit TARGET [FILE] [--dry-run] [--propose]                plan every member onto a target
  .check TARGET [--against FILE…]                           the CI gate
  .accept TARGET MEMBER                                     show the evidence; again to accept
  .output [FILE] [--format parquet|csv] [--force]           route the next result to a file
  .show FILE          the raw head beside what the sidecar says
  .ls [DIR]  .cd DIR  .edit FILE  .schema  .config init  .help [CMD]  .quit
";
    match command {
        None => ALL.to_string(),
        Some(c) => ALL
            .lines()
            .filter(|l| l.trim_start().starts_with(&format!(".{c}")))
            .map(|l| format!("{l}\n"))
            .collect::<String>()
            .trim_end()
            .to_string()
            + "\n",
    }
}
