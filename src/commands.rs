//! The CLI's subcommands as functions that return their text.
//!
//! `tdy sniff` and the console's `.sniff` must print the same thing, and the
//! only way that stays true is if there is one function producing it. So
//! the text lives here, `main.rs` prints it, and `console` returns it. Nothing
//! in this module writes to stdout or stderr.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, Result};

use crate::config::{Backend, Config, Limits};
use crate::provider::{self, PreparedFile, SniffCli};
use crate::spec::{DType, InferenceMethod, ParseSpec};
use crate::{engine, sidecar, sniff};

pub struct SniffOutcome {
    pub text: String,
    pub prepared: PreparedFile,
    pub spec: ParseSpec,
    pub kept_existing: bool,
}

/// `tdy sniff`'s text. Body lifted from `provider::sniff_command`, with
/// `println!` replaced by writes into `text`.
pub async fn sniff_text(path: &Path, cfg: &Config, opts: SniffCli<'_>) -> Result<SniffOutcome> {
    let SniffCli { hint, force, no_llm, quick, .. } = opts;
    let cfg = if no_llm {
        let mut c = cfg.clone();
        c.backend = Backend::None;
        c
    } else {
        cfg.clone()
    };
    // Said out loud rather than discovered: a fresh sidecar is kept unless
    // --force, and a reader who just ran .fit and comes back to .sniff should
    // learn that from the output, not from column names that changed.
    let kept_existing =
        !force && matches!(sidecar::load(path), Ok(sidecar::SidecarStatus::Fresh(_)));
    let prepared =
        provider::ensure_sidecar_opts(path, &cfg, hint, force, sniff::SniffOpts { verify: !quick })
            .await?;
    let sc_path = sidecar::sidecar_path(path);
    let mut text = String::new();
    if kept_existing {
        writeln!(text, "note: {} is fresh and was kept; --force to re-infer", sc_path.display())?;
    }
    writeln!(text, "# {}", sc_path.display())?;
    writeln!(text, "{}", std::fs::read_to_string(&sc_path)?)?;
    let spec = sidecar::load(path)?
        .fresh_spec()
        .ok_or_else(|| anyhow!("internal: sidecar not fresh right after writing it"))?;
    let batch = engine::preview(&spec, path, cfg.limits, 10)?;
    writeln!(
        text,
        "preview ({} method, confidence {}):",
        match prepared.method {
            InferenceMethod::Heuristic => "heuristic",
            InferenceMethod::Llm => "llm",
            InferenceMethod::Manual => "manual",
        },
        prepared.confidence.map(|c| format!("{c:.2}")).unwrap_or_else(|| "n/a".into())
    )?;
    writeln!(text, "{}", datafusion::arrow::util::pretty::pretty_format_batches(&[batch])?)?;
    Ok(SniffOutcome { text, prepared, spec, kept_existing })
}

/// `tdy validate`'s text. Body lifted from `provider::validate_command`.
pub fn validate_text(path: &Path, cfg: &Config, restamp: bool) -> Result<String> {
    let (file, sheet, region) = sidecar::resolve_ref(path)?;
    let sc_path = sidecar::sidecar_path_for(&file, sheet.as_deref(), region);
    let notes = provider::validate_quiet(path, cfg, restamp)?;
    let mut text = String::new();
    if restamp {
        writeln!(text, "re-fingerprinted {} (method = manual)", sc_path.display())?;
    }
    writeln!(text, "{}: ok", sc_path.display())?;
    for n in &notes {
        writeln!(text, "  note: {n}")?;
    }
    Ok(text)
}

pub struct CheckOutcome {
    pub text: String,
    pub ok: bool,
    pub bad: usize,
}

/// `tdy check`'s text path. Body lifted from `main.rs::check_command` (the
/// non-JSON branch): every `println!` becomes a `writeln!(text, …)`, and the
/// two `bail!` sites become `ok = false` with the same wording left to the
/// caller (see `main.rs`, which bails with the identical sentence).
pub fn check_text(target_path: &Path, files: &[PathBuf], limits: Limits) -> Result<CheckOutcome> {
    use crate::conform::{judge, Verdict};
    use crate::target::Target;

    let target = Target::load(target_path)?;
    let mut text = String::new();
    writeln!(
        text,
        "{}: `{}`, {} column(s)",
        target_path.display(),
        target.name,
        target.columns.len()
    )?;

    if files.is_empty() {
        // With a lock, the dataset itself is what CI wants checked, and
        // `dataset::resolve` runs exactly the checks a query would: drift,
        // every member's sidecar present and fresh, every member still
        // conforming, nothing waiting on a human. Reusing it is what keeps
        // the gate and the query from disagreeing.
        //
        // Without a lock there is nothing to check, and saying so beats
        // exiting zero on a target nobody has fitted.
        let lock = crate::lockfile::Lock::load(target_path)?;
        if lock.is_none() {
            writeln!(
                text,
                "\nnothing to check: `{}` has no lock. Run `tdy fit {}` first, or pass \
                 --against <FILE> to check a single sidecar.\ndeclared sources: {}",
                target.name,
                target_path.display(),
                if target.files.is_empty() { "(none)".into() } else { target.files.join(", ") }
            )?;
            return Ok(CheckOutcome { text, ok: true, bad: 0 });
        }
        let resolved = crate::dataset::resolve(target_path, limits, None)?;
        writeln!(text, "\n{} member(s), all conforming:", resolved.members.len())?;
        for m in &resolved.members {
            writeln!(text, "  {:<28} OK", m.rel)?;
        }
        writeln!(text, "\n`{}` is ready to query.", target.name)?;
        return Ok(CheckOutcome { text, ok: true, bad: 0 });
    }

    let mut bad = 0usize;
    for f in files {
        use crate::sidecar::SidecarStatus;
        // `--against book.xlsx#Q1` checks one sheet member's sidecar, which
        // is a file beside the workbook rather than a section of anything.
        let (file, sheet, region) = crate::sidecar::resolve_ref(f)?;
        let (f, sheet) = (file.as_path(), sheet.as_deref());
        let sc = crate::sidecar::sidecar_path_for(f, sheet, region);
        let (spec, stale) = match crate::sidecar::load_member(f, sheet, region) {
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
                // A workbook expanded into sheet members, or a file split
                // into stacked regions, has no plain sidecar, and its
                // members are right there in the same directory: "NO
                // SIDECAR" would be a wrong answer about a file that is
                // fully planned.
                let sheets = if sheet.is_none() { crate::sidecar::sheet_sidecars(f) } else { Vec::new() };
                let regions = if sheet.is_none() {
                    crate::sidecar::region_sidecars(f, None)
                } else {
                    Vec::new()
                };
                if !sheets.is_empty() {
                    writeln!(
                        text,
                        "\n{}: has sheet members {} — check one as `tdy check {} --against {}#{}`, \
                         or the whole dataset with `tdy check {}`",
                        f.display(),
                        sheets.join(", "),
                        target_path.display(),
                        f.display(),
                        sheets[0],
                        target_path.display()
                    )?;
                } else if !regions.is_empty() {
                    let names: Vec<String> =
                        regions.iter().map(|r| format!("{}#{r}", f.display())).collect();
                    writeln!(
                        text,
                        "\n{}: has region members {} — check one as `tdy check {} --against {}`, \
                         or the whole dataset with `tdy check {}`",
                        f.display(),
                        names.join(", "),
                        target_path.display(),
                        names[0],
                        target_path.display()
                    )?;
                } else {
                    writeln!(
                        text,
                        "\n{}: NO SIDECAR — run `tdy sniff {}` first",
                        sc.display(),
                        f.display()
                    )?;
                }
                bad += 1;
                continue;
            }
            Err(e) => {
                writeln!(text, "\n{}: UNREADABLE — {e:#}", sc.display())?;
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
            writeln!(
                text,
                "\n{}: STALE — the file has changed since this spec was written, so this \
                 is not the spec a query would use.\n  Re-sniff it, or \
                 `tdy validate --stamp` if the spec is still right, then check again.",
                sc.display()
            )?;
            bad += 1;
        } else {
            writeln!(text, "\n{}: {}", sc.display(), verdict.label())?;
        }
        for m in verdict.mismatches() {
            writeln!(text, "  {}", m.message())?;
        }
        if let Verdict::Unfitted(m) = &verdict {
            if !m.is_empty() {
                writeln!(
                    text,
                    "  (this sidecar was inferred, not fitted to a target — \
                     `tdy fit` will land it once it exists)"
                )?;
            }
        }
        if !verdict.is_ok() && !stale {
            bad += 1;
        }
    }

    writeln!(
        text,
        "\n{} of {} file(s) conform to `{}`.",
        files.len() - bad,
        files.len(),
        target.name
    )?;
    Ok(CheckOutcome { text, ok: bad == 0, bad })
}

pub struct FitOneOutcome {
    pub text: String,
    pub ok: bool,
    pub wrote: Option<PathBuf>,
    pub gaps: bool,
}

/// Suggestions for columns nothing bound, as pasteable SQL. Moved from
/// `main.rs::print_proposals`.
fn write_proposals(
    text: &mut String,
    file: &Path,
    target: &crate::target::Target,
    limits: Limits,
) -> Result<()> {
    let Ok(proposals) = crate::fit::propose(file, target, limits) else { return Ok(()) };
    for p in &proposals {
        let existing: Vec<String> = target
            .columns
            .iter()
            .find(|c| c.name == p.column)
            .map(|c| std::iter::once(c.name.clone()).chain(c.matches.iter().cloned()).collect())
            .unwrap_or_default();
        writeln!(text, "    `{}` ({}):", p.column, p.want)?;
        for line in p.message(&existing).lines() {
            writeln!(text, "      {line}")?;
        }
    }
    Ok(())
}

/// `tdy fit TARGET FILE`'s text path. Body lifted from `main.rs::fit_command`
/// (the non-JSON branch). `print_proposals` moves here too, as
/// `write_proposals(&mut text, …)`.
pub async fn fit_one_text(
    target_path: &Path,
    file: &Path,
    cfg: &Config,
    dry_run: bool,
    propose: bool,
    progress: Option<&crate::progress::Sink>,
) -> Result<FitOneOutcome> {
    let limits = cfg.limits;
    use crate::fit::FitError;
    use crate::target::Target;

    let target = Target::load(target_path)?;
    let mut text = String::new();
    match crate::fit::plan(file, &target, cfg, progress).await {
        Ok(planned) => {
            let (fitted, method, model) = (planned.fitted, planned.method, planned.model);
            writeln!(text, "{} fits `{}`:", file.display(), target.name)?;
            for c in &fitted.spec.columns {
                writeln!(
                    text,
                    "  {:<16} <- {:<24} {}",
                    c.name,
                    format!("{:?}", c.source_name()),
                    describe_dtype(&c.dtype)
                )?;
            }
            for n in fitted.spec.notes.iter().filter(|n| !crate::fit::is_binding_note(n)) {
                writeln!(text, "  note: {n}")?;
            }
            if dry_run {
                writeln!(text, "\n--dry-run: nothing written.")?;
                return Ok(FitOneOutcome { text, ok: true, wrote: None, gaps: false });
            }
            if let Some(r) = &fitted.review {
                writeln!(text, "  REVIEW: {r}")?;
            }
            let path = crate::sidecar::save(
                file,
                &fitted.spec,
                crate::sidecar::ProvenanceInfo {
                    method,
                    model,
                    prompt_version: None,
                    sampled_bytes: None,
                },
            )?;
            writeln!(text, "\nwrote {}", path.display())?;
            Ok(FitOneOutcome { text, ok: true, wrote: Some(path), gaps: false })
        }
        Err(FitError::Gaps(gaps)) => {
            writeln!(text, "{} cannot reach `{}`:\n", file.display(), target.name)?;
            write!(text, "{}", FitError::Gaps(gaps))?;
            // A stacked file's gaps are about a header read as data, which
            // reads as a type problem and is not one. Say what the file is,
            // and which command splits it.
            if let Some(n) = crate::fit::stacked_note(file, &target, target_path, limits) {
                writeln!(text, "\n  {n}")?;
            }
            if propose {
                writeln!(text, "  suggestions:")?;
                write_proposals(&mut text, file, &target, limits)?;
            }
            Ok(FitOneOutcome { text, ok: false, wrote: None, gaps: true })
        }
        Err(e) => {
            write!(text, "{e}")?;
            Ok(FitOneOutcome { text, ok: false, wrote: None, gaps: false })
        }
    }
}

/// The SQL-ish spelling of a dtype, as the CLI prints it. Moved from main.rs.
pub fn describe_dtype(d: &DType) -> String {
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

/// `tdy profile`'s text, and `.profile`'s. `shown` is the file as the
/// caller typed it. Without `column`: one line per column. With it: that
/// column's top values and every shape with count, share and an example.
///
/// The first line says what was read — and, under `--head`, that it was
/// not the whole file, before anything else is said.
pub fn profile_text(shown: &str, p: &crate::profile::Profile, column: Option<&str>) -> Result<String> {
    use crate::profile::Distinct;
    let mut text = String::new();
    writeln!(text, "{}", profile_heading(shown, p))?;
    let distinct = |d: &Distinct| match d {
        Distinct::Exact(n) => n.to_string(),
        Distinct::AtLeast(n) => format!("{n}+"),
    };
    let share = |n: u64, of: u64| if of == 0 { "-".to_string() } else { format!("{:.1}%", n as f64 * 100.0 / of as f64) };

    let Some(want) = column else {
        let rows: Vec<[String; 8]> = p
            .columns
            .iter()
            .map(|c| {
                [
                    c.position.to_string(),
                    clip(&c.name, 24),
                    c.non_empty.to_string(),
                    c.empty.to_string(),
                    distinct(&c.distinct),
                    clip(c.min.as_deref().unwrap_or("-"), 20),
                    clip(c.max.as_deref().unwrap_or("-"), 20),
                    c.shapes
                        .first()
                        .map(|s| format!("{} ({})", clip(&s.pattern, 20), share(s.count, c.non_empty)))
                        .unwrap_or_else(|| "-".into()),
                ]
            })
            .collect();
        let head = ["#", "column", "non-empty", "empty", "distinct", "min", "max", "most frequent shape"];
        let right = [true, false, true, true, true, false, false, false];
        let widths: Vec<usize> = (0..8)
            .map(|i| rows.iter().map(|r| r[i].chars().count()).chain([head[i].chars().count()]).max().unwrap_or(0))
            .collect();
        let line = |cells: &[String]| -> String {
            let mut l = String::new();
            for (i, c) in cells.iter().enumerate() {
                let pad = widths[i].saturating_sub(c.chars().count());
                if right[i] {
                    l.push_str(&format!("  {}{c}", " ".repeat(pad)));
                } else if i + 1 == cells.len() {
                    l.push_str(&format!("  {c}"));
                } else {
                    l.push_str(&format!("  {c}{}", " ".repeat(pad)));
                }
            }
            l.trim_end().to_string()
        };
        writeln!(text, "{}", line(&head.map(String::from)))?;
        for r in &rows {
            writeln!(text, "{}", line(r))?;
        }
        return Ok(text);
    };

    let c = pick_column(p, want)?;
    writeln!(
        text,
        "column `{}` (position {}): {} non-empty, {} empty, {} distinct",
        c.name,
        c.position,
        c.non_empty,
        c.empty,
        distinct(&c.distinct)
    )?;
    writeln!(text, "  min  {}", c.min.as_deref().unwrap_or("-"))?;
    writeln!(text, "  max  {}", c.max.as_deref().unwrap_or("-"))?;
    match c.distinct {
        Distinct::AtLeast(n) => writeln!(
            text,
            "top values: not given — more than {n} distinct values, and an approximate count \
             is not one anyone could check"
        )?,
        Distinct::Exact(_) if c.top.is_empty() => writeln!(text, "top values: none")?,
        Distinct::Exact(_) => {
            writeln!(text, "top values:")?;
            let w = c.top.iter().map(|(v, _)| v.chars().count()).max().unwrap_or(0).min(40);
            let nw = c.top.iter().map(|(_, n)| n.to_string().len()).max().unwrap_or(0);
            for (v, n) in &c.top {
                let v = clip(v, 40);
                let pad = w.saturating_sub(v.chars().count());
                writeln!(text, "  {v}{}  {n:>nw$}  {}", " ".repeat(pad), share(*n, c.non_empty))?;
            }
        }
    }
    if c.shapes.is_empty() {
        writeln!(text, "shapes: none")?;
    } else {
        writeln!(text, "shapes:")?;
        let w = c.shapes.iter().map(|s| s.pattern.chars().count()).max().unwrap_or(0).min(40);
        let nw = c.shapes.iter().map(|s| s.count.to_string().len()).max().unwrap_or(0);
        for s in &c.shapes {
            let pat = clip(&s.pattern, 40);
            let pad = w.saturating_sub(pat.chars().count());
            let pct = share(s.count, c.non_empty);
            writeln!(text, "  {pat}{}  {:>nw$}  {pct:>6}  {}", " ".repeat(pad), s.count, clip(&s.example, 40))?;
        }
    }
    Ok(text)
}

/// The first line of a profile: what was read, how much of it — under
/// `--head`, that it was NOT the whole table, before anything else is
/// said — and in which frame. Shared with the workbench's Profile view.
pub fn profile_heading(shown: &str, p: &crate::profile::Profile) -> String {
    let mut what = shown.to_string();
    if let Some(s) = &p.sheet {
        what.push_str(&format!(", sheet {s:?}"));
    }
    if let Some(w) = &p.window {
        what.push_str(&format!(", rows {}\u{2013}{}", w.start + 1, w.end));
    }
    let extent = if p.complete {
        format!("{} rows, the whole table", p.rows)
    } else {
        format!("first {} rows only (--head) — NOT the whole table", p.rows)
    };
    format!("{what}: {extent}; {} column(s); frame: {}", p.columns.len(), p.frame)
}

/// The column `--column` names: by the file's own spelling, or `#N` by
/// position. Two columns with one name are refused, naming both positions
/// — picking one would be a guess.
pub fn pick_column<'a>(p: &'a crate::profile::Profile, want: &str) -> Result<&'a crate::profile::ColumnProfile> {
    if let Some(n) = want.strip_prefix('#').and_then(|n| n.parse::<usize>().ok()) {
        if let Some(c) = p.columns.iter().find(|c| c.position == n) {
            return Ok(c);
        }
    }
    let hits: Vec<&crate::profile::ColumnProfile> = p.columns.iter().filter(|c| c.name == want).collect();
    match hits.as_slice() {
        [one] => Ok(one),
        [] => {
            let names: Vec<String> = p.columns.iter().map(|c| format!("`{}`", c.name)).collect();
            Err(anyhow!("no column `{want}` — the columns are {}", names.join(", ")))
        }
        several => {
            let at: Vec<String> = several.iter().map(|c| format!("#{}", c.position)).collect();
            Err(anyhow!(
                "{} columns are called `{want}` — name one by position: --column {}",
                several.len(),
                at.join(" or --column ")
            ))
        }
    }
}

/// At most `n` characters, an ellipsis marking a cut.
fn clip(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        format!("{}…", s.chars().take(n.saturating_sub(1)).collect::<String>())
    }
}
