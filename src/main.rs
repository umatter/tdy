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
    #[command(subcommand)]
    command: Command,

    /// Inference backend: none | local | anthropic | openrouter (overrides config/env)
    #[arg(long, global = true)]
    backend: Option<String>,

    /// Model name for the backend
    #[arg(long, global = true)]
    model: Option<String>,

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
}

#[derive(Subcommand)]
enum ConfigAction {
    /// Print a sample config and its expected location.
    Init,
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
fn check_command(target_path: &std::path::Path, files: &[PathBuf]) -> Result<()> {
    use tdy::conform::{judge, Verdict};
    use tdy::target::Target;

    let target = Target::load(target_path)?;
    println!(
        "{}: `{}`, {} column(s)",
        target_path.display(),
        target.name,
        target.columns.len()
    );

    if files.is_empty() {
        // Slice 1 has no lock and does not resolve globs, so there is nothing
        // to check against yet. Say that, rather than silently succeeding.
        println!(
            "\nnothing to check: pass --against <FILE> for each file whose sidecar \
             should be checked.\ndeclared sources: {}",
            if target.files.is_empty() { "(none)".into() } else { target.files.join(", ") }
        );
        return Ok(());
    }

    let mut bad = 0usize;
    for f in files {
        use tdy::sidecar::SidecarStatus;
        let sc = tdy::sidecar::sidecar_path(f);
        let (spec, stale) = match tdy::sidecar::load(f) {
            Ok(SidecarStatus::Fresh(s)) => (s.spec, false),
            // A stale sidecar is still worth checking: the shape it produces
            // is a property of the spec, not of the file's current bytes. Say
            // that it is stale and check it anyway, rather than making the
            // user re-sniff before they can learn their schema is wrong too.
            Ok(SidecarStatus::Stale(s)) => (s.spec, true),
            Ok(SidecarStatus::Absent) => {
                println!(
                    "\n{}: NO SIDECAR — run `tdy sniff {}` first",
                    sc.display(),
                    f.display()
                );
                bad += 1;
                continue;
            }
            Err(e) => {
                println!("\n{}: UNREADABLE — {e:#}", sc.display());
                bad += 1;
                continue;
            }
        };
        // Nothing is fitted to a target yet: `tdy fit` does not exist. Every
        // non-conforming spec is therefore reported as never-fitted rather
        // than as a contradiction, which is the honest reading and keeps a
        // sniffed sidecar's ordinary differences from looking like defects.
        let verdict = judge(&spec, &target, false);
        println!(
            "\n{}: {}{}",
            sc.display(),
            verdict.label(),
            if stale { "  (sidecar is stale: the file changed since it was written)" } else { "" }
        );
        for m in verdict.mismatches() {
            println!("  {}", m.message());
        }
        if let Verdict::Unfitted(m) = &verdict {
            if !m.is_empty() {
                println!(
                    "  (this sidecar was inferred, not fitted to a target — \
                     `tdy fit` will land it once it exists)"
                );
            }
        }
        if !verdict.is_ok() {
            bad += 1;
        }
    }

    println!(
        "\n{} of {} file(s) conform to `{}`.",
        files.len() - bad,
        files.len(),
        target.name
    );
    if bad > 0 {
        anyhow::bail!("{bad} file(s) do not produce the declared schema");
    }
    Ok(())
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

    match cli.command {
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
        Command::Sniff { file, hint, force, no_llm } => {
            let cfg = config::load(&overrides)?;
            provider::sniff_command(&file, &cfg, hint.as_deref(), force, no_llm).await?;
        }
        Command::Validate { file, stamp } => {
            let cfg = config::load(&overrides)?;
            provider::validate_command(&file, &cfg, stamp)?;
        }
        Command::Check { target, against } => {
            check_command(&target, &against)?;
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
