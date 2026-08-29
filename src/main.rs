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
