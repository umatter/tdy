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
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::DefaultTerminal;
use tokio::sync::mpsc;

use tdy_tui::app::{Action, App, Key, Preview, QueryResult};
use tdy_tui::browser::Browser;
use tdy_tui::workbench::{WbAction, Workbench};
use tdy_tui::{evidence, ui, wb_ui};
use tdy::config::Config;
use tdy::console::repl::{append_history, load_history};
use tdy::console::{raw_head, spec_summary, Outcome, Payload, RawHead, Session, SpecSummary};
use tdy::report::{FitOpts, PileReport};

#[derive(Parser)]
#[command(
    name = "tdy-tui",
    about = "Review a pile of messy files against a declared schema",
    version
)]
struct Cli {
    /// A `.tdy.sql` target (the classic review flow), a data file (the
    /// workbench, rooted at its directory and showing it), or omitted
    /// entirely (the workbench on the working directory).
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

/// Which flow `main` runs: the classic single-target review, or the
/// workbench (rooted at a directory, optionally with a line to run first).
/// Resolved entirely before the terminal is touched — see the comment below
/// on why that matters for `Mode::Classic`, and it costs `Mode::Workbench`
/// nothing to keep the same property.
#[derive(Debug)]
enum Mode {
    Classic(PathBuf, String),
    Workbench { root: PathBuf, initial: Option<String> },
}

/// What `arg` (the CLI's optional positional target) resolves to. Pulled out
/// of `main` so it can be unit-tested without a terminal: `main` itself
/// can't be tested (it ends by taking over the screen), but everything it
/// decides *before* that can be.
fn choose_mode(arg: Option<PathBuf>) -> Result<Mode> {
    match arg {
        // A directory: the workbench, rooted at the directory itself — not
        // its parent, which is what the data-file arm below would do if
        // this guard did not come first. `is_dir` is checked before the
        // `.tdy.sql` suffix check for the same reason: a directory is never
        // a target no matter what its name ends with.
        Some(t) if t.is_dir() => {
            let root = t.canonicalize().with_context(|| format!("cannot open {}", t.display()))?;
            Ok(Mode::Workbench { root, initial: None })
        }
        Some(t) if t.to_string_lossy().ends_with(".tdy.sql") => {
            let sql = std::fs::read_to_string(&t)
                .with_context(|| format!("cannot read target {}", t.display()))?;
            // Fail before touching the terminal: an unparseable target
            // inside the alternate screen is an error nobody can read.
            tdy::target::Target::parse(&sql).with_context(|| format!("in {}", t.display()))?;
            Ok(Mode::Classic(t, sql))
        }
        Some(f) => {
            // A data file: open the workbench in its directory, showing it.
            let f = f.canonicalize().with_context(|| format!("cannot open {}", f.display()))?;
            let root = f.parent().map(Path::to_path_buf).unwrap_or_else(|| PathBuf::from("."));
            let name = f.file_name().unwrap().to_string_lossy().to_string();
            Ok(Mode::Workbench { root, initial: Some(format!(".show {name}")) })
        }
        None => match discover_target() {
            // Exactly one target here: the classic flow, today's behaviour.
            Ok(t) => {
                let sql = std::fs::read_to_string(&t)?;
                tdy::target::Target::parse(&sql).with_context(|| format!("in {}", t.display()))?;
                Ok(Mode::Classic(t, sql))
            }
            // No target, or several: the workbench is the answer now, not
            // an error — `discover_target`'s own error text is written for
            // the case where naming the file is the only fix, which no
            // longer applies here.
            Err(_) => Ok(Mode::Workbench { root: std::env::current_dir()?, initial: None }),
        },
    }
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build()?;
    let mode = choose_mode(cli.target)?;

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
    let result = match mode {
        Mode::Classic(target, sql) => rt.block_on(run(&mut terminal, target, sql, torn_down)),
        Mode::Workbench { root, initial } => {
            rt.block_on(run_workbench(&mut terminal, root, initial, torn_down))
        }
    };
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
        // This message is never shown: `main` falls into the workbench
        // instead of printing it (see `Mode::Workbench`'s `Err(_)` arm) —
        // kept short now that the draft hint it used to carry is dead text.
        0 => anyhow::bail!("no .tdy.sql target here"),
        _ => anyhow::bail!(
            // `tdy ui` is the documented door and forwards its argument here,
            // so the hint spells that form even when tdy-tui was run directly.
            "several targets here; name the one you mean:\n{}",
            found.iter().map(|p| format!("  tdy ui {}", p.display())).collect::<Vec<_>>().join("\n")
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

// ---------------------------------------------------------------------------
// The workbench (Task 5): a console `Session` runs on its own worker task,
// one line at a time; the UI thread only ever decides (`Workbench::key`,
// `::apply`) and draws. Everything here mirrors `run()`/`Msg`/`apply_msg`
// above by design — the two flows share the same shape on purpose, so a
// change to one is a change someone will think to make to the other.
// ---------------------------------------------------------------------------

/// Anything the console worker has to say to the workbench.
enum WbMsg {
    /// The worker began running this line.
    Started(String),
    /// A line finished. The session's cwd rides along because `.cd` is
    /// ordinary typed grammar: the session can move without the browser
    /// being asked, and a browser descent whose `.cd` the session refused
    /// must roll back. The session is the source of truth; every `Done`
    /// re-roots the browser on it (see `Workbench::apply`).
    Done { outcome: Box<Outcome>, cwd: PathBuf },
    /// Work is happening, and this is what it is.
    Progress(String),
    /// A transient remark that does NOT mean work is running — see `Msg::Note`'s
    /// doc comment; the same trap applies here.
    Note(String),
    /// A `PreviewFile` action's result, computed off the UI thread.
    Preview { path: PathBuf, raw: RawHead, spec: Option<SpecSummary> },
}

/// The worker: owns the one `Session` for this workbench and runs lines from
/// `line_rx` one at a time, in order — the console's own serialization (one
/// statement finishes before the next starts), mirrored here as a plain
/// queue rather than anything fancier. Returns the sender the UI dispatches
/// lines on.
fn spawn_console_worker(
    root: PathBuf,
    cfg: Config,
    tx: mpsc::UnboundedSender<WbMsg>,
) -> mpsc::UnboundedSender<String> {
    let (line_tx, mut line_rx) = mpsc::unbounded_channel::<String>();
    tokio::spawn(async move {
        let mut session = match Session::new(&root, cfg) {
            Ok(s) => s,
            Err(e) => {
                let _ = tx.send(WbMsg::Note(format!("{e:#}")));
                return;
            }
        };
        while let Some(line) = line_rx.recv().await {
            let _ = tx.send(WbMsg::Started(line.clone()));
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
                let _ = sink_tx.send(WbMsg::Progress(what));
            });
            let o = session.run(&line, Some(&sink)).await;
            let quit = session.wants_quit();
            let cwd = session.cwd().to_path_buf();
            let _ = tx.send(WbMsg::Done { outcome: Box::new(o), cwd });
            if quit {
                break;
            }
        }
    });
    line_tx
}

/// `InferenceMethod` as the lowercase word its TOML uses — the same mapping
/// `tdy::console`'s own (private) `method_label` applies, reproduced here
/// because it is not exported and this task does not touch that module.
fn wb_method_label(m: &tdy::spec::InferenceMethod) -> &'static str {
    match m {
        tdy::spec::InferenceMethod::Heuristic => "heuristic",
        tdy::spec::InferenceMethod::Llm => "llm",
        tdy::spec::InferenceMethod::Manual => "manual",
    }
}

/// A `WbAction::PreviewFile`, satisfied off the UI thread: the raw head,
/// plus — if a fresh sidecar exists — the spec it describes. Mirrors what
/// `Command::Show` computes inside a `Session`, but a preview can fire (an
/// arrow key, a completed `.sniff`'s follow-up) without a matching command
/// ever going through the worker, so it is computed directly here instead.
fn spawn_wb_preview(tx: mpsc::UnboundedSender<WbMsg>, cfg: Config, path: PathBuf) {
    tokio::task::spawn_blocking(move || {
        let raw = match raw_head(&path, cfg.limits) {
            Ok(r) => r,
            // A preview is a convenience; its failure belongs on the status
            // line, not in place of whatever the main pane already shows —
            // and NOT as progress, which would leave the UI busy for good.
            Err(e) => {
                let _ = tx.send(WbMsg::Note(format!("preview unavailable: {e:#}")));
                return;
            }
        };
        let spec = match tdy::sidecar::load(&path) {
            Ok(tdy::sidecar::SidecarStatus::Fresh(sc)) => {
                Some(spec_summary(&sc.spec, wb_method_label(&sc.provenance.method), sc.spec.confidence))
            }
            _ => None,
        };
        let _ = tx.send(WbMsg::Preview { path, raw, spec });
    });
}

/// One key, filtered the same way `read_key` filters for the classic flow —
/// but unlike `Key`, the workbench needs the *whole* crossterm event
/// (Ctrl-Up, Ctrl-L, Ctrl-Q all carry modifiers `Key` throws away), so this
/// hands the event back unnarrowed. `Workbench::key` re-checks
/// `KeyEventKind::Press` itself; double-filtering is fine.
fn read_wb_key() -> Result<Option<KeyEvent>> {
    let Event::Key(k) = event::read()? else { return Ok(None) };
    if k.kind != KeyEventKind::Press {
        return Ok(None);
    }
    Ok(Some(k))
}

/// Dispatch one line the way `key()`'s own `WbAction::Dispatch` does: mark
/// busy synchronously, before the line ever reaches the worker (`key()`
/// reads `busy` to gate further input, and the worker's own `Started` round
/// trip is a whole poll cycle away — a fast key burst, or a synthesized
/// refit line right after a write, could otherwise slip a second dispatch
/// past the gate before the first shows busy; `begin` is idempotent, so the
/// `Started` message this same line produces later is a harmless no-op),
/// then hand it to the worker. Shared by the `Dispatch` arm below and by
/// `WriteTarget`'s post-write refit, so both go through one busy/scrollback
/// path rather than two that could drift apart.
fn dispatch_line(wb: &mut Workbench, line: String, line_tx: &mpsc::UnboundedSender<String>) {
    wb.begin(&line);
    // A dead worker (the `Session` failed to build, or the task ended) must
    // not leave the UI busy forever with the error hidden behind the busy
    // text.
    if line_tx.send(line).is_err() {
        wb.worker_died("the console worker is gone — restart the workbench");
    }
}

/// Act on what the workbench decided: send a line to the worker, kick off a
/// preview, run `$EDITOR` — the same suspend/reenter dance the classic
/// flow's `OpenEditor` arm does (see `run`), since a workbench member can be
/// opened for editing too — or write a confirmed remedy edit. `WbAction::
/// None`/`Quit` need nothing here: `Workbench` itself already set
/// `should_quit`, which the caller checks after every action.
fn act_on_wb(
    action: WbAction,
    wb: &mut Workbench,
    terminal: &mut DefaultTerminal,
    line_tx: &mpsc::UnboundedSender<String>,
    preview_tx: &mpsc::UnboundedSender<WbMsg>,
    cfg: &Config,
) -> Result<()> {
    match action {
        WbAction::None | WbAction::Quit => {}
        WbAction::Dispatch(line) => dispatch_line(wb, line, line_tx),
        WbAction::PreviewFile(path) => spawn_wb_preview(preview_tx.clone(), cfg.clone(), path),
        // The remedy overlay's write: the ONE sanctioned non-console write
        // in the workbench (spec §8 rule 2), always behind the shown diff
        // `y` just confirmed. `write_target`'s guard refuses a stale write
        // (the file changed since the diff was staged) rather than clobber
        // it; on success the refit goes through the same `dispatch_line`
        // the console uses, so it lands in the scrollback and busy/history
        // behave exactly as if it had been typed.
        WbAction::WriteTarget { path, expected, new_text, refit } => {
            match write_target(&path, &expected, &new_text) {
                Ok(()) => {
                    wb.note("target written".to_string());
                    dispatch_line(wb, refit, line_tx);
                }
                Err(e) => wb.note(format!("{e:#}")),
            }
        }
        WbAction::Edit(path) => {
            // The editor owns the terminal while it runs; taking it back
            // afterwards must restore raw mode and the alternate screen, or
            // every subsequent keystroke lands in the shell. See `run`'s
            // identical `OpenEditor` arm for the rest of the reasoning.
            let _ = terminal.show_cursor();
            ratatui::restore();
            let status = run_editor(&path);
            let re = reenter();
            terminal.clear()?;
            if let Err(e) = re {
                anyhow::bail!("cannot take the terminal back after the editor: {e}");
            }
            if let Err(e) = status {
                wb.note(format!("{e:#}"));
            }
            // The edit may have changed the file's sidecar status (or
            // nothing at all); either way a refresh is cheap and honest.
            wb.browser.refresh();
        }
    }
    Ok(())
}

/// The workbench loop: same shape as `run()` — 60 ms poll, drain worker
/// messages before reading a key — over a `Workbench` instead of an `App`.
async fn run_workbench(
    terminal: &mut DefaultTerminal,
    root: PathBuf,
    initial: Option<String>,
    torn_down: Arc<AtomicBool>,
) -> Result<()> {
    let cfg = tdy::config::load(&Default::default())?;
    let browser = Browser::new(&root)?;
    let mut wb = Workbench::new(browser, load_history(1000));

    let (tx, mut rx) = mpsc::unbounded_channel::<WbMsg>();
    let line_tx = spawn_console_worker(root, cfg.clone(), tx.clone());
    if let Some(line) = initial {
        if line_tx.send(line).is_err() {
            wb.worker_died("the console worker is gone — restart the workbench");
        }
    }

    loop {
        if torn_down.load(Ordering::SeqCst) {
            anyhow::bail!("a background task panicked; the terminal was restored");
        }
        terminal.draw(|f| wb_ui::draw(f, &mut wb))?;

        // Drain everything the worker has said, then wait briefly for a key
        // — see `run`'s identical comment on why polling rather than
        // selecting, and why 60 ms.
        while let Ok(msg) = rx.try_recv() {
            match msg {
                WbMsg::Started(line) => wb.begin(&line),
                WbMsg::Done { outcome: o, cwd } => {
                    // Taken before `apply` consumes `o`: the history file
                    // gets the echo exactly when `apply`'s own in-memory
                    // recall (`editor.remember`, inside `begin`) would have
                    // — non-empty, and not a buffered-SQL `Continue` — and
                    // "was this a fit" has to be known before `apply` moves
                    // the payload into `Context::Pile`.
                    let echo = o.echo.clone();
                    let is_continue = matches!(o.payload, Payload::Continue);
                    let was_fitted = matches!(o.payload, Payload::Fitted(_));
                    let action = wb.apply(*o, &cwd);
                    if !echo.is_empty() && !is_continue {
                        append_history(&echo);
                    }
                    // A sniff/fit/edit may have changed sidecar status;
                    // refreshing is one read_dir + sidecar headers.
                    wb.browser.refresh();
                    // The remedy menu edits the target's own text, and a
                    // fit is the one moment that text is known to be fresh
                    // (it is what `fit_pile` just re-proved every member
                    // against). Re-read it now rather than lazily on the
                    // first digit press, so a menu opened right after a fit
                    // is never staged against text from before it.
                    if was_fitted {
                        if let Some(target) = wb.pile_target().map(Path::to_path_buf) {
                            match std::fs::read_to_string(&target) {
                                Ok(text) => wb.set_target_sql(text),
                                Err(e) => wb.note(format!("cannot read {}: {e:#}", target.display())),
                            }
                        }
                    }
                    if let Some(action) = action {
                        act_on_wb(action, &mut wb, terminal, &line_tx, &tx, &cfg)?;
                    }
                }
                WbMsg::Progress(what) => wb.progress(what),
                WbMsg::Note(what) => wb.note(what),
                WbMsg::Preview { path, raw, spec } => wb.set_preview(path, raw, spec),
            }
        }
        if wb.should_quit {
            break;
        }

        if !event::poll(Duration::from_millis(60))? {
            continue;
        }
        let Some(key) = read_wb_key()? else { continue };
        let action = wb.key(key);
        act_on_wb(action, &mut wb, terminal, &line_tx, &tx, &cfg)?;
        if wb.should_quit {
            break;
        }
    }
    Ok(())
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
                root: None,
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

#[cfg(test)]
mod tests {
    use super::*;

    fn target_sql() -> &'static str {
        "CREATE TABLE t (a TEXT) WITH (files='*.csv');"
    }

    /// Restores the process's current directory on drop — used by the two
    /// tests below that exercise `choose_mode(None)`, which reads it
    /// (`discover_target`, `std::env::current_dir`). Restoring even when an
    /// assertion inside the test panics keeps a failure in one test from
    /// stranding every test after it in a deleted `tempdir`.
    struct RestoreCwd(PathBuf);
    impl Drop for RestoreCwd {
        fn drop(&mut self) {
            let _ = std::env::set_current_dir(&self.0);
        }
    }

    /// Process current directory is one piece of real OS state shared by
    /// every thread in this test binary (`cargo test` runs `#[test]` fns
    /// concurrently by default) — this serializes the two tests that touch
    /// it, the same problem `console::CWD_LOCK` exists to solve in the root
    /// crate for the same reason.
    static CWD_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn a_tdy_sql_argument_is_classic() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("t.tdy.sql");
        std::fs::write(&p, target_sql()).unwrap();
        let Mode::Classic(path, sql) = choose_mode(Some(p.clone())).unwrap() else {
            panic!("expected Mode::Classic");
        };
        assert_eq!(path, p);
        assert_eq!(sql, target_sql());
    }

    #[test]
    fn a_data_file_argument_roots_the_workbench_at_its_parent_and_shows_it() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("a.csv");
        std::fs::write(&p, "A;B\n1;2\n").unwrap();
        let Mode::Workbench { root, initial } = choose_mode(Some(p)).unwrap() else {
            panic!("expected Mode::Workbench");
        };
        assert_eq!(root, d.path().canonicalize().unwrap());
        assert_eq!(initial.as_deref(), Some(".show a.csv"));
    }

    /// The defect the reviewer caught: a bare directory argument used to
    /// fall into the data-file arm above and root the workbench one level
    /// too high (the directory's *parent*), then dispatch a `.show` of the
    /// directory itself, which fails. The `is_dir` guard in `choose_mode`
    /// must come first and root at the directory, not its parent, with no
    /// initial line.
    #[test]
    fn a_directory_argument_roots_the_workbench_at_itself_not_its_parent() {
        let d = tempfile::tempdir().unwrap();
        let Mode::Workbench { root, initial } = choose_mode(Some(d.path().to_path_buf())).unwrap()
        else {
            panic!("expected Mode::Workbench");
        };
        assert_eq!(root, d.path().canonicalize().unwrap());
        assert_eq!(initial, None);
    }

    #[test]
    fn no_argument_and_no_single_target_opens_the_workbench_on_the_working_directory() {
        let _lock = CWD_TEST_LOCK.lock().unwrap();
        let d = tempfile::tempdir().unwrap();
        let _restore = RestoreCwd(std::env::current_dir().unwrap());
        std::env::set_current_dir(d.path()).unwrap();
        let Mode::Workbench { root, initial } = choose_mode(None).unwrap() else {
            panic!("expected Mode::Workbench");
        };
        assert_eq!(root, d.path().canonicalize().unwrap());
        assert_eq!(initial, None);
    }

    #[test]
    fn no_argument_and_exactly_one_target_is_classic() {
        let _lock = CWD_TEST_LOCK.lock().unwrap();
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("only.tdy.sql");
        std::fs::write(&p, target_sql()).unwrap();
        let _restore = RestoreCwd(std::env::current_dir().unwrap());
        std::env::set_current_dir(d.path()).unwrap();
        let Mode::Classic(path, sql) = choose_mode(None).unwrap() else {
            panic!("expected Mode::Classic");
        };
        assert_eq!(path.file_name().unwrap(), "only.tdy.sql");
        assert_eq!(sql, target_sql());
    }
}
