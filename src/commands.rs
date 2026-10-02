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
    // Checked before validating: a successful `--stamp` creates nothing, so
    // the file's existence now is what it was.
    let has_sidecar = sc_path.exists();
    let notes = provider::validate_quiet(path, cfg, restamp)?;
    let mut text = String::new();
    if restamp {
        writeln!(text, "re-fingerprinted {} (method = manual)", sc_path.display())?;
    }
    if has_sidecar {
        writeln!(text, "{}: ok", sc_path.display())?;
    } else {
        // A lock-held plan: the member, not a sidecar that does not exist.
        writeln!(text, "{}: ok", path.display())?;
    }
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

    // A member of a `plans = 'lock'` target may have no sidecar at all: its
    // plan is the one the lock holds, and that is the plan checked.
    let lock = crate::lockfile::Lock::load(target_path).ok().flatten();
    let plans = crate::plans::Plans::new(target_path, lock.as_ref());
    let mut bad = 0usize;
    for f in files {
        use crate::sidecar::SidecarStatus;
        let held = match lock_held(target_path, &plans, f) {
            Ok(h) => h,
            // A lock plan refused for this member (edited, or not recorded
            // for it): what `dataset()` would say, as a line, not an abort.
            Err(e) => {
                writeln!(text, "\n{}: REFUSED — {}", f.display(), one_line(&format!("{e:#}")))?;
                bad += 1;
                continue;
            }
        };
        if let Some((file, plan)) = held {
            let shown = f.display();
            let verdict = judge(&plan.spec, &target, false);
            if plan.edited() {
                writeln!(
                    text,
                    "\n{shown}: EDITED — its plan in {} was edited by hand: it no longer hashes to \
                     its id{}, and a query refuses it.\n  Run `tdy fit {}` to rebuild the lock; give \
                     the member a sidecar (method = \"manual\") to change its plan.",
                    plan.whereabouts(),
                    plans.lock().map(crate::plans::written_by_note).unwrap_or_default(),
                    target_path.display()
                )?;
                bad += 1;
                continue;
            }
            if !plan.is_fresh(&file)? {
                writeln!(
                    text,
                    "\n{shown}: STALE — the file has changed since its plan was recorded in {}, \
                     so this is not the plan a query would use.\n  Run `tdy fit {}` to re-plan it, \
                     then check again.",
                    plan.whereabouts(),
                    target_path.display()
                )?;
                bad += 1;
            } else {
                writeln!(text, "\n{shown}: {} — plan held in {}", verdict.label(), plan.whereabouts())?;
                if !verdict.is_ok() {
                    bad += 1;
                }
            }
            for m in verdict.mismatches() {
                writeln!(text, "  {}", m.message())?;
            }
            continue;
        }
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

/// The plan a target's lock holds for `f` — a data file, or a member
/// reference (`book.xlsx#Q1`) — when the member has no sidecar of its own.
/// `None` when it has one (the sidecar is the plan, and is checked as ever)
/// or when the lock holds nothing for it.
pub fn lock_held(
    target_path: &Path,
    plans: &crate::plans::Plans,
    f: &Path,
) -> Result<Option<(PathBuf, crate::plans::Plan)>> {
    if plans.lock().is_none_or(|l| l.specs.is_empty()) {
        return Ok(None);
    }
    let dir = crate::lockfile::target_dir(target_path);
    let named = crate::member::relative_to_target(&f.to_string_lossy(), &dir);
    let held = |m: &crate::member::MemberRef| plans.entry(m).is_some_and(|e| e.spec.is_some());
    let Ok(Some(m)) = crate::member::MemberRef::resolve(&named, held) else { return Ok(None) };
    let file = dir.join(&m.path);
    if crate::sidecar::sidecar_path_for(&file, m.sheet.as_deref(), m.region).exists() {
        return Ok(None);
    }
    Ok(plans.plan_for(&file, &m)?.map(|p| (file, p)))
}

/// What a sidecar written by `tdy fit T FILE` does to a `plans = 'lock'`
/// pile: the sidecar wins, so it is drift until a pile fit records it.
pub fn overrides_note(target_path: &Path) -> String {
    format!(
        "note: `{0}` keeps its plans in the lock; this sidecar overrides the lock's plan for this \
         member until `tdy fit {0}` records it (a query refuses it as drift until then)",
        target_path.display()
    )
}

fn one_line(s: &str) -> String {
    s.lines().map(str::trim).filter(|l| !l.is_empty()).collect::<Vec<_>>().join(" ")
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
            if target.plans == crate::target::PlanStore::Lock {
                writeln!(text, "{}", overrides_note(target_path))?;
            }
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
    for n in &p.notes {
        writeln!(text, "note: {n}")?;
    }

    let Some(want) = column else {
        let rows = profile_summary_rows(p);
        let head = PROFILE_COLUMNS;
        let widths: Vec<usize> = (0..8)
            .map(|i| rows.iter().map(|r| r[i].chars().count()).chain([head[i].chars().count()]).max().unwrap_or(0))
            .collect();
        let line = |cells: &[String]| -> String {
            let mut l = String::new();
            for (i, c) in cells.iter().enumerate() {
                let pad = widths[i].saturating_sub(c.chars().count());
                if PROFILE_RIGHT[i] {
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
        shown_value(&c.name),
        c.position,
        c.non_empty,
        c.empty,
        distinct_text(&c.distinct)
    )?;
    writeln!(text, "  min  {}", c.min.as_deref().map(shown_value).unwrap_or_else(|| "-".into()))?;
    writeln!(text, "  max  {}", c.max.as_deref().map(shown_value).unwrap_or_else(|| "-".into()))?;
    match c.distinct {
        Distinct::AtLeast(n) => writeln!(
            text,
            "top values: not given — more than {n} distinct values, and an approximate count \
             is not one anyone could check"
        )?,
        Distinct::Exact(_) if c.top.is_empty() => writeln!(text, "top values: none")?,
        Distinct::Exact(_) => {
            writeln!(text, "top values:")?;
            let shown: Vec<String> = c.top.iter().map(|(v, _)| clip(&shown_value(v), 40)).collect();
            let w = shown.iter().map(|v| v.chars().count()).max().unwrap_or(0);
            let nw = c.top.iter().map(|(_, n)| n.to_string().len()).max().unwrap_or(0);
            for (v, (_, n)) in shown.iter().zip(&c.top) {
                let pad = w.saturating_sub(v.chars().count());
                writeln!(text, "  {v}{}  {n:>nw$}  {}", " ".repeat(pad), share(*n, c.non_empty))?;
            }
        }
    }
    if c.shapes.is_empty() {
        writeln!(text, "shapes: none")?;
    } else {
        if c.shapes_complete {
            writeln!(text, "shapes:")?;
        } else {
            writeln!(
                text,
                "shapes — INCOMPLETE: shapes first seen past the {}th were not tracked and are \
                 counted only in {}, so which shape is most frequent is not known:",
                crate::profile::MAX_SHAPE_TRACK,
                crate::profile::OTHER_SHAPES
            )?;
        }
        // Patterns whole here: the detail is where a long one is read.
        let w = c.shapes.iter().map(|s| s.pattern.chars().count()).max().unwrap_or(0);
        let nw = c.shapes.iter().map(|s| s.count.to_string().len()).max().unwrap_or(0);
        for s in &c.shapes {
            let pad = w.saturating_sub(s.pattern.chars().count());
            let pct = share(s.count, c.non_empty);
            writeln!(
                text,
                "  {}{}  {:>nw$}  {pct:>6}  {}",
                s.pattern,
                " ".repeat(pad),
                s.count,
                clip(&shown_value(&s.example), 40)
            )?;
        }
    }
    Ok(text)
}

/// The summary table's header, shared with the workbench's Profile view.
pub const PROFILE_COLUMNS: [&str; 8] =
    ["#", "column", "non-empty", "empty", "distinct", "min", "max", "most frequent shape"];
/// Which of [`PROFILE_COLUMNS`] are numbers, right-aligned.
pub const PROFILE_RIGHT: [bool; 8] = [true, false, true, true, true, false, false, false];

/// One summary row per column, as the CLI prints it and the workbench
/// draws it — one place, so the two cannot say different things. A column
/// whose shapes are not complete has no most frequent shape to show.
pub fn profile_summary_rows(p: &crate::profile::Profile) -> Vec<[String; 8]> {
    p.columns
        .iter()
        .map(|c| {
            let shape = if !c.shapes_complete {
                format!("(shapes past {} not tracked)", crate::profile::MAX_SHAPE_TRACK)
            } else {
                c.shapes
                    .first()
                    .map(|s| format!("{} ({})", clip(&s.pattern, 20), share(s.count, c.non_empty)))
                    .unwrap_or_else(|| "-".into())
            };
            [
                c.position.to_string(),
                clip(&shown_value(&c.name), 24),
                c.non_empty.to_string(),
                c.empty.to_string(),
                distinct_text(&c.distinct),
                clip(&c.min.as_deref().map(shown_value).unwrap_or_else(|| "-".into()), 20),
                clip(&c.max.as_deref().map(shown_value).unwrap_or_else(|| "-".into()), 20),
                shape,
            ]
        })
        .collect()
}

fn distinct_text(d: &crate::profile::Distinct) -> String {
    match d {
        crate::profile::Distinct::Exact(n) => n.to_string(),
        crate::profile::Distinct::AtLeast(n) => format!("{n}+"),
    }
}

/// `n` of `of`, as a percentage with one decimal; `-` of nothing.
pub fn share(n: u64, of: u64) -> String {
    if of == 0 { "-".to_string() } else { format!("{:.1}%", n as f64 * 100.0 / of as f64) }
}

/// A raw value as one line of text: a line break inside it is `↵`, so a
/// quoted multi-line cell cannot break a table apart.
pub fn shown_value(v: &str) -> String {
    v.replace("\r\n", "↵").replace(['\n', '\r'], "↵")
}

/// The first line of a profile: what was read, how much of it — under
/// `--head`, that it was NOT the whole table, before anything else is
/// said — and in which frame. Shared with the workbench's Profile view.
pub fn profile_heading(shown: &str, p: &crate::profile::Profile) -> String {
    let mut what = shown.to_string();
    if let Some(s) = &p.sheet {
        what.push_str(&format!(", sheet {s:?}"));
    }
    if let Some(r) = &p.range {
        what.push_str(&format!(", range {r}"));
    }
    if let Some(w) = &p.window {
        what.push_str(&format!(", rows {}\u{2013}{}", w.start + 1, w.end));
    }
    if let Some(ptr) = &p.pointer {
        what.push_str(&format!(", record array {ptr}"));
        if p.pointer_candidates.len() > 1 {
            what.push_str(&format!(
                " — one of {} candidates ({}); --pointer picks another",
                p.pointer_candidates.len(),
                p.pointer_candidates.join(", ")
            ));
        }
    }
    let extent = if p.complete {
        format!("{} rows, the whole table", p.rows)
    } else {
        format!("first {} rows only (--head) — NOT the whole table", p.rows)
    };
    format!("{what}: {extent}; {} column(s); frame: {}", p.columns.len(), p.frame)
}

/// The column `--column` names: by the file's own spelling, `#N` by
/// position, or `\#N` for a column literally named `#N`. Two columns with
/// one name are refused, naming both positions — picking one would be a
/// guess.
pub fn pick_column<'a>(p: &'a crate::profile::Profile, want: &str) -> Result<&'a crate::profile::ColumnProfile> {
    let name = match want.strip_prefix('\\') {
        Some(literal) if literal.starts_with('#') => literal,
        _ => {
            if let Some(n) = want.strip_prefix('#').and_then(|n| n.parse::<usize>().ok()) {
                return p
                    .columns
                    .iter()
                    .find(|c| c.position == n)
                    .ok_or_else(|| anyhow!("no column #{n} — there are {}", p.columns.len()));
            }
            want
        }
    };
    let hits: Vec<&crate::profile::ColumnProfile> = p.columns.iter().filter(|c| c.name == name).collect();
    match hits.as_slice() {
        [one] => Ok(one),
        [] => {
            let names: Vec<String> = p.columns.iter().map(|c| format!("`{}`", c.name)).collect();
            Err(anyhow!("no column `{name}` — the columns are {}", names.join(", ")))
        }
        several => {
            let at: Vec<String> = several.iter().map(|c| format!("--column '#{}'", c.position)).collect();
            Err(anyhow!(
                "{} columns are called `{name}` — name one by position: {}",
                several.len(),
                at.join(" or ")
            ))
        }
    }
}

/// At most `n` characters, an ellipsis marking a cut.
pub fn clip(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        format!("{}…", s.chars().take(n.saturating_sub(1)).collect::<String>())
    }
}
