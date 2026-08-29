//! DataFusion integration.
//!
//! `messy('file.xlsx')` is a table function. Because
//! `TableFunctionImpl::call` is synchronous (it runs during SQL planning),
//! all potentially-slow, async work — LLM inference — happens in a
//! *pre-pass* over the query text that materializes sidecars on disk. The
//! UDTF then only ever loads a sidecar (or falls back to the synchronous
//! heuristic sniffer) and hands DataFusion an in-memory table.
//!
//! That split is also what `--frozen` hangs off: skip the pre-pass, require
//! fresh sidecars, touch no network — the reproducible CI mode.

use std::collections::HashMap;
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::{anyhow, bail, Context, Result};
use datafusion::arrow::datatypes::Schema;
use datafusion::arrow::record_batch::RecordBatch;
use datafusion::catalog::{TableFunctionImpl, TableProvider};
use datafusion::common::ScalarValue;
use datafusion::datasource::MemTable;
use datafusion::error::{DataFusionError, Result as DfResult};
use datafusion::prelude::{Expr, SessionContext};

use crate::config::{Backend, Config, Limits};
use crate::engine;
use crate::infer;
use crate::sample;
use crate::sidecar::{self, ProvenanceInfo, SidecarStatus};
use crate::sniff;
use crate::spec::{InferenceMethod, ParseSpec};
use crate::sqlscan;

// ---------------------------------------------------------------------------
// The messy() table function
// ---------------------------------------------------------------------------

#[derive(Debug)]
pub struct MessyFunc {
    pub frozen: bool,
    pub limits: Limits,
    /// One parse per file per query. A self-join over `messy('big.csv')`
    /// otherwise reads and parses the whole file once per reference.
    cache: Mutex<HashMap<PathBuf, Arc<MemTable>>>,
}

impl MessyFunc {
    pub fn new(frozen: bool, limits: Limits) -> Self {
        MessyFunc { frozen, limits, cache: Mutex::new(HashMap::new()) }
    }
}

impl TableFunctionImpl for MessyFunc {
    fn call(&self, args: &[Expr]) -> DfResult<Arc<dyn TableProvider>> {
        let path_str = literal_str(args.first()).ok_or_else(|| {
            DataFusionError::Plan(
                "messy() takes a file path string literal, e.g. messy('data.xlsx')".into(),
            )
        })?;
        // Optional second literal (a hint) is consumed by the pre-pass; the
        // planner just tolerates it here.
        let path = PathBuf::from(&path_str);
        // `./data/x.csv` and `data/x.csv` are one file and must be parsed once.
        let key = std::fs::canonicalize(&path).unwrap_or_else(|_| path.clone());

        if let Some(hit) = self
            .cache
            .lock()
            .ok()
            .and_then(|c| c.get(&key).cloned())
        {
            return Ok(hit);
        }

        let spec = resolve_spec_sync(&path, self.frozen, self.limits)
            .map_err(|e| DataFusionError::External(format!("{e:#}").into()))?;
        let batches = engine::execute_batches(&spec, &path, self.limits)
            .with_context(|| format!("executing parse spec for {}", path.display()))
            .map_err(|e| DataFusionError::External(format!("{e:#}").into()))?;
        let schema = batches[0].schema();
        let table = Arc::new(MemTable::try_new(schema, partition(batches))?);
        if let Ok(mut c) = self.cache.lock() {
            c.insert(key, table.clone());
        }
        Ok(table)
    }
}

/// One partition, holding the batches in file order.
///
/// Spreading them round-robin across partitions did let the scan run on more
/// cores, but a multi-partition MemTable emits partitions in whatever order
/// they finish: `SELECT *` over a file bigger than one batch then returned
/// its rows in a different order on every run, and `--frozen` stopped meaning
/// "same file, same answer". DataFusion still repartitions above the scan for
/// work that benefits (aggregation, joins), so the parallelism is kept where
/// it does not cost determinism.
fn partition(batches: Vec<RecordBatch>) -> Vec<Vec<RecordBatch>> {
    vec![batches]
}

fn literal_str(e: Option<&Expr>) -> Option<String> {
    match e {
        Some(Expr::Literal(ScalarValue::Utf8(Some(s)))) => Some(s.clone()),
        Some(Expr::Literal(ScalarValue::LargeUtf8(Some(s)))) => Some(s.clone()),
        _ => None,
    }
}

/// Synchronous spec resolution used inside planning: fresh sidecar, or (if
/// allowed) an on-the-fly heuristic sniff. Never the LLM — that already
/// happened in the pre-pass.
fn resolve_spec_sync(path: &Path, frozen: bool, limits: Limits) -> Result<ParseSpec> {
    if !path.exists() {
        bail!("file not found: {}", path.display());
    }
    match sidecar::load(path)? {
        SidecarStatus::Fresh(sc) => Ok(sc.spec),
        SidecarStatus::Stale(_) if frozen => bail!(
            "--frozen: sidecar for {} exists but the file content changed \
             (hash mismatch). Re-run `tdy sniff` without --frozen.",
            path.display()
        ),
        SidecarStatus::Absent if frozen => bail!(
            "--frozen: no sidecar for {}. Run `tdy sniff {}` first.",
            path.display(),
            path.display()
        ),
        _ => {
            // The pre-pass normally handles this; reaching here means the
            // reference was not visible in the SQL text (a view, a prepared
            // statement). Heuristics only, and nothing is written.
            let s = sample::build(path, 16 * 1024)?;
            let result = sniff::sniff(path, &s, limits)?;
            check_spec(&result.spec, path, limits)?;
            Ok(result.spec)
        }
    }
}

/// Every spec that reaches the executor has passed both gates: the
/// cross-field validation, and an actual run against the head of the real
/// file. A spec that cannot parse its own file is never written to a sidecar
/// and never used.
fn check_spec(spec: &ParseSpec, path: &Path, limits: Limits) -> Result<()> {
    if let Err(errs) = spec.validate() {
        bail!("the inferred spec is not valid:\n- {}", errs.join("\n- "));
    }
    engine::dry_run(spec, path, limits)
        .with_context(|| format!("the inferred spec fails on {}", path.display()))?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Pre-pass: find messy() references, make sure sidecars exist and are fresh
// ---------------------------------------------------------------------------

pub struct PreparedFile {
    pub path: PathBuf,
    pub method: InferenceMethod,
    pub confidence: Option<f32>,
    pub notes: Vec<String>,
}

pub async fn prepare_specs(sql: &str, cfg: &Config) -> Result<Vec<PreparedFile>> {
    let mut prepared = Vec::new();
    for r in sqlscan::find_messy_refs(sql) {
        let path = PathBuf::from(&r.path);
        prepared.push(ensure_sidecar(&path, cfg, r.hint.as_deref(), false).await?);
    }
    Ok(prepared)
}

/// Ensure a fresh sidecar exists for `path`, running heuristics and (when
/// needed and configured) the LLM. `force` re-infers even if fresh.
pub async fn ensure_sidecar(
    path: &Path,
    cfg: &Config,
    hint: Option<&str>,
    force: bool,
) -> Result<PreparedFile> {
    if !path.exists() {
        bail!("file not found: {}", path.display());
    }
    if !force {
        if let SidecarStatus::Fresh(sc) = sidecar::load(path)? {
            return Ok(PreparedFile {
                path: path.to_path_buf(),
                method: sc.provenance.method,
                confidence: sc.spec.confidence,
                notes: sc.spec.notes.clone(),
            });
        }
    }

    let s = sample::build(path, cfg.sample_bytes)?;
    let draft = sniff::sniff(path, &s, cfg.limits)
        .with_context(|| format!("heuristic sniff of {}", path.display()))?;

    let (spec, method, model) = if draft.confidence >= cfg.confidence_threshold {
        (draft.spec, InferenceMethod::Heuristic, None)
    } else if cfg.backend != Backend::None {
        if cfg.backend.is_remote() {
            eprintln!(
                "note: sending {} bytes sampled from {} to {} ({}) for spec inference",
                s.sampled_bytes,
                path.display(),
                cfg.backend.label(),
                cfg.model
            );
        }
        let inferred = infer::infer_spec(cfg, path, &s, Some(&draft), hint).await?;
        (inferred.spec, InferenceMethod::Llm, Some(inferred.model))
    } else {
        eprintln!(
            "warning: heuristics are only {:.0}% confident about {} and no LLM \
             backend is configured; using the heuristic spec. Inspect it with \
             `tdy sniff {}` or configure a backend.",
            draft.confidence * 100.0,
            path.display(),
            path.display()
        );
        (draft.spec, InferenceMethod::Heuristic, None)
    };

    // Both tiers are gated the same way. The LLM path also dry-runs inside
    // its retry loop; doing it again here costs a few hundred rows and means
    // no unexecutable spec can ever reach a sidecar.
    check_spec(&spec, path, cfg.limits)?;

    let confidence = spec.confidence;
    let notes = spec.notes.clone();
    sidecar::save(
        path,
        &spec,
        ProvenanceInfo {
            method,
            model,
            prompt_version: matches!(method, InferenceMethod::Llm)
                .then(|| infer::PROMPT_VERSION.to_string()),
            sampled_bytes: Some(s.sampled_bytes),
        },
    )?;
    Ok(PreparedFile { path: path.to_path_buf(), method, confidence, notes })
}

// ---------------------------------------------------------------------------
// Query running + output
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputFormat {
    Table,
    Csv,
    Json,
    Parquet,
}

impl OutputFormat {
    pub fn parse(s: &str) -> Result<Self> {
        match s.to_ascii_lowercase().as_str() {
            "table" => Ok(Self::Table),
            "csv" => Ok(Self::Csv),
            "json" | "ndjson" | "jsonl" => Ok(Self::Json),
            "parquet" => Ok(Self::Parquet),
            other => bail!("unknown output format `{other}` (table | csv | json | parquet)"),
        }
    }

    pub fn from_path(p: &Path) -> Option<Self> {
        match p.extension()?.to_string_lossy().to_ascii_lowercase().as_str() {
            "csv" => Some(Self::Csv),
            "json" | "ndjson" | "jsonl" => Some(Self::Json),
            "parquet" | "pq" => Some(Self::Parquet),
            _ => None,
        }
    }

    /// What to write to a named output file whose extension we do not know.
    ///
    /// Guessing "table" here means `-o results.dat` prints to the terminal and
    /// writes nothing, exits 0, and looks like it worked.
    pub fn for_output_path(p: &Path) -> Result<Self> {
        Self::from_path(p).ok_or_else(|| {
            anyhow!(
                "cannot tell the output format from {:?}; use --format \
                 (table | csv | json | parquet) or a .csv/.json/.parquet extension",
                p.display().to_string()
            )
        })
    }
}

pub fn session(cfg: &Config, frozen: bool) -> SessionContext {
    let ctx = SessionContext::new();
    ctx.register_udtf("messy", Arc::new(MessyFunc::new(frozen, cfg.limits)));
    ctx
}

pub async fn run_query(
    sql: &str,
    cfg: &Config,
    frozen: bool,
) -> Result<(Arc<Schema>, Vec<RecordBatch>)> {
    if !frozen {
        report(&prepare_specs(sql, cfg).await?, cfg);
    }
    let ctx = session(cfg, frozen);
    // Planning is where messy() runs, so our own errors surface here; unwrap
    // them so the user reads the sentence we wrote rather than DataFusion's
    // adapter chain repeating it.
    let df = ctx.sql(sql).await.map_err(unwrap_df)?;
    let schema: Arc<Schema> = Arc::new(df.schema().as_arrow().clone());
    let batches = df.collect().await.map_err(unwrap_df)?;
    Ok((schema, batches))
}

/// DataFusion wraps our errors in `External(...)`, whose Display already
/// contains the message and whose `source()` contains it again — so the
/// default rendering prints every sentence twice. Unwrap to the innermost
/// message we actually wrote.
fn unwrap_df(e: DataFusionError) -> anyhow::Error {
    fn inner(e: DataFusionError) -> String {
        match e {
            DataFusionError::External(i) => i.to_string(),
            DataFusionError::Context(ctx, i) => {
                let msg = inner(*i);
                if msg.contains(&ctx) {
                    msg
                } else {
                    format!("{ctx}: {msg}")
                }
            }
            other => other.to_string(),
        }
    }
    anyhow!("{}", inner(e))
}

fn report(prepared: &[PreparedFile], cfg: &Config) {
    for p in prepared {
        if let Some(c) = p.confidence {
            if c < cfg.confidence_threshold {
                eprintln!(
                    "note: spec for {} has confidence {:.2}; review with `tdy sniff {}`",
                    p.path.display(),
                    c,
                    p.path.display()
                );
                for n in &p.notes {
                    eprintln!("      - {n}");
                }
            }
        }
    }
}

pub fn write_output(
    schema: &Arc<Schema>,
    batches: &[RecordBatch],
    format: OutputFormat,
    output: Option<&Path>,
) -> Result<()> {
    match (format, output) {
        (OutputFormat::Table, out_path) => {
            let rows: usize = batches.iter().map(|b| b.num_rows()).sum();
            if rows > 10_000 && out_path.is_none() {
                eprintln!(
                    "note: formatting {rows} rows as a table holds the whole result in \
                     memory twice; `-o results.parquet` (or .csv) streams instead"
                );
            }
            let text = datafusion::arrow::util::pretty::pretty_format_batches(batches)
                .context("formatting result")?;
            // `--format table -o file` used to print to the terminal, write
            // nothing, and exit 0.
            let mut w = writer_for(out_path)?;
            writeln!(w, "{text}").context("writing output")?;
            w.flush().context("writing output")?;
        }
        (OutputFormat::Csv, out) => {
            let mut w = writer_for(out)?;
            {
                let mut csv_w = datafusion::arrow::csv::WriterBuilder::new()
                    .with_header(true)
                    .build(&mut w);
                for b in batches {
                    csv_w.write(b).context("writing CSV")?;
                }
            }
            w.flush().context("flushing output")?;
        }
        (OutputFormat::Json, out) => {
            let mut w = writer_for(out)?;
            {
                let mut json_w = datafusion::arrow::json::LineDelimitedWriter::new(&mut w);
                json_w.write_batches(&batches.iter().collect::<Vec<_>>())?;
                json_w.finish()?;
            }
            w.flush().context("flushing output")?;
        }
        (OutputFormat::Parquet, Some(out)) => {
            let file = std::fs::File::create(out)
                .with_context(|| format!("creating {}", out.display()))?;
            let props = parquet_props();
            let mut pw = datafusion::parquet::arrow::ArrowWriter::try_new(
                BufWriter::new(file),
                schema.clone(),
                Some(props),
            )
            .context("opening parquet writer")?;
            for b in batches {
                pw.write(b).context("writing parquet")?;
            }
            pw.close().context("closing parquet file")?;
        }
        (OutputFormat::Parquet, None) => {
            bail!("parquet output needs --output <file> (refusing to write parquet to stdout)")
        }
    }
    Ok(())
}

fn parquet_props() -> datafusion::parquet::file::properties::WriterProperties {
    use datafusion::parquet::basic::{Compression, ZstdLevel};
    use datafusion::parquet::file::properties::WriterProperties;
    WriterProperties::builder()
        .set_compression(Compression::ZSTD(ZstdLevel::try_new(3).unwrap_or_default()))
        .build()
}

fn writer_for(out: Option<&Path>) -> Result<Box<dyn Write>> {
    Ok(match out {
        Some(p) => Box::new(BufWriter::with_capacity(
            256 * 1024,
            std::fs::File::create(p).with_context(|| format!("creating {}", p.display()))?,
        )),
        None => Box::new(BufWriter::with_capacity(256 * 1024, std::io::stdout())),
    })
}

// ---------------------------------------------------------------------------
// Commands
// ---------------------------------------------------------------------------

pub async fn sniff_command(
    path: &Path,
    cfg: &Config,
    hint: Option<&str>,
    force: bool,
    no_llm: bool,
) -> Result<()> {
    let cfg = if no_llm {
        let mut c = cfg.clone();
        c.backend = Backend::None;
        c
    } else {
        cfg.clone()
    };
    let prepared = ensure_sidecar(path, &cfg, hint, force).await?;
    let sc_path = sidecar::sidecar_path(path);
    let text = std::fs::read_to_string(&sc_path)?;
    println!("# {}", sc_path.display());
    println!("{text}");

    let spec = sidecar::load(path)?
        .fresh_spec()
        .ok_or_else(|| anyhow!("internal: sidecar not fresh right after writing it"))?;
    let batch = engine::preview(&spec, path, cfg.limits, 10)?;
    println!(
        "preview ({} method, confidence {}):",
        match prepared.method {
            InferenceMethod::Heuristic => "heuristic",
            InferenceMethod::Llm => "llm",
            InferenceMethod::Manual => "manual",
        },
        prepared
            .confidence
            .map(|c| format!("{c:.2}"))
            .unwrap_or_else(|| "n/a".into())
    );
    let text = datafusion::arrow::util::pretty::pretty_format_batches(&[batch])?;
    println!("{text}");
    Ok(())
}

/// Check a sidecar against its file without running a query: validate the
/// spec, confirm the fingerprint, and execute the spec over the head of the
/// file. Optionally re-stamp the fingerprint for a hand-edited sidecar.
pub fn validate_command(path: &Path, cfg: &Config, restamp: bool) -> Result<()> {
    let sc_path = sidecar::sidecar_path(path);
    if !sc_path.exists() {
        bail!(
            "no sidecar at {}; run `tdy sniff {}` first",
            sc_path.display(),
            path.display()
        );
    }
    if restamp {
        // Check the spec against the file *before* recording that it matches:
        // stamping first would leave a fresh-looking sidecar whose spec is
        // known not to work, which is exactly the state --frozen trusts.
        let text = std::fs::read_to_string(&sc_path)
            .with_context(|| format!("cannot read sidecar {}", sc_path.display()))?;
        let candidate: crate::spec::Sidecar = toml::from_str(&text)
            .with_context(|| format!("sidecar {} is not a valid spec", sc_path.display()))?;
        if let Err(errs) = candidate.spec.validate() {
            bail!(
                "refusing to stamp an invalid spec in {}:\n- {}",
                sc_path.display(),
                errs.join("\n- ")
            );
        }
        engine::preview(&candidate.spec, path, cfg.limits, 200).with_context(|| {
            format!(
                "refusing to stamp: the spec in {} does not parse {}",
                sc_path.display(),
                path.display()
            )
        })?;
        sidecar::stamp(path, InferenceMethod::Manual)?;
        println!("re-fingerprinted {} (method = manual)", sc_path.display());
    }
    match sidecar::load(path)? {
        SidecarStatus::Absent => bail!("no sidecar at {}", sc_path.display()),
        SidecarStatus::Stale(_) => bail!(
            "{} is stale: {} has changed since the spec was written.\n\
             Re-run `tdy sniff {}` to re-infer, or `tdy validate {} --stamp` to keep \
             the spec and accept the new contents.",
            sc_path.display(),
            path.display(),
            path.display(),
            path.display()
        ),
        SidecarStatus::Fresh(sc) => {
            let batch = engine::preview(&sc.spec, path, cfg.limits, 200)?;
            println!(
                "{}: ok — {} column(s), spec {} by {}",
                sc_path.display(),
                batch.num_columns(),
                match sc.provenance.method {
                    InferenceMethod::Heuristic => "inferred",
                    InferenceMethod::Llm => "modelled",
                    InferenceMethod::Manual => "written",
                },
                sc.provenance.tool_version
            );
            for n in &sc.spec.notes {
                println!("  note: {n}");
            }
            Ok(())
        }
    }
}

/// Helper used by tests: query straight against a spec without sidecars.
pub fn spec_to_batch(spec: &ParseSpec, path: &Path) -> Result<RecordBatch> {
    engine::execute(spec, path, Limits::default()).map_err(|e| anyhow!("{e:#}"))
}
