//! Tier-1 sniffer: deterministic heuristics that turn a file into a draft
//! ParseSpec plus a confidence score. High confidence -> the draft ships as
//! the spec (method = heuristic). Low confidence -> the draft goes to the
//! LLM tier as a starting point to correct (method = llm).
//!
//! The governing invariant is that **the sniffer can never propose a spec the
//! engine cannot execute, and never one that reads the wrong column.** The
//! way that is guaranteed is structural: the sniffer builds its column list
//! from the header of a table that has already had every structural transform
//! applied to it. There is no second, parallel notion of what the columns are
//! called — `ColumnSpec.source` is copied from the exact header the executor
//! will see.
//!
//! (It used to guess names from the *raw* header instead. Two columns both
//! called `Betrag` then produced two output columns both reading the first
//! one: the second column's numbers silently disappeared and the first one's
//! were duplicated.)

use std::path::Path;

use anyhow::{bail, Context, Result};
use chrono::{NaiveDate, NaiveDateTime};

use crate::config::Limits;
use crate::detect;
use crate::engine::{self, ExtractOpts, RawTable};
use crate::numfmt;
use crate::sample::{FileSample, FormatGuess, CONTINUES_MARKER};
use crate::spec::{
    ColumnSpec, DType, Extraction, NoMatchPolicy, ParseSpec, RaggedPolicy, Transform, ValueParsing,
};

/// How many rows the sniffer reads to make its decisions. Everything past
/// this is the executor's business, not the sniffer's.
pub(crate) const PROBE_ROWS: usize = 2000;
/// How many body rows inform a column's type guess.
pub(crate) const TYPE_SAMPLE: usize = 500;

pub struct SniffResult {
    pub spec: ParseSpec,
    pub confidence: f32,
}

/// Accumulates the reasons a spec might be wrong, so that the confidence
/// score and the human-readable notes can never drift apart.
#[derive(Default)]
struct Doubts {
    penalty: f32,
    notes: Vec<String>,
}

impl Doubts {
    fn add(&mut self, penalty: f32, note: impl Into<String>) {
        self.penalty += penalty;
        self.notes.push(note.into());
    }
    fn note(&mut self, note: impl Into<String>) {
        self.notes.push(note.into());
    }
    fn finish(self, base: f32) -> (f32, Vec<String>) {
        ((base - self.penalty).clamp(0.0, 1.0), self.notes)
    }
}

/// How much work the sniffer should do before it answers.
#[derive(Debug, Clone, Copy)]
pub struct SniffOpts {
    /// Check every inferred type against the whole file. See [`verify_types`].
    ///
    /// On by default, because a type guessed from 500 rows is a guess about
    /// all the others and getting it wrong means a spec that dies mid-query.
    /// It costs a full read: about 6 s for a 141 MB CSV, 40 s for 987 MB,
    /// paid once when the sidecar is written rather than per query.
    pub verify: bool,
}

impl Default for SniffOpts {
    fn default() -> Self {
        SniffOpts { verify: true }
    }
}

pub fn sniff(path: &Path, sample: &FileSample, limits: Limits) -> Result<SniffResult> {
    sniff_opts(path, sample, limits, SniffOpts::default())
}

pub fn sniff_opts(
    path: &Path,
    sample: &FileSample,
    limits: Limits,
    opts: SniffOpts,
) -> Result<SniffResult> {
    let mut res = match sample.format {
        FormatGuess::Excel => sniff_excel(path, sample, limits),
        FormatGuess::Json => sniff_json(path, limits),
        FormatGuess::Delimited | FormatGuess::Unknown => sniff_text(path, sample, limits),
    }?;
    if opts.verify {
        verify_types(&mut res.spec, path, limits);
    } else {
        // Said in the sidecar rather than only on the terminal, because the
        // sidecar is what a colleague reads six months later and the claim
        // "these are the types" is weaker here than it looks.
        res.spec.notes.push(
            "types were inferred from a sample and NOT checked against the whole file \
             (--quick): a value further in may not fit, and the query will say so when it \
             reaches one. Re-run `tdy sniff` without --quick to check."
                .to_string(),
        );
    }
    Ok(res)
}

/// Check every inferred type against the **whole file**, and widen the ones
/// that do not hold.
///
/// A type inferred from the first 500 rows is a guess about all the others,
/// and real exports break it as a matter of course. Three files from a corpus
/// of twenty-six public data-wrangling repositories, none of them contrived:
///
/// * `hotels.csv` — `children` is an integer for 40,600 rows and then `NA`;
/// * `202306-divvy-tripdata.csv` — `start_station_id` is digits until row 708
///   and `TA1309000067` after it;
/// * `animalRescue.csv` — `incidentnumber` gains a `-18112015` suffix at row
///   4067.
///
/// Every one of those produced a spec that validated, sniffed confidently, and
/// then died mid-query naming a row. Erroring was correct — the alternative is
/// a wrong number — but it was avoidable, and "correct refusal" is not the
/// same as working.
///
/// So the guess is checked and, where it fails, widened to text. Text always
/// holds, so this terminates; and widening loses no information, because the
/// value was already going to be unreadable as the guessed type. The note says
/// which column and why, so a user who wants the narrow type knows exactly
/// what to fix.
///
/// This costs a full read of the file. That is affordable only because
/// extraction streams — before that it would have cost eight times the file in
/// memory — and it is paid once, when the sidecar is written, not per query.
pub fn verify_types(spec: &mut crate::spec::ParseSpec, path: &Path, limits: Limits) {
    // Nothing to check if every column is already text.
    if spec.columns.iter().all(|c| c.dtype == DType::Utf8) {
        return;
    }
    let mut v = match crate::stream::verify(spec, path, limits) {
        Ok(v) => v,
        // A file that cannot be read at all is not this function's problem —
        // the caller's own execution reports it properly. But silence here
        // reads as "verified, and clean", which is a stronger claim than
        // "could not check", so say which one happened.
        Err(e) => {
            spec.notes.push(format!(
                "types were NOT checked against the whole file ({e:#}); they rest on the                  sample alone"
            ));
            spec.confidence = Some(spec.confidence.unwrap_or(1.0).min(0.6));
            return;
        }
    };

    // Drop repeated headers first, then re-check the types: a header sitting
    // in the middle of the data is text in every column, so leaving it in
    // would widen every numeric column in the file and hide the real cause.
    if v.repeated_header_rows > 0 {
        // Only when the names came from the file. With no header row the
        // sniffer invents `col_1`, `col_2`, … and a data row that happens to
        // spell those out is not provably a header — it is a row somebody
        // would lose.
        // A header exists only if something promoted one; otherwise the
        // sniffer invented `col_1`, `col_2`, … and those names are ours, not
        // the file's.
        let named_by_the_file = spec
            .transforms
            .iter()
            .any(|t| matches!(t, Transform::PromoteHeader { .. }));
        if spec.columns.len() > 1 && named_by_the_file {
            // Match the WHOLE row, not its first cell. A pattern anchored on
            // one column deletes every row whose first field happens to equal
            // that header — an `invoice` column containing the literal value
            // "invoice", say — which is real data destroyed on the strength of
            // a detection that proved something else entirely. The executor
            // joins a row with tabs when `column` is None, so this matches
            // exactly the rows that were counted.
            let joined = spec
                .columns
                .iter()
                .map(|c| regex::escape(c.source_name()))
                .collect::<Vec<_>>()
                .join("\t");
            spec.transforms.push(Transform::DropRowsMatching {
                pattern: format!("^{joined}$"),
                column: None,
            });
            spec.notes.push(format!(
                "{DROPPED_NOTE}{} row(s) identical to the header — the file looks like several \
                 exports concatenated together",
                v.repeated_header_rows
            ));
            if let Ok(again) = crate::stream::verify(spec, path, limits) {
                v = again;
            }
        }
    }

    if v.failing.is_empty() {
        return;
    }
    let total = v.rows;
    for (i, _why) in v.failing {
        let Some(off) = v.offenders.iter().find(|o| o.column == i) else {
            continue;
        };
        let shown: Vec<String> = off
            .examples
            .iter()
            .map(|(row, val)| format!("{val:?} (row {row})"))
            .collect();
        // "at least", once counting stopped — a capped number printed bare
        // reads as exact.
        let count = if off.capped {
            format!("at least {}", off.count)
        } else {
            off.count.to_string()
        };
        let Some(col) = spec.columns.get_mut(i) else { continue };
        let was = col.dtype.clone();
        col.dtype = DType::Utf8;
        col.parse = ValueParsing::default();
        spec.notes.push(format!(
            "column `{}`: kept as text — {} of {} values are not {}: {}. \
             If those are strays rather than data, drop them with a \
             `drop_rows_matching` transform and narrow the type by hand.",
            col.name,
            count,
            total,
            type_word(&was),
            shown.join(", ")
        ));
    }
    // The sniffer was wrong about something, and saying so is the point of
    // having a confidence at all.
    spec.confidence = spec.confidence.map(|c| (c - 0.1).max(0.0));
}

fn type_word(d: &DType) -> &'static str {
    match d {
        DType::Utf8 => "text",
        DType::Bool => "a boolean",
        DType::Int64 => "an integer",
        DType::Float64 => "a number",
        DType::Decimal { .. } => "a decimal",
        DType::Date { .. } => "a date",
        DType::Timestamp { .. } => "a timestamp",
    }
}


// ---------------------------------------------------------------------------
// Text: log lines, then delimited, then fixed width
// ---------------------------------------------------------------------------

fn sniff_text(path: &Path, sample: &FileSample, limits: Limits) -> Result<SniffResult> {
    let head = sample_head(sample);
    if head.trim().is_empty() {
        bail!("{} is empty", path.display());
    }

    // A recognised log format is a much stronger signal than any delimiter
    // score, and the delimited sniffer would otherwise carve log lines up on
    // whatever punctuation happens to be most regular.
    if let Some(p) = detect::detect_log_pattern(head) {
        if let Ok(r) = sniff_lines(path, sample, &p, limits) {
            return Ok(r);
        }
    }

    let delim = pick_delimiter(head);
    // Try column alignment when no delimiter carries the file: either none
    // was found at all (one field per line) or the best one is unconvincing.
    if delim.modal_fields < 2 || delim.score < 0.6 {
        if let Some(fields) = detect::detect_fixed_width(head) {
            if let Ok(r) = sniff_fixed_width(path, sample, fields, limits) {
                return Ok(r);
            }
        }
    }
    sniff_delimited(path, sample, delim, limits)
}

fn sample_head(sample: &FileSample) -> &str {
    sample.body.split(CONTINUES_MARKER).next().unwrap_or(&sample.body)
}

fn sample_tail(sample: &FileSample) -> Option<&str> {
    let mut parts = sample.body.split(CONTINUES_MARKER);
    parts.next();
    parts.next()
}

/// The last non-empty line the sample shows, whether the file was read whole
/// or only at its ends. Footer detection needs the real end of the file, not
/// the end of a probe window.
fn last_line(sample: &FileSample) -> Option<&str> {
    let region = sample_tail(sample).unwrap_or_else(|| sample_head(sample));
    region.lines().rev().find(|l| !l.trim().is_empty())
}

struct DelimGuess {
    delimiter: char,
    score: f32,
    modal_fields: usize,
}

fn pick_delimiter(head: &str) -> DelimGuess {
    let mut best: Option<DelimGuess> = None;
    for cand in [',', ';', '\t', '|'] {
        let counts = csv_field_counts(head, cand);
        if counts.is_empty() {
            continue;
        }
        let modal = modal(&counts).unwrap_or(1);
        if modal < 2 {
            continue;
        }
        let lead = counts.iter().take_while(|c| **c != modal).count();
        let body = &counts[lead..];
        let consistent =
            body.iter().filter(|c| **c == modal).count() as f32 / body.len().max(1) as f32;
        let score = consistent * (1.0 + (modal.min(20) as f32) / 40.0) - 0.02 * lead as f32;
        if best.as_ref().map(|b| score > b.score).unwrap_or(true) {
            best = Some(DelimGuess { delimiter: cand, score, modal_fields: modal });
        }
    }
    // No candidate produced more than one field. That is a legitimate shape
    // (a single-column list) but it is also what an unrecognised layout looks
    // like, so it is reported as one field rather than as a confident guess.
    best.unwrap_or(DelimGuess { delimiter: ',', score: 1.0, modal_fields: 1 })
}

fn sniff_delimited(
    path: &Path,
    sample: &FileSample,
    delim: DelimGuess,
    limits: Limits,
) -> Result<SniffResult> {
    let mut doubts = Doubts::default();
    if delim.score < 0.85 && delim.modal_fields > 1 {
        doubts.add(0.2, "delimiter inference was ambiguous");
    }
    if delim.modal_fields < 2 {
        // Every line became one value. For a single-column list that is
        // right; for a report or a log it means the layout was not
        // recognised, and saying so is the difference between a tool that
        // failed and a tool that pretended to succeed.
        doubts.add(
            0.35,
            "no delimiter or column alignment was found: each line is being read as a \
             single value. If this is a fixed-width report or a log format, pass a \
             hint to the LLM tier or write the extraction by hand in the sidecar.",
        );
    }

    let extraction = Extraction::Delimited {
        delimiter: delim.delimiter,
        quote: Some('"'),
        escape: None,
        // An encoding guessed from ASCII-only bytes says nothing, and freezing
        // it into the spec would apply that non-guess to the parts of the file
        // we never looked at. Leaving it unset lets the executor decide from
        // the whole file.
        encoding: if sample.ascii_only { None } else { sample.encoding.clone() },
        comment: None,
        ragged: RaggedPolicy::PadNulls,
    };

    let mut table = engine::extract(&extraction, path, &ExtractOpts::capped(limits, PROBE_ROWS))
        .with_context(|| format!("probing {}", path.display()))?;
    if table.rows.is_empty() {
        bail!("no rows detected in {}", path.display());
    }

    // Leading rows whose arity differs from the modal one are title junk.
    let counts: Vec<usize> = table.rows.iter().map(|r| r.len()).collect();
    let modal_arity = modal(&counts).unwrap_or(1);
    // Leading rows of the wrong width are title junk — but in a file where
    // *every* row has a different width, "leading" is all of them, and
    // skipping them would leave nothing to promote a header from.
    let leading = counts.iter().take_while(|c| **c != modal_arity).count();
    let skip_head = leading.min(counts.len().saturating_sub(2)) as u32;
    if skip_head > 0 {
        doubts.add(0.05, format!("skipped {skip_head} leading non-tabular row(s)"));
    }
    if counts.iter().skip(skip_head as usize).any(|c| *c != modal_arity) {
        doubts.add(0.15, "rows have differing field counts; short rows are padded with nulls");
    }

    let skip_tail = footer_rows(last_line(sample), Some(delim.delimiter));
    if skip_tail > 0 {
        doubts.note("dropped a trailing summary row");
    }

    let mut transforms = Vec::new();
    if skip_head > 0 || skip_tail > 0 {
        transforms.push(Transform::SkipRows { head: skip_head, tail: skip_tail });
    }
    engine::apply_transforms(&mut table, &transforms)?;

    match header_verdict(&table.rows) {
        HeaderVerdict::Present => {
            let t = Transform::PromoteHeader { rows: 1, join: " ".into() };
            engine::apply_transforms(&mut table, std::slice::from_ref(&t))?;
            transforms.push(t);
        }
        HeaderVerdict::Absent => {
            doubts.add(0.1, "no header row detected; columns are named col_1, col_2, ...");
        }
        HeaderVerdict::AbsentButSuspicious => {
            doubts.add(
                0.3,
                "the first row is a full row of distinct labels but every column it heads \
                 holds the same kind of value it does, so it was read as data. If it is \
                 really a header, add promote_header to the sidecar.",
            );
        }
    }

    finish(extraction, transforms, table, 0.95, doubts)
}

fn sniff_lines(
    path: &Path,
    sample: &FileSample,
    p: &detect::LinePattern,
    limits: Limits,
) -> Result<SniffResult> {
    let mut doubts = Doubts::default();
    doubts.note(format!(
        "recognised as a {} log: {} of {} sampled lines are records",
        p.name,
        p.records,
        p.records + p.skipped
    ));
    if p.skipped > 0 {
        // Dropping lines is the right behaviour for banners and stack traces,
        // but it is still dropping data, so it is said out loud.
        doubts.add(
            0.05,
            format!(
                "{} sampled line(s) do not match the pattern and are skipped \
                 (continuation lines, banners); set on_no_match = \"error\" to fail instead",
                p.skipped
            ),
        );
    }
    let extraction = Extraction::Lines {
        pattern: p.pattern.clone(),
        encoding: if sample.ascii_only { None } else { sample.encoding.clone() },
        on_no_match: NoMatchPolicy::Skip,
    };
    let table = engine::extract(&extraction, path, &ExtractOpts::capped(limits, PROBE_ROWS))
        .with_context(|| format!("probing {}", path.display()))?;
    if table.rows.is_empty() {
        bail!("the log pattern matched no lines");
    }
    finish(extraction, vec![], table, 0.9, doubts)
}

fn sniff_fixed_width(
    path: &Path,
    sample: &FileSample,
    fields: Vec<crate::spec::FixedField>,
    limits: Limits,
) -> Result<SniffResult> {
    let mut doubts = Doubts::default();
    doubts.add(
        0.15,
        format!(
            "read as a fixed-width report with {} columns; check the field boundaries",
            fields.len()
        ),
    );
    let named = fields.iter().any(|f| !f.name.starts_with("col_"));
    let extraction = Extraction::FixedWidth {
        encoding: if sample.ascii_only { None } else { sample.encoding.clone() },
        fields,
    };
    let mut table = engine::extract(&extraction, path, &ExtractOpts::capped(limits, PROBE_ROWS))
        .with_context(|| format!("probing {}", path.display()))?;
    if table.rows.is_empty() {
        bail!("no rows detected");
    }
    // The names came from the first line, which is therefore a header row and
    // must not also be data.
    let mut transforms = Vec::new();
    if named {
        let t = Transform::SkipRows { head: 1, tail: 0 };
        engine::apply_transforms(&mut table, std::slice::from_ref(&t))?;
        transforms.push(t);
    }
    finish(extraction, transforms, table, 0.8, doubts)
}

// ---------------------------------------------------------------------------
// Excel
// ---------------------------------------------------------------------------

fn sniff_excel(path: &Path, sample: &FileSample, limits: Limits) -> Result<SniffResult> {
    let mut doubts = Doubts::default();
    let sheet = pick_sheet(path, sample, limits);
    if sample.sheets.len() > 1 {
        doubts.note(format!(
            "workbook has {} sheets; reading {:?}",
            sample.sheets.len(),
            sheet.as_deref().unwrap_or("the first")
        ));
    }
    sniff_excel_sheet(path, sheet, limits, doubts)
}

/// Frame one sheet of a workbook: title rows, footer, header promotion.
///
/// Split out of [`sniff_excel`] so `fit` can frame *every* sheet when the
/// workbook has several — each sheet gets its own framing, because "the data
/// starts on row 4 under a merged band" is a fact about a sheet, not about
/// the file.
fn sniff_excel_sheet(
    path: &Path,
    sheet: Option<String>,
    limits: Limits,
    mut doubts: Doubts,
) -> Result<SniffResult> {
    let extraction = Extraction::Excel {
        sheet_name: sheet,
        sheet_index: None,
        range: None,
    };

    // No row cap here, unlike the text formats: calamine materialises the
    // whole sheet to answer any question about it, so capping saves nothing
    // and would hide the last row — which is exactly where a "Total" line
    // lives. `limits.max_cells` is the real guard.
    let mut table = engine::extract(&extraction, path, &ExtractOpts::full(limits))
        .with_context(|| format!("probing {}", path.display()))?;
    if table.rows.is_empty() {
        bail!("sheet appears to be empty");
    }
    let width = table.width();
    let non_empty = |r: &Vec<String>| r.iter().filter(|c| !c.trim().is_empty()).count();

    // The header is the first substantially populated row that is followed by
    // another populated row.
    let mut header_idx = 0usize;
    for (i, r) in table.rows.iter().enumerate() {
        let ok = non_empty(r) as f32 >= 0.6 * width as f32;
        let next_ok = table
            .rows
            .get(i + 1)
            .map(|n| non_empty(n) as f32 >= 0.5 * width as f32)
            .unwrap_or(false);
        if ok && next_ok {
            header_idx = i;
            break;
        }
    }
    if header_idx > 0 {
        doubts.add(0.1, format!("skipped {header_idx} leading row(s) before the header"));
    }

    // Footer detection reads the *last row of the sheet*. Reading the last row
    // of a truncated probe window instead is how "Total" rows used to survive
    // into the data of any sheet longer than the window — and a total row in
    // the data doubles every sum computed from it.
    let last_row_cells: Option<Vec<String>> = table.rows.last().cloned();
    let skip_tail = if table.truncated {
        doubts.add(
            0.05,
            format!(
                "sheet is longer than the {PROBE_ROWS}-row probe; a trailing total row \
                 would not have been noticed"
            ),
        );
        0
    } else {
        // Test the cells, not a joined line: a summary row often carries its
        // label in the second column ("2025" | "Total" | 5051.25).
        last_row_cells.as_deref().map(footer_row_cells).unwrap_or(0)
    };
    if skip_tail > 0 {
        doubts.note("dropped a trailing summary row");
    }

    let header_row_has_gaps = table
        .rows
        .get(header_idx)
        .map(|r| r.iter().any(|c| c.trim().is_empty()))
        .unwrap_or(false);

    let mut transforms = Vec::new();
    if header_idx > 0 || skip_tail > 0 {
        let t = Transform::SkipRows { head: header_idx as u32, tail: skip_tail };
        engine::apply_transforms(&mut table, std::slice::from_ref(&t))?;
        transforms.push(t);
    }

    // Only promote a header if the first row actually reads like one; a sheet
    // that starts straight into data would otherwise lose its first record.
    match header_verdict(&table.rows) {
        HeaderVerdict::Present => {
            let t = Transform::PromoteHeader { rows: 1, join: " ".into() };
            engine::apply_transforms(&mut table, std::slice::from_ref(&t))?;
            transforms.push(t);
            if header_row_has_gaps {
                doubts.add(
                    0.2,
                    "the header row has blank cells: this may be a multi-row or merged \
                     header, which needs promote_header with rows > 1",
                );
            }
        }
        HeaderVerdict::Absent => {
            doubts.add(0.15, "no header row detected; columns are named col_1, col_2, ...");
        }
        HeaderVerdict::AbsentButSuspicious => {
            doubts.add(
                0.3,
                "the first row is a full row of distinct labels but every column it heads \
                 holds the same kind of value it does, so it was read as data. If it is \
                 really a header, add promote_header to the sidecar.",
            );
        }
    }

    finish(extraction, transforms, table, 0.9, doubts)
}

/// Frame one named sheet, for `fit`'s sheet elimination. The doubts a
/// sniff would record are irrelevant there — the declared table either
/// binds or it does not — so they start empty.
pub(crate) fn frame_excel_sheet(
    path: &Path,
    sheet: &str,
    limits: Limits,
) -> Result<crate::spec::ParseSpec> {
    sniff_excel_sheet(path, Some(sheet.to_string()), limits, Doubts::default())
        .map(|r| r.spec)
}

/// Prefer the first sheet that actually holds a table over a cover sheet.
///
/// One workbook open for all sheets: calamine re-parses the whole archive on
/// every open, so asking sheet by sheet costs a full parse per sheet.
fn pick_sheet(path: &Path, sample: &FileSample, limits: Limits) -> Option<String> {
    let shapes = engine::excel_sheet_shapes(path, limits).ok()?;
    // A cover page is prose and a legend is a two-column glossary; the data
    // sheet is the one with quantities in it. Rank by "has numbers at all"
    // first and by size second, and break ties toward the earlier sheet —
    // workbooks put their data before their appendices.
    let best = shapes
        .iter()
        .enumerate()
        .filter(|(_, s)| s.rows >= 3 && s.cols >= 2)
        .max_by_key(|(i, s)| (s.numeric_cells > 0, s.rows, usize::MAX - i));
    best.map(|(_, s)| s.name.clone())
        .or_else(|| shapes.first().map(|s| s.name.clone()))
        .or_else(|| sample.sheets.first().cloned())
}

// ---------------------------------------------------------------------------
// JSON
// ---------------------------------------------------------------------------

fn sniff_json(path: &Path, limits: Limits) -> Result<SniffResult> {
    let mut doubts = Doubts::default();
    let bytes = crate::fileio::read_all(path, limits.max_file_bytes)?;
    let (text, _) = crate::sample::decode_text(&bytes, None);
    drop(bytes);
    let trimmed = text.trim_start();

    let (lines, pointer) = match serde_json::from_str::<serde_json::Value>(&text) {
        Ok(doc) => match &doc {
            serde_json::Value::Array(_) => (false, None),
            serde_json::Value::Object(_) => {
                // The records array may be nested — `{"data": {"items": [...]}}`
                // is as common in API dumps as a top-level one.
                let mut found = Vec::new();
                find_record_arrays(&doc, &mut String::new(), &mut found, 0);
                if found.is_empty() {
                    bail!(
                        "this JSON document contains no array of records; set \
                         `pointer` in the sidecar to the array you mean"
                    );
                }
                // Prefer arrays of objects, then longer, then shallower.
                found.sort_by_key(|c| (!c.of_objects, usize::MAX - c.len, c.depth, c.pointer.clone()));
                let best = &found[0];
                if found.len() > 1 {
                    doubts.add(
                        0.25,
                        format!(
                            "the document has {} candidate record arrays; reading {:?}. \
                             Set `pointer` in the sidecar to choose a different one.",
                            found.len(),
                            best.pointer
                        ),
                    );
                } else if best.depth > 1 {
                    doubts.note(format!("records read from the nested array at {:?}", best.pointer));
                }
                (false, Some(best.pointer.clone()))
            }
            _ => bail!("JSON document is a scalar; nothing tabular to extract"),
        },
        Err(_) if trimmed.starts_with('{') || trimmed.starts_with('[') => {
            // Not one valid document: NDJSON.
            (true, None)
        }
        Err(e) => bail!("file has a .json-ish extension but does not parse as JSON: {e}"),
    };

    let extraction = Extraction::Json { lines, pointer };
    let table = engine::extract(&extraction, path, &ExtractOpts::capped(limits, PROBE_ROWS))
        .with_context(|| format!("probing {}", path.display()))?;
    finish(extraction, vec![], table, 0.95, doubts)
}

/// RFC 6901: `~` is `~0` and `/` is `~1`.
fn escape_pointer_token(s: &str) -> String {
    s.replace('~', "~0").replace('/', "~1")
}

pub(crate) struct ArrayCandidate {
    pub(crate) pointer: String,
    len: usize,
    of_objects: bool,
    depth: usize,
}

/// Every record-array pointer in the document, best-ranked first.
///
/// This is `fit`'s half of the JSON ambiguity story: the sniffer, with no
/// target, can only rank the candidates and say it is unsure. A *declared*
/// table changes the problem — each candidate can be tried against it, and
/// "exactly one produces the declared columns" is a proof by elimination
/// where "the longest array of objects" was a guess.
pub(crate) fn json_record_pointers(path: &Path, limits: Limits) -> Vec<String> {
    let Ok(bytes) = crate::fileio::read_all(path, limits.max_file_bytes) else {
        return Vec::new();
    };
    let (text, _) = crate::sample::decode_text(&bytes, None);
    let Ok(doc) = serde_json::from_str::<serde_json::Value>(&text) else {
        return Vec::new();
    };
    if !doc.is_object() {
        return Vec::new();
    }
    let mut found = Vec::new();
    find_record_arrays(&doc, &mut String::new(), &mut found, 0);
    found.sort_by_key(|c| (!c.of_objects, usize::MAX - c.len, c.depth, c.pointer.clone()));
    found.into_iter().map(|c| c.pointer).collect()
}

/// Walk the document for arrays that could be the records array.
fn find_record_arrays(
    v: &serde_json::Value,
    prefix: &mut String,
    out: &mut Vec<ArrayCandidate>,
    depth: usize,
) {
    // Deeply nested configuration files are not record sources; stop before
    // the search becomes a crawl of the whole document.
    if depth > 6 || out.len() > 64 {
        return;
    }
    match v {
        serde_json::Value::Array(a) if !a.is_empty() => {
            out.push(ArrayCandidate {
                pointer: prefix.clone(),
                len: a.len(),
                of_objects: a.iter().take(8).all(|e| e.is_object()),
                depth,
            });
        }
        serde_json::Value::Object(map) => {
            for (k, child) in map {
                let mark = prefix.len();
                prefix.push('/');
                prefix.push_str(&escape_pointer_token(k));
                find_record_arrays(child, prefix, out, depth + 1);
                prefix.truncate(mark);
            }
        }
        _ => {}
    }
}

// ---------------------------------------------------------------------------
// Shared machinery
// ---------------------------------------------------------------------------

/// Turn a fully transformed probe table into a spec.
///
/// This is the single place column specs are created, and it takes its names
/// from `table.header` — the exact header the executor will see, after every
/// transform in `transforms` has run. That is what makes the sniffer's output
/// executable by construction.
fn finish(
    extraction: Extraction,
    transforms: Vec<Transform>,
    mut table: RawTable,
    base_confidence: f32,
    mut doubts: Doubts,
) -> Result<SniffResult> {
    table.ensure_header()?;
    let header = table.header.clone().unwrap_or_default();
    if header.is_empty() {
        bail!("no columns detected");
    }
    let body: Vec<&Vec<String>> = table.rows.iter().take(TYPE_SAMPLE).collect();
    let columns = guess_columns(&header, &body, &mut doubts);

    // A table that types as almost entirely text usually means the layout was
    // misread, not that the data is textual.
    if columns.len() >= 3 {
        let utf8 = columns.iter().filter(|c| c.dtype == DType::Utf8).count() as f32;
        if utf8 / columns.len() as f32 > 0.8 && !body.is_empty() {
            doubts.add(0.15, "nearly every column typed as text; the layout may be misread");
        }
    }

    let (confidence, notes) = doubts.finish(base_confidence);
    let spec = ParseSpec {
        extraction,
        transforms,
        columns,
        confidence: Some(confidence),
        notes,
    };
    Ok(SniffResult { spec, confidence })
}

fn csv_field_counts(text: &str, delimiter: char) -> Vec<usize> {
    if !delimiter.is_ascii() {
        return Vec::new();
    }
    let mut rdr = csv::ReaderBuilder::new()
        .has_headers(false)
        .flexible(true)
        .delimiter(delimiter as u8)
        .from_reader(text.as_bytes());
    rdr.records().filter_map(|r| r.ok()).map(|r| r.len()).collect()
}

fn modal(counts: &[usize]) -> Option<usize> {
    let mut m: std::collections::BTreeMap<usize, usize> = std::collections::BTreeMap::new();
    for c in counts {
        *m.entry(*c).or_insert(0) += 1;
    }
    m.into_iter().max_by_key(|(w, n)| (*n, *w)).map(|(c, _)| c)
}

/// Words that name a summary line in the languages this tool grew up around.
///
/// A *field* must be exactly one of these (optionally with a trailing colon):
/// "Total" is a summary label, but "Total Quality AG" is a customer, and
/// dropping that company's last invoice because its name starts with "Total"
/// is silent data loss.
const FOOTER_FIELD: &str = r"(?i)^\s*(total|totals|summe|gesamt|gesamtsumme|zwischensumme|subtotal|sub-total|sum|endsumme|insgesamt|grand total)\s*:?\s*$";
/// For files with no delimiter, the whole line is tested as a prefix instead.
const FOOTER_LINE: &str = r"(?i)^\s*(total|totals|summe|gesamt|gesamtsumme|zwischensumme|subtotal|sum|endsumme|insgesamt)\b";

/// Does the file's last line look like a summary row? Returns the number of
/// trailing rows to skip (0 or 1).
/// Does this row (as separate cells) look like a summary row?
fn footer_row_cells(cells: &[String]) -> u32 {
    let re = regex::Regex::new(FOOTER_FIELD).expect("static regex");
    u32::from(cells.iter().any(|c| re.is_match(c.trim().trim_matches('"'))))
}

fn footer_rows(last_line: Option<&str>, delimiter: Option<char>) -> u32 {
    let Some(line) = last_line else { return 0 };
    let field_re = regex::Regex::new(FOOTER_FIELD).expect("static regex");
    // The label is not always in the first column: `2025-12-31,Total,,14337.00`
    // is a perfectly ordinary summary row, so every field is tested — but only
    // for an exact match.
    let matches_any_field = match delimiter {
        Some(d) => line.split(d).any(|f| field_re.is_match(f.trim().trim_matches('"'))),
        None => false,
    };
    let line_re = regex::Regex::new(FOOTER_LINE).expect("static regex");
    u32::from(matches_any_field || (delimiter.is_none() && line_re.is_match(line)))
}

enum HeaderVerdict {
    Present,
    /// No header, and the first row does not look like one either.
    Absent,
    /// No header found, but the first row is a full row of distinct labels.
    /// Reading it as data may be right (a headerless export) or may be a
    /// missed header — and a missed header silently sums a year column as if
    /// it were money. Neither answer is safe enough to ship quietly.
    AbsentButSuspicious,
}

/// Would the first row make a plausible header for the rows below it?
///
/// The test is comparative rather than absolute: a header is a row that is
/// *unlike* the data under it. Asking instead whether any header cell looks
/// numeric misjudges the very common case of a column literally named
/// `2025`, and pushes the real header row into the data.
fn header_verdict(rows: &[Vec<String>]) -> HeaderVerdict {
    let Some(first) = rows.first() else {
        return HeaderVerdict::Absent;
    };
    let width = first.len();
    if width == 0 {
        return HeaderVerdict::Absent;
    }
    let named = first.iter().filter(|c| !c.trim().is_empty()).count();
    if named * 2 < width {
        return HeaderVerdict::Absent; // mostly blank: not a header
    }

    if rows.len() == 1 {
        // A file with a header and no data rows still has a header — but a
        // single-column, single-row file has exactly one datum. Promoting it
        // returns an EMPTY table from a file that plainly has content, which
        // is worse than any naming (2026-09-03 corpus audit:
        // workflows-version.txt, one line, "1.0.1").
        let unique = {
            let mut s = std::collections::HashSet::new();
            first.iter().all(|c| s.insert(c.trim().to_ascii_lowercase()))
        };
        let texty = first.iter().all(|c| c.trim().is_empty() || !looks_scalar(c));
        return if unique && texty && named == width && width > 1 {
            HeaderVerdict::Present
        } else {
            HeaderVerdict::Absent
        };
    }

    let mut evidence = 0usize;
    let mut against = 0usize;
    for c in 0..width {
        let body: Vec<&str> = rows
            .iter()
            .skip(1)
            .take(50)
            .filter_map(|r| r.get(c))
            .map(|s| s.as_str())
            .filter(|v| !is_na(v))
            .collect();
        if body.is_empty() {
            continue;
        }
        let body_scalar = body.iter().all(|v| looks_scalar(v));
        if !body_scalar {
            continue;
        }
        let head_cell = first.get(c).map(|s| s.as_str()).unwrap_or("");
        if looks_scalar(head_cell) {
            // A wide report names its value columns after periods — `2024`,
            // `Q1`, `Jan`. Those are scalars, so counting them against a
            // header loses the header of every pivot table there is.
            if looks_like_period_label(head_cell) && !body.iter().any(|v| looks_like_period_label(v))
            {
                evidence += 1;
            } else {
                against += 1;
            }
        } else if !head_cell.trim().is_empty() {
            evidence += 1;
        }
    }
    if evidence > 0 && evidence >= against {
        return HeaderVerdict::Present;
    }
    let unique = {
        let mut s = std::collections::HashSet::new();
        first.iter().all(|c| s.insert(c.trim().to_ascii_lowercase()))
    };
    if evidence == 0 && against == 0 {
        // An all-text table: a header is the row whose cells are all distinct
        // labels. With ONE column that test is vacuous — a single cell is
        // trivially "all distinct" — and promoting on it consumed a record
        // from a list of file paths (2026-09-03 corpus audit). A one-column
        // all-text list offers no way to tell a header from its data.
        if unique && named == width && width > 1 {
            return HeaderVerdict::Present;
        }
    }
    // Only suspicious if the row *reads* like labels: at least one cell that
    // is not a number. A table of bare numbers with no header is an ordinary
    // shape and must not be second-guessed.
    let has_a_label = first.iter().any(|c| !c.trim().is_empty() && !looks_scalar(c));
    if unique && named == width && against > 0 && has_a_label {
        return HeaderVerdict::AbsentButSuspicious;
    }
    HeaderVerdict::Absent
}

/// Does this cell name a period rather than measure one? Year, quarter,
/// half, month name, or `2025-01` — the labels a wide report puts above its
/// value columns.
fn looks_like_period_label(v: &str) -> bool {
    let t = v.trim();
    if t.is_empty() {
        return false;
    }
    if let Ok(y) = t.parse::<i32>() {
        return (1900..=2100).contains(&y);
    }
    let low = t.to_ascii_lowercase();
    if regex::Regex::new(r"^(q[1-4]|[hs][12]|kw ?\d{1,2}|\d{4}[-/](0[1-9]|1[0-2]))$")
        .expect("static regex")
        .is_match(&low)
    {
        return true;
    }
    const MONTHS: &[&str] = &[
        "jan", "feb", "mar", "mär", "apr", "may", "mai", "jun", "jul", "aug", "sep", "oct",
        "okt", "nov", "dec", "dez",
    ];
    MONTHS.iter().any(|m| low.starts_with(m)) && low.len() <= 12
}

/// Numbers, dates and booleans: the things a header cell is usually not.
fn looks_scalar(v: &str) -> bool {
    let t = v.trim();
    if t.is_empty() {
        return false;
    }
    if numfmt::infer(&[t]).is_some() {
        return true;
    }
    for f in DATE_FORMATS {
        if NaiveDate::parse_from_str(t, f).is_ok() {
            return true;
        }
    }
    matches!(
        t.to_ascii_lowercase().as_str(),
        "true" | "false" | "ja" | "nein" | "yes" | "no"
    )
}

/// Prefix of the note the auto-drop writes.
///
/// `fit` carries this note into a fitted spec and the CLI prints it, so the
/// wording is a contract between three files rather than prose. A spec that
/// silently removes rows is the failure this project exists to prevent, and
/// the note is the only place it says so.
pub const DROPPED_NOTE: &str = "dropped ";

pub(crate) const NA_TOKENS: &[&str] = &[
    "", "na", "n/a", "null", "none", "nil", "-", "–", "—", "#n/a", "#na", "nan", "k.a.", "keine",
];

pub(crate) fn is_na(v: &str) -> bool {
    NA_TOKENS.contains(&v.trim().to_ascii_lowercase().as_str())
}

/// Date formats the sniffer will consider, in preference order. ISO first
/// because it is unambiguous; day-first before month-first because a
/// genuinely ambiguous `03/04/2025` is far more often the fourth of March
/// than the third of April outside the United States — and either way the
/// ambiguity is reported rather than hidden.
pub(crate) const DATE_FORMATS: &[&str] = &[
    "%Y-%m-%d",
    "%d.%m.%Y",
    "%Y/%m/%d",
    "%d/%m/%Y",
    "%m/%d/%Y",
    "%d-%m-%Y",
    "%Y%m%d",
];

pub(crate) const TS_FORMATS: &[&str] = &[
    "%Y-%m-%d %H:%M:%S",
    "%Y-%m-%dT%H:%M:%S",
    "%Y-%m-%d %H:%M:%S%.f",
    "%Y-%m-%dT%H:%M:%S%.f",
    "%d.%m.%Y %H:%M:%S",
    "%d.%m.%Y %H:%M",
    "%Y-%m-%d %H:%M",
];

struct TypeGuess {
    dtype: DType,
    parse: ValueParsing,
    penalty: f32,
    note: Option<String>,
}

fn guess_columns(
    header: &[String],
    body: &[&Vec<String>],
    doubts: &mut Doubts,
) -> Vec<ColumnSpec> {
    let mut names: Vec<String> = header.iter().map(|h| sanitize(h)).collect();
    dedupe(&mut names);
    header
        .iter()
        .enumerate()
        .map(|(i, original)| {
            let values: Vec<&str> = body
                .iter()
                .filter_map(|r| r.get(i))
                .map(|s| s.as_str())
                .collect();
            let name = names[i].clone();
            let g = guess_type(&values, &name);
            if let Some(n) = g.note {
                doubts.add(g.penalty, format!("column `{name}`: {n}"));
            } else if g.penalty > 0.0 {
                doubts.penalty += g.penalty;
            }
            ColumnSpec {
                // `source` is the post-transform header name, verbatim: this
                // is the guarantee that it resolves.
                source: if &name == original { None } else { Some(original.clone()) },
                name,
                dtype: g.dtype,
                nullable: true,
                parse: g.parse,
            }
        })
        .collect()
}

/// Words that mean "this column is money" in the languages these files come
/// in. Crude, but it is the strongest signal available and it is right there
/// in the header — and being wrong costs an exact type where a float would
/// have done, not a wrong number.
fn looks_monetary(name: &str) -> bool {
    const WORDS: &[&str] = &[
        "umsatz", "betrag", "preis", "kosten", "saldo", "wert", "summe", "brutto", "netto",
        "mwst", "steuer", "gebuehr", "honorar", "lohn", "gehalt", "rechnung", "zahlung",
        "guthaben", "erloes", "aufwand", "ertrag", "amount", "price", "cost", "total",
        "revenue", "sales", "balance", "fee", "salary", "payment", "invoice", "charge",
        "credit", "debit", "tax", "subtotal", "montant", "prix", "solde", "chf", "eur",
        "usd", "gbp", "cash", "budget",
    ];
    let n = name.to_ascii_lowercase();
    WORDS.iter().any(|w| n.contains(w))
}

fn guess_type(values: &[&str], name: &str) -> TypeGuess {
    let sample: Vec<&str> = values
        .iter()
        .map(|v| v.trim())
        .filter(|v| !is_na(v))
        .take(TYPE_SAMPLE)
        .collect();

    let na_seen: Vec<String> = {
        let mut seen = std::collections::BTreeSet::new();
        for v in values.iter().take(TYPE_SAMPLE * 2) {
            let t = v.trim();
            if !t.is_empty() && is_na(t) {
                seen.insert(t.to_string());
            }
        }
        seen.into_iter().collect()
    };

    let text = |note: Option<String>, penalty: f32| TypeGuess {
        dtype: DType::Utf8,
        // Deliberately no auto na_values on a text column: "NA" is Namibia,
        // "-" is a valid label, and nulling a real string is data loss that
        // no later step can undo. Typed columns below do get them, because
        // there the token cannot be a value.
        parse: ValueParsing::default(),
        penalty,
        note,
    };

    if sample.is_empty() {
        return text(None, 0.0);
    }
    let with_na = |mut p: ValueParsing| {
        // The *whole* vocabulary, not only the tokens this sample happened to
        // contain. A column typed as a number, date or boolean cannot have
        // "NA" as a value — that is the argument the text branch above turns
        // down, and it does not depend on where in the file the token sits.
        //
        // Declaring only what was seen made the same file behave two ways: a
        // real 119k-row export (datascience-box `hotels.csv`) types `children`
        // as an integer from its first 500 rows and then dies at row 40,601 on
        // an `NA`, while the identical file with that `NA` near the top reads
        // fine. Inconsistency is worse than either answer, and this is the
        // answer the code already gave whenever it happened to look.
        p.na_values = NA_TOKENS.iter().map(|t| t.to_string()).collect();
        p.na_values.retain(|t| !t.is_empty());
        // ...except where the column's own vocabulary claims the token. Only
        // booleans have one, and `spec::validate` refuses a spec where the two
        // overlap, because the executor resolves the tie as "missing" and a
        // declared FALSE would vanish.
        p.na_values.retain(|t| {
            !p.true_values.iter().chain(p.false_values.iter()).any(|b| b.eq_ignore_ascii_case(t))
        });
        p
    };
    // Kept for the note below: what this file actually contained.
    let _ = &na_seen;

    // Integers, but only where nothing is lost by calling them integers.
    if numfmt::all_integral(&sample) {
        if sample.iter().any(|v| numfmt::has_significant_leading_zero(v)) {
            return text(
                Some("kept as text: the leading zeros are part of the value".into()),
                0.0,
            );
        }
        if !sample.iter().all(|v| numfmt::fits_i64(v)) {
            return text(
                Some("kept as text: values do not fit in a 64-bit integer".into()),
                0.0,
            );
        }
        return TypeGuess {
            dtype: DType::Int64,
            parse: with_na(ValueParsing::default()),
            penalty: 0.0,
            note: None,
        };
    }

    // Numbers with separators: the convention has to be *proved*, not tried.
    if let Some(fmt) = numfmt::infer(&sample) {
        let mut parse = with_na(ValueParsing {
            thousands_separator: fmt.thousands,
            decimal_separator: fmt.decimal,
            ..Default::default()
        });
        let scales: Vec<usize> = sample
            .iter()
            .map(|v| numfmt::frac_digits_with(v, fmt.decimal, fmt.thousands))
            .collect();
        let max_scale = scales.iter().copied().max().unwrap_or(0);
        let consistent = scales.iter().all(|s| *s == max_scale);
        // Money goes to an exact decimal; everything else stays a float.
        // Three signals, any of which is enough: it is grouped in thousands,
        // every value carries the same (small) number of decimals — how
        // formatted money is written — or the column is called something like
        // `betrag`, which catches the case where trailing zeros were dropped.
        let money_like = fmt.thousands.is_some()
            || (consistent && (1..=4).contains(&max_scale))
            || (looks_monetary(name) && (1..=4).contains(&max_scale));
        let note = if fmt.ambiguous {
            Some(format!(
                "{:?} could be a thousands separator or a decimal point here; read as a \
                 {} separator. Check this.",
                fmt.thousands.or(fmt.decimal).unwrap_or('.'),
                if fmt.thousands.is_some() { "thousands" } else { "decimal" }
            ))
        } else {
            None
        };
        let penalty = if fmt.ambiguous { 0.25 } else { 0.0 };

        if money_like && max_scale <= 6 {
            // Exact decimals: money must not go through binary floating point.
            // Precision 38 is the Decimal128 maximum, so there is no realistic
            // way for a later row to overflow it.
            // The scale comes from a sample, and a later row with more
            // fractional digits is rounded. That is a change to the data, so
            // it is always stated — not only when the sample disagreed with
            // itself.
            let n = Some(format!(
                "{}read as decimal({}) — scale inferred from the first {} rows; any \
                 later value with more fractional digits is rounded half away from zero",
                note.map(|s| format!("{s} ")).unwrap_or_default(),
                max_scale,
                TYPE_SAMPLE
            ));
            return TypeGuess {
                dtype: DType::Decimal { precision: 38, scale: max_scale as i8 },
                parse,
                penalty,
                note: n,
            };
        }
        if fmt.thousands.is_none() && fmt.decimal.is_none() {
            parse.thousands_separator = None;
            parse.decimal_separator = None;
        }
        return TypeGuess { dtype: DType::Float64, parse, penalty, note };
    }

    // Scientific notation: `1.5e10` is a number, and demoting the column to
    // text because of the exponent helps nobody.
    if sample.iter().any(|v| v.contains(['e', 'E']))
        && sample.iter().all(|v| v.trim().parse::<f64>().map(|f| f.is_finite()).unwrap_or(false))
    {
        return TypeGuess {
            dtype: DType::Float64,
            parse: with_na(ValueParsing::default()),
            penalty: 0.0,
            note: None,
        };
    }

    // Dates. Every format that fits the whole column is a candidate; more than
    // one means the column is genuinely ambiguous.
    let date_hits: Vec<&str> = DATE_FORMATS
        .iter()
        .copied()
        .filter(|f| {
            sample
                .iter()
                .all(|v| NaiveDate::parse_from_str(v, f).is_ok() && four_digit_year_ok(v, f))
        })
        .collect();
    if let Some(best) = date_hits.first() {
        let ambiguous = date_hits.len() > 1;
        return TypeGuess {
            dtype: DType::Date { format: (*best).into() },
            parse: with_na(ValueParsing::default()),
            penalty: if ambiguous { 0.25 } else { 0.0 },
            note: if ambiguous {
                Some(format!(
                    "date format is ambiguous ({} both fit); read as {best}",
                    date_hits.join(" and ")
                ))
            } else {
                None
            },
        };
    }

    for fmt in TS_FORMATS {
        if sample.iter().all(|v| NaiveDateTime::parse_from_str(v, fmt).is_ok()) {
            return TypeGuess {
                dtype: DType::Timestamp { format: (*fmt).into(), timezone: None },
                parse: with_na(ValueParsing::default()),
                penalty: 0.0,
                note: None,
            };
        }
    }

    // Booleans (only for non-numeric tokens; 0/1 stayed Int64 above).
    let truthy = ["true", "yes", "y", "ja", "wahr"];
    let falsy = ["false", "no", "n", "nein", "falsch"];
    if sample.iter().all(|v| {
        let l = v.to_ascii_lowercase();
        truthy.contains(&l.as_str()) || falsy.contains(&l.as_str())
    }) {
        return TypeGuess {
            dtype: DType::Bool,
            parse: with_na(ValueParsing::default()),
            penalty: 0.0,
            note: None,
        };
    }

    let note = if na_seen.is_empty() {
        None
    } else {
        Some(format!(
            "text column containing {:?}; add them to na_values by hand if they mean \
             \"missing\" rather than a literal value",
            na_seen
        ))
    };
    text(note, 0.0)
}

/// chrono's `%Y` accepts one to four digits, so `01/02/25` "parses" as the
/// year 25. A column of two-digit years must not be typed as a four-digit
/// one.
fn four_digit_year_ok(v: &str, format: &str) -> bool {
    if !format.contains("%Y") {
        return true;
    }
    v.chars()
        .collect::<Vec<_>>()
        .split(|c: &char| !c.is_ascii_digit())
        .any(|run| run.len() == 4)
}

pub(crate) fn sanitize(name: &str) -> String {
    let mut out = String::new();
    let mut prev_us = false;
    for ch in name.trim().chars() {
        let mapped: Option<String> = match ch {
            'ä' | 'Ä' => Some("ae".into()),
            'ö' | 'Ö' => Some("oe".into()),
            'ü' | 'Ü' => Some("ue".into()),
            'ß' => Some("ss".into()),
            'é' | 'è' | 'ê' | 'É' | 'È' | 'Ê' => Some("e".into()),
            'à' | 'á' | 'â' | 'À' | 'Á' | 'Â' => Some("a".into()),
            'ç' | 'Ç' => Some("c".into()),
            'ñ' | 'Ñ' => Some("n".into()),
            'ø' | 'Ø' => Some("o".into()),
            'å' | 'Å' => Some("a".into()),
            '²' => Some("2".into()),
            '³' => Some("3".into()),
            c if c.is_ascii_alphanumeric() => Some(c.to_ascii_lowercase().to_string()),
            _ => None,
        };
        match mapped {
            Some(s) => {
                out.push_str(&s);
                prev_us = false;
            }
            None => {
                if !prev_us && !out.is_empty() {
                    out.push('_');
                    prev_us = true;
                }
            }
        }
    }
    let out = out.trim_end_matches('_').to_string();
    if out.is_empty() {
        "col".to_string()
    } else if out.starts_with(|c: char| c.is_ascii_digit()) {
        format!("c_{out}")
    } else {
        out
    }
}

fn dedupe(names: &mut [String]) {
    let mut taken: std::collections::HashSet<String> = std::collections::HashSet::new();
    for n in names.iter_mut() {
        if taken.insert(n.clone()) {
            continue;
        }
        let mut i = 2usize;
        loop {
            let cand = format!("{n}_{i}");
            if !taken.contains(&cand) {
                taken.insert(cand.clone());
                *n = cand;
                break;
            }
            i += 1;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rows(v: &[&[&str]]) -> Vec<Vec<String>> {
        v.iter()
            .map(|r| r.iter().map(|s| s.to_string()).collect())
            .collect()
    }

    #[test]
    fn a_column_literally_named_2025_does_not_defeat_header_detection() {
        let r = rows(&[
            &["Datum", "Umsatz", "2025"],
            &["2025-01-01", "10.5", "42"],
            &["2025-01-02", "11.5", "17"],
        ]);
        assert!(matches!(header_verdict(&r), HeaderVerdict::Present));
    }

    #[test]
    fn an_all_numeric_table_has_no_header() {
        let r = rows(&[&["1", "2"], &["3", "4"], &["5", "6"]]);
        assert!(matches!(header_verdict(&r), HeaderVerdict::Absent));
    }

    #[test]
    fn an_undecidable_first_row_is_flagged_rather_than_assumed() {
        // Customer numbers as column headers: not period labels, so nothing
        // proves this is a header — but reading it as data would sum 8001 as
        // if it were a measurement, so it must not pass quietly.
        let r = rows(&[
            &["kunde", "8001", "8002"],
            &["Ost", "10", "20"],
            &["West", "11", "21"],
        ]);
        assert!(matches!(header_verdict(&r), HeaderVerdict::AbsentButSuspicious));
    }

    #[test]
    fn a_header_only_file_still_has_a_header() {
        let r = rows(&[&["a", "b"]]);
        assert!(matches!(header_verdict(&r), HeaderVerdict::Present));
    }

    #[test]
    fn a_blank_cell_does_not_veto_a_header() {
        let r = rows(&[
            &["id", "", "value"],
            &["1", "x", "2"],
            &["3", "y", "4"],
        ]);
        assert!(matches!(header_verdict(&r), HeaderVerdict::Present));
    }

    #[test]
    fn decimal_comma_column_is_typed_as_a_decimal_comma_column() {
        let g = guess_type(&["1,5", "2,75", "10,25"], "v");
        assert_eq!(g.parse.decimal_separator, Some(','));
        assert_eq!(g.parse.thousands_separator, None);
    }

    #[test]
    fn money_becomes_an_exact_decimal_not_a_float() {
        let g = guess_type(&["1'234.50", "12'000.00"], "v");
        assert!(matches!(g.dtype, DType::Decimal { scale: 2, .. }));
        assert_eq!(g.parse.thousands_separator, Some('\''));
    }

    #[test]
    fn plain_measurements_stay_floats() {
        let g = guess_type(&["1.23456", "2.5", "0.000001"], "v");
        assert_eq!(g.dtype, DType::Float64);
        // Uneven decimals with a non-monetary name: a score, not a price.
        let g = guess_type(&["3.5", "1.25", "2.0"], "score");
        assert_eq!(g.dtype, DType::Float64);
    }

    #[test]
    fn a_money_column_that_dropped_its_trailing_zeros_is_still_money() {
        // Same values, but the header says what they are.
        let g = guess_type(&["10.5", "10.50", "12"], "betrag_chf");
        assert!(matches!(g.dtype, DType::Decimal { scale: 2, .. }), "{:?}", g.dtype);
        let g = guess_type(&["10.5", "10.50", "12"], "score");
        assert_eq!(g.dtype, DType::Float64);
    }

    #[test]
    fn a_pivot_tables_year_columns_do_not_defeat_header_detection() {
        let r = rows(&[
            &["Region", "2023", "2024"],
            &["Ost", "1200.50", "1300.00"],
            &["West", "990.25", "1500.75"],
        ]);
        assert!(matches!(header_verdict(&r), HeaderVerdict::Present));
    }

    #[test]
    fn period_labels_are_recognised() {
        assert!(looks_like_period_label("2024"));
        assert!(looks_like_period_label("Q1"));
        assert!(looks_like_period_label("Jan"));
        assert!(looks_like_period_label("2025-01"));
        assert!(!looks_like_period_label("1200"));
        assert!(!looks_like_period_label("Ost"));
        assert!(!looks_like_period_label("12.5"));
    }

    #[test]
    fn identifiers_keep_their_leading_zeros() {
        let g = guess_type(&["007", "0123", "0999"], "v");
        assert_eq!(g.dtype, DType::Utf8);
        let g = guess_type(&["8001", "3000"], "v");
        assert_eq!(g.dtype, DType::Int64);
    }

    #[test]
    fn oversized_integers_stay_text() {
        let g = guess_type(&["99999999999999999999", "99999999999999999998"], "v");
        assert_eq!(g.dtype, DType::Utf8);
    }

    #[test]
    fn unambiguous_dates_are_read_the_right_way_round() {
        let g = guess_type(&["13/02/2025", "01/02/2025"], "v");
        assert_eq!(g.dtype, DType::Date { format: "%d/%m/%Y".into() });
        assert_eq!(g.penalty, 0.0);
    }

    #[test]
    fn ambiguous_dates_are_flagged() {
        let g = guess_type(&["01/02/2025", "03/04/2025"], "v");
        assert!(matches!(g.dtype, DType::Date { .. }));
        assert!(g.penalty > 0.2);
        assert!(g.note.unwrap().contains("ambiguous"));
    }

    #[test]
    fn two_digit_years_are_not_dates() {
        let g = guess_type(&["01/02/25", "03/04/25"], "v");
        assert_eq!(g.dtype, DType::Utf8);
    }

    #[test]
    fn text_columns_do_not_get_automatic_na_tokens() {
        // "NA" is Namibia here, not a missing value.
        let g = guess_type(&["CH", "DE", "NA", "AT"], "v");
        assert_eq!(g.dtype, DType::Utf8);
        assert!(g.parse.na_values.is_empty());
    }

    #[test]
    fn typed_columns_do_get_na_tokens() {
        let g = guess_type(&["1", "n/a", "3"], "v");
        assert_eq!(g.dtype, DType::Int64);
        assert!(g.parse.na_values.contains(&"n/a".to_string()));
    }

    #[test]
    fn names_are_sanitised_including_uppercase_umlauts() {
        assert_eq!(sanitize("Änderung %"), "aenderung");
        assert_eq!(sanitize("Größe (m²)"), "groesse_m2");
        assert_eq!(sanitize("Umsatz (CHF)"), "umsatz_chf");
        assert_eq!(sanitize("2025"), "c_2025");
        assert_eq!(sanitize("  "), "col");
        assert_eq!(sanitize("Région"), "region");
    }

    #[test]
    fn dedupe_does_not_create_a_new_collision() {
        let mut n = vec!["a".to_string(), "a".to_string(), "a_2".to_string()];
        dedupe(&mut n);
        assert_eq!(n, vec!["a", "a_2", "a_2_2"]);
    }

    #[test]
    fn footer_detection_wants_an_exact_label() {
        assert_eq!(footer_rows(Some("Total;1;2"), Some(';')), 1);
        assert_eq!(footer_rows(Some("Zwischensumme,1"), Some(',')), 1);
        // The label is not always first.
        assert_eq!(footer_rows(Some("2025-12-31,Total,,14337.00"), Some(',')), 1);
        assert_eq!(footer_rows(Some("Summe:,1"), Some(',')), 1);
        // ...but a company whose name merely starts with one is a data row,
        // and dropping it would be silent data loss.
        assert_eq!(footer_rows(Some("2025-12-31,Total Quality AG,1"), Some(',')), 0);
        assert_eq!(footer_rows(Some("Totally Fine Ltd,1"), Some(',')), 0);
        assert_eq!(footer_rows(Some("Summe Ost GmbH,1"), Some(',')), 0);
        assert_eq!(footer_rows(Some("Ost,1"), Some(',')), 0);
        assert_eq!(footer_rows(None, Some(',')), 0);
        // No delimiter: the line is tested as a prefix.
        assert_eq!(footer_rows(Some("GESAMT   1 574 559.68"), None), 1);
    }
}
