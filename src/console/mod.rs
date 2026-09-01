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
pub mod line;

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

/// Guards every place this module changes the *process's* actual working
/// directory (`.cd`, `.draft`'s brief realignment so `draft_target` sees
/// the same cwd-relative shape `tdy draft` does, and `run_sql`'s — see
/// `Command::Cd`, `Command::Draft`, `Session::run_sql`). A `Session`'s own
/// `cwd` field is what every other method actually resolves against, so in
/// production — one session per process — this lock is never contended. It
/// exists for the test binary, where multiple `Session`s in the same
/// process race on this one piece of real OS state.
///
/// `tokio::sync::Mutex`, not `std::sync::Mutex`: `run_sql` holds the guard
/// across an `.await` (the flip must span the whole query, not just the
/// `set_current_dir` call — see its doc comment), and a `std::sync::Mutex`
/// guard held across an await point makes the enclosing future `!Send`,
/// which broke `tokio::spawn(session.run(...))` — exactly how the workbench
/// slice is designed to drive it. Tokio's mutex has no poisoning, so there
/// is no `into_inner()` to unwrap here.
static CWD_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// RAII guard shared by `.draft` and `run_sql`: flips the process's actual
/// working directory to `dir` for the duration of a call that — unlike the
/// rest of this module — consults process cwd rather than `Session::cwd`
/// (`draft_target`'s bare-name table-naming convention; SQL's relative
/// `messy()`/`dataset()` paths, resolved by `provider::prepare_specs`'s
/// pre-pass). Held under `CWD_LOCK` for its whole lifetime — the flip, the
/// call, and the restore — including across an `.await` in `run_sql`'s
/// case, which is exactly why `CWD_LOCK` is a `tokio::sync::Mutex` rather
/// than a `std` one (see its doc comment). Restores the previous directory
/// on drop, including on an early return via `?` inside the wrapped call
/// or an unwinding panic — a sniff or a query failure must not leave the
/// process pointed at this session's cwd forever. `.cd`'s own, permanent
/// flip does not use this: it has nothing to restore to.
struct RestoreCwd {
    _lock: tokio::sync::MutexGuard<'static, ()>,
    prev: Option<PathBuf>,
}

impl RestoreCwd {
    async fn to(dir: &Path) -> Result<RestoreCwd> {
        let lock = CWD_LOCK.lock().await;
        let prev = std::env::current_dir().ok();
        std::env::set_current_dir(dir)?;
        Ok(RestoreCwd { _lock: lock, prev })
    }
}

impl Drop for RestoreCwd {
    fn drop(&mut self) {
        if let Some(p) = self.prev.take() {
            let _ = std::env::set_current_dir(p);
        }
    }
}

pub struct Session {
    root: PathBuf,
    cwd: PathBuf,
    cfg: Config,
    quit: bool,
    sql_buffer: String,
    output: Option<(PathBuf, crate::provider::OutputFormat)>,
    /// The review gate's own memory: set by `.accept`'s step one (evidence
    /// shown, nothing written), cleared by anything except an identical
    /// repeat of that same line — see `run`'s reset rule.
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

    /// The target and member `.accept` is waiting to hear repeated back, if
    /// any — a frontend can use this to show "press again to confirm"
    /// without re-deriving the rule itself.
    pub fn pending_accept(&self) -> Option<(&Path, &str)> {
        self.pending_accept.as_ref().map(|(p, m)| (p.as_path(), m.as_str()))
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

    /// Resolve a path that names something not written yet — `.draft --to`
    /// and (Task 8) `.output` — by confining its *parent directory* rather
    /// than the path itself: `fileio::confine` canonicalises, which a
    /// not-yet-existing file cannot survive. The parent must exist and
    /// resolve under root; the file name is appended to its canonical form
    /// so a parent reached through a symlink still lands where root says it
    /// does.
    fn resolve_new(&self, p: &str) -> Result<PathBuf> {
        let joined = self.cwd.join(p);
        let name = joined.file_name().with_context(|| format!("{p}: not a file name"))?.to_owned();
        let parent = match joined.parent() {
            Some(p) if !p.as_os_str().is_empty() => p,
            _ => Path::new("."),
        };
        let confined_parent = crate::fileio::confine(parent, &self.root).with_context(|| p.to_string())?;
        Ok(confined_parent.join(name))
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
    ///
    /// A line starting with `.` is a dot-command, dispatched immediately;
    /// anything else is SQL, buffered until a `;` ends the statement
    /// (sqlite's rule — see `parse`'s module doc). A dot-command typed while
    /// a statement is pending discards the buffer and says so: silently
    /// dropping half-typed SQL would be its own kind of wrong answer.
    pub async fn run(&mut self, line: &str, progress: Option<&progress::Sink>) -> Outcome {
        let trimmed = line.trim_end();
        let is_dot = trimmed.trim_start().starts_with('.');
        // The review gate's second step is reachable only by literally
        // repeating the same `.accept TARGET MEMBER` line back — anything
        // else in between resets it to step one: any other dot-command, an
        // unparsable line, a completed SQL statement, a fragment that only
        // extends the SQL buffer (`Continue`) without running anything, and
        // a blank line (bare Enter is still "a line that is not that exact
        // `.accept`", and the invariant is ANY other line resets it) — so
        // this runs before the blank-line early return below, not after it.
        // Decided from the parse, before dispatch, parsed once here and
        // reused below rather than parsed again.
        let dot_parsed = is_dot.then(|| parse(trimmed));
        if !matches!(&dot_parsed, Some(Ok(Command::Accept { .. }))) {
            self.pending_accept = None;
        }
        if trimmed.trim().is_empty() && self.sql_buffer.is_empty() {
            return Outcome { echo: String::new(), text: String::new(), payload: Payload::Nothing, ok: true };
        }
        let mut prefix = String::new();
        if is_dot && !self.sql_buffer.is_empty() {
            prefix = format!("note: discarded incomplete statement: {}\n", first_line(&self.sql_buffer));
            self.sql_buffer.clear();
        }
        if !is_dot {
            if !self.sql_buffer.is_empty() {
                self.sql_buffer.push('\n');
            }
            self.sql_buffer.push_str(trimmed);
            if !self.sql_buffer.trim_end().ends_with(';') {
                return Outcome {
                    echo: trimmed.to_string(),
                    text: String::new(),
                    payload: Payload::Continue,
                    ok: true,
                };
            }
            let sql = std::mem::take(&mut self.sql_buffer);
            let mut o = match self.run_sql(&sql).await {
                Ok(o) => o,
                Err(e) => Outcome::error(&sql, format!("{e:#}")),
            };
            o.echo = sql;
            return o;
        }
        let cmd = match dot_parsed.expect("is_dot implies dot_parsed is Some") {
            Ok(c) => c,
            Err(e) => return Outcome::error(trimmed, e.to_string()),
        };
        let echo = trimmed.to_string();
        let mut o = match self.dispatch(cmd, progress).await {
            Ok(mut o) => {
                if o.echo.is_empty() {
                    o.echo = echo;
                }
                o
            }
            Err(e) => Outcome::error(&echo, format!("{e:#}")),
        };
        if !prefix.is_empty() {
            o.text = prefix + &o.text;
        }
        o
    }

    /// Whether a SQL statement is buffered, waiting for a `;`.
    pub fn sql_pending(&self) -> bool {
        !self.sql_buffer.is_empty()
    }

    /// Run one complete SQL statement (already `;`-terminated) and format
    /// its result — to `.output`'s route if one is active, to the screen
    /// otherwise. Each statement builds its own query context (see the
    /// design doc §4): a `.sniff --force` or a rewritten export between two
    /// statements must not serve a `MemTable` built from the old spec.
    async fn run_sql(&mut self, sql: &str) -> Result<Outcome> {
        // A relative `messy('2025-01.csv')` inside the SQL text is resolved
        // two ways: `MessyFunc` confines it against `self.root` regardless
        // of the process's actual directory, but `provider::prepare_specs`'s
        // pre-pass (sniffing, sidecar I/O) uses plain relative-path I/O —
        // the same assumption the plain CLI makes, that process cwd *is*
        // the working directory. `RestoreCwd` (shared with `.draft`) holds
        // the realignment across the whole call, since the pre-pass and the
        // query it feeds are one `.await`.
        let (schema, batches) = {
            let _restore = RestoreCwd::to(&self.cwd).await?;
            crate::provider::run_query_rooted(sql, &self.cfg, false, Some(self.root.clone())).await?
        };
        let table = table_of(&schema, &batches, QUERY_ROWS_CAP);
        let text = match self.output.take() {
            Some((path, fmt)) => {
                crate::provider::write_output(&schema, &batches, fmt, Some(&path))?;
                format!("wrote {} row(s) to {}\n", table.total, self.display_rel(&path))
            }
            None => crate::provider::format_table(&batches)?,
        };
        Ok(Outcome { echo: String::new(), text, payload: Payload::Query(table), ok: true })
    }

    async fn dispatch(&mut self, cmd: Command, progress: Option<&progress::Sink>) -> Result<Outcome> {
        // Taken here, before `confine_command_paths` gets a chance to bail
        // on a bad path — a `.accept BADTARGET M` line still parses as
        // `Command::Accept`, so `run`'s own reset (see its doc comment)
        // deliberately preserves the marker for it, expecting the `Accept`
        // arm to settle whether it is really the same line repeated. If
        // that line then errors out of `confine_command_paths` before ever
        // reaching the arm, a stale step-one marker from a *different*
        // `.accept` would otherwise survive an unrelated failed attempt.
        // Taking it unconditionally here is a no-op for every other
        // command, since `run` already cleared it before calling `dispatch`
        // for anything that did not parse as `Command::Accept`.
        let pending_accept = self.pending_accept.take();
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
                // `CWD_LOCK` (see its doc comment) keeps this from racing
                // `.draft`'s own realignment in another `Session`.
                {
                    let _lock = CWD_LOCK.lock().await;
                    std::env::set_current_dir(&self.cwd)?;
                }
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
            Command::Sniff { file, quick, force, no_llm, hint } => {
                let path = self.resolve(&file)?;
                let out = crate::commands::sniff_text(
                    &path,
                    &self.cfg,
                    crate::provider::SniffCli { hint: hint.as_deref(), force, no_llm, quick, json: false },
                )
                .await?;
                let method = method_label(&out.prepared.method);
                let summary = spec_summary(&out.spec, &method, out.prepared.confidence);
                let batch = crate::engine::preview(&out.spec, &path, self.cfg.limits, 10)?;
                let preview = table_of(&batch.schema(), std::slice::from_ref(&batch), 10);
                Outcome::ok(
                    out.text,
                    Payload::Sniffed { path, spec: summary, preview, kept_existing: out.kept_existing },
                )
            }
            Command::Validate { file, stamp } => {
                let path = self.resolve(&file)?;
                Outcome::ok(crate::commands::validate_text(&path, &self.cfg, stamp)?, Payload::Nothing)
            }
            Command::Show { file } => {
                let path = self.resolve(&file)?;
                let raw = raw_head(&path, self.cfg.limits)?;
                let spec = match crate::sidecar::load(&path)? {
                    crate::sidecar::SidecarStatus::Fresh(sc) => {
                        Some(spec_summary(&sc.spec, &method_label(&sc.provenance.method), sc.spec.confidence))
                    }
                    _ => None,
                };
                let text = render_shown(&file, &raw, spec.as_ref());
                Outcome::ok(text, Payload::Shown { path, raw, spec })
            }
            Command::Draft { files, to } => {
                let paths = self.expand(&files)?;
                // The line as actually run: globs and defaults expanded
                // (design §4) — the file list root-relative and quoted the
                // way the console's own tokenizer reads it back, with
                // `--to` appended when given.
                let echo = format!(
                    ".draft {}{}",
                    paths.iter().map(|p| quote_rel(&self.display_rel(p))).collect::<Vec<_>>().join(" "),
                    to.as_ref().map(|t| format!(" --to {t}")).unwrap_or_default()
                );
                // `draft_target` falls back to naming the table `dataset`
                // only when the first file it is handed carries no
                // directory component (see `table_name` in `draft.rs`) —
                // exactly how `tdy draft` is normally invoked: cd into the
                // pile, then a shell-expanded glob with no path prefix. The
                // console always resolves to an absolute path, so it
                // reproduces that shape itself, aligning the process's
                // actual directory to `self.cwd` for the call — the same
                // thing `.cd` does permanently, on the same reasoning that a
                // console owns the process it runs in (see `Command::Cd`).
                let rel: Vec<PathBuf> = paths
                    .iter()
                    .map(|p| p.strip_prefix(&self.cwd).map(Path::to_path_buf).unwrap_or_else(|_| p.clone()))
                    .collect();
                let ddl = {
                    // `RestoreCwd` (see its doc comment) keeps this
                    // realignment from racing `.cd`'s own, permanent one in
                    // another `Session`, held across the flip, the call, and
                    // the restore.
                    let _restore = RestoreCwd::to(&self.cwd).await?;
                    crate::draft::draft_target(&rel, self.cfg.limits)?
                };
                let wrote = match to {
                    Some(t) => {
                        let dest = self.resolve_new(&t)?;
                        if dest.exists() {
                            bail!("{t} exists; choose another name or remove it first");
                        }
                        crate::fileio::atomic_write(&dest, &ddl)?;
                        Some(dest)
                    }
                    None => None,
                };
                let text = match &wrote {
                    Some(p) => format!("wrote {}\n", self.display_rel(p)),
                    None => ddl.clone(),
                };
                Outcome { echo, text, payload: Payload::Drafted { ddl, wrote }, ok: true }
            }
            Command::Fit { target, file: Some(file), dry_run, propose } => {
                let (t, f) = (self.resolve(&target)?, self.resolve(&file)?);
                let out =
                    crate::commands::fit_one_text(&t, &f, &self.cfg, dry_run, propose, progress).await?;
                let mut text = out.text;
                if !out.ok {
                    let msg = if out.gaps {
                        "no plan reaches the declared schema".to_string()
                    } else {
                        format!("could not fit {file}")
                    };
                    writeln!(text, "Error: {msg}")?;
                }
                Outcome { echo: String::new(), text, payload: Payload::Nothing, ok: out.ok }
            }
            Command::Fit { target, file: None, dry_run, propose } => {
                let t = self.resolve(&target)?;
                self.fit_pile(&t, &[], dry_run, propose, progress).await?
            }
            Command::Check { target, against } => {
                let t = self.resolve(&target)?;
                // No shell is in front of this console (design §3): a glob
                // in `--against` must be expanded here, the same way
                // `confine_command_paths` already expands it to check
                // confinement, or `.check t.tdy.sql --against 2025-*.csv`
                // would pass that pre-check and then fail `check_text` as a
                // literal file named `2025-*.csv`.
                let files = self.expand(&against)?;
                let out = crate::commands::check_text(&t, &files, self.cfg.limits)?;
                let mut text = out.text;
                if !out.ok {
                    writeln!(text, "Error: {} file(s) do not produce the declared schema", out.bad)?;
                }
                Outcome { echo: String::new(), text, payload: Payload::Nothing, ok: out.ok }
            }
            Command::Schema => Outcome::ok(
                format!("{}\n", serde_json::to_string_pretty(&crate::spec::ParseSpec::json_schema())?),
                Payload::Nothing,
            ),
            Command::ConfigInit => {
                let path = crate::config::config_file_path()
                    .map(|p| p.display().to_string())
                    .unwrap_or_else(|| "~/.config/tdy/config.toml".into());
                Outcome::ok(
                    format!("# write this to {path}\n\n{}\n", crate::config::SAMPLE_CONFIG),
                    Payload::Nothing,
                )
            }
            Command::Edit { file } => {
                let p = self.resolve(&file)?;
                Outcome::ok(String::new(), Payload::Edit(p))
            }
            Command::Output { file: None, .. } => {
                self.output = None;
                Outcome::ok("next result -> screen\n".into(), Payload::Nothing)
            }
            Command::Output { file: Some(f), format, force } => {
                // `resolve_new`, not `resolve`: the file need not exist yet
                // (see its doc comment, which names `.output` explicitly).
                let path = self.resolve_new(&f)?;
                if path.exists() && !force {
                    bail!("{f} exists; --force to overwrite");
                }
                let fmt = match format {
                    Some(x) => crate::provider::OutputFormat::parse(&x)?,
                    None => crate::provider::OutputFormat::for_output_path(&path)?,
                };
                self.output = Some((path, fmt));
                Outcome::ok(format!("next result -> {f}\n"), Payload::Nothing)
            }
            Command::Accept { target, member } => {
                // `pending_accept` was already taken at the top of
                // `dispatch`, before `confine_command_paths` could bail on
                // a bad target — see that comment. So every failure path
                // below (an unresolvable target, a member outside root, no
                // fresh sidecar, nothing to review) has already consumed
                // any stale marker; only the successful step-one tail below
                // sets it again.
                let pending = pending_accept;
                let t = self.resolve(&target)?;
                // The member's path is named relative to the target's own
                // directory (what the pile report and `fit_pile`'s `accept`
                // both use), not resolved against `self.cwd` the way a plain
                // file argument would be — confined the same way `resolve`
                // is, and with the same error preserved (a typo is "does not
                // exist", not "outside").
                let member_path = crate::fileio::confine(&crate::lockfile::target_dir(&t).join(&member), &self.root)
                    .with_context(|| member.clone())?;
                let same = pending.as_ref().map(|(pt, pm)| pt == &t && pm == &member).unwrap_or(false);
                if same {
                    // Step two: the same line, run again, is the only path
                    // to `accepted == true`.
                    let mut o = self.fit_pile(&t, &[PathBuf::from(&member)], false, false, progress).await?;
                    o.text = format!("accepted {member}\n\n{}", o.text);
                    return Ok(o);
                }
                // Step one: evidence only, nothing written.
                let sc = match crate::sidecar::load(&member_path)? {
                    crate::sidecar::SidecarStatus::Fresh(sc) => sc,
                    _ => bail!("{member} has no fresh sidecar; run `.fit {target}` first"),
                };
                let reasons = crate::fit::review_reasons(&sc.spec);
                if reasons.is_empty() {
                    bail!("nothing to accept: {member} has no judgement waiting on review");
                }
                let model_framed = matches!(sc.provenance.method, crate::spec::InferenceMethod::Llm);
                let rows =
                    crate::evidence::for_spec(&sc.spec, &member_path, self.cfg.limits, &reasons.join("; "), model_framed)?;
                let mut text = format!("evidence for {member} (nothing written):\n");
                for r in &reasons {
                    let _ = writeln!(text, "  review: {r}");
                }
                text.push_str(&render_evidence(&rows));
                let _ = writeln!(text, "\nrun `.accept {target} {member}` again to accept");
                self.pending_accept = Some((t.clone(), member.clone()));
                Outcome::ok(text, Payload::Evidence { target: t, member, rows })
            }
            other => bail!("`{}` is not implemented yet", describe_command(&other)),
        })
    }

    /// Shared by `.draft --to`, `.fit`'s pile path, and (Task 9) `.accept`'s
    /// second step: fits every member the target's globs match and writes
    /// the lock only if all of them fit, rendering the same text `tdy fit`
    /// prints — with the same failure sentence appended when the pile does
    /// not fully fit, since a partial lock is refused either way.
    async fn fit_pile(
        &mut self,
        target: &Path,
        accept: &[PathBuf],
        dry_run: bool,
        propose: bool,
        progress: Option<&progress::Sink>,
    ) -> Result<Outcome> {
        let r = crate::report::fit_pile(
            target,
            &self.cfg,
            crate::report::FitOpts {
                dry_run,
                accept,
                propose,
                progress: progress.cloned(),
                root: Some(&self.root),
            },
        )
        .await?;
        let mut text = crate::report::render_pile_text(&r);
        let ok = r.failed == 0;
        if !ok {
            writeln!(
                text,
                "Error: {} file(s) cannot reach the declared schema; no lock written. \
                 Fix them, exclude them, or widen the target.",
                r.failed
            )?;
        }
        Ok(Outcome { echo: String::new(), text, payload: Payload::Fitted(r), ok })
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

/// How many rows a bare SQL result carries in its `Table` payload; the text
/// (screen or `.output`) always reflects the whole result, this only bounds
/// what a frontend holds in memory to render it.
const QUERY_ROWS_CAP: usize = 500;

/// The first line of a buffered SQL statement, for the "discarded" note —
/// naming the whole thing would dump arbitrarily long SQL into a one-line
/// message.
fn first_line(s: &str) -> &str {
    s.lines().next().unwrap_or("")
}

/// Quote a root-relative path the way the console's own tokenizer expects
/// to read it back: `Debug` wraps a name carrying whitespace in double
/// quotes with the same escaping `tokenize` already handles, so it
/// round-trips through `.draft`'s echo — spec §4 wants `Outcome.echo` to be
/// the line as actually run, globs and defaults expanded.
fn quote_rel(s: &str) -> String {
    if s.chars().any(char::is_whitespace) { format!("{s:?}") } else { s.to_string() }
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

/// A rendered `ParseSpec`, for `.sniff` and `.show`'s payload and text.
pub fn spec_summary(spec: &crate::spec::ParseSpec, method: &str, confidence: Option<f32>) -> SpecSummary {
    SpecSummary {
        method: method.to_string(),
        confidence,
        extraction: serde_json::to_string(&spec.extraction).unwrap_or_default(),
        transforms: spec.transforms.iter().map(|t| serde_json::to_string(t).unwrap_or_default()).collect(),
        columns: spec
            .columns
            .iter()
            .map(|c| (c.name.clone(), c.source_name().to_string(), crate::commands::describe_dtype(&c.dtype)))
            .collect(),
        notes: spec.notes.clone(),
    }
}

/// A batch of results as text-table rows, capped at `cap` rows (`total` and
/// `truncated` still reflect the whole result).
pub fn table_of(
    schema: &datafusion::arrow::datatypes::Schema,
    batches: &[datafusion::arrow::record_batch::RecordBatch],
    cap: usize,
) -> Table {
    use datafusion::arrow::util::display::array_value_to_string;
    let total: usize = batches.iter().map(|b| b.num_rows()).sum();
    let mut rows = Vec::new();
    'outer: for b in batches {
        for i in 0..b.num_rows() {
            if rows.len() >= cap {
                break 'outer;
            }
            rows.push(
                (0..b.num_columns())
                    .map(|c| {
                        let col = b.column(c);
                        if col.is_null(i) { String::new() } else { array_value_to_string(col, i).unwrap_or_default() }
                    })
                    .collect(),
            );
        }
    }
    Table {
        columns: schema.fields().iter().map(|f| f.name().clone()).collect(),
        types: schema.fields().iter().map(|f| f.data_type().to_string()).collect(),
        truncated: total > rows.len(),
        rows,
        total,
    }
}

const HEAD_BYTES: usize = 16 * 1024;
const HEAD_LINES: usize = 40;
const WORKBOOK_EXT: [&str; 5] = ["xlsx", "xlsm", "xls", "xlsb", "ods"];

/// The raw head of a file, for `.show` — what tdy sees before any spec is
/// applied. Workbooks report per-sheet shape instead of lines (a sheet has
/// no single "head" worth printing).
pub fn raw_head(path: &Path, limits: crate::config::Limits) -> Result<RawHead> {
    let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("").to_ascii_lowercase();
    if WORKBOOK_EXT.contains(&ext.as_str()) {
        let sheets = crate::engine::excel_sheet_shapes(path, limits)?
            .into_iter()
            .map(|s| (s.name, s.rows, s.cols))
            .collect();
        return Ok(RawHead { lines: Vec::new(), truncated: false, sheets });
    }
    use std::io::Read;
    let mut f = std::fs::File::open(path).with_context(|| format!("cannot open {}", path.display()))?;
    let mut buf = Vec::with_capacity(HEAD_BYTES);
    f.by_ref().take(HEAD_BYTES as u64).read_to_end(&mut buf)?;
    let more = f.metadata().map(|m| m.len() as usize > buf.len()).unwrap_or(false);
    let text = String::from_utf8_lossy(&buf);
    let mut lines: Vec<String> = text.lines().map(|l| l.to_string()).collect();
    if more && !text.ends_with('\n') {
        lines.pop(); // a torn last line is not a line
    }
    let truncated = more || lines.len() > HEAD_LINES;
    lines.truncate(HEAD_LINES);
    Ok(RawHead { lines, truncated, sheets: Vec::new() })
}

/// The `.show` text: the raw head (or sheet shapes), then the sidecar
/// summary if one exists.
fn render_shown(name: &str, raw: &RawHead, spec: Option<&SpecSummary>) -> String {
    let mut s = String::new();
    let _ = writeln!(s, "{name}:");
    if raw.sheets.is_empty() {
        for l in &raw.lines {
            let _ = writeln!(s, "  {l}");
        }
        if raw.truncated {
            let _ = writeln!(s, "  …");
        }
    } else {
        for (n, r, c) in &raw.sheets {
            let _ = writeln!(s, "  sheet {n:?}: {r} row(s) x {c} col(s)");
        }
    }
    match spec {
        None => {
            let _ = writeln!(s, "\nno sidecar — `.sniff {name}` to infer one");
        }
        Some(sp) => {
            let _ = writeln!(
                s,
                "\nsidecar ({} method, confidence {}):",
                sp.method,
                sp.confidence.map(|c| format!("{c:.2}")).unwrap_or_else(|| "n/a".into())
            );
            let _ = writeln!(s, "  extraction  {}", sp.extraction);
            for t in &sp.transforms {
                let _ = writeln!(s, "  transform   {t}");
            }
            for (n, src, ty) in &sp.columns {
                let _ = writeln!(s, "  {n:<16} <- {src:<24} {ty}");
            }
            for n in &sp.notes {
                let _ = writeln!(s, "  note: {n}");
            }
        }
    }
    s
}

/// The evidence `.accept`'s step one shows: what accepting each judgement
/// in `rows` would actually do, not merely why it is a judgement — see
/// `evidence::for_spec`'s doc comment on why restating the reason is not
/// enough.
pub fn render_evidence(rows: &[crate::evidence::Evidence]) -> String {
    use crate::evidence::Evidence;
    let mut s = String::new();
    for e in rows {
        let _ = writeln!(s, "\n  {}", e.headline());
        match e {
            Evidence::Shift { head, smallest, largest, .. } => {
                for p in head.iter().take(5) {
                    let _ = writeln!(s, "    row {:<6} {:>14}  ->  {}", p.row, p.raw, p.parsed);
                }
                if let Some(p) = smallest {
                    let _ = writeln!(s, "    smallest  row {:<6} {:>14}  ->  {}", p.row, p.raw, p.parsed);
                }
                if let Some(p) = largest {
                    let _ = writeln!(s, "    largest   row {:<6} {:>14}  ->  {}", p.row, p.raw, p.parsed);
                }
            }
            Evidence::Frame { header, head, .. } => {
                let _ = writeln!(s, "    header: {}", header.join(" | "));
                for r in head.iter().take(5) {
                    let _ = writeln!(s, "    {}", r.join(" | "));
                }
            }
            Evidence::Constant { .. } | Evidence::Unillustrated { .. } => {}
        }
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
