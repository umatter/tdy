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
use ratatui::crossterm::event::{self, Event, KeyEvent, KeyEventKind};
use ratatui::DefaultTerminal;
use tokio::sync::mpsc;

use tdy_tui::browser::Browser;
use tdy_tui::wb_ui;
use tdy_tui::workbench::{WbAction, Workbench};
use tdy::config::Config;
use tdy::console::repl::{append_history, load_history};
use tdy::console::{raw_head, spec_summary, Outcome, Payload, RawHead, Session, SpecSummary};

#[derive(Parser)]
#[command(
    name = "tdy-tui",
    about = "Review a pile of messy files against a declared schema",
    version
)]
struct Cli {
    /// A `.tdy.sql` target (the workbench, rooted at its directory, with the
    /// first fit dispatched as a dry run), a data file (the workbench,
    /// rooted at its directory and showing it), or omitted entirely (the
    /// workbench on the working directory).
    target: Option<PathBuf>,
}

/// Which flow `main` runs — today just the one, the workbench (rooted at a
/// directory, optionally with a line to run first). Kept as an enum rather
/// than the two fields alone because `choose_mode` is what decides this
/// *before* the terminal is touched, and matching on it at the one call
/// site in `main` is what keeps that ordering visible there.
#[derive(Debug)]
enum Mode {
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
        Some(t) if t.to_string_lossy().ends_with(".tdy.sql") => dry_run_target_mode(t),
        Some(f) => {
            // A data file: open the workbench in its directory, showing it.
            let f = f.canonicalize().with_context(|| format!("cannot open {}", f.display()))?;
            let root = f.parent().map(Path::to_path_buf).unwrap_or_else(|| PathBuf::from("."));
            let name = f.file_name().unwrap().to_string_lossy().to_string();
            Ok(Mode::Workbench { root, initial: Some(format!(".show {name}")) })
        }
        None => {
            // The one `.tdy.sql` beside the working directory, if there is
            // exactly one — a picker screen is the friendlier answer for
            // zero or several, and deliberately not v1: naming the file is
            // one word, and a menu that appears before you have seen
            // anything is a menu you cannot yet answer. An unreadable
            // directory falls into the same "plain workbench" arm as zero
            // or several targets, rather than erroring here.
            let mut found: Vec<PathBuf> = std::fs::read_dir(".")
                .into_iter()
                .flatten()
                .flatten()
                .map(|e| e.path())
                .filter(|p| p.to_string_lossy().ends_with(".tdy.sql"))
                .collect();
            found.sort();
            match found.len() {
                1 => dry_run_target_mode(found.remove(0)),
                _ => Ok(Mode::Workbench { root: std::env::current_dir()?, initial: None }),
            }
        }
    }
}

/// A `.tdy.sql` target, named on the command line or the lone one found in
/// the working directory: the workbench rooted at its directory, with the
/// first fit synthesized as a DRY RUN. Opening a review tool to look must
/// not write — a person who opens it, looks at a pile, and quits should
/// leave the directory exactly as they found it; `f` is the key that writes
/// for real (see `Workbench::refit_pile`).
///
/// `--propose` rides along because the proposals ARE the remedy menu's
/// ranking (spec §7: "the remedy menu ranked by `--propose`"), and without
/// them a refused member's menu is the file's header in file order. It is
/// read-only analysis — `report::fit_pile` calls `fit::propose`, which
/// samples, frames and probes and writes nothing — so it cannot compromise
/// the dry run. The two flags are independent switches in the console
/// grammar (`console::parse`), so the order they appear in is free.
fn dry_run_target_mode(t: PathBuf) -> Result<Mode> {
    let t = t.canonicalize().with_context(|| format!("cannot open {}", t.display()))?;
    let sql = std::fs::read_to_string(&t)
        .with_context(|| format!("cannot read target {}", t.display()))?;
    // Fail before touching the terminal: an unparseable target inside the
    // alternate screen is an error nobody can read.
    tdy::target::Target::parse(&sql).with_context(|| format!("in {}", t.display()))?;
    let root = t.parent().map(Path::to_path_buf).unwrap_or_else(|| PathBuf::from("."));
    let name = t.file_name().unwrap().to_string_lossy().to_string();
    Ok(Mode::Workbench {
        root,
        initial: Some(format!(".fit {} --dry-run --propose", quote_name(&name))),
    })
}

/// Quote a bare file name the way the console's tokenizer reads it back —
/// mirrors `workbench::quote_rel` (private, and this is a separate crate
/// from the library), Debug-quoting only when the name contains whitespace.
fn quote_name(s: &str) -> String {
    if s.chars().any(char::is_whitespace) {
        format!("{s:?}")
    } else {
        s.to_string()
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
    let Mode::Workbench { root, initial } = mode;
    let result = rt.block_on(run_workbench(&mut terminal, root, initial, torn_down));
    ratatui::restore();
    result
}

// ---------------------------------------------------------------------------
// The workbench: a console `Session` runs on its own worker task, one line
// at a time; the UI thread only ever decides (`Workbench::key`, `::apply`)
// and draws.
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
    /// A transient remark that does NOT mean work is running. Sending a
    /// `Progress` for this would leave the UI busy forever — a busy UI
    /// takes no orders but quit.
    Note(String),
    /// A `PreviewFile` action's result, computed off the UI thread. `gen` is
    /// the `preview_gen` the request was spawned for — `Workbench::
    /// set_preview` drops anything that no longer matches the current
    /// counter, since a slower, older request finishing after a fresher one
    /// must not clobber it. `stale` is `SidecarStatus::Stale` (a sidecar
    /// exists but its fingerprint no longer matches the file) — `spec`
    /// stays `None` for it exactly as before, this only adds the flag the
    /// footer needs to say why.
    Preview { gen: u64, path: PathBuf, raw: RawHead, spec: Option<SpecSummary>, stale: bool },
    /// A `PreviewFile` action FAILED, computed off the UI thread. Same
    /// `gen`/`path` staleness rules as `Preview` (see `Workbench::
    /// preview_failed`) — sent instead of a bare `Note`, which left the
    /// Member/File pane showing "loading…" forever with no way to tell why.
    PreviewFailed { gen: u64, path: PathBuf, msg: String },
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
                    Event::Note(t) => {
                        let _ = sink_tx.send(WbMsg::Note(t));
                        return;
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
fn spawn_wb_preview(tx: mpsc::UnboundedSender<WbMsg>, cfg: Config, path: PathBuf, gen: u64) {
    tokio::task::spawn_blocking(move || {
        let raw = match raw_head(&path, cfg.limits) {
            Ok(r) => r,
            // A preview is a convenience; its failure belongs on the status
            // line AND in the pane it was meant to fill — a bare `Note`
            // (the previous behaviour) left the Member/File pane reading
            // "loading…" forever with no way to tell why. NOT as progress,
            // either, which would leave the UI busy for good.
            Err(e) => {
                let _ = tx.send(WbMsg::PreviewFailed { gen, path, msg: format!("{e:#}") });
                return;
            }
        };
        let mut stale = false;
        let spec = match tdy::sidecar::load(&path) {
            Ok(tdy::sidecar::SidecarStatus::Fresh(sc)) => {
                Some(spec_summary(&sc.spec, wb_method_label(&sc.provenance.method), sc.spec.confidence))
            }
            // Kept as `None`, exactly as before — the footer is what
            // distinguishes this from "never sniffed" now, not the spec.
            Ok(tdy::sidecar::SidecarStatus::Stale(_)) => {
                stale = true;
                None
            }
            _ => None,
        };
        let _ = tx.send(WbMsg::Preview { gen, path, raw, spec, stale });
    });
}

/// One key, filtered down to a real press: the workbench needs the *whole*
/// crossterm event (Ctrl-Up, Ctrl-L, Ctrl-Q all carry modifiers a narrower
/// type would throw away), so this hands the event back unnarrowed.
/// `Workbench::key` re-checks `KeyEventKind::Press` itself; double-filtering
/// is fine.
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
/// preview, run `$EDITOR` — suspending the terminal and re-entering it
/// afterwards, since a workbench member can be opened for editing too — or
/// write a confirmed remedy edit. `WbAction::None`/`Quit` need nothing
/// here: `Workbench` itself already set `should_quit`, which the caller
/// checks after every action.
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
        // `wb.preview_gen` was already bumped by the `Workbench` method that
        // produced this very action (`preview_action` — see its doc
        // comment), so it already names the generation this request is for.
        WbAction::PreviewFile(path) => {
            spawn_wb_preview(preview_tx.clone(), cfg.clone(), path, wb.preview_gen)
        }
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
            // every subsequent keystroke lands in the shell. The cursor has
            // to come back too — ratatui hides it, and an editor with no
            // cursor is unusable.
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
            after_editing(wb, &path);
        }
    }
    Ok(())
}

/// The workbench loop: draw, drain whatever the worker has said, poll for a
/// key (60 ms — under the threshold where a keystroke feels delayed), act.
async fn run_workbench(
    terminal: &mut DefaultTerminal,
    root: PathBuf,
    initial: Option<String>,
    torn_down: Arc<AtomicBool>,
) -> Result<()> {
    let cfg = tdy::config::load(&Default::default())?;
    let browser = Browser::new(&root)?;
    let mut wb = Workbench::new(browser, load_history(1000), cfg.confidence_threshold);

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

        // Drain everything the worker has said, then wait briefly for a
        // key. Polling rather than selecting keeps the loop obvious, and
        // 60 ms is under the threshold where a keystroke feels delayed.
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
                WbMsg::Preview { gen, path, raw, spec, stale } => {
                    wb.set_preview(gen, path, raw, spec, stale)
                }
                WbMsg::PreviewFailed { gen, path, msg } => wb.preview_failed(gen, path, msg),
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

/// `$EDITOR` has returned. If what was edited is the pile's own target,
/// two things must happen before the next keystroke.
///
/// **Say the lock is stale** (spec §8 rule 2: "`.edit` is the honest
/// exception… on return the browser status updates and the console notes
/// 'target edited; lock is stale — `.fit` to re-prove'"). The note is
/// emitted here rather than as the `Outcome`'s text because
/// `Command::Edit`'s arm in `console::dispatch` returns `Payload::Edit`
/// *before* the editor runs — the console cannot know what the editor did,
/// or whether it even exited cleanly. The runtime is the only place that
/// knows the editor has returned, so it is the honest place to say so.
///
/// **Re-read the declaration.** The remedy menu stages its edits against
/// `wb.target_sql`, and `write_target`'s guard refuses to write when the
/// file no longer matches that text. Leaving it stale would turn the very
/// next remedy digit into a refusal ("changed since it was read"), which is
/// safe but useless; re-reading makes the next remedy stage against what
/// the human just wrote. A read failure is reported, not swallowed — and
/// leaves the old text in place, where the guard will catch it.
fn after_editing(wb: &mut Workbench, edited: &Path) {
    let Some(target) = wb.pile_target() else { return };
    // Compare canonically: the dispatched `.edit` line is spelled relative
    // to the session's cwd, the context's target is absolute.
    let same = match (target.canonicalize(), edited.canonicalize()) {
        (Ok(a), Ok(b)) => a == b,
        _ => target == edited,
    };
    if !same {
        return;
    }
    let target = target.to_path_buf();
    match std::fs::read_to_string(&target) {
        Ok(text) => {
            wb.set_target_sql(text);
            wb.note("target edited; lock is stale — `.fit` to re-prove".to_string());
        }
        Err(e) => wb.note(format!("cannot read {}: {e:#}", target.display())),
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
    // Temp + rename, the same way every sidecar is written: a target
    // truncated by a crash or a full disk mid-write is a declaration that
    // no longer parses, and the lock beside it still claims twelve proofs
    // against it.
    tdy::fileio::atomic_write(path, new_text)
        .with_context(|| format!("cannot write {}", path.display()))
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

    /// A named `.tdy.sql` argument opens the workbench rooted at the
    /// target's own directory, with the first fit synthesized as a DRY RUN
    /// — opening a review tool to look must not write; `f` is the key that
    /// writes for real.
    #[test]
    fn a_tdy_sql_argument_opens_the_workbench_with_a_dry_run_fit() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("t.tdy.sql");
        std::fs::write(&p, target_sql()).unwrap();
        let Mode::Workbench { root, initial } = choose_mode(Some(p.clone())).unwrap();
        assert_eq!(root, d.path().canonicalize().unwrap());
        assert_eq!(initial.as_deref(), Some(".fit t.tdy.sql --dry-run --propose"));
    }

    #[test]
    fn a_data_file_argument_roots_the_workbench_at_its_parent_and_shows_it() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("a.csv");
        std::fs::write(&p, "A;B\n1;2\n").unwrap();
        let Mode::Workbench { root, initial } = choose_mode(Some(p)).unwrap();
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
        let Mode::Workbench { root, initial } =
            choose_mode(Some(d.path().to_path_buf())).unwrap();
        assert_eq!(root, d.path().canonicalize().unwrap());
        assert_eq!(initial, None);
    }

    #[test]
    fn no_argument_and_no_single_target_opens_the_workbench_on_the_working_directory() {
        let _lock = CWD_TEST_LOCK.lock().unwrap();
        let d = tempfile::tempdir().unwrap();
        let _restore = RestoreCwd(std::env::current_dir().unwrap());
        std::env::set_current_dir(d.path()).unwrap();
        let Mode::Workbench { root, initial } = choose_mode(None).unwrap();
        assert_eq!(root, d.path().canonicalize().unwrap());
        assert_eq!(initial, None);
    }

    #[test]
    fn no_argument_and_exactly_one_target_opens_the_workbench_with_a_dry_run_fit() {
        let _lock = CWD_TEST_LOCK.lock().unwrap();
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("only.tdy.sql");
        std::fs::write(&p, target_sql()).unwrap();
        let _restore = RestoreCwd(std::env::current_dir().unwrap());
        std::env::set_current_dir(d.path()).unwrap();
        let Mode::Workbench { root, initial } = choose_mode(None).unwrap();
        assert_eq!(root, d.path().canonicalize().unwrap());
        assert_eq!(initial.as_deref(), Some(".fit only.tdy.sql --dry-run --propose"));
    }

    /// However the single target was found — named on the command line, or
    /// the lone `*.tdy.sql` discovered in the working directory — the fit
    /// `main` dispatches first must be a dry run: opening a review tool to
    /// look and quitting must leave the directory exactly as found. It must
    /// also ask for `--propose`, because the proposals are what ranks a
    /// refused member's remedy menu (spec §7); the flags are independent
    /// switches, so this asserts both are *present* rather than pinning an
    /// order the grammar does not care about.
    #[test]
    fn the_initial_fit_line_is_always_a_dry_run_that_proposes() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("t.tdy.sql");
        std::fs::write(&p, target_sql()).unwrap();
        let Mode::Workbench { initial, .. } = choose_mode(Some(p)).unwrap();
        let line = initial.expect("expected an initial line");
        assert!(line.starts_with(".fit "), "{line}");
        assert!(line.contains("--dry-run"), "{line}");
        assert!(line.contains("--propose"), "{line}");
    }

    /// …and the console grammar really accepts the two together — asserted
    /// against `console::parse` itself rather than by reading the flag
    /// table, so a grammar change that made them exclusive fails here
    /// instead of at the next launch.
    #[test]
    fn the_initial_fit_line_parses_as_a_dry_run_with_proposals() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("t.tdy.sql");
        std::fs::write(&p, target_sql()).unwrap();
        let Mode::Workbench { initial, .. } = choose_mode(Some(p)).unwrap();
        let line = initial.expect("expected an initial line");
        assert_eq!(
            tdy::console::parse(&line).expect("the console must accept the launch line"),
            tdy::console::Command::Fit {
                target: "t.tdy.sql".into(),
                file: None,
                dry_run: true,
                propose: true,
            }
        );
    }
}
