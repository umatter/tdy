//! `tdy-tui` — the review loop, on one screen.
//!
//! The loop tdy's design implies is: declare → fit → read → judge → re-fit.
//! On the CLI every arrow in that is "read scrollback, edit a file, run it
//! again". This collapses the latency and nothing else: every action here is
//! one the CLI can already do, and a session leaves behind exactly the files
//! a CLI session would — the target, the sidecars, the lock. There is no
//! parallel state anywhere, so the git diff after using this reads like any
//! other.
//!
//! Work runs off the UI thread. Fitting a pile verifies types over whole
//! files and may consult a model; blocking the draw loop on that would give
//! a frozen screen for the one operation the user most wants narrated, so
//! [`tdy::progress`] events arrive on a channel and the status line says
//! what is happening while it happens.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use clap::Parser;
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::DefaultTerminal;
use tokio::sync::mpsc;

use tdy_tui::app::{Action, App, Key, Preview, QueryResult};
use tdy_tui::{evidence, ui};
use tdy::config::Config;
use tdy::report::{FitOpts, PileReport};

#[derive(Parser)]
#[command(
    name = "tdy-tui",
    about = "Review a pile of messy files against a declared schema",
    version
)]
struct Cli {
    /// The target .tdy.sql file. Omit to find one beside the working
    /// directory.
    target: Option<PathBuf>,
}

/// Anything a worker has to say to the screen.
enum Msg {
    Report(Box<PileReport>),
    /// Work is happening, and this is what it is.
    Progress(String),
    /// A remark for the status line that does NOT mean work is running.
    /// Sending a `Progress` for this would leave the UI busy forever — and a
    /// busy UI takes no orders but quit.
    Note(String),
    Evidence(Vec<evidence::Evidence>),
    Preview(Preview),
    Query(QueryResult),
    Error(String),
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build()?;

    let target = match cli.target {
        Some(t) => t,
        None => discover_target()?,
    };
    let sql = std::fs::read_to_string(&target)
        .with_context(|| format!("cannot read target {}", target.display()))?;
    // Fail before touching the terminal: an unparseable target inside the
    // alternate screen is an error nobody can read.
    tdy::target::Target::parse(&sql)
        .with_context(|| format!("in {}", target.display()))?;

    let mut terminal = ratatui::init();
    // `ratatui::init` installs a panic hook that restores the terminal. That
    // is right for a panic on this thread — and wrong for one on a worker,
    // where it tears the screen down while the draw loop keeps running into
    // it. So the hook also raises a flag the loop checks: a panicking worker
    // means the run is over, and the loop leaves rather than drawing onto a
    // terminal that no longer belongs to it.
    let torn_down = Arc::new(AtomicBool::new(false));
    {
        let flag = torn_down.clone();
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            flag.store(true, Ordering::SeqCst);
            previous(info);
        }));
    }
    let result = rt.block_on(run(&mut terminal, target, sql, torn_down));
    ratatui::restore();
    result
}

/// The one `.tdy.sql` beside the working directory, or an error naming the
/// candidates. A picker screen is the friendlier answer and is deliberately
/// not v1: naming the file is one word, and a menu that appears before you
/// have seen anything is a menu you cannot yet answer.
fn discover_target() -> Result<PathBuf> {
    let mut found: Vec<PathBuf> = std::fs::read_dir(".")
        .context("cannot read the working directory")?
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.to_string_lossy().ends_with(".tdy.sql"))
        .collect();
    found.sort();
    match found.len() {
        1 => Ok(found.remove(0)),
        0 => anyhow::bail!(
            "no .tdy.sql target here. Write one, or draft a scaffold:\n  \
             tdy draft *.csv > sales.tdy.sql"
        ),
        _ => anyhow::bail!(
            "several targets here; name the one you mean:\n{}",
            found.iter().map(|p| format!("  tdy-tui {}", p.display())).collect::<Vec<_>>().join("\n")
        ),
    }
}

async fn run(
    terminal: &mut DefaultTerminal,
    target: PathBuf,
    sql: String,
    torn_down: Arc<AtomicBool>,
) -> Result<()> {
    let cfg = tdy::config::load(&Default::default())?;
    let (tx, mut rx) = mpsc::unbounded_channel::<Msg>();
    let mut app = App::new(target.clone(), sql);

    // The first fit is a DRY RUN. Opening a review tool must not write: a
    // person who opens it to look at a pile and quits should leave the
    // directory exactly as they found it. `f` is the key that writes.
    spawn_fit(&tx, &cfg, &target, Vec::new(), true);

    loop {
        if torn_down.load(Ordering::SeqCst) {
            anyhow::bail!("a background task panicked; the terminal was restored");
        }
        terminal.draw(|f| ui::draw(f, &mut app))?;

        // Drain everything the workers have said, then wait briefly for a
        // key. Polling rather than selecting keeps the loop obvious, and
        // 60 ms is under the threshold where a keystroke feels delayed.
        while let Ok(msg) = rx.try_recv() {
            apply_msg(&mut app, msg);
        }
        if !event::poll(Duration::from_millis(60))? {
            continue;
        }
        let Some(key) = read_key()? else { continue };

        let action = app.handle(key);
        match action {
            Action::None => {}
            Action::Quit => break,
            Action::Refit { accept } => spawn_fit(&tx, &cfg, &target, accept, false),
            Action::WriteTarget { text } => match write_target(&target, &app.target_sql, &text) {
                Ok(()) => {
                    // Only now is the in-memory copy the file's copy. Setting
                    // it before the write would leave the next diff quoting a
                    // "before" line that is not in the file, and smuggle the
                    // failed edit into the following one.
                    app.target_sql = text;
                    spawn_fit(&tx, &cfg, &target, Vec::new(), false);
                }
                Err(e) => app.set_error(format!("{e:#}")),
            },
            Action::ComputeEvidence { member } => {
                spawn_evidence(&tx, &cfg, &app, &member);
            }
            Action::ComputePreview { member } => {
                spawn_preview(&tx, &cfg, &app, &member);
            }
            Action::RunQuery(sql) => spawn_query(&tx, &cfg, sql),
            Action::OpenEditor(path) => {
                // The editor owns the terminal while it runs; taking it back
                // afterwards must restore raw mode and the alternate screen,
                // or every subsequent keystroke lands in the shell. The
                // cursor has to come back too — ratatui hides it, and an
                // editor with no cursor is unusable.
                let _ = terminal.show_cursor();
                ratatui::restore();
                let status = run_editor(&path);
                // `ratatui::init` would install ANOTHER panic hook on top of
                // ours, once per editor session. Rebuild the terminal
                // directly instead.
                let re = reenter();
                terminal.clear()?;
                if let Err(e) = re {
                    anyhow::bail!("cannot take the terminal back after the editor: {e}");
                }
                match status {
                    Ok(()) => {
                        if path == target {
                            match std::fs::read_to_string(&target) {
                                Ok(s) => app.target_sql = s,
                                Err(e) => app.set_error(format!("{e}")),
                            }
                        }
                        app.busy = Some("re-fitting".into());
                        spawn_fit(&tx, &cfg, &target, Vec::new(), false);
                    }
                    Err(e) => app.set_error(format!("{e:#}")),
                }
            }
        }
        if app.should_quit {
            break;
        }
    }
    Ok(())
}

/// Crossterm keys, narrowed to the ones the app understands.
///
/// The `KeyEventKind::Press` guard is what stops one keystroke counting
/// twice on terminals that also report releases.
fn read_key() -> Result<Option<Key>> {
    let Event::Key(k) = event::read()? else { return Ok(None) };
    if k.kind != KeyEventKind::Press {
        return Ok(None);
    }
    // Ctrl-C leaves, wherever you are: a TUI that traps it is a TUI people
    // learn to kill from another window.
    if k.modifiers.contains(KeyModifiers::CONTROL)
        && matches!(k.code, KeyCode::Char('c') | KeyCode::Char('C'))
    {
        return Ok(Some(Key::Interrupt));
    }
    Ok(match k.code {
        KeyCode::Up => Some(Key::Up),
        KeyCode::Down => Some(Key::Down),
        KeyCode::Enter => Some(Key::Enter),
        KeyCode::Esc => Some(Key::Esc),
        KeyCode::Tab => Some(Key::Tab),
        KeyCode::Backspace => Some(Key::Backspace),
        KeyCode::Char(c) => Some(Key::Char(c)),
        _ => None,
    })
}

fn apply_msg(app: &mut App, msg: Msg) {
    match msg {
        Msg::Report(r) => app.set_report(*r),
        Msg::Progress(what) => app.busy = Some(what),
        Msg::Note(what) => app.note(what),
        Msg::Evidence(e) => {
            app.evidence = Some(e);
            app.busy = None;
        }
        Msg::Preview(p) => app.preview = Some(p),
        Msg::Query(q) => {
            app.query_result = Some(q);
            app.busy = None;
        }
        Msg::Error(e) => app.set_error(e),
    }
}

/// Write the target, but only onto the bytes we last read.
///
/// The TUI keeps the declaration in memory to build diffs against. If the
/// file changed underneath — an `$EDITOR` in another window, a `git
/// checkout` — writing our copy would silently discard that work, and the
/// diff the user confirmed was against text that is no longer there.
fn write_target(path: &Path, expected: &str, new_text: &str) -> Result<()> {
    let on_disk = std::fs::read_to_string(path)
        .with_context(|| format!("cannot read {}", path.display()))?;
    if on_disk != expected {
        anyhow::bail!(
            "{} changed since it was read — the diff you confirmed was against older \
             text. Press f to re-read and re-fit, then try again.",
            path.display()
        );
    }
    std::fs::write(path, new_text)
        .with_context(|| format!("cannot write {}", path.display()))
}

fn spawn_fit(
    tx: &mpsc::UnboundedSender<Msg>,
    cfg: &Config,
    target: &Path,
    accept: Vec<String>,
    dry_run: bool,
) {
    let (tx, cfg, target) = (tx.clone(), cfg.clone(), target.to_path_buf());
    tokio::spawn(async move {
        let sink_tx = tx.clone();
        let sink: tdy::progress::Sink = Arc::new(move |e| {
            use tdy::progress::Event;
            let what = match e {
                Event::MemberStarted { path, index, total } => {
                    format!("fitting {path} ({} of {total})", index + 1)
                }
                Event::MemberFinished { .. } => return,
                Event::Consulting { path, backend, model, bytes } => {
                    format!("asking {model} via {backend} about {path} ({bytes} bytes sent)")
                }
            };
            let _ = sink_tx.send(Msg::Progress(what));
        });
        let accept_paths: Vec<PathBuf> = accept.iter().map(PathBuf::from).collect();
        let msg = match tdy::report::fit_pile(
            &target,
            &cfg,
            FitOpts {
                dry_run,
                accept: &accept_paths,
                // The proposals ARE the remedy menu's ranking: which of the
                // file's columns could actually produce the declared type.
                propose: true,
                progress: Some(sink),
            },
        )
        .await
        {
            Ok(r) => Msg::Report(Box::new(r)),
            Err(e) => Msg::Error(format!("{e:#}")),
        };
        let _ = tx.send(msg);
    });
}

/// The spec a member is currently planned with, for the read-only screens.
fn member_spec(app: &App, member: &str) -> Result<(PathBuf, tdy::spec::ParseSpec)> {
    let path = app.target_dir().join(member);
    let spec = tdy::sidecar::load(&path)?
        .fresh_spec()
        .context("this member has no fresh spec yet — re-fit first")?;
    Ok((path, spec))
}

/// A spec good enough to *look* at the file with.
///
/// A refused member has no sidecar — `fit_pile` writes one only for members
/// that fit — and a refused member is exactly the one whose screen most needs
/// to show the file: "no column of this file binds" is a question answered by
/// looking. So when there is no plan, fall back to the sniffer's own view,
/// which is what tdy sees before any target is applied.
fn preview_spec(app: &App, member: &str, limits: tdy::config::Limits) -> Result<(PathBuf, tdy::spec::ParseSpec)> {
    if let Ok(planned) = member_spec(app, member) {
        return Ok(planned);
    }
    let path = app.target_dir().join(member);
    let sample = tdy::sample::build(&path, 16 * 1024, limits)?;
    let sniffed = tdy::sniff::sniff_opts(
        &path,
        &sample,
        limits,
        // The whole-file type check is a fit's business, not a preview's:
        // this only needs the frame and the first rows.
        tdy::sniff::SniffOpts { verify: false },
    )?;
    Ok((path, sniffed.spec))
}

fn spawn_evidence(tx: &mpsc::UnboundedSender<Msg>, cfg: &Config, app: &App, member: &str) {
    let review = app
        .selected_member()
        .and_then(|m| m.review.clone())
        .unwrap_or_default();
    // Whether a model chose the frame is a recorded fact, not something to
    // infer from the wording of the review reason.
    let model_framed = app.selected_member().and_then(|m| m.via.as_deref()) == Some("llm");
    let found = member_spec(app, member);
    let (tx, limits) = (tx.clone(), cfg.limits);
    tokio::task::spawn_blocking(move || {
        let msg = match found.and_then(|(path, spec)| {
            evidence::for_spec(&spec, &path, limits, &review, model_framed)
        }) {
            Ok(e) => Msg::Evidence(e),
            Err(e) => Msg::Error(format!("{e:#}")),
        };
        let _ = tx.send(msg);
    });
}

fn spawn_preview(tx: &mpsc::UnboundedSender<Msg>, cfg: &Config, app: &App, member: &str) {
    let found = preview_spec(app, member, cfg.limits);
    let (tx, limits) = (tx.clone(), cfg.limits);
    tokio::task::spawn_blocking(move || {
        let msg = match found.and_then(|(path, spec)| {
            // Shown as the FILE spells it, and as text. This panel answers
            // "which of these columns supplies my declared column?", and the
            // answer is written into a `matches = '…'` clause — which needs
            // the file's own header, not tdy's sanitised version of it, and
            // the raw value, not the value after a type it has not agreed to
            // yet.
            let spec = tdy::spec::ParseSpec {
                extraction: spec.extraction,
                transforms: spec.transforms,
                columns: spec
                    .columns
                    .iter()
                    .map(|c| tdy::spec::ColumnSpec {
                        name: c.source_name().to_string(),
                        source: Some(c.source_name().to_string()),
                        dtype: tdy::spec::DType::Utf8,
                        nullable: true,
                        parse: tdy::spec::ValueParsing::default(),
                    })
                    .collect(),
                confidence: None,
                notes: vec![],
            };
            let batch = tdy::engine::preview(&spec, &path, limits, 12)?;
            Ok(Preview {
                header: batch
                    .schema()
                    .fields()
                    .iter()
                    .map(|f| f.name().clone())
                    .collect(),
                rows: (0..batch.num_rows())
                    .map(|i| {
                        (0..batch.num_columns())
                            .map(|c| cell(batch.column(c), i))
                            .collect()
                    })
                    .collect(),
            })
        }) {
            Ok(p) => Msg::Preview(p),
            // A preview is a convenience; its failure belongs on the status
            // line, not in place of the gap report the user came to read —
            // and NOT as progress, which would leave the UI busy for good.
            Err(e) => Msg::Note(format!("preview unavailable: {e:#}")),
        };
        let _ = tx.send(msg);
    });
}

/// Does this statement already bound its own result?
///
/// A textual check, deliberately conservative: `limit` inside a string
/// literal is not a LIMIT clause, and anything this misses only costs the
/// scratchpad a bound it would have added anyway.
fn has_limit(sql: &str) -> bool {
    let mut in_str = false;
    let mut code = String::new();
    for c in sql.chars() {
        match c {
            '\'' => in_str = !in_str,
            _ if !in_str => code.push(c.to_ascii_lowercase()),
            _ => {}
        }
    }
    code.split_whitespace().any(|w| w.trim_matches(|c: char| !c.is_alphabetic()) == "limit")
}

fn spawn_query(tx: &mpsc::UnboundedSender<Msg>, cfg: &Config, sql: String) {
    const CAP: usize = 500;
    // A scratchpad is for looking. Without a bound, `SELECT *` over a
    // year of exports materialises every row inside the UI process to show
    // twenty of them — so one is added when the statement has none, and
    // respected when it has its own.
    let sql = if has_limit(&sql) { sql } else { format!("{sql} LIMIT {CAP}") };
    let (tx, cfg) = (tx.clone(), cfg.clone());
    tokio::spawn(async move {
        let msg = match tdy::provider::run_query(&sql, &cfg, false).await {
            Ok((schema, batches)) => {
                let total: usize = batches.iter().map(|b| b.num_rows()).sum();
                let mut rows = Vec::new();
                'outer: for b in &batches {
                    for i in 0..b.num_rows() {
                        if rows.len() >= CAP {
                            break 'outer;
                        }
                        rows.push(
                            (0..b.num_columns()).map(|c| cell(b.column(c), i)).collect(),
                        );
                    }
                }
                Msg::Query(QueryResult {
                    columns: schema.fields().iter().map(|f| f.name().clone()).collect(),
                    truncated: total > rows.len(),
                    rows,
                    total,
                })
            }
            Err(e) => Msg::Error(format!("{e:#}")),
        };
        let _ = tx.send(msg);
    });
}

fn cell(col: &dyn datafusion::arrow::array::Array, i: usize) -> String {
    if col.is_null(i) {
        return String::new();
    }
    datafusion::arrow::util::display::array_value_to_string(col, i).unwrap_or_default()
}

/// Re-enter the alternate screen and raw mode after an editor, without
/// installing a second panic hook (which `ratatui::init` would do, once per
/// editor session, until the stack is deep enough to matter).
fn reenter() -> Result<()> {
    use ratatui::crossterm::terminal::{enable_raw_mode, EnterAlternateScreen};
    ratatui::crossterm::execute!(std::io::stdout(), EnterAlternateScreen)?;
    enable_raw_mode()?;
    Ok(())
}

fn run_editor(path: &Path) -> Result<()> {
    let editor = std::env::var("VISUAL")
        .or_else(|_| std::env::var("EDITOR"))
        .unwrap_or_else(|_| "vi".into());
    // `$EDITOR` may carry arguments ("code -w"), which is why this splits.
    let mut parts = editor.split_whitespace();
    let program = parts.next().unwrap_or("vi");
    let status = std::process::Command::new(program)
        .args(parts)
        .arg(path)
        .status()
        .with_context(|| format!("cannot run {editor:?} (set $EDITOR)"))?;
    if !status.success() {
        anyhow::bail!("{editor} exited with {status}");
    }
    Ok(())
}
