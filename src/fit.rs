//! Planning a spec *onto* a declared target.
//!
//! This is the inversion the whole design turns on. [`crate::sniff`] runs
//! forward — it reads a file and emits whatever columns it finds. `fit` runs
//! backward: it is handed the columns you *want* and has to find, for each
//! one, a column of this file that produces it, or say why it cannot.
//!
//! ```text
//!   sniff:  file ──► whatever columns fall out
//!   fit:    target + file ──► a spec proved to land on the target, or a gap
//! ```
//!
//! # How it works
//!
//! 1. **Frame.** Ask the sniffer for the file's shape — delimiter, encoding,
//!    sheet, title rows, header rows. That part of its answer is about the
//!    file and is reused wholesale; only its *columns* are thrown away.
//! 2. **Bind.** For each declared column, find the post-transform header cell
//!    that supplies it. Zero candidates is a gap. **Two is also a gap** — a
//!    binder that takes the first of two columns named `Betrag` is right half
//!    the time and silent about which half.
//! 3. **Type by checking, not by preferring.** The type is already declared,
//!    so there is nothing to infer: the question is only whether this column's
//!    values *can* produce it. Candidates are checked with
//!    [`crate::engine::build_column_at`] — the same function the executor
//!    uses — so a candidate that type-checks here cannot fail differently
//!    there.
//! 4. **Gate.** `validate` + [`crate::conform::conforms`] + a dry run.
//!
//! # What it refuses, and why that is the feature
//!
//! Two date formats that both parse every value are not a tie to be broken by
//! preference. `03.04.2025` is either March or April, the file does not say,
//! and a `Date32` column with the wrong month is exactly the plausible wrong
//! number the project exists to refuse. So ambiguity is a gap — unless the
//! target's `date_order` settles it, which is a human saying which convention
//! their exports use.
//!
//! The ambiguity test is exact rather than heuristic: two formats are
//! ambiguous only if they **disagree on a value actually in this file**. Both
//! are applied to the probe and the resulting arrays compared, so `%d.%m.%Y`
//! and `%m.%d.%Y` are a gap on a file containing `03.04.2025` and are not a
//! gap on one whose every day-of-month exceeds twelve.

use std::path::Path;

use anyhow::{Context, Result};
use datafusion::arrow::array::ArrayRef;
use datafusion::arrow::datatypes::DataType as ArrowType;

use crate::config::Limits;
use crate::conform::{conforms, Mismatch};
use crate::engine::{self, ExtractOpts};
use crate::numfmt;
use crate::sniff;
use crate::spec::{ColumnSpec, DType, ParseSpec, ValueParsing};
use crate::target::{DateOrder, MatchMode, Target};

/// A declared column this file cannot supply, and why.
///
/// Every variant carries what to *do* about it. A gap the user cannot act on
/// is just a refusal.
#[derive(Debug, Clone)]
pub enum Gap {
    /// Nothing in the file's header binds.
    NoCandidate {
        column: String,
        want: String,
        /// Every name that was looked for, so the user can see what to add.
        tried: Vec<String>,
        header: Vec<String>,
    },
    /// More than one header cell binds, and tdy will not choose.
    Ambiguous {
        column: String,
        /// (0-based position, the file's own spelling)
        candidates: Vec<(usize, String)>,
    },
    /// A column binds, but its values cannot produce the declared type.
    Untypable {
        column: String,
        source: String,
        want: String,
        why: String,
    },
    /// Several formats parse every value and disagree about what they mean.
    AmbiguousFormat {
        column: String,
        source: String,
        formats: Vec<String>,
        example: String,
    },
}

impl Gap {
    pub fn column(&self) -> &str {
        match self {
            Gap::NoCandidate { column, .. }
            | Gap::Ambiguous { column, .. }
            | Gap::Untypable { column, .. }
            | Gap::AmbiguousFormat { column, .. } => column,
        }
    }

    /// What a user reads, ending in the edit that fixes it.
    pub fn message(&self) -> String {
        match self {
            Gap::NoCandidate { column, want, tried, header } => {
                let shown: Vec<String> =
                    header.iter().take(12).map(|h| format!("{h:?}")).collect();
                let more = header.len().saturating_sub(shown.len());
                format!(
                    "`{column}` ({want}): no column of this file binds\n    \
                     looked for {}\n    \
                     the file has [{}{}]\n    \
                     If one of those supplies it, say so:\n      \
                     {column} {want} OPTIONS(matches = '…')\n    \
                     If none does, this file cannot join the dataset.",
                    tried.iter().map(|t| format!("{t:?}")).collect::<Vec<_>>().join(", "),
                    shown.join(", "),
                    if more > 0 { format!(", … {more} more") } else { String::new() }
                )
            }
            Gap::Ambiguous { column, candidates } => format!(
                "`{column}`: {} columns of this file match, which is ambiguous\n    \
                 {}\n    \
                 tdy will not choose between them — they may well mean different things.",
                candidates.len(),
                candidates
                    .iter()
                    .map(|(i, n)| format!("column {} named {n:?}", i + 1))
                    .collect::<Vec<_>>()
                    .join(" and ")
            ),
            Gap::Untypable { column, source, want, why } => format!(
                "`{column}` ({want}): reads {source:?}, whose values cannot produce that type\n    \
                 {why}"
            ),
            Gap::AmbiguousFormat { column, source, formats, example } => format!(
                "`{column}`: {source:?} parses under more than one format, and they disagree\n    \
                 {}\n    \
                 {example}\n    \
                 Declare which convention these exports use: WITH (date_order = 'dmy').",
                formats.iter().map(|f| format!("{f:?}")).collect::<Vec<_>>().join(" and ")
            ),
        }
    }
}

/// A spec proved to land on the target, plus anything worth saying about it.
#[derive(Debug, Clone)]
pub struct Fitted {
    pub spec: ParseSpec,
    /// Per-column notes: what was bound to what, and anything a reviewer
    /// should look at. Recorded so the mapping is auditable after the fact.
    pub notes: Vec<String>,
}

/// Why a fit failed. Gaps are the interesting case; the rest are errors about
/// the file or the plan rather than about the mapping.
#[derive(Debug)]
pub enum FitError {
    /// The file could not be read or framed at all.
    Unreadable(anyhow::Error),
    /// One or more declared columns could not be supplied.
    Gaps(Vec<Gap>),
    /// A plan was built and then failed its own gate. A bug if it happens:
    /// the binder should not be able to produce a non-conforming plan.
    Rejected(Vec<Mismatch>),
    /// The plan conformed but could not parse the file.
    DryRun(anyhow::Error),
}

impl std::fmt::Display for FitError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FitError::Unreadable(e) => write!(f, "{e:#}"),
            FitError::Gaps(g) => {
                for gap in g {
                    writeln!(f, "  {}", gap.message())?;
                }
                Ok(())
            }
            FitError::Rejected(m) => {
                writeln!(f, "  internal: the fitted plan does not conform to its own target")?;
                for x in m {
                    writeln!(f, "  {}", x.message())?;
                }
                Ok(())
            }
            FitError::DryRun(e) => write!(f, "  the plan conforms but does not parse: {e:#}"),
        }
    }
}

/// Plan a spec for `path` that lands on `target`.
pub fn fit(path: &Path, target: &Target, limits: Limits) -> Result<Fitted, FitError> {
    // 1. The frame. What the sniffer knows about this file's *shape* is about
    //    the file, not about the columns anyone wants, so it is reused whole.
    //    Only its `columns` are discarded.
    let sample = crate::sample::build(path, 16 * 1024, limits)
        .with_context(|| format!("sampling {}", path.display()))
        .map_err(FitError::Unreadable)?;
    let draft = sniff::sniff(path, &sample, limits)
        .with_context(|| format!("framing {}", path.display()))
        .map_err(FitError::Unreadable)?
        .spec;

    // 2. The probe: the same table the executor will see, capped.
    let Probe { header, origin, rows } = probe(path, &draft, limits).map_err(FitError::Unreadable)?;

    // 3. Bind and type, collecting every gap rather than stopping at the
    //    first — a user fixing a twelve-file dataset wants the whole list.
    let mut columns = Vec::with_capacity(target.columns.len());
    let mut gaps = Vec::new();
    let mut notes = Vec::new();

    for tc in &target.columns {
        // Matching is done against the *file's* spelling, so two columns the
        // file calls `Betrag` are two candidates rather than one — the dedupe
        // that renames the second to `Betrag_2` exists so a spec can address
        // it, not so a planner can pretend the collision did not happen.
        let candidates = bind(tc, &origin, target.match_mode);
        let source = match candidates.len() {
            0 => {
                gaps.push(Gap::NoCandidate {
                    column: tc.name.clone(),
                    want: render(&tc.dtype),
                    tried: std::iter::once(tc.name.clone())
                        .chain(tc.matches.iter().cloned())
                        .collect(),
                    header: origin.clone(),
                });
                continue;
            }
            // Exactly one candidate: translate the file's spelling back to
            // the addressable name at the same position.
            1 => {
                let at = candidates[0].0;
                header.get(at).cloned().unwrap_or_else(|| candidates[0].1.clone())
            }
            _ => {
                gaps.push(Gap::Ambiguous {
                    column: tc.name.clone(),
                    candidates: candidates.clone(),
                });
                continue;
            }
        };

        let idx = header.iter().position(|h| *h == source).expect("bound to a real header cell");
        let values: Vec<&str> =
            rows.iter().map(|r| r.get(idx).map(|s| s.as_str()).unwrap_or("")).collect();

        match type_for(&tc.name, &source, &tc.dtype, tc.nullable, &values, target.date_order) {
            Ok((dtype, parse, note)) => {
                if let Some(n) = note {
                    notes.push(n);
                }
                notes.push(format!("`{}` <- {source:?}", tc.name));
                columns.push(ColumnSpec {
                    name: tc.name.clone(),
                    source: Some(source),
                    dtype,
                    nullable: tc.nullable,
                    parse,
                });
            }
            Err(g) => gaps.push(g),
        }
    }

    if !gaps.is_empty() {
        return Err(FitError::Gaps(gaps));
    }

    let spec = ParseSpec {
        extraction: draft.extraction,
        transforms: draft.transforms,
        columns,
        // A fitted spec is not a guess, so it carries no confidence: it either
        // passed every gate or it does not exist.
        confidence: None,
        notes: notes.clone(),
    };

    // 4. The gate. `conforms` proves the shape without I/O; the dry run proves
    //    the values parse. Both, in that order, because the cheap total proof
    //    should reject before the expensive partial one runs.
    if let Err(m) = conforms(&spec, target) {
        return Err(FitError::Rejected(m));
    }
    engine::dry_run(&spec, path, limits)
        .map_err(FitError::DryRun)?;

    Ok(Fitted { spec, notes })
}

/// The post-transform header and body the executor will see, capped.
fn probe(path: &Path, draft: &ParseSpec, limits: Limits) -> Result<Probe> {
    let opts = ExtractOpts::capped(limits, sniff::PROBE_ROWS);
    let mut table = engine::extract(&draft.extraction, path, &opts)
        .with_context(|| format!("extracting {}", path.display()))?;
    engine::apply_transforms(&mut table, &draft.transforms)?;
    table.ensure_header()?;
    let header = table.header.clone().unwrap_or_default();
    let origin = table.header_origin.clone().unwrap_or_else(|| header.clone());
    Ok(Probe { header, origin, rows: table.rows })
}

/// The table the executor will see, plus the header as the file spelt it.
struct Probe {
    /// Addressable names — duplicates disambiguated, so a spec can name them.
    header: Vec<String>,
    /// The file's own spelling, where a duplicate is still a duplicate.
    origin: Vec<String>,
    rows: Vec<Vec<String>>,
}

/// Header cells that supply a declared column name.
///
/// Returns *all* of them: two matches is information the caller needs, not a
/// tie to break.
fn bind(
    tc: &crate::target::TargetColumn,
    header: &[String],
    mode: MatchMode,
) -> Vec<(usize, String)> {
    // The column's own name first, then each declared alias in the order the
    // user wrote them. Tiers are tried in order and the first that matches
    // anything wins: an alias list is a statement of preference, so a file
    // carrying both `Betrag` and `Amount` uses whichever was declared first
    // rather than being called ambiguous.
    let names: Vec<&String> = std::iter::once(&tc.name).chain(tc.matches.iter()).collect();

    let at = |pred: &dyn Fn(&str) -> bool| -> Vec<(usize, String)> {
        header
            .iter()
            .enumerate()
            .filter(|(_, h)| pred(h))
            .map(|(i, h)| (i, h.clone()))
            .collect()
    };

    for want in names {
        let hits = at(&|h| h == want);
        if !hits.is_empty() {
            return hits;
        }
        if mode == MatchMode::Exact {
            continue;
        }
        let w = crate::target::norm(want);
        let hits = at(&|h| crate::target::norm(h) == w);
        if !hits.is_empty() {
            return hits;
        }
        // The sniffer sanitises names for SQL (lowercase, ASCII-folded), so a
        // target written in that style must still bind to the raw header.
        let hits = at(&|h| sniff::sanitize(h) == *want);
        if !hits.is_empty() {
            return hits;
        }
    }
    Vec::new()
}

fn render(t: &ArrowType) -> String {
    match t {
        ArrowType::Utf8 => "TEXT".into(),
        ArrowType::Boolean => "BOOLEAN".into(),
        ArrowType::Int64 => "BIGINT".into(),
        ArrowType::Float64 => "DOUBLE".into(),
        ArrowType::Decimal128(p, s) => format!("DECIMAL({p},{s})"),
        ArrowType::Date32 => "DATE".into(),
        ArrowType::Timestamp(_, None) => "TIMESTAMP".into(),
        ArrowType::Timestamp(_, Some(tz)) => format!("TIMESTAMP WITH TIME ZONE {tz}"),
        other => format!("{other}"),
    }
}

fn na() -> Vec<String> {
    sniff::NA_TOKENS.iter().map(|s| s.to_string()).collect()
}

/// Can these values produce the declared type, and how?
///
/// Nothing here *infers* a type — the target already said what it wants. Each
/// candidate is checked by building the column with the executor's own
/// function, so anything accepted here parses identically at execution.
fn type_for(
    column: &str,
    source: &str,
    want: &ArrowType,
    nullable: bool,
    values: &[&str],
    date_order: Option<DateOrder>,
) -> Result<(DType, ValueParsing, Option<String>), Gap> {
    let untypable = |why: String| Gap::Untypable {
        column: column.to_string(),
        source: source.to_string(),
        want: render(want),
        why,
    };

    // Every candidate carries the NA vocabulary: a blank cell is a null, not
    // an unparseable value, in every type.
    let base = ValueParsing { na_values: na(), ..ValueParsing::default() };

    match want {
        // Anything is text. No candidate can fail, so there is nothing to check.
        ArrowType::Utf8 => Ok((DType::Utf8, base, None)),

        ArrowType::Boolean => check_one(column, source, DType::Bool, base, nullable, values)
            .map(|(d, p)| (d, p, None))
            .map_err(untypable),

        ArrowType::Int64 => {
            // A grouped integer ("1'234") is still an integer; the separator
            // has to be declared for it to parse.
            let mut cands = vec![base.clone()];
            if let Some(f) = numfmt::infer(values) {
                if f.thousands.is_some() && !f.ambiguous {
                    cands.push(ValueParsing { thousands_separator: f.thousands, ..base.clone() });
                }
            }
            first_ok(column, source, DType::Int64, cands, nullable, values).map_err(untypable)
        }

        ArrowType::Float64 => {
            let cands = numeric_candidates(&base, values);
            first_ok(column, source, DType::Float64, cands, nullable, values).map_err(untypable)
        }

        ArrowType::Decimal128(p, s) => {
            let cands = numeric_candidates(&base, values);
            let dtype = DType::Decimal { precision: *p, scale: *s };
            let (d, parse, _) =
                first_ok(column, source, dtype, cands, nullable, values).map_err(untypable)?;
            // Rounding is a value change, so it is said out loud rather than
            // discovered later in a total that is off by a rappen.
            let over = values
                .iter()
                .filter(|v| !sniff::is_na(v))
                .any(|v| frac_digits(v, &parse) > *s as usize);
            let note = over.then(|| {
                format!(
                    "`{column}`: some values carry more than {s} fractional digits and are \
                     rounded half away from zero"
                )
            });
            Ok((d, parse, note))
        }

        ArrowType::Date32 => pick_format(
            column, source, sniff::DATE_FORMATS, nullable, values, &base, date_order,
            |f| DType::Date { format: f.to_string() },
        ),

        ArrowType::Timestamp(_, tz) => {
            let zone = tz.as_ref().map(|z| z.to_string());
            pick_format(
                column, source, sniff::TS_FORMATS, nullable, values, &base, date_order,
                move |f| DType::Timestamp { format: f.to_string(), timezone: zone.clone() },
            )
        }

        other => Err(untypable(format!("tdy cannot produce {other}"))),
    }
}

/// Separator conventions worth trying, informed by the column's own shape.
///
/// `numfmt::infer` decides by shape rather than by trial — it is what stops
/// `1,5` becoming `15` — so its answer leads. A plain candidate follows for
/// the ordinary case where no separator is involved.
fn numeric_candidates(base: &ValueParsing, values: &[&str]) -> Vec<ValueParsing> {
    let mut out = Vec::new();
    if let Some(f) = numfmt::infer(values) {
        if !f.ambiguous && (f.decimal.is_some() || f.thousands.is_some()) {
            out.push(ValueParsing {
                decimal_separator: f.decimal,
                thousands_separator: f.thousands,
                ..base.clone()
            });
        }
    }
    out.push(base.clone());
    out
}

fn frac_digits(v: &str, p: &ValueParsing) -> usize {
    numfmt::frac_digits_with(v.trim(), p.decimal_separator, p.thousands_separator)
}

/// The day/month/year order a format implies, if it has one.
fn field_order(f: &str) -> Option<DateOrder> {
    let (mut d, mut m, mut y) = (None, None, None);
    let bytes: Vec<char> = f.chars().collect();
    for (i, w) in bytes.windows(2).enumerate() {
        if w[0] != '%' {
            continue;
        }
        match w[1] {
            'd' => d = Some(i),
            'm' | 'b' | 'B' => m = Some(i),
            'Y' | 'y' => y = Some(i),
            _ => {}
        }
    }
    match (d, m, y) {
        (Some(d), Some(m), Some(y)) => {
            if y < d && y < m {
                Some(DateOrder::Ymd)
            } else if d < m {
                Some(DateOrder::Dmy)
            } else {
                Some(DateOrder::Mdy)
            }
        }
        _ => None,
    }
}

/// Choose a strftime format, refusing when more than one reading is possible.
///
/// The ambiguity test is exact: every format that parses the whole probe is
/// applied, and they are only ambiguous if two of them **disagree about a
/// value in this file**. `%d.%m.%Y` and `%m.%d.%Y` are a gap on a file
/// containing `03.04.2025`, and are not a gap on one where every day-of-month
/// is over twelve — because there, they mean the same thing.
fn pick_format<F>(
    column: &str,
    source: &str,
    formats: &[&'static str],
    nullable: bool,
    values: &[&str],
    base: &ValueParsing,
    date_order: Option<DateOrder>,
    make: F,
) -> Result<(DType, ValueParsing, Option<String>), Gap>
where
    F: Fn(&str) -> DType,
{
    let mut ok: Vec<(&'static str, ArrayRef)> = Vec::new();
    let mut last_err = String::new();
    for f in formats {
        let col = ColumnSpec {
            name: column.to_string(),
            source: None,
            dtype: make(f),
            nullable,
            parse: base.clone(),
        };
        match engine::build_column_at(&col, values, 0) {
            Ok((_, arr)) => ok.push((f, arr)),
            Err(e) => last_err = format!("{e:#}"),
        }
    }

    let Some((first, first_arr)) = ok.first().cloned() else {
        return Err(Gap::Untypable {
            column: column.to_string(),
            source: source.to_string(),
            want: "a date/time".into(),
            why: if last_err.is_empty() {
                "no format tdy knows parses these values".into()
            } else {
                last_err
            },
        });
    };

    // Only formats that DISAGREE with the winner are ambiguous. Two spellings
    // that produce the same array on this file mean the same thing here,
    // whatever they might mean on some other file.
    let disagreeing: Vec<&'static str> = ok
        .iter()
        .skip(1)
        .filter(|(_, arr)| arr.as_ref() != first_arr.as_ref())
        .map(|(f, _)| *f)
        .collect();

    if disagreeing.is_empty() {
        return Ok((make(first), base.clone(), None));
    }

    // There is a real conflict. A declared `date_order` is a human saying
    // which convention their exports use, so it *resolves* the conflict — it
    // does not pre-prune the candidate list. Pruning threw away `%Y-%m-%d` on
    // a dataset declared 'dmy', even though an ISO date cannot be confused
    // with a day-first one and no choice was ever needed.
    if let Some(order) = date_order {
        let matching: Vec<&'static str> = std::iter::once(first)
            .chain(disagreeing.iter().copied())
            .filter(|f| field_order(f) == Some(order))
            .collect();
        if matching.len() == 1 {
            return Ok((make(matching[0]), base.clone(), None));
        }
    }

    let example = values
        .iter()
        .find(|v| !sniff::is_na(v))
        .map(|v| format!("e.g. {v:?} means two different dates under them"))
        .unwrap_or_default();
    let mut formats = vec![first.to_string()];
    formats.extend(disagreeing.iter().map(|f| f.to_string()));
    Err(Gap::AmbiguousFormat {
        column: column.to_string(),
        source: source.to_string(),
        formats,
        example,
    })
}

/// Build one candidate and report whether the executor accepts it.
fn check_one(
    column: &str,
    _source: &str,
    dtype: DType,
    parse: ValueParsing,
    nullable: bool,
    values: &[&str],
) -> Result<(DType, ValueParsing), String> {
    let col = ColumnSpec {
        name: column.to_string(),
        source: None,
        dtype: dtype.clone(),
        nullable,
        parse: parse.clone(),
    };
    match engine::build_column_at(&col, values, 0) {
        Ok(_) => Ok((dtype, parse)),
        Err(e) => Err(format!("{e:#}")),
    }
}

fn first_ok(
    column: &str,
    source: &str,
    dtype: DType,
    candidates: Vec<ValueParsing>,
    nullable: bool,
    values: &[&str],
) -> Result<(DType, ValueParsing, Option<String>), String> {
    let mut last = String::new();
    for parse in candidates {
        match check_one(column, source, dtype.clone(), parse, nullable, values) {
            Ok((d, p)) => return Ok((d, p, None)),
            Err(e) => last = e,
        }
    }
    Err(last)
}
