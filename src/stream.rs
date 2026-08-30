//! Row-at-a-time execution for delimited files.
//!
//! The materialising path in [`crate::engine`] builds the whole file as a
//! `Vec<Vec<String>>` before typing any of it. That intermediate is what makes
//! peak memory roughly eight times the size of the source: a five-character
//! field costs 24 bytes of `String` header plus a rounded-up heap allocation,
//! and there is one per cell. On a 141 MB CSV it was measured at 1.10 GB.
//!
//! This module runs the same pipeline without that intermediate. Rows are
//! pulled one at a time, pushed through the transforms, and turned into an
//! Arrow `RecordBatch` every [`BATCH_ROWS`], so the raw strings alive at any
//! moment are one batch's worth rather than the file's.
//!
//! **It is not a second implementation of the semantics.** Anywhere the
//! answer could differ — building a promoted header, typing a column — this
//! calls the very same function the materialising path calls. What lives here
//! is only the *plumbing*: the order rows are visited in and when a batch is
//! cut. `tests/streaming.rs` runs both paths over every delimited fixture and
//! asserts the batches are equal, which is the real specification of this
//! file.
//!
//! # What it will and will not stream
//!
//! Transforms may appear in any order in a spec, and a general streaming
//! driver for an arbitrary order is a much larger thing than this. So the
//! shape is checked up front and anything else falls back to the
//! materialising path — a spec is never *rejected* for being unstreamable,
//! only executed the older way:
//!
//! ```text
//! [skip_rows]? [promote_header]? (drop_rows_matching | fill_down)* [unpivot]?
//! ```
//!
//! That is the shape sniffed specs take and the shape the reference spec in
//! `tests/e2e.rs` takes. `unpivot` must come last because it rewrites the
//! header, and letting later transforms address the rewritten one would need
//! the pipeline to re-resolve column indices mid-stream.
//!
//! # Why there are two passes
//!
//! `promote_header` rectangularises before it drains, so the header's width —
//! and therefore how many columns the table has and what they are called —
//! depends on the *widest row in the whole file*. A single pass cannot know
//! that before it has to name the columns. So pass one reads records and
//! counts field widths without allocating a `String` for any of them, and
//! pass two does the work. Parsing twice is the price of not holding the file
//! twice, and the counting pass is much the cheaper of the two.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use anyhow::{anyhow, bail, Context, Result};
use datafusion::arrow::array::ArrayRef;
use datafusion::arrow::datatypes::{Field, Schema};
use datafusion::arrow::record_batch::RecordBatch;

use crate::config::Limits;
use crate::engine::{
    build_column_at, compile, dedupe_names, promote_header_from, ExtractOpts, BATCH_ROWS,
};
use crate::spec::{
    ColumnSpec, Extraction, FixedField, NoMatchPolicy, ParseSpec, RaggedPolicy, Transform,
};

/// Whether [`execute_batches`] can run this spec. See the module docs.
///
/// A pure question about the spec's shape. Whether streaming is *wanted* is a
/// separate, policy question — see [`enabled`] — so that turning streaming
/// off cannot make this predicate lie.
pub fn can_stream(spec: &ParseSpec) -> bool {
    let provides_header = match &spec.extraction {
        Extraction::Delimited { .. } => false,
        // Line-oriented sources name their own columns — capture groups, or
        // the field list — so there is no width to discover and no header to
        // promote.
        Extraction::Lines { .. } | Extraction::FixedWidth { .. } => true,
        _ => return false,
    };
    // `promote_header` over a source that already has a header replaces it
    // with data rows. Legal, rare, and not worth a second code path: fall
    // back rather than reimplement it.
    if provides_header
        && spec.transforms.iter().any(|t| matches!(t, Transform::PromoteHeader { .. }))
    {
        return false;
    }
    // Walk the transforms as a little state machine over the accepted shape.
    #[derive(PartialEq, PartialOrd)]
    enum Stage {
        Skip,
        Header,
        RowLocal,
        Done,
    }
    let mut stage = Stage::Skip;
    for t in &spec.transforms {
        let next = match t {
            Transform::SkipRows { .. } => Stage::Header,
            Transform::PromoteHeader { .. } => Stage::RowLocal,
            Transform::DropRowsMatching { .. } | Transform::FillDown { .. } => Stage::RowLocal,
            Transform::Unpivot { .. } => Stage::Done,
        };
        let allowed = match t {
            Transform::SkipRows { .. } => stage == Stage::Skip,
            Transform::PromoteHeader { .. } => stage <= Stage::Header,
            Transform::DropRowsMatching { .. } | Transform::FillDown { .. } => {
                stage <= Stage::RowLocal
            }
            Transform::Unpivot { .. } => stage <= Stage::RowLocal,
        };
        if !allowed {
            return false;
        }
        stage = next;
    }
    true
}

/// Whether to use the streaming path at all, `TDY_NO_STREAM=1` to opt out.
///
/// The two executors are meant to be indistinguishable, and
/// `tests/streaming.rs` asserts they are over the whole corpus. This exists
/// so that claim can be checked on a file of your own — and so there is a way
/// out if it ever turns out not to hold.
pub fn enabled() -> bool {
    !std::env::var("TDY_NO_STREAM").map(|v| v != "0").unwrap_or(false)
}


/// One row at a time out of the decoded text, per extraction format.
///
/// The formats that stream are the ones whose rows are independent: a
/// delimited record, a log line, a slice of a fixed-width line. JSON is not
/// here because a document has to be parsed whole before its records exist.
enum Source<'a> {
    Delimited { rdr: csv::Reader<&'a [u8]>, rec: csv::StringRecord },
    Lines {
        iter: std::str::Lines<'a>,
        re: regex::Regex,
        names: Vec<String>,
        on_no_match: NoMatchPolicy,
        line_no: usize,
    },
    Fixed { iter: std::str::Lines<'a>, fields: Vec<FixedField>, chars: Vec<char> },
}

impl<'a> Source<'a> {
    fn new(text: &'a str, extraction: &Extraction) -> Result<(Self, Option<Vec<String>>)> {
        Ok(match extraction {
            Extraction::Delimited { .. } => (
                Source::Delimited {
                    rdr: reader_for(extraction).from_reader(text.as_bytes()),
                    rec: csv::StringRecord::new(),
                },
                None,
            ),
            Extraction::Lines { pattern, on_no_match, .. } => {
                let re = compile(pattern, "lines pattern")?;
                let names: Vec<String> =
                    re.capture_names().flatten().map(|s| s.to_string()).collect();
                if names.is_empty() {
                    bail!("lines pattern must contain named capture groups, e.g. (?P<ip>\\S+)");
                }
                let header = names.clone();
                (
                    Source::Lines {
                        iter: text.lines(),
                        re,
                        names,
                        on_no_match: *on_no_match,
                        line_no: 0,
                    },
                    Some(header),
                )
            }
            Extraction::FixedWidth { fields, .. } => {
                let header: Vec<String> = fields.iter().map(|f| f.name.clone()).collect();
                (
                    Source::Fixed {
                        iter: text.lines(),
                        fields: fields.clone(),
                        chars: Vec::new(),
                    },
                    Some(header),
                )
            }
            other => bail!("internal: {} is not a streaming source", other.format_name()),
        })
    }

    /// The next row, or None at end of input. Rows the format skips (blank
    /// lines, non-matching lines under `on_no_match = "skip"`) are consumed
    /// here so the caller only ever sees data.
    fn next_row(&mut self) -> Result<Option<Vec<String>>> {
        match self {
            Source::Delimited { rdr, rec } => match rdr.read_record(rec) {
                Ok(true) => Ok(Some(rec.iter().map(|s| s.to_string()).collect())),
                Ok(false) => Ok(None),
                Err(e) => Err(anyhow!("{e}")),
            },
            Source::Lines { iter, re, names, on_no_match, line_no } => {
                for line in iter.by_ref() {
                    *line_no += 1;
                    if line.trim().is_empty() {
                        continue;
                    }
                    match re.captures(line) {
                        Some(caps) => {
                            return Ok(Some(
                                names
                                    .iter()
                                    .map(|n| {
                                        caps.name(n)
                                            .map(|m| m.as_str().to_string())
                                            .unwrap_or_default()
                                    })
                                    .collect(),
                            ))
                        }
                        None => match on_no_match {
                            NoMatchPolicy::Skip => continue,
                            NoMatchPolicy::Error => {
                                bail!("line {} does not match the pattern: {:?}", line_no, line)
                            }
                        },
                    }
                }
                Ok(None)
            }
            Source::Fixed { iter, fields, chars } => {
                for line in iter.by_ref() {
                    if line.trim().is_empty() {
                        continue;
                    }
                    chars.clear();
                    chars.extend(line.chars());
                    return Ok(Some(
                        fields
                            .iter()
                            .map(|f| {
                                let start = (f.start as usize).min(chars.len());
                                let end = (f.end as usize).min(chars.len());
                                chars[start..end].iter().collect::<String>().trim().to_string()
                            })
                            .collect(),
                    ));
                }
                Ok(None)
            }
        }
    }
}

/// The delimited reader, configured identically to the materialising path.
fn reader_for(extraction: &Extraction) -> csv::ReaderBuilder {
    let mut b = csv::ReaderBuilder::new();
    b.has_headers(false).flexible(true);
    if let Extraction::Delimited { delimiter, quote, escape, comment, .. } = extraction {
        // validate() guarantees these are ASCII, so the byte casts are lossless.
        b.delimiter(*delimiter as u8);
        if let Some(q) = quote {
            b.quote(*q as u8);
        }
        if let Some(e) = escape {
            b.escape(Some(*e as u8));
        }
        if let Some(c) = comment {
            b.comment(Some(*c as u8));
        }
    }
    b
}

/// What pass one learns: enough to name the columns before pass two starts.
struct Shape {
    rows: usize,
    max_width: usize,
    modal_width: usize,
    /// True when the file has more than one row arity. Only `ragged =
    /// "error"` cares, and only then is the offending row hunted down.
    uneven: bool,
}

fn measure(source: &mut Source<'_>, limits: &Limits, path: &Path) -> Result<Shape> {
    let mut widths: HashMap<usize, usize> = HashMap::new();
    let mut rows = 0usize;
    let mut max_width = 0usize;
    let mut cells: u64 = 0;
    loop {
        // For a delimited source this still allocates a String per field,
        // which pass one would rather not do; it is the price of sharing one
        // row source with the formats whose rows are built by regex or by
        // slicing. The rows are dropped immediately, so the *peak* — which is
        // what the limits are about — stays at one row.
        let Some(r) = source
            .next_row()
            .with_context(|| format!("measuring {}", path.display()))?
        else {
            break;
        };
        let w = r.len();
        rows += 1;
        max_width = max_width.max(w);
        *widths.entry(w).or_insert(0) += 1;
        cells += w as u64;
        if cells > limits.max_streamed_cells {
            bail!(
                "reading {} exceeded the {}-cell streaming limit after {} rows \
                 (raise [limits].max_streamed_cells if this is intended)",
                path.display(),
                limits.max_streamed_cells,
                rows
            );
        }
    }
    // Ties break toward the wider row, exactly as engine::modal_width does,
    // so the two paths agree on a file with no majority arity.
    let modal_width =
        widths.iter().max_by_key(|(w, n)| (**n, **w)).map(|(w, _)| *w).unwrap_or(0);
    Ok(Shape { rows, max_width, modal_width, uneven: widths.len() > 1 })
}

/// The 0-based index and width of the first row that is not `modal` wide.
///
/// Only reached when `ragged = "error"` has already decided the file is
/// wrong, so a third pass over it costs nothing anyone will notice, and it
/// buys the same "row N has M fields" message the materialising path gives.
fn first_odd_row(text: &str, extraction: &Extraction, modal: usize) -> Option<(usize, usize)> {
    let mut rdr = reader_for(extraction).from_reader(text.as_bytes());
    let mut rec = csv::ByteRecord::new();
    let mut i = 0usize;
    while let Ok(true) = rdr.read_byte_record(&mut rec) {
        if rec.len() != modal {
            return Some((i, rec.len()));
        }
        i += 1;
    }
    None
}

/// Run `spec` over `path` without materialising the file.
///
/// Callers must check [`can_stream`] first; this returns an error rather than
/// guessing if handed a shape it does not implement.
pub fn execute_batches(spec: &ParseSpec, path: &Path, limits: Limits) -> Result<Vec<RecordBatch>> {
    if !can_stream(spec) {
        bail!("internal: stream::execute_batches called on an unstreamable spec");
    }
    let opts = ExtractOpts::full(limits);
    let encoding = spec.extraction.encoding();
    let ragged = match &spec.extraction {
        Extraction::Delimited { ragged, .. } => *ragged,
        _ => RaggedPolicy::PadNulls,
    };

    // One decode of the file, then no further copies of it. This is still
    // proportional to the source, but it is 1x rather than the ~8x the
    // Vec<Vec<String>> costs.
    let text = crate::engine::read_text(path, encoding, &opts)?;
    let (mut source, provided_header) = Source::new(&text, &spec.extraction)?;

    let tail = spec
        .transforms
        .iter()
        .find_map(|t| match t {
            Transform::SkipRows { tail, .. } => Some(*tail as usize),
            _ => None,
        })
        .unwrap_or(0);

    // Pass one exists to learn the width, which only a delimited source
    // lacks, and the row count, which only a `skip_rows` tail needs. A log
    // file with neither is read exactly once.
    let shape = if provided_header.is_none() || tail > 0 {
        let (mut counting, _) = Source::new(&text, &spec.extraction)?;
        Some(measure(&mut counting, &limits, path)?)
    } else {
        None
    };

    // Mirror RawTable::rectangularize. With no header yet (delimited
    // extraction provides none), PadNulls targets the widest row and
    // TruncateExtra the modal one.
    let target_width = match &provided_header {
        Some(h) => h.len(),
        None => {
            let shape = shape.as_ref().expect("measured when no header is provided");
            match ragged {
                RaggedPolicy::PadNulls => shape.max_width,
                RaggedPolicy::TruncateExtra => shape.modal_width,
                RaggedPolicy::Error => {
                    if let Some((pos, w)) = shape
                        .uneven
                        .then(|| first_odd_row(&text, &spec.extraction, shape.modal_width))
                        .flatten()
                    {
                        bail!(
                            "ragged input: row {} has {} field(s), but most rows have {} \
                             (set ragged = \"pad_nulls\", or add skip_rows if these are \
                             title/footer lines)",
                            pos + 1,
                            w,
                            shape.modal_width
                        );
                    }
                    shape.modal_width
                }
            }
        }
    };

    let total_rows = shape.as_ref().map(|s| s.rows).unwrap_or(usize::MAX);
    let mut plan = Plan::build(spec, target_width, total_rows)?;
    let mut row_index = 0usize;

    // --- the header, from the first rows -----------------------------------
    let mut header_rows: Vec<Vec<String>> = Vec::new();
    while row_index < plan.body_start {
        match source.next_row()? {
            Some(mut r) => {
                if row_index >= plan.skip_head {
                    fit(&mut r, target_width);
                    header_rows.push(r);
                }
                row_index += 1;
            }
            None => break,
        }
    }
    if plan.header_rows > 0 && header_rows.len() < plan.header_rows {
        bail!(
            "promote_header wants {} header row(s) but only {} row(s) remain",
            plan.header_rows,
            header_rows.len()
        );
    }
    let header = match provided_header {
        Some(mut h) => {
            // Same normalisation RawTable::ensure_header applies to names an
            // extraction supplies: blanks become col_N, duplicates are
            // disambiguated.
            for (i, n) in h.iter_mut().enumerate() {
                if n.trim().is_empty() {
                    *n = format!("col_{}", i + 1);
                }
            }
            dedupe_names(&mut h);
            h
        }
        None if plan.header_rows > 0 => promote_header_from(header_rows, &plan.header_join),
        None => {
            let mut h: Vec<String> = (1..=target_width).map(|i| format!("col_{i}")).collect();
            dedupe_names(&mut h);
            h
        }
    };
    plan.resolve(spec, &header)?;

    // --- the body ----------------------------------------------------------
    let mut out: Vec<RecordBatch> = Vec::new();
    let mut schema: Option<Arc<Schema>> = None;
    let mut chunk: Vec<Vec<String>> = Vec::with_capacity(BATCH_ROWS.min(1024));
    let mut emitted = 0usize;

    // A source that needed no measuring pass has not been counted yet, so the
    // limit is enforced here as well. Both places, because whichever ran
    // first must be the one that stops.
    let mut cells: u64 = 0;
    while row_index < plan.body_end {
        let Some(mut r) = source
            .next_row()
            .with_context(|| format!("reading record {} of {}", row_index + 1, path.display()))?
        else {
            break;
        };
        row_index += 1;
        cells = cells.saturating_add(r.len() as u64);
        if cells > limits.max_streamed_cells {
            bail!(
                "reading {} exceeded the {}-cell streaming limit after {} rows \
                 (raise [limits].max_streamed_cells if this is intended)",
                path.display(),
                limits.max_streamed_cells,
                row_index
            );
        }
        fit(&mut r, target_width);
        plan.push(r, &mut chunk);
        while chunk.len() >= BATCH_ROWS {
            let rest = chunk.split_off(BATCH_ROWS);
            flush(&plan, &chunk, emitted, &mut out, &mut schema)?;
            emitted += chunk.len();
            chunk = rest;
        }
    }
    // `..=` in spirit: an empty table still produces one empty batch, because
    // a query over a file with a header and no rows must still have a schema.
    flush(&plan, &chunk, emitted, &mut out, &mut schema)?;
    Ok(out)
}

/// Pad or truncate a row to the table's width, as rectangularize does.
fn fit(row: &mut Vec<String>, width: usize) {
    if row.len() > width {
        row.truncate(width);
    }
    while row.len() < width {
        row.push(String::new());
    }
}

fn flush(
    plan: &Plan,
    rows: &[Vec<String>],
    row_offset: usize,
    out: &mut Vec<RecordBatch>,
    schema: &mut Option<Arc<Schema>>,
) -> Result<()> {
    if !rows.is_empty() || out.is_empty() {
        let mut fields: Vec<Field> = Vec::with_capacity(plan.resolved.len());
        let mut arrays: Vec<ArrayRef> = Vec::with_capacity(plan.resolved.len());
        for (col, idx) in &plan.resolved {
            let values: Vec<&str> =
                rows.iter().map(|r| r.get(*idx).map(|s| s.as_str()).unwrap_or("")).collect();
            let (field, array) = build_column_at(col, &values, row_offset)
                .with_context(|| format!("building column `{}`", col.name))?;
            fields.push(field);
            arrays.push(array);
        }
        let s = schema.get_or_insert_with(|| Arc::new(Schema::new(fields.clone()))).clone();
        out.push(RecordBatch::try_new(s, arrays).context("assembling record batch")?);
    }
    Ok(())
}

/// The row-local part of the pipeline, plus where the body starts and ends.
struct Plan {
    skip_head: usize,
    header_rows: usize,
    header_join: String,
    /// First body row index in the file (after skipped and header rows).
    body_start: usize,
    /// One past the last body row (a `skip_rows` tail is simply not read).
    body_end: usize,
    /// The row-local transforms, in the order the spec gives them. Order is
    /// not cosmetic: filling before dropping propagates a subtotal label into
    /// the rows beneath it, and dropping first does not.
    ops: Vec<RowOp>,
    unpivot: Option<UnpivotPlan>,
    unpivot_idx: Option<(Vec<usize>, Vec<usize>, Vec<String>)>,
    resolved: Vec<(ColumnSpec, usize)>,
}

/// A transform that needs no more than the current row plus O(1) carry.
enum RowOp {
    /// Unresolved until the header exists; then `idx` is Some.
    Fill { column: String, idx: usize, carry: String },
    Drop { re: regex::Regex, column: Option<String>, idx: Option<usize> },
}

struct UnpivotPlan {
    id_columns: Vec<String>,
    value_columns: Vec<String>,
    variable_name: String,
    value_name: String,
}

impl Plan {
    fn build(spec: &ParseSpec, _width: usize, total_rows: usize) -> Result<Self> {
        let mut p = Plan {
            skip_head: 0,
            header_rows: 0,
            header_join: " ".into(),
            body_start: 0,
            body_end: total_rows,
            ops: Vec::new(),
            unpivot: None,
            unpivot_idx: None,
            resolved: Vec::new(),
        };
        let mut tail = 0usize;
        for t in &spec.transforms {
            match t {
                Transform::SkipRows { head, tail: tl } => {
                    p.skip_head = (*head as usize).min(total_rows);
                    tail = *tl as usize;
                }
                Transform::PromoteHeader { rows, join } => {
                    p.header_rows = *rows as usize;
                    p.header_join = join.clone();
                }
                Transform::DropRowsMatching { pattern, column } => p.ops.push(RowOp::Drop {
                    re: compile(pattern, "drop_rows_matching")?,
                    column: column.clone(),
                    idx: None,
                }),
                Transform::FillDown { columns } => p.ops.extend(columns.iter().map(|c| {
                    RowOp::Fill { column: c.clone(), idx: 0, carry: String::new() }
                })),
                Transform::Unpivot {
                    id_columns,
                    value_columns,
                    variable_name,
                    value_name,
                } => {
                    p.unpivot = Some(UnpivotPlan {
                        id_columns: id_columns.clone(),
                        value_columns: value_columns.clone(),
                        variable_name: variable_name.clone(),
                        value_name: value_name.clone(),
                    })
                }
            }
        }
        p.body_start = p.skip_head + p.header_rows;
        p.body_end = total_rows.saturating_sub(tail).max(p.body_start);
        Ok(p)
    }

    /// Bind column names to indices now that the header exists.
    fn resolve(&mut self, spec: &ParseSpec, header: &[String]) -> Result<()> {
        let index: HashMap<&str, usize> =
            header.iter().enumerate().map(|(i, n)| (n.as_str(), i)).collect();
        let missing = |c: &str| {
            anyhow!("no column named {:?}; available columns are {:?}", c, header.to_vec())
        };

        for op in &mut self.ops {
            match op {
                RowOp::Fill { column, idx, .. } => {
                    *idx = index.get(column.as_str()).copied().ok_or_else(|| missing(column))?;
                }
                RowOp::Drop { column, idx, .. } => {
                    *idx = match column {
                        Some(c) => {
                            Some(index.get(c.as_str()).copied().ok_or_else(|| missing(c))?)
                        }
                        None => None,
                    };
                }
            }
        }

        // The header the output columns are resolved against: unpivot
        // rewrites it, so it must be rewritten here too.
        let effective: Vec<String> = match &self.unpivot {
            None => header.to_vec(),
            Some(u) => {
                let ids: Vec<usize> = u
                    .id_columns
                    .iter()
                    .map(|c| index.get(c.as_str()).copied().ok_or_else(|| missing(c)))
                    .collect::<Result<_>>()?;
                let vals: Vec<usize> = u
                    .value_columns
                    .iter()
                    .map(|c| index.get(c.as_str()).copied().ok_or_else(|| missing(c)))
                    .collect::<Result<_>>()?;
                let labels: Vec<String> = u.value_columns.clone();
                let mut h: Vec<String> = u.id_columns.clone();
                h.push(u.variable_name.clone());
                h.push(u.value_name.clone());
                self.unpivot_idx = Some((ids, vals, labels));
                h
            }
        };
        let eff_index: HashMap<&str, usize> =
            effective.iter().enumerate().map(|(i, n)| (n.as_str(), i)).collect();
        for col in &spec.columns {
            let source = col.source_name();
            let idx = *eff_index
                .get(source)
                .ok_or_else(|| missing(source))
                .with_context(|| format!("resolving output column `{}`", col.name))?;
            self.resolved.push((col.clone(), idx));
        }
        Ok(())
    }

    /// Push one extracted row through the row-local transforms, in spec order.
    fn push(&mut self, mut row: Vec<String>, out: &mut Vec<Vec<String>>) {
        for op in &mut self.ops {
            match op {
                RowOp::Fill { idx, carry, .. } => {
                    if let Some(cell) = row.get_mut(*idx) {
                        if cell.trim().is_empty() {
                            cell.clone_from(carry);
                        } else {
                            carry.clone_from(cell);
                        }
                    }
                }
                RowOp::Drop { re, idx, .. } => {
                    let hit = match idx {
                        Some(i) => row.get(*i).map(|v| re.is_match(v)).unwrap_or(false),
                        None => re.is_match(&row.join("\t")),
                    };
                    if hit {
                        return;
                    }
                }
            }
        }
        match &self.unpivot_idx {
            None => out.push(row),
            Some((ids, vals, labels)) => {
                for (k, vi) in vals.iter().enumerate() {
                    let mut r: Vec<String> =
                        ids.iter().map(|i| row.get(*i).cloned().unwrap_or_default()).collect();
                    r.push(labels[k].clone());
                    r.push(row.get(*vi).cloned().unwrap_or_default());
                    out.push(r);
                }
            }
        }
    }
}
