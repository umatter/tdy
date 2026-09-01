//! What a judgement actually does to the data.
//!
//! The review gate exists because tdy cannot check a claim about the world:
//! that these integers are Rappen, that November is all Ticino, that this is
//! the sheet you meant. Its value is entirely in a human *reading* before
//! saying yes — so the accept screen must show the consequence, not restate
//! the reason.
//!
//! That is what this module computes. For a `decimal_shift` it pairs raw
//! text with the parsed value it becomes, and — because a shift is wrong in
//! a way only the extremes reveal — it reads the **whole column** to report
//! the largest and smallest results. That read is streamed and projected to
//! one column, which is what makes "the whole file" affordable for something
//! a person triggers by pressing a key.

use anyhow::{Context, Result};
use crate::config::Limits;
use crate::spec::{ColumnSpec, DType, ParseSpec, Transform, ValueParsing};

use datafusion::arrow::array::Array;
use std::path::Path;

/// One row of "this text becomes this value".
#[derive(Debug, Clone, PartialEq)]
pub struct Pair {
    /// 1-based index into the DATA rows — the rows the spec produces, after
    /// title rows are skipped and a header is promoted. Deliberately not
    /// called a line number: it is not one, and pointing a reader at the
    /// wrong line of their file is its own small wrong answer.
    pub row: usize,
    pub raw: String,
    pub parsed: String,
}

/// What accepting this member would do, ready to render.
#[derive(Debug, Clone)]
pub enum Evidence {
    /// A column's values are being moved by a decimal shift.
    Shift {
        column: String,
        source: String,
        shift: i8,
        /// The first rows, as they read and as they parse.
        head: Vec<Pair>,
        /// The extremes, over every row of the file — a shift in the wrong
        /// direction is invisible in the head of a file and obvious here.
        smallest: Option<Pair>,
        largest: Option<Pair>,
        rows: usize,
    },
    /// A column is being filled with a value the file never contained.
    Constant { column: String, value: String, rows: usize },
    /// A model chose how to read this file.
    Frame { description: String, head: Vec<Vec<String>>, header: Vec<String>, rows: usize },
    /// A judgement with no computable consequence — shown as its reason
    /// alone rather than as a blank panel pretending to be evidence.
    Unillustrated { reason: String },
}

impl Evidence {
    /// One line saying what the reader is looking at.
    pub fn headline(&self) -> String {
        match self {
            Evidence::Shift { column, shift, rows, .. } => format!(
                "`{column}`: {rows} value(s), each with its decimal point moved {} place(s) \
                 {}",
                shift.abs(),
                // `engine::shift_decimal_point` adds the shift to the integer
                // part's length, so a NEGATIVE shift moves the point left and
                // divides: 170000 -> 1700.00. Naming that "right" is the kind
                // of error this whole screen exists to prevent.
                if *shift < 0 { "left (÷)" } else { "right (×)" }
            ),
            Evidence::Constant { column, value, rows } => {
                format!("`{column}`: {value:?} asserted into all {rows} row(s)")
            }
            Evidence::Frame { description, rows, .. } => {
                format!("frame: {description} — {rows} row(s) read this way")
            }
            Evidence::Unillustrated { .. } => "no computable consequence to show".into(),
        }
    }
}

/// Compute the evidence for a member's judgement — **all** of it.
///
/// A spec can carry several judgements at once: two shifted columns, a shift
/// and an asserted constant, a model-chosen frame with a shift inside it.
/// Returning only the first would mean accepting the rest unseen, which is
/// the one thing this screen exists to prevent — so this returns every one,
/// and the screen shows them all.
///
/// Reads the file. Called when a human opens the accept screen, never during
/// a fit.
pub fn for_spec(
    spec: &ParseSpec,
    path: &Path,
    limits: Limits,
    review: &str,
    model_framed: bool,
) -> Result<Vec<Evidence>> {
    let mut out = Vec::new();
    for c in spec.columns.iter().filter(|c| c.parse.decimal_shift.unwrap_or(0) != 0) {
        out.push(shift_evidence(spec, path, limits, c)?);
    }
    for t in &spec.transforms {
        if let Transform::Constant { name, value } = t {
            if !value.is_empty() {
                out.push(Evidence::Constant {
                    column: name.clone(),
                    value: value.clone(),
                    rows: row_count(spec, path, limits)?,
                });
            }
        }
    }
    // Whether a model chose the frame is recorded in the sidecar's
    // provenance, and that is what the caller passes in. Deciding it by
    // looking for the word "frame" in free-text prose was a guess that a
    // reworded review reason would silently break.
    if model_framed {
        out.push(frame_evidence(spec, path, limits)?);
    }
    if out.is_empty() {
        out.push(Evidence::Unillustrated { reason: review.to_string() });
    }
    Ok(out)
}

/// Is `a` numerically less than `b`, both being decimal strings?
///
/// String surgery rather than a float parse, for the reason `shift_decimal_point`
/// itself does string surgery: the values here are money, and an f64 cannot
/// hold DECIMAL(38, 2) faithfully.
fn decimal_lt(a: &str, b: &str) -> bool {
    let (na, a) = split_sign(a.trim());
    let (nb, b) = split_sign(b.trim());
    match (na, nb) {
        (true, false) => return true,
        (false, true) => return false,
        _ => {}
    }
    let less = magnitude_lt(a, b);
    // Among negatives, the bigger magnitude is the smaller number.
    if na { magnitude_lt(b, a) } else { less }
}

fn split_sign(s: &str) -> (bool, &str) {
    match s.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, s.strip_prefix('+').unwrap_or(s)),
    }
}

fn magnitude_lt(a: &str, b: &str) -> bool {
    let (ai, af) = a.split_once('.').unwrap_or((a, ""));
    let (bi, bf) = b.split_once('.').unwrap_or((b, ""));
    let (ai, bi) = (ai.trim_start_matches('0'), bi.trim_start_matches('0'));
    if ai.len() != bi.len() {
        return ai.len() < bi.len();
    }
    if ai != bi {
        return ai < bi;
    }
    // Same integer part: compare fractions digit by digit, padded.
    let n = af.len().max(bf.len());
    let pad = |s: &str| format!("{s:0<n$}");
    pad(af) < pad(bf)
}

/// The same column twice: once as the file spells it, once as tdy parses it.
fn shift_evidence(
    spec: &ParseSpec,
    path: &Path,
    limits: Limits,
    col: &ColumnSpec,
) -> Result<Evidence> {
    let shift = col.parse.decimal_shift.unwrap_or(0);
    let source = col.source_name().to_string();

    // Two one-column specs over the same frame. Projecting to one column is
    // what keeps a whole-file read cheap; reading them separately (rather
    // than as one two-column spec) keeps the raw side genuinely untouched —
    // no na_values, no separators, no strip.
    let raw_spec = ParseSpec {
        extraction: spec.extraction.clone(),
        transforms: spec.transforms.clone(),
        columns: vec![ColumnSpec {
            name: "raw".into(),
            source: Some(source.clone()),
            dtype: DType::Utf8,
            nullable: true,
            parse: ValueParsing::default(),
        }],
        confidence: None,
        notes: vec![],
    };
    let parsed_spec = ParseSpec {
        extraction: spec.extraction.clone(),
        transforms: spec.transforms.clone(),
        columns: vec![col.clone()],
        confidence: None,
        notes: vec![],
    };

    let raw = strings_of(&raw_spec, path, limits).context("reading the column as text")?;
    let parsed = strings_of(&parsed_spec, path, limits).context("parsing the column")?;
    let rows = raw.len().min(parsed.len());

    let pair_at = |i: usize| Pair {
        row: i + 1,
        raw: raw.get(i).cloned().unwrap_or_default(),
        parsed: parsed.get(i).cloned().unwrap_or_default(),
    };
    let head: Vec<Pair> = (0..rows.min(6)).map(pair_at).collect();

    // The extremes are found on the PARSED values, ordered numerically — that
    // is the axis a wrong shift distorts, and ordering the raw text would
    // sort "9" above "1000".
    //
    // Compared as decimal strings rather than through f64: this column is
    // usually money, DECIMAL(38, s) holds more digits than an f64 has
    // precision, and two amounts that differ in the last rappen would
    // compare equal. Showing the wrong row as "the largest" on the screen
    // whose whole job is to be checkable is not a rounding error.
    let mut smallest: Option<(usize, &str)> = None;
    let mut largest: Option<(usize, &str)> = None;
    for i in 0..rows {
        let Some(v) = parsed.get(i).map(|s| s.as_str()) else { continue };
        if v.trim().is_empty() {
            continue;
        }
        if smallest.map(|(_, s)| decimal_lt(v, s)).unwrap_or(true) {
            smallest = Some((i, v));
        }
        if largest.map(|(_, s)| decimal_lt(s, v)).unwrap_or(true) {
            largest = Some((i, v));
        }
    }
    let smallest = smallest.map(|(i, _)| pair_at(i));
    let largest = largest.map(|(i, _)| pair_at(i));

    Ok(Evidence::Shift {
        column: col.name.clone(),
        source,
        shift,
        head,
        smallest,
        largest,
        rows,
    })
}

fn frame_evidence(spec: &ParseSpec, path: &Path, limits: Limits) -> Result<Evidence> {
    // The head comes from `preview`, which is bounded; the row count from a
    // streamed pass, which holds one batch. Collecting every batch to show
    // six rows made the accept screen fail on members an ordinary query
    // handles fine.
    let batch = crate::engine::preview(spec, path, limits, 6)?;
    let header: Vec<String> =
        spec.columns.iter().map(|c| format!("{} ← {}", c.name, c.source_name())).collect();
    let head = (0..batch.num_rows())
        .map(|i| (0..batch.num_columns()).map(|c| cell(batch.column(c), i)).collect())
        .collect();
    Ok(Evidence::Frame {
        description: describe_extraction(spec),
        header,
        head,
        rows: row_count(spec, path, limits)?,
    })
}

fn describe_extraction(spec: &ParseSpec) -> String {
    use crate::spec::Extraction;
    let mut parts = vec![spec.extraction.format_name().to_string()];
    match &spec.extraction {
        Extraction::Json { pointer: Some(p), .. } => parts.push(format!("pointer {p:?}")),
        Extraction::Excel { sheet_name: Some(s), .. } => parts.push(format!("sheet {s:?}")),
        Extraction::Lines { pattern, .. } => parts.push(format!("pattern {pattern:?}")),
        _ => {}
    }
    for t in &spec.transforms {
        parts.push(match t {
            Transform::SkipRows { head, tail } => format!("skip_rows {head}+{tail}"),
            Transform::PromoteHeader { rows, .. } => format!("promote_header {rows}"),
            Transform::DropRowsMatching { .. } => "drop_rows_matching".into(),
            Transform::FillDown { .. } => "fill_down".into(),
            Transform::Unpivot { .. } => "unpivot".into(),
            Transform::Constant { name, .. } => format!("constant {name:?}"),
        });
    }
    parts.join(", ")
}

/// Every value of a one-column spec, rendered as text.
///
/// Streamed where the shape allows, so the whole-file read this module
/// promises costs one batch of memory rather than the file.
fn strings_of(spec: &ParseSpec, path: &Path, limits: Limits) -> Result<Vec<String>> {
    let mut out = Vec::new();
    if crate::stream::enabled() && crate::stream::can_stream(spec) {
        crate::stream::execute_with(spec, path, limits, |b| {
            let col = b.column(0);
            for i in 0..b.num_rows() {
                out.push(cell(col, i));
            }
            Ok(())
        })?;
    } else {
        for b in crate::engine::execute_batches(spec, path, limits)? {
            let col = b.column(0);
            for i in 0..b.num_rows() {
                out.push(cell(col, i));
            }
        }
    }
    Ok(out)
}

/// How many rows the spec produces, streamed where the shape allows — the
/// count is one number and must not cost the file.
fn row_count(spec: &ParseSpec, path: &Path, limits: Limits) -> Result<usize> {
    let mut n = 0usize;
    if crate::stream::enabled() && crate::stream::can_stream(spec) {
        crate::stream::execute_with(spec, path, limits, |b| {
            n += b.num_rows();
            Ok(())
        })?;
    } else {
        n = crate::engine::execute_batches(spec, path, limits)?.iter().map(|b| b.num_rows()).sum();
    }
    Ok(n)
}

fn cell(col: &dyn Array, i: usize) -> String {
    if col.is_null(i) {
        return String::new();
    }
    datafusion::arrow::util::display::array_value_to_string(col, i).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn corpus() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("testdata")
            .join("drifting_exports")
    }

    /// The Rappen file, with the shift a human would write. The evidence must
    /// show the raw integers beside the francs they become, and it must find
    /// the extremes over the WHOLE file — a shift in the wrong direction is
    /// invisible in the head and unmissable at the ends.
    #[test]
    fn a_shift_is_shown_as_raw_beside_parsed_with_the_extremes() {
        let dir = tempfile::tempdir().unwrap();
        let csv = dir.path().join("2025-07.csv");
        std::fs::copy(corpus().join("2025-07.csv"), &csv).unwrap();

        let spec: ParseSpec = toml::from_str(
            r#"
[extraction]
format = "delimited"
delimiter = ";"
quote = '"'
encoding = "windows-1252"
ragged = "pad_nulls"
[[transforms]]
op = "promote_header"
rows = 1
join = " "
[[columns]]
name = "amount_chf"
source = "Betrag Rp."
nullable = false
[columns.dtype]
type = "decimal"
precision = 14
scale = 2
[columns.parse]
decimal_shift = -2
"#,
        )
        .expect("the fixture spec parses");

        let all = for_spec(&spec, &csv, Limits::default(), "decimal_shift", false).unwrap();
        assert_eq!(all.len(), 1, "{all:?}");
        let Evidence::Shift { head, smallest, largest, rows, shift, .. } = all[0].clone() else {
            panic!("expected shift evidence, got {all:?}");
        };
        assert_eq!(shift, -2);
        assert_eq!(rows, 4);
        // Raw Rappen, parsed francs — the whole point, side by side.
        assert_eq!(head[0].raw, "170000");
        assert_eq!(head[0].parsed, "1700.00");
        // The generator's July is 1700..1730; the extremes must be those.
        assert_eq!(smallest.unwrap().parsed, "1700.00");
        assert_eq!(largest.unwrap().parsed, "1730.00");
    }

    /// An asserted constant is shown with the number of rows it reaches —
    /// "Ticino, in all 4 rows" is the sentence a reviewer needs.
    #[test]
    fn a_constant_is_shown_with_the_rows_it_reaches() {
        let dir = tempfile::tempdir().unwrap();
        let csv = dir.path().join("2025-11.csv");
        std::fs::copy(corpus().join("2025-11.csv"), &csv).unwrap();

        let spec: ParseSpec = toml::from_str(
            r#"
[extraction]
format = "delimited"
delimiter = ";"
quote = '"'
ragged = "pad_nulls"
[[transforms]]
op = "promote_header"
rows = 1
join = " "
[[transforms]]
op = "constant"
name = "region"
value = "Ticino"
[[columns]]
name = "region"
nullable = true
[columns.dtype]
type = "utf8"
"#,
        )
        .expect("the fixture spec parses");

        let all = for_spec(&spec, &csv, Limits::default(), "constant", false).unwrap();
        let Evidence::Constant { column, value, rows } = all[0].clone() else {
            panic!("expected constant evidence, got {all:?}");
        };
        assert_eq!((column.as_str(), value.as_str(), rows), ("region", "Ticino", 4));
    }

    /// Money is compared as decimals, not through f64. DECIMAL(38, 2) holds
    /// more digits than an f64 has precision, so ordering through a float
    /// picks the wrong row as "the largest" on the one screen whose whole
    /// job is to be checkable.
    #[test]
    fn extremes_are_ordered_as_decimals_not_as_floats() {
        // Two amounts an f64 cannot tell apart: both round to the same
        // double, so a float ordering reports whichever came first.
        let a = "9007199254740993.01";
        let b = "9007199254740993.02";
        assert_eq!(
            a.parse::<f64>().unwrap(),
            b.parse::<f64>().unwrap(),
            "if these differ as f64 the test proves nothing"
        );
        assert!(decimal_lt(a, b), "decimal ordering must separate them");

        // …and the ordinary properties hold.
        assert!(decimal_lt("9.99", "10.00"), "digit count beats lexicography");
        assert!(decimal_lt("-1700.00", "0.00"));
        assert!(decimal_lt("-1730.00", "-1700.00"), "more negative is smaller");
        assert!(decimal_lt("1700.5", "1700.50001"));
        assert!(!decimal_lt("1700.00", "1700.00"));
        assert!(decimal_lt("0.1", "0.2"));
    }

    /// A spec carrying two judgements shows both. Returning only the first
    /// would mean the second is accepted unseen — the exact failure this
    /// screen exists to prevent.
    #[test]
    fn every_judgement_in_a_spec_is_illustrated() {
        let dir = tempfile::tempdir().unwrap();
        let csv = dir.path().join("both.csv");
        std::fs::write(&csv, "Datum;Betrag Rp.\n31.07.2025;170000\n31.07.2025;173000\n")
            .unwrap();

        let spec: ParseSpec = toml::from_str(
            r#"
[extraction]
format = "delimited"
delimiter = ";"
quote = '"'
ragged = "pad_nulls"
[[transforms]]
op = "promote_header"
rows = 1
join = " "
[[transforms]]
op = "constant"
name = "region"
value = "Ticino"
[[columns]]
name = "amount_chf"
source = "Betrag Rp."
nullable = false
[columns.dtype]
type = "decimal"
precision = 14
scale = 2
[columns.parse]
decimal_shift = -2
[[columns]]
name = "region"
nullable = true
[columns.dtype]
type = "utf8"
"#,
        )
        .unwrap();

        let all = for_spec(&spec, &csv, Limits::default(), "two judgements", false).unwrap();
        assert_eq!(all.len(), 2, "both judgements must be shown: {all:?}");
        assert!(all.iter().any(|e| matches!(e, Evidence::Shift { .. })));
        assert!(all.iter().any(|e| matches!(e, Evidence::Constant { .. })));
    }

    /// The direction the headline names is the direction the executor moves
    /// the point. A negative shift divides — 170000 becomes 1700.00 — and
    /// calling that "right" would be a wrong statement on the screen whose
    /// purpose is to be read.
    #[test]
    fn the_headline_names_the_direction_the_executor_moves() {
        let e = Evidence::Shift {
            column: "amount_chf".into(),
            source: "Betrag Rp.".into(),
            shift: -2,
            head: vec![],
            smallest: None,
            largest: None,
            rows: 4,
        };
        let h = e.headline();
        assert!(h.contains("left"), "{h}");
        assert!(h.contains('÷'), "{h}");
        // And the executor agrees with the word.
        assert_eq!(crate::engine::shift_decimal_point("170000", -2), "1700.00");
    }

    /// A judgement with nothing to compute says so, rather than rendering an
    /// empty panel that looks like evidence of nothing being wrong.
    #[test]
    fn a_judgement_with_no_consequence_says_so() {
        let dir = tempfile::tempdir().unwrap();
        let csv = dir.path().join("2025-01.csv");
        std::fs::copy(corpus().join("2025-01.csv"), &csv).unwrap();
        let spec: ParseSpec = toml::from_str(
            r#"
[extraction]
format = "delimited"
delimiter = ";"
quote = '"'
ragged = "pad_nulls"
[[transforms]]
op = "promote_header"
rows = 1
join = " "
[[columns]]
name = "region"
source = "Region"
nullable = true
[columns.dtype]
type = "utf8"
"#,
        )
        .unwrap();
        let all = for_spec(&spec, &csv, Limits::default(), "something unusual", false).unwrap();
        assert!(matches!(all[0], Evidence::Unillustrated { .. }), "{all:?}");
        assert!(all[0].headline().contains("no computable consequence"));
    }
}
