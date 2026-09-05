//! The executor: ParseSpec + file -> one tidy Arrow RecordBatch.
//!
//! Pipeline: extract (format-specific, all-string, possibly ragged)
//!        -> transforms (in spec order; rectangularization happens lazily so
//!           `skip_rows` can remove title/footer rows *before* the ragged
//!           policy is enforced)
//!        -> column projection + typed casting.
//!
//! Two rules govern everything here:
//!
//! - **Never produce a wrong value.** Where the spec and the data disagree —
//!   a thousands separator that does not group in threes, a two-digit year
//!   under a four-digit format, a timezone that cannot be resolved — the
//!   answer is an error naming the row, not a plausible number.
//! - **Read only what is asked for.** [`preview`] and [`dry_run`] cap the
//!   extraction itself, so checking a spec against a 2 GB file costs
//!   kilobytes rather than gigabytes.

use std::borrow::Cow;
use std::path::Path;
use std::sync::Arc;

use anyhow::{anyhow, bail, Context, Result};
use calamine::{open_workbook_auto, Data, Range, Reader, Sheets};
use chrono::{NaiveDate, NaiveDateTime};
use datafusion::arrow::array::{
    ArrayRef, BooleanArray, Date32Array, Decimal128Array, Float64Array, Int64Array, StringArray,
    TimestampMicrosecondArray,
};
use datafusion::arrow::datatypes::{DataType as ArrowType, Field, Schema, TimeUnit};
use datafusion::arrow::record_batch::RecordBatch;
use regex::{Regex, RegexBuilder};

use crate::config::Limits;
use crate::fileio;
use crate::numfmt;
use crate::sample::render_cell;
use crate::spec::{
    parse_a1_range, parse_fixed_offset, ColumnSpec, DType, Extraction, NoMatchPolicy, ParseSpec,
    RaggedPolicy, Transform, ValueParsing,
};

/// Compiled-regex size ceiling. A pattern from a sidecar or a model is
/// untrusted input; the `regex` crate cannot backtrack, but it can be asked
/// to build an enormous automaton.
const REGEX_SIZE_LIMIT: usize = 8 * 1024 * 1024;

pub(crate) fn compile(pattern: &str, what: &str) -> Result<Regex> {
    RegexBuilder::new(pattern)
        .size_limit(REGEX_SIZE_LIMIT)
        .dfa_size_limit(REGEX_SIZE_LIMIT)
        .build()
        .with_context(|| format!("{what}: invalid or too large regex {pattern:?}"))
}

/// How much of the file to read, and the guard rails to enforce.
#[derive(Debug, Clone, Copy)]
pub struct ExtractOpts {
    pub limits: Limits,
    /// Stop after this many rows. `None` = the whole file.
    pub max_rows: Option<usize>,
}

impl ExtractOpts {
    pub fn full(limits: Limits) -> Self {
        ExtractOpts { limits, max_rows: None }
    }
    pub fn capped(limits: Limits, max_rows: usize) -> Self {
        ExtractOpts { limits, max_rows: Some(max_rows) }
    }
    fn room_left(&self, have: usize) -> bool {
        self.max_rows.map(|m| have < m).unwrap_or(true)
    }
}

// ---------------------------------------------------------------------------
// Raw table
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct RawTable {
    /// None until a header exists (promote_header, or extraction-provided
    /// names for fixed_width / lines / json).
    pub header: Option<Vec<String>>,
    pub rows: Vec<Vec<String>>,
    ragged: RaggedPolicy,
    /// The header as the *file* spelt it, before duplicate names were
    /// disambiguated.
    ///
    /// `dedupe_names` renames the second `Betrag` to `Betrag_2` so a spec can
    /// address it at all. That is right for addressing and wrong for
    /// *matching*: a planner looking for `Betrag` would find one candidate and
    /// bind it silently, when the honest answer is that the file has two
    /// columns by that name and does not say which is meant. Keeping the
    /// original spelling is what lets the collision still be seen.
    pub header_origin: Option<Vec<String>>,
    /// True when extraction stopped at `max_rows` before the end of the file.
    /// Anything that reasons about the *end* of the data (a trailing total
    /// row) must not trust a truncated table.
    pub truncated: bool,
    /// The 0-based sheet column that this table's column 0 actually came
    /// from — 0 for every format except Excel, where it is the used range's
    /// (or a declared `range`'s) own start column, straight from the same
    /// `calamine::Range` extraction builds `rows` from. A sheet whose data
    /// does not start at column A (a title in column A, say) makes table
    /// column 0 mean sheet column C or D, not A — and anything that maps a
    /// sheet-absolute column index (like `xlmoney`'s, decoded from `<c
    /// r="D10">`) onto this table's columns must subtract this first, or it
    /// binds two columns over. Reading it from the extraction's own `Range`
    /// rather than recomputing it elsewhere is what keeps the two from being
    /// able to disagree.
    pub col_offset: u32,
}

impl RawTable {
    fn new(rows: Vec<Vec<String>>, ragged: RaggedPolicy, truncated: bool) -> Self {
        RawTable { header: None, header_origin: None, rows, ragged, truncated, col_offset: 0 }
    }

    fn with_header(header: Vec<String>, rows: Vec<Vec<String>>, truncated: bool) -> Self {
        RawTable {
            header_origin: Some(header.clone()),
            header: Some(header),
            rows,
            ragged: RaggedPolicy::PadNulls,
            truncated,
            col_offset: 0,
        }
    }

    pub fn width(&self) -> usize {
        self.header
            .as_ref()
            .map(|h| h.len())
            .or_else(|| self.rows.iter().map(|r| r.len()).max())
            .unwrap_or(0)
    }

    /// Enforce the ragged policy, making every row the same width.
    fn rectangularize(&mut self) -> Result<()> {
        let target = match self.ragged {
            RaggedPolicy::Error => {
                // The reference arity is the *modal* one, not the first row's:
                // a title line at the top is exactly the case where the first
                // row is the odd one out, and blaming every real row for
                // disagreeing with it sends the reader in the wrong direction.
                let modal = modal_width(&self.rows).unwrap_or(0);
                if let Some(pos) = self.rows.iter().position(|r| r.len() != modal) {
                    bail!(
                        "ragged input: row {} has {} field(s), but most rows have {} \
                         (set ragged = \"pad_nulls\", or add skip_rows if these are \
                         title/footer lines)",
                        pos + 1,
                        self.rows[pos].len(),
                        modal
                    );
                }
                modal.max(self.header.as_ref().map(|h| h.len()).unwrap_or(0))
            }
            RaggedPolicy::PadNulls => self.width(),
            RaggedPolicy::TruncateExtra => self
                .header
                .as_ref()
                .map(|h| h.len())
                .or_else(|| modal_width(&self.rows))
                .unwrap_or(0),
        };
        for row in &mut self.rows {
            if row.len() > target {
                row.truncate(target);
            }
            while row.len() < target {
                row.push(String::new());
            }
        }
        if let Some(h) = &mut self.header {
            while h.len() < target {
                h.push(String::new());
            }
            h.truncate(target);
        }
        Ok(())
    }

    pub fn ensure_header(&mut self) -> Result<()> {
        self.rectangularize()?;
        if self.header.is_none() {
            let w = self.width();
            self.header = Some((1..=w).map(|i| format!("col_{i}")).collect());
        } else {
            // Extraction-provided names may still be blank or duplicated
            // (a hand-written sidecar, a JSON document with an "" key).
            let mut h = self.header.take().unwrap();
            for (i, n) in h.iter_mut().enumerate() {
                if n.trim().is_empty() {
                    *n = format!("col_{}", i + 1);
                }
            }
            if self.header_origin.is_none() {
                self.header_origin = Some(h.clone());
            }
            dedupe_names(&mut h);
            self.header = Some(h);
        }
        Ok(())
    }

    /// Name -> position, built once. `col_index` is a linear scan, which is
    /// fine for one lookup and quadratic for one lookup per column: a
    /// 100k-column file used to take minutes in header resolution alone.
    fn header_index(&self) -> Result<std::collections::HashMap<&str, usize>> {
        let header = self
            .header
            .as_ref()
            .ok_or_else(|| anyhow!("internal: header not established"))?;
        let mut m = std::collections::HashMap::with_capacity(header.len());
        for (i, h) in header.iter().enumerate() {
            m.entry(h.as_str()).or_insert(i);
        }
        Ok(m)
    }

    fn missing_column(&self, name: &str) -> anyhow::Error {
        let header = self.header.as_deref().unwrap_or(&[]);
        let shown: Vec<String> = header.iter().take(50).map(|h| format!("\"{h}\"")).collect();
        let more = header.len().saturating_sub(shown.len());
        anyhow!(
            "no column named `{}`; available columns: [{}{}]",
            name,
            shown.join(", "),
            if more > 0 { format!(", ... {more} more") } else { String::new() }
        )
    }

    fn col_index(&self, name: &str) -> Result<usize> {
        let header = self
            .header
            .as_ref()
            .ok_or_else(|| anyhow!("internal: header not established"))?;
        header
            .iter()
            .position(|h| h == name)
            .ok_or_else(|| self.missing_column(name))
    }

    fn check_size(&self, limits: &Limits) -> Result<()> {
        let cells = (self.rows.len() as u64).saturating_mul(self.width().max(1) as u64);
        if cells > limits.max_cells {
            bail!(
                "table has {} cells ({} rows x {} columns), above the limit of {} \
                 (raise [limits].max_cells if this is intended)",
                cells,
                self.rows.len(),
                self.width(),
                limits.max_cells
            );
        }
        Ok(())
    }
}

/// The most common row width. Ties are broken toward the *wider* row so the
/// result does not depend on hash iteration order — the same file must parse
/// the same way on every run.
pub(crate) fn modal_width(rows: &[Vec<String>]) -> Option<usize> {
    let mut counts: std::collections::BTreeMap<usize, usize> = std::collections::BTreeMap::new();
    for r in rows {
        *counts.entry(r.len()).or_insert(0) += 1;
    }
    counts
        .into_iter()
        .max_by_key(|(w, c)| (*c, *w))
        .map(|(w, _)| w)
}

// ---------------------------------------------------------------------------
// Extraction
// ---------------------------------------------------------------------------

pub fn extract(extraction: &Extraction, path: &Path, opts: &ExtractOpts) -> Result<RawTable> {
    let table = match extraction {
        Extraction::Delimited {
            delimiter,
            quote,
            escape,
            encoding,
            comment,
            ragged,
        } => extract_delimited(
            path,
            *delimiter,
            *quote,
            *escape,
            encoding.as_deref(),
            *comment,
            *ragged,
            opts,
        ),
        Extraction::Excel { sheet_name, sheet_index, range } => {
            extract_excel(path, sheet_name.as_deref(), *sheet_index, range.as_deref(), opts)
        }
        Extraction::FixedWidth { encoding, fields } => {
            extract_fixed_width(path, encoding.as_deref(), fields, opts)
        }
        Extraction::Lines { pattern, encoding, on_no_match } => {
            extract_lines(path, pattern, encoding.as_deref(), *on_no_match, opts)
        }
        Extraction::Json { lines, pointer } => extract_json(path, *lines, pointer.as_deref(), opts),
    }?;
    table.check_size(&opts.limits)?;
    Ok(table)
}

/// A decoder that had to substitute replacement characters was given the
/// wrong encoding. Say so: the alternative is a table full of `\u{fffd}`
/// that looks like the data really is that way.
fn warn_mojibake(path: &Path, declared: Option<&str>, used: &str, had_errors: bool) {
    if !had_errors {
        return;
    }
    // One file is read more than once in a run (probe, dry run, execution).
    // Repeating the same warning three times teaches people to ignore it.
    static WARNED: std::sync::OnceLock<std::sync::Mutex<std::collections::HashSet<std::path::PathBuf>>> =
        std::sync::OnceLock::new();
    let seen = WARNED.get_or_init(Default::default);
    if let Ok(mut set) = seen.lock() {
        if !set.insert(path.to_path_buf()) {
            return;
        }
    }
    match declared {
        Some(label) => eprintln!(
            "warning: {} does not decode cleanly as {label:?}; some characters were \
             replaced. Set a different `encoding` in the sidecar (or remove it to let \
             tdy detect one).",
            path.display()
        ),
        None => eprintln!(
            "warning: {} does not decode cleanly as {used}; some characters were \
             replaced. Set `encoding` in the sidecar if you know the right one.",
            path.display()
        ),
    }
}

/// Bytes read for a capped extraction. A preview or dry run is a smoke test,
/// not a parse: it must cost the same on a 2 GB file as on a 2 KB one.
const PREVIEW_BYTES: usize = 4 * 1024 * 1024;

/// Decode the text this extraction needs — the whole file for a real run,
/// a bounded prefix when the caller asked for at most N rows.
pub(crate) fn read_text(path: &Path, encoding: Option<&str>, opts: &ExtractOpts) -> Result<String> {
    if opts.max_rows.is_none() {
        let bytes = fileio::read_all(path, opts.limits.max_file_bytes)?;
        let (text, used, had_errors) = crate::sample::decode_owned(bytes, encoding);
        warn_mojibake(path, encoding, &used, had_errors);
        return Ok(text);
    }
    let ht = fileio::read_head_tail(path, PREVIEW_BYTES, 0)?;
    let truncated = ht.total > ht.head.len() as u64;
    let (mut text, used, had_errors) = crate::sample::decode_owned(ht.head, encoding);
    warn_mojibake(path, encoding, &used, had_errors);
    if truncated {
        // The prefix almost certainly ends mid-record; a torn last line would
        // look like a row with the wrong number of fields.
        if let Some(i) = text.rfind('\n') {
            text.truncate(i + 1);
        }
    }
    Ok(text)
}

#[allow(clippy::too_many_arguments)]
fn extract_delimited(
    path: &Path,
    delimiter: char,
    quote: Option<char>,
    escape: Option<char>,
    encoding: Option<&str>,
    comment: Option<char>,
    ragged: RaggedPolicy,
    opts: &ExtractOpts,
) -> Result<RawTable> {
    // validate() guarantees these are ASCII, so the byte casts are lossless.
    let text = read_text(path, encoding, opts)?;
    let mut builder = csv::ReaderBuilder::new();
    builder
        .has_headers(false)
        .flexible(true)
        .delimiter(delimiter as u8);
    if let Some(q) = quote {
        builder.quote(q as u8);
    }
    if let Some(e) = escape {
        builder.escape(Some(e as u8));
    }
    if let Some(c) = comment {
        builder.comment(Some(c as u8));
    }
    let mut rdr = builder.from_reader(text.as_bytes());
    let mut rows = Vec::new();
    let mut truncated = false;
    let mut record = csv::StringRecord::new();
    let mut cells: u64 = 0;
    loop {
        if cells > opts.limits.max_cells {
            bail!(
                "reading {} exceeded the {}-cell limit after {} rows \
                 (raise [limits].max_cells if this is intended)",
                path.display(),
                opts.limits.max_cells,
                rows.len()
            );
        }
        match rdr.read_record(&mut record) {
            Ok(true) => {
                // Read first, then check the cap: a file with exactly
                // `max_rows` rows is complete, not truncated, and marking it
                // truncated would suppress its `skip_rows` tail.
                if !opts.room_left(rows.len()) {
                    truncated = true;
                    break;
                }
                cells += record.len() as u64;
                rows.push(record.iter().map(|s| s.to_string()).collect());
            }
            Ok(false) => break,
            Err(e) => {
                return Err(anyhow!("{e}"))
                    .with_context(|| format!("CSV parse error at record {}", rows.len() + 1))
            }
        }
    }
    Ok(RawTable::new(rows, ragged, truncated))
}

/// `worksheet_range`, refusing a sheet whose *declared* extent is over the
/// cell limit.
///
/// xlsx and xlsm are the formats that will tell us before they allocate:
/// `XlsxCellReader::dimensions()` reads the `<dimension>` the file declares
/// without building the grid. The other readers do not expose it, and are
/// bounded by `xlguard::preflight` (ods, xlsb) or by the format itself
/// (xls, whose 16-bit indices cap a sheet at 65536 x 256).
pub(crate) fn checked_worksheet_range(
    wb: &mut Sheets<std::io::BufReader<std::fs::File>>,
    name: &str,
    limits: &Limits,
) -> Result<Range<Data>> {
    if let Sheets::Xlsx(x) = wb {
        if let Ok(reader) = x.worksheet_cells_reader(name) {
            let declared = reader.dimensions().len();
            if declared > limits.max_cells {
                bail!(
                    "sheet {name:?} declares {} cells, above the limit of {} \
                     (raise [limits].max_cells if this is intended)",
                    declared,
                    limits.max_cells
                );
            }
        }
    }
    wb.worksheet_range(name).with_context(|| format!("cannot read sheet {name:?}"))
}

fn extract_excel(
    path: &Path,
    sheet_name: Option<&str>,
    sheet_index: Option<u32>,
    a1_range: Option<&str>,
    opts: &ExtractOpts,
) -> Result<RawTable> {
    // Bound the container before anything reads it: for .ods, opening the
    // workbook *is* the allocation. See src/xlguard.rs.
    crate::xlguard::preflight(path, &opts.limits)?;
    let mut wb = open_workbook_auto(path)
        .with_context(|| format!("cannot open workbook {}", path.display()))?;
    let names = wb.sheet_names().to_vec();
    let name = match (sheet_name, sheet_index) {
        (Some(n), _) => {
            if !names.iter().any(|s| s == n) {
                bail!("no sheet named {:?}; available sheets: {:?}", n, names);
            }
            n.to_string()
        }
        (None, Some(i)) => names.get(i as usize).cloned().ok_or_else(|| {
            anyhow!("sheet_index {} out of range; sheets: {:?}", i, names)
        })?,
        (None, None) => names
            .first()
            .cloned()
            .ok_or_else(|| anyhow!("workbook has no sheets"))?,
    };
    let full = checked_worksheet_range(&mut wb, &name, &opts.limits)?;

    let range = match a1_range {
        Some(spec_str) => {
            // validate() has already rejected malformed and backwards ranges;
            // clamp to the used range so an over-large range does not append
            // phantom all-empty rows that later look like data.
            let ((r0, c0), (r1, c1)) = parse_a1_range(spec_str)?;
            let (h, w) = (full.height() as u32, full.width() as u32);
            if h == 0 || w == 0 {
                bail!("sheet {name:?} is empty, so range {spec_str:?} selects nothing");
            }
            let (start_row, start_col) = full.start().unwrap_or((0, 0));
            if r0 >= start_row + h || c0 >= start_col + w {
                bail!(
                    "range {spec_str:?} starts past the end of sheet {name:?} \
                     ({} rows x {} cols of data)",
                    h,
                    w
                );
            }
            let r1 = r1.min(start_row + h - 1);
            let c1 = c1.min(start_col + w - 1);
            full.range((r0, c0), (r1, c1))
        }
        None => full,
    };
    // Table column 0 is sheet column `col_offset`, not sheet column A,
    // whenever the used range (or a declared `range`) does not start at the
    // sheet's origin — a title in column A, say, pushes the real data to C.
    // Read from `range` itself (the one `.rows()` below actually iterates),
    // so anything downstream that maps a sheet-absolute column index onto
    // this table (`xlmoney`'s column tally) cannot drift from what was
    // really extracted.
    let col_offset = range.start().map(|(_, c)| c).unwrap_or(0);

    let mut rows: Vec<Vec<String>> = Vec::new();
    let mut truncated = false;
    for row in range.rows() {
        if !opts.room_left(rows.len()) {
            truncated = true;
            break;
        }
        rows.push(row.iter().map(render_cell).collect());
    }
    // Trailing all-empty rows are an artefact of the used range, not data.
    while rows.last().map(|r| r.iter().all(|c| c.trim().is_empty())).unwrap_or(false) {
        rows.pop();
    }
    Ok(RawTable { col_offset, ..RawTable::new(rows, RaggedPolicy::PadNulls, truncated) })
}

fn extract_fixed_width(
    path: &Path,
    encoding: Option<&str>,
    fields: &[crate::spec::FixedField],
    opts: &ExtractOpts,
) -> Result<RawTable> {
    let text = read_text(path, encoding, opts)?;
    let header: Vec<String> = fields.iter().map(|f| f.name.clone()).collect();
    let mut rows = Vec::new();
    let mut truncated = false;
    // Character positions, not byte positions: see the doc on
    // `Extraction::FixedWidth`.
    let mut chars: Vec<char> = Vec::new();
    for line in text.lines() {
        if line.trim().is_empty() {
            continue;
        }
        if !opts.room_left(rows.len()) {
            truncated = true;
            break;
        }
        chars.clear();
        chars.extend(line.chars());
        let row: Vec<String> = fields
            .iter()
            .map(|f| {
                let start = (f.start as usize).min(chars.len());
                let end = (f.end as usize).min(chars.len());
                chars[start..end].iter().collect::<String>().trim().to_string()
            })
            .collect();
        rows.push(row);
    }
    Ok(RawTable::with_header(header, rows, truncated))
}

fn extract_lines(
    path: &Path,
    pattern: &str,
    encoding: Option<&str>,
    on_no_match: NoMatchPolicy,
    opts: &ExtractOpts,
) -> Result<RawTable> {
    let text = read_text(path, encoding, opts)?;
    let re = compile(pattern, "lines pattern")?;
    let names: Vec<String> = re.capture_names().flatten().map(|s| s.to_string()).collect();
    if names.is_empty() {
        bail!("lines pattern must contain named capture groups, e.g. (?P<ip>\\S+)");
    }
    let mut rows = Vec::new();
    let mut truncated = false;
    for (i, line) in text.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        if !opts.room_left(rows.len()) {
            truncated = true;
            break;
        }
        match re.captures(line) {
            Some(caps) => rows.push(
                names
                    .iter()
                    .map(|n| caps.name(n).map(|m| m.as_str().to_string()).unwrap_or_default())
                    .collect(),
            ),
            None => match on_no_match {
                NoMatchPolicy::Skip => {}
                NoMatchPolicy::Error => {
                    bail!("line {} does not match the pattern: {:?}", i + 1, line)
                }
            },
        }
    }
    Ok(RawTable::with_header(names, rows, truncated))
}

fn extract_json(
    path: &Path,
    lines: bool,
    pointer: Option<&str>,
    opts: &ExtractOpts,
) -> Result<RawTable> {
    let text = read_text(path, None, opts)?;
    let mut truncated = false;
    let records: Vec<serde_json::Value> = if lines {
        let mut out = Vec::new();
        for (i, l) in text.lines().enumerate() {
            if l.trim().is_empty() {
                continue;
            }
            if !opts.room_left(out.len()) {
                truncated = true;
                break;
            }
            let parsed: serde_json::Value = serde_json::from_str(l).map_err(|e| {
                let last = text.lines().filter(|x| !x.trim().is_empty()).count() == i + 1;
                if last && e.is_eof() {
                    anyhow!(
                        "line {} is a truncated JSON record — the file looks like it was \
                         cut mid-write. Complete or remove the last line; tdy will not \
                         silently drop a partial record.",
                        i + 1
                    )
                } else {
                    anyhow!("invalid JSON on line {}: {e}", i + 1)
                }
            })?;
            out.push(parsed);
        }
        out
    } else {
        let doc: serde_json::Value =
            serde_json::from_str(&text).context("invalid JSON document")?;
        let node = match pointer {
            Some(p) => doc
                .pointer(p)
                .ok_or_else(|| anyhow!("JSON pointer {p:?} matched nothing"))?
                .clone(),
            None => doc,
        };
        match node {
            serde_json::Value::Array(mut a) => {
                if let Some(m) = opts.max_rows {
                    if a.len() > m {
                        a.truncate(m);
                        truncated = true;
                    }
                }
                a
            }
            other => bail!(
                "expected a JSON array of records{}, found {}",
                pointer.map(|p| format!(" at pointer {p:?}")).unwrap_or_default(),
                json_kind(&other)
            ),
        }
    };

    // Union of keys, first-seen order.
    let mut header: Vec<String> = Vec::new();
    let mut seen = std::collections::HashSet::new();
    let mut objects = 0usize;
    for rec in &records {
        if let serde_json::Value::Object(map) = rec {
            objects += 1;
            for k in map.keys() {
                if seen.insert(k.clone()) {
                    header.push(k.clone());
                }
            }
        }
    }

    // A mix of objects and scalars has no single tabular reading; saying so
    // beats silently dropping one shape into the first column of the other.
    if objects > 0 && objects != records.len() {
        bail!(
            "{} of {} JSON records are not objects; a records array must be all \
             objects (or all scalars)",
            records.len() - objects,
            records.len()
        );
    }
    if objects == 0 {
        header = vec!["value".to_string()];
    }

    let rows: Vec<Vec<String>> = records
        .iter()
        .map(|rec| match rec {
            serde_json::Value::Object(map) => header
                .iter()
                .map(|k| map.get(k).map(json_scalar).unwrap_or_default())
                .collect(),
            other => vec![json_scalar(other)],
        })
        .collect();

    Ok(RawTable::with_header(header, rows, truncated))
}

pub(crate) fn json_scalar(v: &serde_json::Value) -> String {
    match v {
        serde_json::Value::Null => String::new(),
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Bool(b) => b.to_string(),
        serde_json::Value::Number(n) => n.to_string(),
        nested => serde_json::to_string(nested).unwrap_or_default(),
    }
}

fn json_kind(v: &serde_json::Value) -> &'static str {
    match v {
        serde_json::Value::Null => "null",
        serde_json::Value::Bool(_) => "a boolean",
        serde_json::Value::Number(_) => "a number",
        serde_json::Value::String(_) => "a string",
        serde_json::Value::Array(_) => "an array",
        serde_json::Value::Object(_) => "an object",
    }
}

// ---------------------------------------------------------------------------
// Transforms
// ---------------------------------------------------------------------------

/// Turn the first `n` rows of a table into one header, as `promote_header`
/// means it.
///
/// Factored out so the streaming executor in `stream` builds headers with
/// *this* code rather than a copy of it: a header that differed between the
/// two paths would rename columns, which is the quietest way to return the
/// wrong data.
pub(crate) fn promote_header_from(header_rows: Vec<Vec<String>>, join: &str) -> Vec<String> {
    promote_header_recording(header_rows, join).0
}

/// As [`promote_header_from`], but also returning the header **before**
/// duplicate names were disambiguated — see `RawTable::header_origin`.
pub(crate) fn promote_header_recording(
    header_rows: Vec<Vec<String>>,
    join: &str,
) -> (Vec<String>, Vec<String>) {
    let width = header_rows.iter().map(|r| r.len()).max().unwrap_or(0);
    let last = header_rows.len().saturating_sub(1);
    let filled: Vec<Vec<String>> = header_rows
        .into_iter()
        .enumerate()
        .map(|(i, mut r)| {
            r.resize(width, String::new());
            // Fill-right only on rows above the last one: those carry
            // horizontally merged titles. A blank in the final header row is a
            // nameless column, and giving it its left neighbour's name would
            // label one column with another column's meaning.
            if i < last {
                let mut carry = String::new();
                for cell in &mut r {
                    if cell.trim().is_empty() {
                        cell.clone_from(&carry);
                    } else {
                        carry.clone_from(cell);
                    }
                }
            }
            r
        })
        .collect();
    let mut header: Vec<String> = (0..width)
        .map(|c| {
            let parts: Vec<&str> =
                filled.iter().map(|r| r[c].trim()).filter(|s| !s.is_empty()).collect();
            if parts.is_empty() {
                format!("col_{}", c + 1)
            } else {
                parts.join(join)
            }
        })
        .collect();
    let origin = header.clone();
    dedupe_names(&mut header);
    (header, origin)
}

pub fn apply_transforms(table: &mut RawTable, transforms: &[Transform]) -> Result<()> {
    for t in transforms {
        match t {
            Transform::SkipRows { head, tail } => {
                let head = (*head as usize).min(table.rows.len());
                table.rows.drain(..head);
                if *tail > 0 {
                    // On a truncated read the real end of the file was never
                    // seen, so "drop the last row" would drop an arbitrary
                    // middle row instead. Previews and dry runs accept that
                    // the footer is still present.
                    if !table.truncated {
                        let keep = table.rows.len().saturating_sub(*tail as usize);
                        table.rows.truncate(keep);
                    }
                }
            }
            Transform::PromoteHeader { rows, join } => {
                table.rectangularize()?;
                let n = *rows as usize;
                if table.rows.len() < n {
                    bail!(
                        "promote_header wants {} header row(s) but only {} row(s) remain",
                        n,
                        table.rows.len()
                    );
                }
                let header_rows: Vec<Vec<String>> = table.rows.drain(..n).collect();
                let (header, origin) = promote_header_recording(header_rows, join);
                table.header_origin = Some(origin);
                table.header = Some(header);
            }
            Transform::DropRowsMatching { pattern, column } => {
                let re = compile(pattern, "drop_rows_matching")?;
                match column {
                    Some(name) => {
                        table.ensure_header()?;
                        let idx = table.col_index(name)?;
                        table
                            .rows
                            .retain(|r| r.get(idx).map(|v| !re.is_match(v)).unwrap_or(true));
                    }
                    None => {
                        let mut joined = String::new();
                        table.rows.retain(|r| {
                            joined.clear();
                            for (i, c) in r.iter().enumerate() {
                                if i > 0 {
                                    joined.push('\t');
                                }
                                joined.push_str(c);
                            }
                            !re.is_match(&joined)
                        });
                    }
                }
            }
            Transform::FillDown { columns } => {
                table.ensure_header()?;
                let index = table.header_index()?;
                let resolved: Vec<usize> = columns
                    .iter()
                    .map(|c| index.get(c.as_str()).copied().ok_or_else(|| table.missing_column(c)))
                    .collect::<Result<_>>()?;
                for idx in resolved {
                    let mut last = String::new();
                    for row in &mut table.rows {
                        let Some(cell) = row.get_mut(idx) else { continue };
                        if cell.trim().is_empty() {
                            cell.clone_from(&last);
                        } else {
                            last.clone_from(cell);
                        }
                    }
                }
            }
            Transform::Constant { name, value } => {
                table.ensure_header()?;
                let h = table.header.as_mut().expect("ensure_header sets it");
                if h.iter().any(|c| c == name) {
                    bail!(
                        "constant: the file already has a column named {name:?} — a \
                         constant may only add a column, never shadow one"
                    );
                }
                h.push(name.clone());
                if let Some(o) = table.header_origin.as_mut() {
                    o.push(name.clone());
                }
                for row in &mut table.rows {
                    row.push(value.clone());
                }
            }
            Transform::Unpivot {
                id_columns,
                value_columns,
                variable_name,
                value_name,
            } => {
                table.ensure_header()?;
                let index = table.header_index()?;
                let lookup = |c: &String| {
                    index.get(c.as_str()).copied().ok_or_else(|| table.missing_column(c))
                };
                let id_idx: Vec<usize> = id_columns.iter().map(lookup).collect::<Result<_>>()?;
                let val_idx: Vec<usize> =
                    value_columns.iter().map(lookup).collect::<Result<_>>()?;
                let out_rows = table.rows.len().saturating_mul(val_idx.len());
                let mut new_rows = Vec::with_capacity(out_rows);
                for row in &table.rows {
                    for (vi, vname) in val_idx.iter().zip(value_columns.iter()) {
                        let mut nr: Vec<String> = id_idx
                            .iter()
                            .map(|i| row.get(*i).cloned().unwrap_or_default())
                            .collect();
                        nr.push(vname.clone());
                        nr.push(row.get(*vi).cloned().unwrap_or_default());
                        new_rows.push(nr);
                    }
                }
                let mut header = id_columns.clone();
                header.push(variable_name.clone());
                header.push(value_name.clone());
                dedupe_names(&mut header);
                table.header = Some(header);
                table.rows = new_rows;
                table.ragged = RaggedPolicy::PadNulls;
            }
        }
    }
    Ok(())
}

/// Make every name unique, without inventing a name that is already taken:
/// `["a", "a", "a_2"]` must not become `["a", "a_2", "a_2"]`.
pub(crate) fn dedupe_names(names: &mut [String]) {
    let mut taken: std::collections::HashSet<String> = std::collections::HashSet::new();
    for n in names.iter_mut() {
        if taken.insert(n.clone()) {
            continue;
        }
        let mut i = 2usize;
        loop {
            let candidate = format!("{n}_{i}");
            if !taken.contains(&candidate) {
                taken.insert(candidate.clone());
                *n = candidate;
                break;
            }
            i += 1;
        }
    }
}

// ---------------------------------------------------------------------------
// Typed projection
// ---------------------------------------------------------------------------

/// Rows per output batch. Arrow string arrays address their data with 32-bit
/// offsets, so a single batch holding more than 2 GB of text in one column
/// overflows; chunking also gives DataFusion something to parallelise over.
pub const BATCH_ROWS: usize = 65_536;

/// The Arrow schema a spec produces, without reading the file.
///
/// Derived by building every column over *zero* rows, so it is the same code
/// that types real data — a hand-written mapping from `DType` to Arrow would
/// be a second source of truth, and the first thing to drift would be the
/// timestamp timezone label.
///
/// A streaming table provider needs this: DataFusion plans the query before
/// any batch exists, so the schema cannot come from the data.
pub fn schema_of(spec: &ParseSpec) -> Result<Schema> {
    let mut fields = Vec::with_capacity(spec.columns.len());
    for col in &spec.columns {
        let (field, _) = build_column_at(col, &[], 0)
            .with_context(|| format!("deriving the type of column `{}`", col.name))?;
        fields.push(field);
    }
    Ok(Schema::new(fields))
}

pub fn to_record_batch(spec: &ParseSpec, table: &mut RawTable) -> Result<RecordBatch> {
    let batches = to_record_batches(spec, table)?;
    let schema = batches[0].schema();
    datafusion::arrow::compute::concat_batches(&schema, &batches)
        .context("assembling record batch")
}

/// The projection, produced in bounded chunks.
pub fn to_record_batches(spec: &ParseSpec, table: &mut RawTable) -> Result<Vec<RecordBatch>> {
    table.ensure_header()?;
    let index = table.header_index()?;
    let mut resolved: Vec<(&ColumnSpec, usize)> = Vec::with_capacity(spec.columns.len());
    for col in &spec.columns {
        let source = col.source_name();
        let idx = *index
            .get(source)
            .ok_or_else(|| table.missing_column(source))
            .with_context(|| format!("resolving output column `{}`", col.name))?;
        resolved.push((col, idx));
    }

    let total = table.rows.len();
    let mut out = Vec::new();
    let mut schema: Option<Arc<Schema>> = None;
    let mut start = 0usize;
    // `..=total` so an empty table still produces one (empty) batch: a query
    // over a file with a header and no rows must still have a schema.
    loop {
        let end = (start + BATCH_ROWS).min(total);
        let rows = &table.rows[start..end];
        let mut fields = Vec::with_capacity(resolved.len());
        let mut arrays: Vec<ArrayRef> = Vec::with_capacity(resolved.len());
        for (col, idx) in &resolved {
            let values: Vec<&str> = rows
                .iter()
                .map(|r| r.get(*idx).map(|s| s.as_str()).unwrap_or(""))
                .collect();
            let (field, array) = build_column_at(col, &values, start)
                .with_context(|| format!("building column `{}`", col.name))?;
            fields.push(field);
            arrays.push(array);
        }
        let sch = schema.get_or_insert_with(|| Arc::new(Schema::new(fields))).clone();
        out.push(
            RecordBatch::try_new(sch, arrays).context("assembling record batch")?,
        );
        start = end;
        if start >= total {
            break;
        }
    }
    Ok(out)
}

/// `row_offset` is the index of `values[0]` within the whole table, so that a
/// parse error names the row a person would find in their file rather than
/// its position inside an internal 64k batch.
pub(crate) fn build_column_at(
    col: &ColumnSpec,
    values: &[&str],
    row_offset: usize,
) -> Result<(Field, ArrayRef)> {
    let p = &col.parse;
    let strip_re = p
        .strip
        .as_ref()
        .map(|s| compile(s, "strip"))
        .transpose()?;

    // trim -> replace -> na -> strip. Borrowed until something actually
    // changes, so a clean column costs no allocations at all.
    let cleaned: Vec<Option<Cow<str>>> = values
        .iter()
        .map(|raw| {
            let mut v: Cow<str> = Cow::Borrowed(raw.trim());
            for r in &p.replace {
                if v.contains(&r.from) {
                    v = Cow::Owned(v.replace(&r.from, &r.to));
                }
            }
            // Case-insensitively: `NA`, `na` and `N/A` are the same claim
            // about a value, and a sidecar that had to list every casing
            // would be a list nobody could keep complete. `sniff::is_na`
            // already folds case when it decides a token is missing, so the
            // executor folding it too is what makes the two agree — they did
            // not, and a column typed from a sample containing `NA` failed on
            // a later `NULL`.
            let is_na = v.is_empty()
                || p.na_values.iter().any(|na| na.eq_ignore_ascii_case(v.as_ref()));
            if is_na {
                return None;
            }
            if let Some(re) = &strip_re {
                if re.is_match(&v) {
                    let stripped = re.replace_all(&v, "").trim().to_string();
                    if stripped.is_empty() {
                        return None;
                    }
                    v = Cow::Owned(stripped);
                }
            }
            Some(v)
        })
        .collect();

    if !col.nullable {
        if let Some(row) = cleaned.iter().position(|v| v.is_none()) {
            bail!(
                "row {}: null in non-nullable column (raw value {:?}); \
                 set nullable = true or extend na_values/transforms",
                row_offset + row + 1,
                values[row]
            );
        }
    }

    // Numeric normalisation, verified rather than assumed: a thousands
    // separator that does not group in threes is a wrong spec, not a
    // character to delete. This is what keeps "1,5" from becoming 15.
    let numeric = |v: &str| -> Result<String> {
        numfmt::check_grouping(v, p.thousands_separator, p.decimal_separator)
            .map_err(|e| anyhow!("{e}"))?;
        let mut s = Cow::Borrowed(v);
        if let Some(t) = p.thousands_separator {
            if s.contains(t) {
                s = Cow::Owned(s.replace(t, ""));
            }
        }
        if let Some(d) = p.decimal_separator {
            if d != '.' && s.contains(d) {
                s = Cow::Owned(s.replace(d, "."));
            }
        }
        // Applied last, on a canonical number, so it moves the point the user
        // sees rather than interacting with a separator convention.
        if let Some(shift) = p.decimal_shift {
            if shift != 0 {
                s = Cow::Owned(shift_decimal_point(&s, shift));
            }
        }
        Ok(s.into_owned())
    };

    macro_rules! parse_all {
        ($ty:ty, $f:expr) => {{
            let mut out: Vec<Option<$ty>> = Vec::with_capacity(cleaned.len());
            for (i, v) in cleaned.iter().enumerate() {
                match v {
                    None => out.push(None),
                    Some(s) => match $f(s.as_ref()) {
                        Ok(x) => out.push(Some(x)),
                        Err(e) => {
                            bail!("row {}: cannot parse {:?}: {}", row_offset + i + 1, s, e)
                        }
                    },
                }
            }
            out
        }};
    }

    let (arrow_type, array): (ArrowType, ArrayRef) = match &col.dtype {
        DType::Utf8 => {
            let arr = StringArray::from_iter(cleaned.iter().map(|v| v.as_deref()));
            (ArrowType::Utf8, Arc::new(arr))
        }
        DType::Bool => {
            let out = parse_all!(bool, |s: &str| parse_bool(s, p));
            (ArrowType::Boolean, Arc::new(BooleanArray::from(out)))
        }
        DType::Int64 => {
            let out = parse_all!(i64, |s: &str| {
                numeric(s)?
                    .trim_start_matches('+')
                    .parse::<i64>()
                    .map_err(|e| anyhow!("{e}"))
            });
            (ArrowType::Int64, Arc::new(Int64Array::from(out)))
        }
        DType::Float64 => {
            let out = parse_all!(f64, |s: &str| {
                let n = numeric(s)?;
                let t = n.trim_start_matches('+');
                // Reject the words f64 accepts but a data file never means.
                if t.eq_ignore_ascii_case("nan")
                    || t.eq_ignore_ascii_case("inf")
                    || t.eq_ignore_ascii_case("infinity")
                    || t.eq_ignore_ascii_case("-inf")
                    || t.eq_ignore_ascii_case("-infinity")
                {
                    bail!("{t:?} is not a number (add it to na_values if it means \"missing\")");
                }
                t.parse::<f64>().map_err(|e| anyhow!("{e}"))
            });
            (ArrowType::Float64, Arc::new(Float64Array::from(out)))
        }
        DType::Decimal { precision, scale } => {
            let out = parse_all!(i128, |s: &str| parse_decimal(&numeric(s)?, *precision, *scale));
            let arr = Decimal128Array::from(out)
                .with_precision_and_scale(*precision, *scale)
                .context("decimal precision/scale")?;
            (ArrowType::Decimal128(*precision, *scale), Arc::new(arr))
        }
        DType::Date { format } => {
            let out = parse_all!(i32, |s: &str| parse_date_days(s, format));
            (ArrowType::Date32, Arc::new(Date32Array::from(out)))
        }
        DType::Timestamp { format, timezone } => {
            let offset = timezone.as_deref().map(|tz| {
                parse_fixed_offset(tz)
                    .ok_or_else(|| anyhow!("timezone {tz:?} is not a fixed offset"))
            });
            let offset = match offset {
                Some(Ok(o)) => Some(o),
                Some(Err(e)) => return Err(e),
                None => None,
            };
            let out = parse_all!(i64, |s: &str| parse_timestamp_micros(s, format, offset));
            // Store the offset in the one spelling every Arrow consumer
            // parses: "Z", "utc" and "GMT" are readable in a sidecar but not
            // all of them survive a round trip through Arrow's tz handling.
            let label: Option<Arc<str>> = offset.map(|o| Arc::<str>::from(canonical_offset(o)));
            let arr = TimestampMicrosecondArray::from(out).with_timezone_opt(label.clone());
            (ArrowType::Timestamp(TimeUnit::Microsecond, label), Arc::new(arr))
        }
    };

    Ok((Field::new(&col.name, arrow_type, col.nullable), array))
}

/// Move a decimal number's point by `shift` places, exactly.
///
/// String surgery on the digits rather than arithmetic: `123450` shifted by
/// -2 is `1234.50`, with no float involved and nothing rounded. That matters
/// because the whole reason this exists is money, and a `* 0.01` would
/// introduce exactly the representation error `decimal` was chosen to avoid.
pub fn shift_decimal_point(v: &str, shift: i8) -> String {
    let v = v.trim();
    let (sign, rest) = match v.strip_prefix('-') {
        Some(r) => ("-", r),
        None => ("", v.strip_prefix('+').unwrap_or(v)),
    };
    let (int_part, frac_part) = match rest.split_once('.') {
        Some((i, f)) => (i.to_string(), f.to_string()),
        None => (rest.to_string(), String::new()),
    };
    let mut digits: Vec<u8> = int_part.bytes().chain(frac_part.bytes()).collect();
    // Where the point currently sits, counted from the left of `digits`.
    let mut point = int_part.len() as i64 + shift as i64;

    // Pad so the point lands inside the digit string.
    while point < 0 {
        digits.insert(0, b'0');
        point += 1;
    }
    while point > digits.len() as i64 {
        digits.push(b'0');
    }

    let (lhs, rhs) = digits.split_at(point as usize);
    let lhs = String::from_utf8_lossy(lhs);
    let rhs = String::from_utf8_lossy(rhs);
    let lhs = if lhs.is_empty() { "0" } else { &lhs };
    if rhs.is_empty() {
        format!("{sign}{lhs}")
    } else {
        format!("{sign}{lhs}.{rhs}")
    }
}

fn parse_bool(s: &str, p: &ValueParsing) -> Result<bool> {
    let low = s.to_ascii_lowercase();
    let truthy: Vec<String> = if p.true_values.is_empty() {
        ["true", "1", "yes", "y", "ja", "wahr"].iter().map(|s| s.to_string()).collect()
    } else {
        p.true_values.iter().map(|v| v.to_ascii_lowercase()).collect()
    };
    let falsy: Vec<String> = if p.false_values.is_empty() {
        ["false", "0", "no", "n", "nein", "falsch"].iter().map(|s| s.to_string()).collect()
    } else {
        p.false_values.iter().map(|v| v.to_ascii_lowercase()).collect()
    };
    if truthy.contains(&low) {
        Ok(true)
    } else if falsy.contains(&low) {
        Ok(false)
    } else {
        bail!("not in true_values/false_values")
    }
}

/// Exact decimal parse to a scaled i128 mantissa. Rounds half away from zero
/// when the value has more fractional digits than `scale`.
fn parse_decimal(s: &str, precision: u8, scale: i8) -> Result<i128> {
    let s = s.trim().trim_start_matches('+');
    let (neg, s) = match s.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, s),
    };
    let (int_part, frac_part) = match s.split_once('.') {
        Some((i, f)) => (i, f),
        None => (s, ""),
    };
    if int_part.is_empty() && frac_part.is_empty() {
        bail!("empty number");
    }
    if !int_part.chars().all(|c| c.is_ascii_digit())
        || !frac_part.chars().all(|c| c.is_ascii_digit())
    {
        bail!("not a decimal number");
    }
    let scale_u = usize::try_from(scale).map_err(|_| anyhow!("negative scale unsupported"))?;
    let mut mantissa: i128 = if int_part.is_empty() {
        0
    } else {
        int_part.parse::<i128>().map_err(|e| anyhow!("{e}"))?
    };
    let mut frac = frac_part.to_string();
    let mut round_up = false;
    if frac.len() > scale_u {
        let next = frac.as_bytes()[scale_u] - b'0';
        round_up = next >= 5;
        frac.truncate(scale_u);
    }
    while frac.len() < scale_u {
        frac.push('0');
    }
    let frac_val: i128 = if frac.is_empty() {
        0
    } else {
        frac.parse::<i128>().map_err(|e| anyhow!("{e}"))?
    };
    mantissa = mantissa
        .checked_mul(10_i128.pow(scale_u as u32))
        .and_then(|m| m.checked_add(frac_val))
        .ok_or_else(|| anyhow!("decimal overflow"))?;
    if round_up {
        mantissa = mantissa.checked_add(1).ok_or_else(|| anyhow!("decimal overflow"))?;
    }
    let max = 10_i128.checked_pow(u32::from(precision)).unwrap_or(i128::MAX);
    if mantissa >= max {
        bail!("value exceeds decimal({precision}, {scale})");
    }
    Ok(if neg { -mantissa } else { mantissa })
}

/// "+02:00" / "-05:30" / "+00:00" — the spelling Arrow and DataFusion agree on.
fn canonical_offset(o: chrono::FixedOffset) -> String {
    let secs = o.local_minus_utc();
    let sign = if secs < 0 { '-' } else { '+' };
    let a = secs.abs();
    format!("{sign}{:02}:{:02}", a / 3600, (a % 3600) / 60)
}

fn epoch() -> NaiveDate {
    NaiveDate::from_ymd_opt(1970, 1, 1).expect("1970-01-01 is a date")
}

fn parse_date_days(s: &str, format: &str) -> Result<i32> {
    let date = NaiveDate::parse_from_str(s, format)
        .or_else(|e| {
            // Month-year forms ("%b %Y" on "Jan 2025") lack a day; pin day 1.
            // Only when the format does not itself ask for one.
            if format.contains("%d") || format.contains("%e") {
                Err(e)
            } else {
                NaiveDate::parse_from_str(&format!("1 {s}"), &format!("%d {format}"))
            }
        })
        .map_err(|e| anyhow!("date does not match format {format:?}: {e}"))?;
    check_year(s, format)?;
    Ok((date - epoch()).num_days() as i32)
}

/// `%Y` happily accepts one to four digits, so "01/02/25" under "%d/%m/%Y"
/// parses as the year 25 and lands 2000 years from the intended date. That is
/// exactly the silent kind of wrong this tool must not produce.
fn check_year(s: &str, format: &str) -> Result<()> {
    if !format.contains("%Y") {
        return Ok(());
    }
    let mut digits = 0usize;
    let mut runs: Vec<usize> = Vec::new();
    for c in s.chars() {
        if c.is_ascii_digit() {
            digits += 1;
        } else if digits > 0 {
            runs.push(digits);
            digits = 0;
        }
    }
    if digits > 0 {
        runs.push(digits);
    }
    if !runs.is_empty() && runs.iter().all(|r| *r <= 2) {
        bail!(
            "{s:?} has no four-digit year but the format says %Y; use %y for \
             two-digit years (and decide explicitly which century they mean)"
        );
    }
    Ok(())
}

fn parse_timestamp_micros(
    s: &str,
    format: &str,
    offset: Option<chrono::FixedOffset>,
) -> Result<i64> {
    use chrono::{DateTime, TimeZone};

    // If the format itself carries an offset, it wins: the value says what
    // instant it is, and no declared timezone can override that.
    if format.contains("%z") || format.contains("%:z") || format.contains("%#z") {
        let dt = DateTime::parse_from_str(s, format)
            .map_err(|e| anyhow!("timestamp does not match format {format:?}: {e}"))?;
        check_year(s, format)?;
        return Ok(dt.timestamp_micros());
    }

    let naive = NaiveDateTime::parse_from_str(s, format)
        .or_else(|e| {
            NaiveDate::parse_from_str(s, format)
                .map(|d| d.and_hms_opt(0, 0, 0).expect("midnight exists"))
                .map_err(|_| e)
        })
        .map_err(|e| anyhow!("timestamp does not match format {format:?}: {e}"))?;
    check_year(s, format)?;

    match offset {
        // A timezone-bearing Arrow timestamp is a UTC instant. The written
        // wall clock is in `offset`, so convert rather than relabel.
        Some(off) => off
            .from_local_datetime(&naive)
            .single()
            .ok_or_else(|| anyhow!("{s:?} is ambiguous or does not exist in offset {off}"))
            .map(|dt| dt.timestamp_micros()),
        None => Ok(naive.and_utc().timestamp_micros()),
    }
}

// ---------------------------------------------------------------------------
// Full pipeline
// ---------------------------------------------------------------------------

/// Run extraction + transforms + projection over the entire file.
pub fn execute(spec: &ParseSpec, path: &Path, limits: Limits) -> Result<RecordBatch> {
    run(spec, path, &ExtractOpts::full(limits))
}

/// The same, but keeping the output in bounded batches rather than
/// concatenating them into one.
pub fn execute_batches(spec: &ParseSpec, path: &Path, limits: Limits) -> Result<Vec<RecordBatch>> {
    let opts = ExtractOpts::full(limits);
    let mut table = extract(&spec.extraction, path, &opts)
        .with_context(|| format!("extracting {}", path.display()))?;
    apply_transforms(&mut table, &spec.transforms)?;
    to_record_batches(spec, &mut table)
}

/// What a sheet looks like, from a single open of the workbook.
#[derive(Debug, Clone)]
pub struct SheetShape {
    pub name: String,
    /// Rows containing at least one non-blank cell.
    pub rows: usize,
    pub cols: usize,
    /// Cells in the first rows that read as numbers. A legend or a cover page
    /// is all prose; a data sheet has quantities in it.
    pub numeric_cells: usize,
}

/// One open of a workbook, reporting the shape of every sheet.
pub fn excel_sheet_shapes(path: &Path, limits: Limits) -> Result<Vec<SheetShape>> {
    crate::xlguard::preflight(path, &limits)?;
    let mut wb = open_workbook_auto(path)
        .with_context(|| format!("cannot open workbook {}", path.display()))?;
    let names = wb.sheet_names().to_vec();
    let mut out = Vec::with_capacity(names.len());
    for name in names {
        // A sheet too big to hold is skipped, not fatal: the workbook may
        // still have a perfectly good sheet next to it, and this function
        // exists to help *choose* one.
        match checked_worksheet_range(&mut wb, &name, &limits) {
            Ok(r) => {
                let populated = r
                    .rows()
                    .filter(|row| row.iter().any(|c| !render_cell(c).trim().is_empty()))
                    .count();
                let numeric_cells = r
                    .rows()
                    .skip(1) // a header row of years would flatter a legend
                    .take(50)
                    .flat_map(|row| row.iter())
                    .filter(|c| {
                        let t = render_cell(c);
                        let t = t.trim();
                        !t.is_empty() && crate::numfmt::infer(&[t]).is_some()
                    })
                    .count();
                out.push(SheetShape { name, rows: populated, cols: r.width(), numeric_cells });
            }
            Err(_) => out.push(SheetShape { name, rows: 0, cols: 0, numeric_cells: 0 }),
        }
    }
    Ok(out)
}

/// The first `max_rows` rows x `max_cols` cells of one sheet, formatted
/// exactly as `extract_excel` formats them (`render_cell`) — the raw view's
/// job is to show the file's own spelling and separators, not a second
/// opinion of what they mean. Goes through the same guard sequence as every
/// other workbook-touching path: `xlguard::preflight` before the workbook is
/// opened at all, then `checked_worksheet_range` for the one sheet read.
///
/// A clipped read says so **in the grid**: this is the only place that knows
/// both the cap and the sheet's true extent, so an `…` cell is appended to
/// every row when columns were cut, and a final `["…"]` row when rows were.
/// A silently clipped grid is a small version of the failure this project
/// exists to prevent — someone reads twelve columns as the whole sheet and
/// writes a `matches` clause for a column that is not the one they saw.
/// Marking here also means every renderer (console `.show`, the TUI's raw
/// panel) shows it without knowing the cap.
pub fn sheet_grid(
    path: &Path,
    sheet: &str,
    limits: Limits,
    max_rows: usize,
    max_cols: usize,
) -> Result<Vec<Vec<String>>> {
    crate::xlguard::preflight(path, &limits)?;
    let mut wb = open_workbook_auto(path)
        .with_context(|| format!("cannot open workbook {}", path.display()))?;
    let range = checked_worksheet_range(&mut wb, sheet, &limits)?;
    let clipped_cols = range.width() > max_cols;
    let clipped_rows = range.height() > max_rows;
    let mut out: Vec<Vec<String>> = range
        .rows()
        .take(max_rows)
        .map(|row| {
            let mut cells: Vec<String> = row.iter().take(max_cols).map(render_cell).collect();
            if clipped_cols {
                cells.push("…".to_string());
            }
            cells
        })
        .collect();
    if clipped_rows {
        out.push(vec!["…".to_string()]);
    }
    Ok(out)
}

/// The same pipeline, but producing at most `max_rows` output rows.
///
/// The cap is on *output*, not on extraction: a spec that skips a four-line
/// title block would otherwise spend its whole budget on rows it then throws
/// away, and a ten-row preview of a file with a twelve-line header would fail
/// with "promote_header wants 1 header row but only 0 remain".
///
/// Because the end of the file is never seen, a `skip_rows` *tail* is not
/// applied — see [`apply_transforms`].
pub fn preview(
    spec: &ParseSpec,
    path: &Path,
    limits: Limits,
    max_rows: usize,
) -> Result<RecordBatch> {
    let slack = spec
        .transforms
        .iter()
        .map(|t| match t {
            Transform::SkipRows { head, .. } => *head as usize,
            Transform::PromoteHeader { rows, .. } => *rows as usize,
            _ => 0,
        })
        .sum::<usize>();
    let extract_rows = max_rows.saturating_mul(4).saturating_add(slack).max(200);
    let opts = ExtractOpts::capped(limits, extract_rows);
    let mut table = extract(&spec.extraction, path, &opts)
        .with_context(|| format!("extracting {}", path.display()))?;
    apply_transforms(&mut table, &spec.transforms)?;
    table.rows.truncate(max_rows);
    to_record_batch(spec, &mut table)
}

fn run(spec: &ParseSpec, path: &Path, opts: &ExtractOpts) -> Result<RecordBatch> {
    let mut table = extract(&spec.extraction, path, opts)
        .with_context(|| format!("extracting {}", path.display()))?;
    apply_transforms(&mut table, &spec.transforms)?;
    to_record_batch(spec, &mut table)
}

/// Tier-3 of the retry loop: actually run the spec on a slice of the real
/// file. Returns the error text for the model on failure.
pub fn dry_run(spec: &ParseSpec, path: &Path, limits: Limits) -> Result<RecordBatch> {
    preview(spec, path, limits, 200)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dedupe_never_collides_with_an_existing_name() {
        let mut n = vec!["a".to_string(), "a".to_string(), "a_2".to_string()];
        dedupe_names(&mut n);
        assert_eq!(n, vec!["a", "a_2", "a_2_2"]);
        let unique: std::collections::HashSet<_> = n.iter().collect();
        assert_eq!(unique.len(), n.len());
    }

    #[test]
    fn modal_width_is_deterministic_on_ties() {
        let rows = |ws: &[usize]| -> Vec<Vec<String>> {
            ws.iter().map(|w| vec![String::new(); *w]).collect()
        };
        // 2 and 3 tie; the wider one must win, every time.
        for _ in 0..50 {
            assert_eq!(modal_width(&rows(&[2, 2, 3, 3])), Some(3));
        }
        assert_eq!(modal_width(&rows(&[])), None);
    }

    #[test]
    fn decimal_rounding_and_bounds() {
        assert_eq!(parse_decimal("1.005", 12, 2).unwrap(), 101);
        assert_eq!(parse_decimal("-1.005", 12, 2).unwrap(), -101);
        assert_eq!(parse_decimal("2.344", 12, 2).unwrap(), 234);
        assert_eq!(parse_decimal("1200.50", 12, 2).unwrap(), 120050);
        assert_eq!(parse_decimal("0", 12, 2).unwrap(), 0);
        assert!(parse_decimal("12345.67", 5, 2).is_err());
        assert!(parse_decimal("abc", 12, 2).is_err());
        assert!(parse_decimal("", 12, 2).is_err());
    }

    #[test]
    fn two_digit_years_are_refused_under_percent_capital_y() {
        assert!(parse_date_days("01/02/25", "%d/%m/%Y").is_err());
        assert!(parse_date_days("01/02/2025", "%d/%m/%Y").is_ok());
        // %y is the explicit opt-in.
        assert!(parse_date_days("01/02/25", "%d/%m/%y").is_ok());
    }

    #[test]
    fn month_year_pinning_only_when_the_format_lacks_a_day() {
        assert!(parse_date_days("2025 Jan", "%Y %b").is_ok());
        // A format that wants a day must actually get one.
        assert!(parse_date_days("2025 Jan", "%Y %b %d").is_err());
    }

    #[test]
    fn timestamps_convert_from_the_declared_offset_to_utc() {
        let off = parse_fixed_offset("+02:00").unwrap();
        let with = parse_timestamp_micros("2026-01-05 10:00:00", "%Y-%m-%d %H:%M:%S", Some(off)).unwrap();
        let without = parse_timestamp_micros("2026-01-05 10:00:00", "%Y-%m-%d %H:%M:%S", None).unwrap();
        assert_eq!(without - with, 2 * 3600 * 1_000_000);
    }

    #[test]
    fn offsets_are_stored_canonically() {
        assert_eq!(canonical_offset(parse_fixed_offset("Z").unwrap()), "+00:00");
        assert_eq!(canonical_offset(parse_fixed_offset("utc").unwrap()), "+00:00");
        assert_eq!(canonical_offset(parse_fixed_offset("+02:00").unwrap()), "+02:00");
        assert_eq!(canonical_offset(parse_fixed_offset("-0530").unwrap()), "-05:30");
    }

    #[test]
    fn a_format_borne_offset_wins() {
        let a = parse_timestamp_micros(
            "2026-01-05 10:00:00 +0200",
            "%Y-%m-%d %H:%M:%S %z",
            None,
        )
        .unwrap();
        let b = parse_timestamp_micros("2026-01-05 08:00:00", "%Y-%m-%d %H:%M:%S", None).unwrap();
        assert_eq!(a, b);
    }

    #[test]
    fn nan_and_infinity_are_not_numbers() {
        let col = ColumnSpec {
            name: "v".into(),
            source: None,
            dtype: DType::Float64,
            nullable: true,
            parse: ValueParsing::default(),
        };
        assert!(build_column_at(&col, &["NaN"], 0).is_err());
        assert!(build_column_at(&col, &["Infinity"], 0).is_err());
        assert!(build_column_at(&col, &["1.5"], 0).is_ok());
    }

    #[test]
    fn a_thousands_separator_that_does_not_group_is_an_error() {
        let col = ColumnSpec {
            name: "v".into(),
            source: None,
            dtype: DType::Float64,
            nullable: true,
            parse: ValueParsing { thousands_separator: Some(','), ..Default::default() },
        };
        let err = build_column_at(&col, &["1,5"], 0).unwrap_err();
        assert!(format!("{err:#}").contains("grouped"), "{err:#}");
        assert!(build_column_at(&col, &["1,234"], 0).is_ok());
    }
}

#[cfg(test)]
mod shift_tests {
    use super::shift_decimal_point as sh;

    /// Exact string surgery, not arithmetic — the whole reason this exists is
    /// money, and a `* 0.01` would reintroduce the representation error
    /// `decimal` was chosen to avoid.
    #[test]
    fn moving_the_point_is_exact_in_both_directions() {
        assert_eq!(sh("123450", -2), "1234.50");
        assert_eq!(sh("1", -2), "0.01");
        assert_eq!(sh("0", -2), "0.00");
        assert_eq!(sh("12", -4), "0.0012");
        assert_eq!(sh("1234.5", -2), "12.345");
        assert_eq!(sh("1234.50", 2), "123450", "a trailing point must be elided");
        assert_eq!(sh("-123450", -2), "-1234.50");
        assert_eq!(sh("+50", -2), "0.50");
        assert_eq!(sh("7", 3), "7000");
        assert_eq!(sh("123450", 0), "123450");
    }

    /// A shift that leaves nothing to the left of the point must still be a
    /// number, not ".01".
    #[test]
    fn a_shift_past_the_leading_digit_keeps_a_zero() {
        assert_eq!(sh("5", -1), "0.5");
        assert_eq!(sh("5", -3), "0.005");
    }
}
