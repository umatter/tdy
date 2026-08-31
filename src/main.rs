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
}

#[derive(Subcommand)]
enum ConfigAction {
    /// Print a sample config and its expected location.
    Init,
}

/// Suggestions for columns nothing bound, as pasteable SQL.
fn print_proposals(
    file: &std::path::Path,
    target: &tdy::target::Target,
    limits: tdy::config::Limits,
) {
    let Ok(proposals) = tdy::fit::propose(file, target, limits) else { return };
    for p in &proposals {
        let existing: Vec<String> = target
            .columns
            .iter()
            .find(|c| c.name == p.column)
            .map(|c| std::iter::once(c.name.clone()).chain(c.matches.iter().cloned()).collect())
            .unwrap_or_default();
        println!("    `{}` ({}):", p.column, p.want);
        for line in p.message(&existing).lines() {
            println!("      {line}");
        }
    }
}

/// `tdy fit <TARGET>` — fit every member and record what they resolved to.
///
/// Plans every file the globs match, prints every gap rather than stopping at
/// the first (a twelve-file dataset should not be a twelve-round game), writes
/// a sidecar for each member that fits, and writes the lock only if *all* of
/// them did. A partial lock would be a dataset that silently omits a month.
async fn fit_dataset(
    target_path: &std::path::Path,
    cfg: &tdy::config::Config,
    dry_run: bool,
    accept: &[PathBuf],
    propose: bool,
) -> Result<()> {
    let limits = cfg.limits;
    use tdy::fit::FitError;
    use tdy::lockfile::{self, Lock, Member, LOCK_VERSION};
    use tdy::target::Target;

    let target = Target::load(target_path)?;
    let dir = lockfile::target_dir(target_path);
    let rels = lockfile::resolve(&target, target_path)?;

    if rels.is_empty() {
        anyhow::bail!(
            "no files matched {:?} beside {}",
            target.files,
            target_path.display()
        );
    }

    println!(
        "{}: {} file(s) match, {} declared column(s)\n",
        target.name,
        rels.len(),
        target.columns.len()
    );

    // A previous lock's acceptances carry over for entries that have not
    // changed — drift is what expires them, so re-fitting an untouched
    // dataset must not ask the same question twice.
    let previous = Lock::load(target_path)?;
    // A member is identified by its path *relative to the target*, so that is
    // what --accept must name. Matching on the basename accepted the wrong
    // file when two directories held the same name, and could never accept a
    // member in a subdirectory at all.
    let accepted_now: Vec<String> = accept
        .iter()
        .map(|a| {
            let a = a.strip_prefix(&dir).unwrap_or(a);
            a.to_string_lossy().replace('\\', "/")
        })
        .collect();
    for a in &accepted_now {
        if !rels.contains(a) {
            anyhow::bail!(
                "--accept {a:?} is not a member of `{}`. Members are named relative to the \
                 target: {}",
                target.name,
                rels.iter().take(6).map(|r| format!("{r:?}")).collect::<Vec<_>>().join(", ")
            );
        }
    }

    let mut members = Vec::new();
    let mut failed = 0usize;
    let mut needs_review = 0usize;
    for rel in &rels {
        let p = dir.join(rel);
        // A fresh sidecar that still conforms IS the plan, whoever wrote it.
        // A hand-written one is a human assertion the planner must never
        // overwrite (a contradiction is an error, not a replan); a
        // tool-written one is reused because the acceptance machinery is
        // about *that recorded plan* — replanning on every run would let a
        // nondeterministic model quietly swap the frame out from under a
        // review, and it would re-spend money answering a settled question.
        // Either way it is re-proved: conformance and a dry run, every time.
        if let Ok(tdy::sidecar::SidecarStatus::Fresh(sc)) = tdy::sidecar::load(&p) {
            let manual = sc.provenance.method == tdy::spec::InferenceMethod::Manual;
            let conforming = tdy::conform::conforms(&sc.spec, &target).is_ok();
            if manual || conforming {
                let spec = sc.spec;
                let label = match sc.provenance.method {
                    tdy::spec::InferenceMethod::Manual => "(hand-written spec)",
                    tdy::spec::InferenceMethod::Llm => "(model-framed spec)",
                    tdy::spec::InferenceMethod::Heuristic => "(existing spec)",
                };
                if let Err(m) = tdy::conform::conforms(&spec, &target) {
                    failed += 1;
                    println!("  {rel:<24} CONTRADICTS  {label}");
                    for x in &m {
                        println!("      {}", x.message());
                    }
                    continue;
                }
                if let Err(e) = tdy::engine::dry_run(&spec, &p, limits) {
                    failed += 1;
                    println!("  {rel:<24} ERROR  {label}: {e:#}");
                    continue;
                }
                let review = {
                    let mut rs = tdy::fit::review_reasons(&spec);
                    // A model-framed plan's judgement is recorded in its
                    // provenance, not in the spec: reconstruct it, or the
                    // review gate would evaporate on the second `tdy fit`.
                    if sc.provenance.method == tdy::spec::InferenceMethod::Llm {
                        rs.push(tdy::fit::llm_frame_reason(
                            &spec,
                            sc.provenance.model.as_deref().unwrap_or("a model"),
                        ));
                    }
                    (!rs.is_empty()).then(|| rs.join("; "))
                };
                let (blake3, bytes) = tdy::sidecar::hash_file(&p)?;
                let carried = previous
                    .as_ref()
                    .and_then(|l| l.member(rel))
                    .filter(|m| m.blake3 == blake3 && m.review == review)
                    .map(|m| m.accepted)
                    .unwrap_or(false);
                let is_accepted = carried || accepted_now.iter().any(|a| a == rel);
                match (&review, is_accepted) {
                    (Some(r), false) => {
                        needs_review += 1;
                        println!("  {rel:<24} REVIEW  {label}");
                        println!("      {r}");
                        println!(
                            "      tdy does not accept a value-changing step on its own \
                             judgement."
                        );
                        println!("      Accept:  tdy fit {} --accept {rel}", target_path.display());
                    }
                    (Some(_), true) => println!("  {rel:<24} accepted  {label}"),
                    (None, _) => println!("  {rel:<24} fits      {label}"),
                }
                members.push(Member {
                    path: rel.clone(),
                    blake3,
                    bytes,
                    spec_digest: lockfile::spec_digest(&p),
                    review,
                    accepted: is_accepted,
                });
                continue;
            }
        }
        match tdy::fit::plan(&p, &target, cfg).await {
            Ok(planned) => {
                let (fitted, method, model) = (planned.fitted, planned.method, planned.model);
                let sources: Vec<String> = fitted
                    .spec
                    .columns
                    .iter()
                    .map(|c| format!("{}<-{:?}", c.name, c.source_name()))
                    .collect();
                let via = match method {
                    tdy::spec::InferenceMethod::Llm => "fits ~ ",
                    _ => "fits    ",
                };
                println!("  {rel:<24} {via}{}", sources.join("  "));
                if !dry_run {
                    tdy::sidecar::save(
                        &p,
                        &fitted.spec,
                        tdy::sidecar::ProvenanceInfo {
                            method,
                            model: model.clone(),
                            prompt_version: None,
                            sampled_bytes: None,
                        },
                    )?;
                }
                let (blake3, bytes) = tdy::sidecar::hash_file(&p)?;
                let carried = previous
                    .as_ref()
                    .and_then(|l| l.member(rel))
                    .filter(|m| m.blake3 == blake3 && m.review == fitted.review)
                    .map(|m| m.accepted)
                    .unwrap_or(false);
                let is_accepted = carried || accepted_now.iter().any(|a| a == rel);
                if let (Some(r), false) = (&fitted.review, is_accepted) {
                    needs_review += 1;
                    println!("      REVIEW: {r}");
                    println!(
                        "      Accept:  tdy fit {} --accept {rel}",
                        target_path.display()
                    );
                }
                members.push(Member {
                    path: rel.clone(),
                    blake3,
                    bytes,
                    spec_digest: lockfile::spec_digest(&p),
                    review: fitted.review.clone(),
                    accepted: is_accepted,
                });
            }
            Err(FitError::Gaps(gaps)) => {
                failed += 1;
                println!("  {rel:<24} GAP");
                for g in &gaps {
                    for line in g.message().lines() {
                        println!("      {line}");
                    }
                }
                if propose {
                    print_proposals(&p, &target, limits);
                }
            }
            Err(e) => {
                failed += 1;
                println!("  {rel:<24} ERROR");
                for line in format!("{e}").lines() {
                    println!("      {line}");
                }
            }
        }
    }

    println!(
        "\n{} of {} file(s) fit `{}`.",
        members.len(),
        rels.len(),
        target.name
    );

    if needs_review > 0 {
        println!(
            "{needs_review} member(s) need a human before they can join. \
             Nothing is wrong with them mechanically — that is the point."
        );
    }
    if failed > 0 {
        // No partial lock. A dataset missing a month is the failure this
        // whole design refuses, and writing one here would make it the
        // default outcome of a bad afternoon.
        anyhow::bail!(
            "{failed} file(s) cannot reach the declared schema; no lock written. \
             Fix them, exclude them, or widen the target."
        );
    }
    if dry_run {
        println!("--dry-run: no sidecars and no lock written.");
        return Ok(());
    }

    let lock = Lock {
        lock_version: LOCK_VERSION,
        target: target.name.clone(),
        target_hash: lockfile::target_hash(&target),
        tool_version: env!("CARGO_PKG_VERSION").to_string(),
        created_at: tdy::sidecar::now_rfc3339(),
        members,
    };
    let p = lock.save(target_path)?;
    println!("wrote {}", p.display());
    println!(
        "\nQuery it:  tdy query \"SELECT * FROM dataset('{}')\"",
        target_path.display()
    );
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
) -> Result<()> {
    let limits = cfg.limits;
    use tdy::fit::FitError;
    use tdy::target::Target;

    let target = Target::load(target_path)?;
    match tdy::fit::plan(file, &target, cfg).await {
        Ok(planned) => {
            let (fitted, method, model) = (planned.fitted, planned.method, planned.model);
            println!("{} fits `{}`:", file.display(), target.name);
            for c in &fitted.spec.columns {
                println!(
                    "  {:<16} <- {:<24} {}",
                    c.name,
                    format!("{:?}", c.source_name()),
                    describe(&c.dtype)
                );
            }
            for n in fitted.spec.notes.iter().filter(|n| !tdy::fit::is_binding_note(n)) {
                println!("  note: {n}");
            }
            if dry_run {
                println!("\n--dry-run: nothing written.");
                return Ok(());
            }
            if let Some(r) = &fitted.review {
                println!("  REVIEW: {r}");
            }
            let path = tdy::sidecar::save(
                file,
                &fitted.spec,
                tdy::sidecar::ProvenanceInfo {
                    method,
                    model,
                    prompt_version: None,
                    sampled_bytes: None,
                },
            )?;
            println!("\nwrote {}", path.display());
            Ok(())
        }
        Err(FitError::Gaps(gaps)) => {
            println!("{} cannot reach `{}`:\n", file.display(), target.name);
            print!("{}", FitError::Gaps(gaps));
            if propose {
                println!("  suggestions:");
                print_proposals(file, &target, limits);
            }
            anyhow::bail!("no plan reaches the declared schema")
        }
        Err(e) => {
            print!("{e}");
            anyhow::bail!("could not fit {}", file.display())
        }
    }
}

/// A column's type, in the language the target is written in.
fn describe(d: &tdy::spec::DType) -> String {
    use tdy::spec::DType;
    match d {
        DType::Utf8 => "TEXT".into(),
        DType::Bool => "BOOLEAN".into(),
        DType::Int64 => "BIGINT".into(),
        DType::Float64 => "DOUBLE".into(),
        DType::Decimal { precision, scale } => format!("DECIMAL({precision},{scale})"),
        DType::Date { format } => format!("DATE  ({format})"),
        DType::Timestamp { format, timezone } => match timezone {
            Some(tz) => format!("TIMESTAMP  ({format}, {tz})"),
            None => format!("TIMESTAMP  ({format})"),
        },
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
) -> Result<()> {
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
        // With a lock, the dataset itself is what CI wants checked, and
        // `dataset::resolve` runs exactly the checks a query would: drift,
        // every member's sidecar present and fresh, every member still
        // conforming, nothing waiting on a human. Reusing it is what keeps
        // the gate and the query from disagreeing.
        //
        // Without a lock there is nothing to check, and saying so beats
        // exiting zero on a target nobody has fitted.
        let lock = tdy::lockfile::Lock::load(target_path)?;
        if lock.is_none() {
            println!(
                "\nnothing to check: `{}` has no lock. Run `tdy fit {}` first, or pass \
                 --against <FILE> to check a single sidecar.\ndeclared sources: {}",
                target.name,
                target_path.display(),
                if target.files.is_empty() { "(none)".into() } else { target.files.join(", ") }
            );
            return Ok(());
        }
        let resolved = tdy::dataset::resolve(target_path, limits)?;
        println!("\n{} member(s), all conforming:", resolved.members.len());
        for m in &resolved.members {
            println!("  {:<28} OK", m.rel);
        }
        println!("\n`{}` is ready to query.", target.name);
        return Ok(());
    }

    let mut bad = 0usize;
    for f in files {
        use tdy::sidecar::SidecarStatus;
        let sc = tdy::sidecar::sidecar_path(f);
        let (spec, stale) = match tdy::sidecar::load(f) {
            Ok(SidecarStatus::Fresh(s)) => (s.spec, false),
            // A stale sidecar is still worth *checking* — the shape it
            // produces is a property of the spec, not of the file's current
            // bytes — but it must not pass. Every other consumer treats stale
            // as fatal: `validate` bails, `--frozen` bails, and a non-frozen
            // query throws the spec away and re-sniffs. So the spec this would
            // otherwise bless is one no query will ever use, and going green
            // on it means going green on exactly the drift this gate exists to
            // catch.
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
        if stale {
            println!(
                "\n{}: STALE — the file has changed since this spec was written, so this \
                 is not the spec a query would use.\n  Re-sniff it, or \
                 `tdy validate --stamp` if the spec is still right, then check again.",
                sc.display()
            );
            bad += 1;
        } else {
            println!("\n{}: {}", sc.display(), verdict.label());
        }
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
        if !verdict.is_ok() && !stale {
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
        Command::Sniff { file, hint, force, no_llm, quick } => {
            let cfg = config::load(&overrides)?;
            provider::sniff_command(&file, &cfg, hint.as_deref(), force, no_llm, quick).await?;
        }
        Command::Validate { file, stamp } => {
            let cfg = config::load(&overrides)?;
            provider::validate_command(&file, &cfg, stamp)?;
        }
        Command::Draft { files } => {
            let cfg = config::load(&overrides)?;
            print!("{}", tdy::draft::draft_target(&files, cfg.limits)?);
        }
        Command::Fit { target, file, accept, dry_run, propose } => {
            let cfg = config::load(&overrides)?;
            match file {
                Some(f) => fit_command(&target, &f, &cfg, dry_run, propose).await?,
                None => fit_dataset(&target, &cfg, dry_run, &accept, propose).await?,
            }
        }
        Command::Check { target, against } => {
            let cfg = config::load(&overrides)?;
            check_command(&target, &against, cfg.limits)?;
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
