//! The `tdy` CLI.

use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::Result;
use clap::{Parser, Subcommand};

use tdy::config::{self, Overrides};
use tdy::provider::{self, OutputFormat};

#[derive(Parser)]
#[command(
    name = "tdy",
    version,
    about = "Pure SQL over messy files. Parsing lives in auditable sidecar specs, not in your query.",
    after_help = "examples:\n  \
      tdy query \"SELECT region, sum(umsatz_chf) FROM messy('umsatz_2025.xlsx') GROUP BY 1\"\n  \
      tdy query \"SELECT * FROM messy('server.log', 'nginx access log')\" -o out.parquet\n  \
      tdy sniff exports/kunden.csv\n  \
      tdy validate exports/kunden.csv\n  \
      tdy query -f \"SELECT * FROM messy('data.csv')\"   # frozen: sidecars must exist & match"
)]
struct Cli {
    /// With no subcommand: the console (or the workbench, if `tdy-tui` is
    /// installed and this is a terminal).
    #[command(subcommand)]
    command: Option<Command>,

    /// Inference backend: none | local | anthropic | openrouter (overrides config/env)
    #[arg(long, global = true)]
    backend: Option<String>,

    /// Model name for the backend
    #[arg(long, global = true)]
    model: Option<String>,

    /// Emit machine-readable JSON instead of text (sniff, fit, check)
    #[arg(long, global = true)]
    json: bool,

    /// Base URL for the local (OpenAI-compatible) backend
    #[arg(long, global = true)]
    base_url: Option<String>,
}

#[derive(Subcommand)]
enum Command {
    /// Run a SQL query. Tables come from messy('path'[, 'hint']).
    Query {
        /// The SQL text
        sql: String,
        /// Write results to this file (format from extension unless --format)
        #[arg(short, long)]
        output: Option<PathBuf>,
        /// table | csv | json | parquet
        #[arg(long)]
        format: Option<String>,
        /// Frozen mode: no inference, no sidecar writes; every messy() file
        /// must have a fresh sidecar (reproducible CI runs)
        #[arg(short, long)]
        frozen: bool,
    },
    /// Infer (or re-infer) the parsing spec for a file and preview it.
    Sniff {
        file: PathBuf,
        /// Skip checking the inferred types against the whole file.
        ///
        /// Faster on a large file, and the spec it writes may fail on a value
        /// further in. The sidecar records that it was not checked.
        #[arg(long)]
        quick: bool,
        /// Free-text hint passed to the LLM tier
        #[arg(long)]
        hint: Option<String>,
        /// Re-infer even if a fresh sidecar exists
        #[arg(long)]
        force: bool,
        /// Heuristics only, even if a backend is configured
        #[arg(long)]
        no_llm: bool,
    },
    /// Check an existing sidecar: valid spec, matching fingerprint, and it
    /// actually parses the file.
    Validate {
        file: PathBuf,
        /// Re-fingerprint the sidecar against the current file, keeping the
        /// spec (for a hand-edited spec, or after the file legitimately
        /// changed).
        #[arg(long)]
        stamp: bool,
    },
    /// Open the terminal UI (requires `tdy-tui` on PATH).
    ///
    /// A `.tdy.sql` target opens the classic review flow; a data file opens
    /// the workbench rooted at its directory and showing that file; omitted
    /// opens the classic flow on the one discoverable `.tdy.sql` file if
    /// there is exactly one, else the workbench on the working directory.
    ///
    /// A thin shim, the way cargo finds its subcommands: the UI is a separate
    /// binary so that ratatui and crossterm stay out of `tdy`'s dependency
    /// tree, and this is here so nobody has to remember that.
    Ui {
        /// The target .tdy.sql file, a data file to show, or directory. Omit
        /// to use the one discoverable `.tdy.sql` here if there is exactly
        /// one, else the workbench on the current directory.
        target: Option<PathBuf>,
    },
    /// Serve tdy's tools over the Model Context Protocol (stdio).
    ///
    /// For AI agents: the same sniff/draft/fit/check/query/validate surface,
    /// with structured results. Every path is confined to --root. Acceptance
    /// of review-gated members is DISABLED unless --allow-accept is given,
    /// because a review reason is a judgement tdy reserves for a human.
    Mcp {
        /// Directory the server may read; every path must resolve inside it.
        #[arg(long)]
        root: Option<PathBuf>,
        /// Let the connected agent accept review-gated members itself.
        #[arg(long)]
        allow_accept: bool,
    },
    /// Draft a target declaration from a pile of files.
    ///
    /// Sniffs every file and prints a CREATE TABLE covering what it measured:
    /// column names in every spelling seen, merged types, which files carry
    /// which columns. A scaffold to edit, not an answer — the header comment
    /// lists exactly which judgements are left to you.
    Draft {
        /// The files the dataset should cover.
        files: Vec<PathBuf>,
    },
    /// Plan a spec for a file that lands on a declared target schema.
    ///
    /// The inverse of `sniff`: instead of describing whatever the file
    /// contains, this is handed the columns you want and finds, for each one,
    /// a column of this file that produces it — or says why it cannot.
    Fit {
        /// Path to the target .tdy.sql file
        target: PathBuf,
        /// Accept a member whose plan changes values (see the REVIEW line in
        /// `tdy fit`'s output). Repeatable.
        #[arg(long = "accept", value_name = "FILE")]
        accept: Vec<PathBuf>,
        /// The data file to fit. Omit to fit every member the target's globs
        /// match, and write the lock.
        file: Option<PathBuf>,
        /// Print the plan without writing a sidecar
        #[arg(long)]
        dry_run: bool,
        /// For columns nothing binds, suggest which of the file's columns
        /// could produce them, as pasteable SQL. Suggestions only — a
        /// type-compatible column is not necessarily the right one.
        #[arg(long)]
        propose: bool,
    },
    /// Check sidecars against a declared target schema.
    ///
    /// The target is a SQL CREATE TABLE statement declaring the dataset you
    /// want. This proves, without reading a byte of data, that a file's spec
    /// produces exactly those columns with exactly those types.
    Check {
        /// Path to the target .tdy.sql file
        target: PathBuf,
        /// The data file whose sidecar to check (repeatable)
        #[arg(long = "against", value_name = "FILE")]
        against: Vec<PathBuf>,
    },
    /// Print the JSON Schema the LLM is constrained by.
    Schema,
    /// Config helpers.
    Config {
        #[command(subcommand)]
        action: ConfigAction,
    },
    /// The plain console, always (even when the workbench is installed).
    Console,
}

#[derive(Subcommand)]
enum ConfigAction {
    /// Print a sample config and its expected location.
    Init,
}

/// `tdy check --json`: the same gate, as one object a machine can act on.
fn check_json(
    target_path: &std::path::Path,
    target: &tdy::target::Target,
    files: &[PathBuf],
    limits: tdy::config::Limits,
) -> Result<()> {
    use tdy::conform::judge;
    if files.is_empty() {
        let val = if tdy::lockfile::Lock::load(target_path)?.is_none() {
            serde_json::json!({
                "target": target.name, "ready": false,
                "reason": format!("no lock — run `tdy fit {}` first", target_path.display()),
            })
        } else {
            match tdy::dataset::resolve(target_path, limits, None) {
                Ok(resolved) => serde_json::json!({
                    "target": target.name, "ready": true,
                    "members": resolved.members.iter().map(|m| m.rel.clone()).collect::<Vec<_>>(),
                }),
                Err(e) => serde_json::json!({
                    "target": target.name, "ready": false, "reason": format!("{e:#}"),
                }),
            }
        };
        let ready = val["ready"].as_bool().unwrap_or(false);
        println!("{}", serde_json::to_string_pretty(&val)?);
        if !ready {
            anyhow::bail!("dataset `{}` is not ready", target.name);
        }
        return Ok(());
    }

    let mut out = Vec::new();
    let mut bad = 0usize;
    for f in files {
        use tdy::sidecar::SidecarStatus;
        let entry = match tdy::sidecar::load(f) {
            Ok(SidecarStatus::Fresh(sc)) => {
                let v = judge(&sc.spec, target, false);
                let mismatches: Vec<String> =
                    v.mismatches().iter().map(|m| m.message()).collect();
                if !mismatches.is_empty() {
                    bad += 1;
                }
                serde_json::json!({
                    "path": f.display().to_string(),
                    "verdict": v.label().to_ascii_lowercase(),
                    "mismatches": mismatches,
                })
            }
            Ok(SidecarStatus::Stale(_)) => {
                bad += 1;
                serde_json::json!({"path": f.display().to_string(), "verdict": "stale"})
            }
            Ok(SidecarStatus::Absent) => {
                bad += 1;
                serde_json::json!({"path": f.display().to_string(), "verdict": "no_sidecar"})
            }
            Err(e) => {
                bad += 1;
                serde_json::json!({
                    "path": f.display().to_string(),
                    "verdict": "unreadable",
                    "error": format!("{e:#}"),
                })
            }
        };
        out.push(entry);
    }
    println!(
        "{}",
        serde_json::to_string_pretty(&serde_json::json!({
            "target": target.name, "files": out, "failing": bad,
        }))?
    );
    if bad > 0 {
        anyhow::bail!("{bad} file(s) do not conform to `{}`", target.name);
    }
    Ok(())
}

/// `tdy fit <TARGET>` — fit every member and record what they resolved to.
///
/// The orchestration lives in `report::fit_pile`; this renders its report as
/// text (or JSON with `--json`) and turns "any member failed" into a nonzero
/// exit, because a gate that exits zero when it found a problem is not a gate.
async fn fit_dataset(
    target_path: &std::path::Path,
    cfg: &tdy::config::Config,
    dry_run: bool,
    accept: &[PathBuf],
    propose: bool,
    json: bool,
) -> Result<()> {
    let r = tdy::report::fit_pile(
        target_path,
        cfg,
        tdy::report::FitOpts {
            dry_run,
            accept,
            propose,
            // The one thing a terminal user needs unprompted while this runs
            // is that a file is being sent to a model.
            progress: Some(tdy::progress::stderr_sink()),
            root: None,
        },
    )
    .await?;
    if json {
        println!("{}", serde_json::to_string_pretty(&r)?);
    } else {
        print!("{}", tdy::report::render_pile_text(&r));
    }
    if r.failed > 0 {
        // No partial lock. A dataset missing a month is the failure this
        // whole design refuses.
        anyhow::bail!(
            "{} file(s) cannot reach the declared schema; no lock written. \
             Fix them, exclude them, or widen the target.",
            r.failed
        );
    }
    Ok(())
}

/// `tdy fit <TARGET> <FILE>`
///
/// Plans a spec that lands on the declared target, proves it (conformance,
/// then a dry run), and writes the sidecar. On failure it prints every gap
/// rather than the first, because a user fixing a pile wants the whole list.
async fn fit_command(
    target_path: &std::path::Path,
    file: &std::path::Path,
    cfg: &tdy::config::Config,
    dry_run: bool,
    propose: bool,
    json: bool,
) -> Result<()> {
    use tdy::fit::FitError;
    use tdy::target::Target;

    if !json {
        let out = tdy::commands::fit_one_text(
            target_path,
            file,
            cfg,
            dry_run,
            propose,
            Some(&tdy::progress::stderr_sink()),
        )
        .await?;
        print!("{}", out.text);
        if !out.ok {
            if out.gaps {
                anyhow::bail!("no plan reaches the declared schema");
            }
            anyhow::bail!("could not fit {}", file.display());
        }
        return Ok(());
    }

    let target = Target::load(target_path)?;
    match tdy::fit::plan(file, &target, cfg, Some(&tdy::progress::stderr_sink())).await {
        Ok(planned) => {
            let (fitted, method, model) = (planned.fitted, planned.method, planned.model);
            let report = serde_json::json!({
                "path": file.display().to_string(),
                "status": if fitted.review.is_some() { "needs_review" } else { "fits" },
                "via": match method {
                    tdy::spec::InferenceMethod::Llm => "llm",
                    tdy::spec::InferenceMethod::Manual => "manual",
                    tdy::spec::InferenceMethod::Heuristic => "heuristic",
                },
                "sources": fitted.spec.columns.iter().map(|c| serde_json::json!({
                    "column": c.name, "source": c.source_name(),
                })).collect::<Vec<_>>(),
                "review": fitted.review,
                "notes": fitted.spec.notes,
                "dry_run": dry_run,
            });
            if !dry_run {
                tdy::sidecar::save(
                    file,
                    &fitted.spec,
                    tdy::sidecar::ProvenanceInfo {
                        method,
                        model,
                        prompt_version: None,
                        sampled_bytes: None,
                    },
                )?;
            }
            println!("{}", serde_json::to_string_pretty(&report)?);
            Ok(())
        }
        Err(FitError::Gaps(gaps)) => {
            let e = FitError::Gaps(gaps);
            println!(
                "{}",
                serde_json::to_string_pretty(&serde_json::json!({
                    "path": file.display().to_string(),
                    "status": "gaps",
                    "problems": tdy::report::problems_json(&e),
                }))?
            );
            anyhow::bail!("no plan reaches the declared schema")
        }
        Err(e) => {
            print!("{e}");
            anyhow::bail!("could not fit {}", file.display())
        }
    }
}

/// `tdy check <TARGET> --against <FILE>…`
///
/// A CI gate for a question nobody can answer today: *do the sidecars I have
/// still produce the exact columns and types my downstream expects?* It reads
/// no data — the proof is a comparison of `engine::schema_of(spec)` against the
/// target's Arrow schema — so it is a fast check to run on every commit.
///
/// Exits non-zero if any file's spec does not conform, because a gate that
/// exits zero when it found a problem is not a gate.
fn check_command(
    target_path: &std::path::Path,
    files: &[PathBuf],
    limits: tdy::config::Limits,
    json: bool,
) -> Result<()> {
    if json {
        let target = tdy::target::Target::load(target_path)?;
        return check_json(target_path, &target, files, limits);
    }
    let out = tdy::commands::check_text(target_path, files, limits)?;
    print!("{}", out.text);
    if !out.ok {
        anyhow::bail!("{} file(s) do not produce the declared schema", out.bad);
    }
    Ok(())
}

/// Whether `tdy-tui` is on `PATH` — the precondition for opening the
/// workbench instead of the plain console when `tdy` is run with no
/// arguments.
fn workbench_on_path() -> bool {
    std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).any(|d| d.join("tdy-tui").is_file()))
        .unwrap_or(false)
}

/// Hand the terminal to `tdy-tui`, exactly as `tdy ui` does.
fn exec_workbench(target: Option<PathBuf>) -> Result<()> {
    let mut cmd = std::process::Command::new("tdy-tui");
    if let Some(t) = target {
        cmd.arg(t);
    }
    match cmd.status() {
        Ok(st) => std::process::exit(st.code().unwrap_or(1)),
        Err(e) => anyhow::bail!("cannot run tdy-tui: {e}"),
    }
}

#[tokio::main]
async fn main() -> ExitCode {
    // A panic is a bug in tdy, not a bad file. Say so, and say where to send
    // it, instead of printing a bare backtrace at someone who just wanted to
    // read a spreadsheet.
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        eprintln!(
            "\ntdy panicked — this is a bug in tdy, not a problem with your file.\n\
             Please report it with the file shape that triggered it.\n"
        );
        default_hook(info);
    }));

    match run().await {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("Error: {e:#}");
            ExitCode::FAILURE
        }
    }
}

async fn run() -> Result<()> {
    let cli = Cli::parse();
    let overrides = Overrides {
        backend: cli.backend.clone(),
        base_url: cli.base_url.clone(),
        model: cli.model.clone(),
    };

    let command = match cli.command {
        Some(c) => c,
        None => {
            // `tdy` alone opens the console. The design's end state routes a
            // bare `tdy` to the workbench once the workbench IS the console
            // plus panes (slice 2); today's tdy-tui is the target-centric
            // review TUI, which without a target is an error — the wrong
            // thing to land someone in. Until slice 2, the console is the
            // front door and `tdy ui` reaches the TUI explicitly.
            Command::Console
        }
    };

    match command {
        Command::Query { sql, output, format, frozen } => {
            let cfg = config::load(&overrides)?;
            let fmt = match (&format, &output) {
                (Some(f), _) => OutputFormat::parse(f)?,
                // An unrecognised extension used to fall back to printing a
                // table on stdout and writing nothing — a silent no-op that
                // exits 0.
                (None, Some(p)) => OutputFormat::for_output_path(p)?,
                (None, None) => OutputFormat::Table,
            };
            let (schema, batches) = provider::run_query(&sql, &cfg, frozen).await?;
            provider::write_output(&schema, &batches, fmt, output.as_deref())?;
        }
        Command::Sniff { file, hint, force, no_llm, quick } => {
            let cfg = config::load(&overrides)?;
            provider::sniff_command(
                &file,
                &cfg,
                provider::SniffCli {
                    hint: hint.as_deref(),
                    force,
                    no_llm,
                    quick,
                    json: cli.json,
                },
            )
            .await?;
        }
        Command::Validate { file, stamp } => {
            let cfg = config::load(&overrides)?;
            provider::validate_command(&file, &cfg, stamp)?;
        }
        Command::Ui { target } => {
            if !workbench_on_path() {
                anyhow::bail!(
                    "`tdy-tui` is not on PATH. It ships as its own binary so that the \
                     terminal UI's dependencies stay out of tdy. From a source checkout:\n  \
                     cargo install --path tdy-tui\n\
                     (or `cargo install tdy-tui` once it is published)"
                );
            }
            exec_workbench(target)?;
        }
        Command::Console => {
            let cfg = config::load(&overrides)?;
            let mut session = tdy::console::Session::new(std::path::Path::new("."), cfg)?;
            if tdy::console::repl::stdio_is_tty() {
                tdy::console::repl::run_interactive(&mut session).await?;
            } else {
                let stdin = std::io::stdin();
                let mut stdout = std::io::stdout();
                let code = tdy::console::repl::run_batch(&mut session, stdin.lock(), &mut stdout).await?;
                if code != 0 {
                    std::process::exit(code);
                }
            }
        }
        Command::Mcp { root, allow_accept } => {
            let cfg = config::load(&overrides)?;
            tdy::mcp::serve(cfg, root, allow_accept).await?;
        }
        Command::Draft { files } => {
            let cfg = config::load(&overrides)?;
            print!("{}", tdy::draft::draft_target(&files, cfg.limits)?);
        }
        Command::Fit { target, file, accept, dry_run, propose } => {
            let cfg = config::load(&overrides)?;
            match file {
                Some(f) => fit_command(&target, &f, &cfg, dry_run, propose, cli.json).await?,
                None => {
                    fit_dataset(&target, &cfg, dry_run, &accept, propose, cli.json).await?
                }
            }
        }
        Command::Check { target, against } => {
            let cfg = config::load(&overrides)?;
            check_command(&target, &against, cfg.limits, cli.json)?;
        }
        Command::Schema => {
            println!(
                "{}",
                serde_json::to_string_pretty(&tdy::spec::ParseSpec::json_schema())?
            );
        }
        Command::Config { action: ConfigAction::Init } => {
            let path = config::config_file_path()
                .map(|p| p.display().to_string())
                .unwrap_or_else(|| "~/.config/tdy/config.toml".into());
            println!("# write this to {path}\n\n{}", config::SAMPLE_CONFIG);
        }
    }
    Ok(())
}
