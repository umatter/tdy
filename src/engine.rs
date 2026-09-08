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
    parse_a1_range, parse_fixed_offset, ColumnSpec, DType, EpochUnit, Extraction, FillDirection, NegativeStyle, NoMatchPolicy, ParseSpec, RaggedPolicy, RowWindow, ShortSplit, SourcePart,
    SplitBy, Transform, ValueParsing,
};

/// Cut `v` into exactly `n` parts, or `None` when it yields fewer.
///
/// Total by construction for a delimiter: `splitn(n, ..)` can return at most
/// `n` pieces, so the only failure is *too few*. `Positions` never fails —
/// a fixed layout with a short line has empty trailing fields, exactly as
/// `fixed_width` reads it — and a `Regex` fails when it does not match.
fn split_value(v: &str, by: &SplitBy, n: usize, re: Option<&regex::Regex>) -> Option<Vec<String>> {
    let parts = split_partial(v, by, n, re)?;
    (parts.len() == n).then_some(parts)
}

/// The same cut, returning however many parts it managed. Used by
/// `on_short = "null"` so the head of a short value survives into the
/// leading columns instead of the whole row going null.
fn split_partial(
    v: &str,
    by: &SplitBy,
    n: usize,
    re: Option<&regex::Regex>,
) -> Option<Vec<String>> {
    match by {
        SplitBy::Delimiter { value } => {
            Some(v.splitn(n, value.as_str()).map(|s| s.to_string()).collect())
        }
        SplitBy::Positions { at } => {
            // Character offsets, not bytes: see the type's doc comment.
            let chars: Vec<char> = v.chars().collect();
            let mut out = Vec::with_capacity(n);
            let mut start = 0usize;
            for cut in at {
                let end = (*cut as usize).min(chars.len()).max(start);
                out.push(chars[start..end].iter().collect::<String>().trim().to_string());
                start = end;
            }
            out.push(chars[start.min(chars.len())..].iter().collect::<String>().trim().to_string());
            Some(out)
        }
        SplitBy::Regex { pattern: _ } => {
            let caps = re?.captures(v)?;
            Some((1..caps.len()).map(|i| caps.get(i).map_or(String::new(), |m| m.as_str().to_string())).collect())
        }
    }
}

/// Follow an RFC 6901 pointer into one JSON value.
///
/// An unresolvable pointer is missing, not an error: a key absent from some
/// records is the ordinary shape of a JSON export, and `Extraction::Json`
/// already takes the union of every record's keys for the same reason. What
/// *is* an error is a pointer that lands on an object or an array, because the
/// column would quietly go back to holding JSON text — the state a pointer is
/// declared to get out of.
fn json_pointer_value(raw: &str, ptr: &str, row: usize) -> Result<String> {
    let t = raw.trim();
    if t.is_empty() {
        return Ok(String::new());
    }
    let v: serde_json::Value = serde_json::from_str(t)
        .map_err(|e| anyhow!("row {row}: `pointer` needs a JSON value here, and {t:?} is not one: {e}"))?;
    let found = if ptr.is_empty() { Some(&v) } else { v.pointer(ptr) };
    match found {
        None | Some(serde_json::Value::Null) => Ok(String::new()),
        Some(serde_json::Value::String(s)) => Ok(s.clone()),
        Some(serde_json::Value::Bool(b)) => Ok(b.to_string()),
        Some(serde_json::Value::Number(n)) => Ok(n.to_string()),
        Some(other) => bail!(
            "row {row}: `pointer` {ptr:?} lands on {} — a column cannot hold one, and \
             leaving it as JSON text is the state a pointer exists to leave. Point at a \
             value inside it",
            if other.is_array() { "an array" } else { "an object" }
        ),
    }
}

/// An integer count since 1970, in the declared unit, as microseconds.
///
/// Refuses anything that is not an integer rather than reaching for a float:
/// an epoch is a count, and `1.7e9` in a timestamp column is a value somebody
/// should look at, not one to round.
fn epoch_micros(v: &str, unit: EpochUnit) -> Result<i64> {
    let n: i64 = v
        .trim()
        .trim_start_matches('+')
        .parse()
        .map_err(|_| anyhow!("{v:?} is not a whole number of {unit:?} since 1970"))?;
    let scale: i64 = match unit {
        EpochUnit::Seconds => 1_000_000,
        EpochUnit::Milliseconds => 1_000,
        EpochUnit::Microseconds => 1,
    };
    n.checked_mul(scale)
        .ok_or_else(|| anyhow!("{v:?} in {unit:?} is further from 1970 than a timestamp reaches"))
}

/// Which negative-number marker a value carries, if any — the shape only, not
/// a claim that it means minus. Used to notice that a `strip` regex ate one.
fn sign_marker(v: &str) -> Option<&'static str> {
    let t = v.trim();
    if t.len() > 2 && t.starts_with('(') && t.ends_with(')') {
        Some("parentheses")
    } else if t.len() > 1 && t.ends_with('-') {
        Some("trailing_minus")
    } else {
        None
    }
}

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
    /// Where these rows came from. Set by `extract`, which is the only place
    /// that knows, and read by `source_name` — the transform that turns a fact
    /// about the file's location into a column of data. It sits on the table
    /// for the same reason `col_offset` does: it is a property of this
    /// extraction, and passing it separately would be a second thing that
    /// could disagree with the rows.
    pub source: SourceRef,
}

/// The file (and sheet) a `RawTable` was read from.
#[derive(Debug, Clone, Default)]
pub struct SourceRef {
    pub path: Option<std::path::PathBuf>,
    pub sheet: Option<String>,
    /// The 1-based ordinal of the stacked block this table was read from,
    /// when the extraction was narrowed to one region of a file or sheet.
    pub region: Option<u32>,
}

impl RawTable {
    fn new(rows: Vec<Vec<String>>, ragged: RaggedPolicy, truncated: bool) -> Self {
        RawTable {
            header: None,
            header_origin: None,
            rows,
            ragged,
            truncated,
            col_offset: 0,
            source: SourceRef::default(),
        }
    }

    fn with_header(header: Vec<String>, rows: Vec<Vec<String>>, truncated: bool) -> Self {
        RawTable {
            header_origin: Some(header.clone()),
            header: Some(header),
            rows,
            ragged: RaggedPolicy::PadNulls,
            truncated,
            col_offset: 0,
            source: SourceRef::default(),
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
        // When both the wanted name and every name the table has are the
        // generated `col_N`, listing them says nothing: the reader is looking
        // at a nameless table that came out narrower than the spec expects,
        // and the useful fact is *why* two reads of one file disagreed about
        // its width. They disagree when parse state crosses a boundary — an
        // unbalanced quote is the usual one — because the spec's columns come
        // from a sample and this table came from the file.
        let generated = |n: &str| {
            n.strip_prefix("col_").is_some_and(|d| !d.is_empty() && d.bytes().all(|b| b.is_ascii_digit()))
        };
        if generated(name) && !header.is_empty() && header.iter().all(|h| generated(h)) {
            return anyhow!(
                "the spec names `{name}`, but this file's rows yield {} column(s). Two reads \
                 of the file disagreed about its width, which happens when the declared \
                 `quote` is not the character the file actually quotes with: a partial read \
                 then splits rows differently from a whole one. Check `quote` in the sidecar \
                 against the file",
                header.len()
            );
        }
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
    let mut table = match extraction {
        Extraction::Delimited {
            delimiter,
            quote,
            escape,
            encoding,
            comment,
            ragged,
            region,
        } => extract_delimited(
            path,
            *delimiter,
            *quote,
            *escape,
            encoding.as_deref(),
            *comment,
            *ragged,
            *region,
            opts,
        ),
        Extraction::Excel { sheet_name, sheet_index, range, .. } => {
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
    table.source = SourceRef {
        path: Some(path.to_path_buf()),
        sheet: match extraction {
            Extraction::Excel { sheet_name, .. } => sheet_name.clone(),
            _ => None,
        },
        region: match extraction {
            Extraction::Delimited { region, .. } => region.map(|w| w.ordinal),
            Extraction::Excel { region_ordinal, .. } => *region_ordinal,
            _ => None,
        },
    };
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
    Ok(read_text_ex(path, encoding, opts)?.0)
}

/// As [`read_text`], but also says whether the returned text is the *whole*
/// file (`true`) or a capped prefix a `max_rows` read stopped short of the
/// real end (`false`). A caller that needs to tell "there is no such row"
/// from "the sample never got that far" — a `region` past what came back —
/// needs this; nothing else does, which is why `read_text` still exists as
/// the plain form everyone else calls.
pub(crate) fn read_text_ex(
    path: &Path,
    encoding: Option<&str>,
    opts: &ExtractOpts,
) -> Result<(String, bool)> {
    if opts.max_rows.is_none() {
        let bytes = fileio::read_all(path, opts.limits.max_file_bytes)?;
        let (text, used, had_errors) = crate::sample::decode_owned(bytes, encoding);
        warn_mojibake(path, encoding, &used, had_errors);
        return Ok((text, true));
    }
    let ht = fileio::read_head_tail(path, PREVIEW_BYTES, 0, opts.limits.max_decompressed_bytes)?;
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
    Ok((text, !truncated))
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
    region: Option<RowWindow>,
    opts: &ExtractOpts,
) -> Result<RawTable> {
    // validate() guarantees these are ASCII, so the byte casts are lossless.
    // `complete` says whether `text` is the whole file or a `max_rows`
    // read's capped prefix — a `region` past what came back means two very
    // different things depending on which.
    let (text, complete) = read_text_ex(path, encoding, opts)?;
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
    // A record's index is the line its first byte is on: `region` is a
    // window over the file's raw physical lines, not over the records
    // `read_record` yields. A blank line consumes an index although
    // `read_record` silently discards it and never yields it as a record,
    // so it is recovered from the physical-line delta the CSV core tracks:
    // a record whose line count advanced by more than its own terminator
    // (minus any newlines the record's own quoted fields carry, which
    // advance the line count without being a row boundary) had that many
    // blank lines ahead of it, each its own raw row.
    let mut raw_index: u64 = 0;
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
        let lines_before = region.map(|_| rdr.position().line());
        match rdr.read_record(&mut record) {
            Ok(true) => {
                if let Some(w) = region {
                    let advanced = rdr.position().line() - lines_before.unwrap();
                    let embedded: u64 =
                        record.iter().map(|f| f.matches('\n').count() as u64).sum();
                    raw_index += advanced.saturating_sub(1 + embedded);
                    let idx = raw_index;
                    // The index space is *physical* lines, which is what
                    // `regions_of` counted when it named the block, so a
                    // record spanning several lines advances by all of
                    // them. Subtracting `embedded` here as well (which the
                    // first cut did) made a quoted newline in one block
                    // shift every later block's window up by one and eat
                    // its header.
                    raw_index += 1 + embedded;
                    if idx < w.start {
                        continue;
                    }
                    if idx >= w.end {
                        break;
                    }
                }
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
    if let Some(w) = region {
        if raw_index <= w.start {
            if complete {
                bail!(
                    "region rows {}..{} start past the end of {} ({} rows)",
                    w.start,
                    w.end,
                    path.display(),
                    raw_index
                );
            }
            // `text` was only a `max_rows` read's capped prefix, and the
            // window's own rows never showed up in it — that says nothing
            // about whether the file, read whole, would have them. A
            // preview of a block beyond the sample is empty, not wrong.
            truncated = true;
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
    let mut wb = open_workbook(path, &opts.limits)?;
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
            Transform::FillDown { columns, direction } => {
                table.ensure_header()?;
                let index = table.header_index()?;
                let resolved: Vec<usize> = columns
                    .iter()
                    .map(|c| index.get(c.as_str()).copied().ok_or_else(|| table.missing_column(c)))
                    .collect::<Result<_>>()?;
                for idx in resolved {
                    let mut last = String::new();
                    // One loop, two directions: filling up is filling down
                    // over the reversed table, and writing it that way keeps
                    // the carry rule in exactly one place.
                    let rows: Box<dyn Iterator<Item = &mut Vec<String>>> = match direction {
                        FillDirection::Down => Box::new(table.rows.iter_mut()),
                        FillDirection::Up => Box::new(table.rows.iter_mut().rev()),
                    };
                    for row in rows {
                        let Some(cell) = row.get_mut(idx) else { continue };
                        if cell.trim().is_empty() {
                            cell.clone_from(&last);
                        } else {
                            last.clone_from(cell);
                        }
                    }
                }
            }
            Transform::Transpose => {
                // A partial read has not seen every row, and every row it has
                // not seen is a *column* of the result — not a few missing
                // records but a table of the wrong shape. `skip_rows`'s tail
                // is skipped on a truncated table for the weaker version of
                // this reason; here it has to be refused outright.
                if table.truncated {
                    bail!(
                        "transpose needs the whole table: this one stopped early, and the \
                         rows it never read would each have been a column. Raise \
                         `[limits] max_cells`, or read a smaller range"
                    );
                }
                if table.header.is_some() {
                    bail!("internal: transpose ran after a header was established");
                }
                // Ragged input transposes to a rectangle: a row that stopped
                // short contributes an empty cell to each column beyond it,
                // which is what the missing value was.
                let width = table.rows.iter().map(|r| r.len()).max().unwrap_or(0);
                let mut flipped: Vec<Vec<String>> =
                    vec![Vec::with_capacity(table.rows.len()); width];
                for row in &table.rows {
                    for (c, out) in flipped.iter_mut().enumerate() {
                        out.push(row.get(c).cloned().unwrap_or_default());
                    }
                }
                table.rows = flipped;
            }
            Transform::SplitColumn { source, into, by, on_short } => {
                table.ensure_header()?;
                let idx = table.col_index(source)?;
                let n = into.len();
                let re = match by {
                    SplitBy::Regex { pattern } => Some(compile(pattern, "split_column")?),
                    _ => None,
                };

                // The header first, so a table with no rows still comes out
                // the right shape — which is what `schema_of` reads when it
                // builds every column over zero rows.
                if let Some(header) = table.header.as_mut() {
                    header.splice(idx..=idx, into.iter().cloned());
                }

                for (r, row) in table.rows.iter_mut().enumerate() {
                    let Some(cell) = row.get(idx) else { continue };
                    let parts = split_value(cell, by, n, re.as_ref());
                    let parts = match parts {
                        Some(p) => p,
                        None => match on_short {
                            ShortSplit::Null => {
                                let mut p = vec![String::new(); n];
                                // Whatever the value *did* yield stays in the
                                // leading parts: a missing tail is missing,
                                // and the head is not lost with it.
                                if let Some(got) = split_partial(cell, by, n, re.as_ref()) {
                                    for (i, v) in got.into_iter().enumerate() {
                                        p[i] = v;
                                    }
                                }
                                p
                            }
                            ShortSplit::Error => bail!(
                                "row {}: splitting `{source}` gave fewer than {n} parts \
                                 for {:?}; the value has no separator where the spec \
                                 expects one. Fix the split, or declare \
                                 `on_short = \"null\"` if the tail is optional",
                                r + 1,
                                cell
                            ),
                        },
                    };
                    row.splice(idx..=idx, parts);
                }
            }
            Transform::SourceName { name, from, pattern } => {
                table.ensure_header()?;
                let src = &table.source;
                let path = src
                    .path
                    .as_deref()
                    .ok_or_else(|| anyhow!("internal: source_name ran on a table with no path"))?;
                let part: String = match from {
                    SourcePart::FileStem => {
                        path.file_stem().unwrap_or_default().to_string_lossy().into_owned()
                    }
                    SourcePart::FileName => {
                        path.file_name().unwrap_or_default().to_string_lossy().into_owned()
                    }
                    SourcePart::Path => path.to_string_lossy().into_owned(),
                    SourcePart::Sheet => src.sheet.clone().ok_or_else(|| {
                        anyhow!(
                            "source_name `{name}`: `from = \"sheet\"` needs a workbook whose \
                             sheet the spec names; this extraction has none"
                        )
                    })?,
                    SourcePart::Region => src.region.map(|r| r.to_string()).ok_or_else(|| {
                        anyhow!(
                            "source_name `{name}`: `from = \"region\"` needs a spec that reads \
                             one block of a file; this extraction reads the whole file"
                        )
                    })?,
                };
                let value = match pattern {
                    None => part,
                    Some(p) => {
                        let re = compile(p, "source_name")?;
                        let caps = re.captures(&part).ok_or_else(|| {
                            anyhow!(
                                "source_name `{name}`: {p:?} does not match {part:?}. An empty \
                                 column on one member of a pile is the silent gap this \
                                 refuses — fix the pattern, or use `constant` if the value is \
                                 not in the path"
                            )
                        })?;
                        caps.get(1).or_else(|| caps.get(0)).map_or(String::new(), |m| {
                            m.as_str().to_string()
                        })
                    }
                };
                let header = table.header.as_mut().expect("ensure_header");
                if header.iter().any(|h| h == name) {
                    bail!(
                        "source_name `{name}`: the file already has a column by that name; \
                         a derived column may only add, never shadow"
                    );
                }
                header.push(name.clone());
                if let Some(origin) = table.header_origin.as_mut() {
                    origin.push(name.clone());
                }
                let width = table.header.as_ref().map_or(0, |h| h.len());
                for row in &mut table.rows {
                    row.resize(width.saturating_sub(1), String::new());
                    row.push(value.clone());
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

    // A `region` can put every row of interest outside a `max_rows`-capped
    // preview/dry-run sample: the table then has zero rows, and since a
    // Delimited extraction supplies no header of its own, there is no
    // header text to resolve a declared column against either — not
    // because anything is wrong, but because the sample never reached
    // that far. That combination cannot arise any other way (every other
    // extractor's `truncated` only ever fires after already keeping
    // `max_rows` rows, so it is never true with zero rows kept), so this is
    // new with `region`, not a relaxation of an existing check: a preview
    // that saw nothing has no evidence about columns and must say so with
    // an empty batch of the declared shape, not by claiming one is missing.
    if table.truncated && table.rows.is_empty() {
        let mut fields = Vec::with_capacity(spec.columns.len());
        let mut arrays: Vec<ArrayRef> = Vec::with_capacity(spec.columns.len());
        for col in &spec.columns {
            let (field, array) = build_column_at(col, &[], 0)
                .with_context(|| format!("building column `{}`", col.name))?;
            fields.push(field);
            arrays.push(array);
        }
        let schema = Arc::new(Schema::new(fields));
        return Ok(vec![RecordBatch::try_new(schema, arrays).context("assembling record batch")?]);
    }

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

    // A `strip` that deletes a sign marker turns -1234.50 into +1234.50, and
    // nothing downstream can tell: the column types, the dry run passes, the
    // sidecar fingerprints, and the number is wrong by twice itself. Watched
    // for here rather than refused in `validate`, because a strip that never
    // meets a parenthesis is perfectly fine and only the data knows.
    let sign_guard = p.negative.is_none()
        && strip_re.is_some()
        && matches!(col.dtype, DType::Int64 | DType::Float64 | DType::Decimal { .. });
    let mut ate_sign: Option<(usize, &'static str)> = None;

    let pointed_refs: Vec<&str>;
    // A declared pointer reads inside the value before anything else looks at
    // it, so the rest of the chain sees an ordinary scalar.
    let pointed: Vec<String>;
    let values: &[&str] = match &col.pointer {
        None => values,
        Some(ptr) => {
            pointed = values
                .iter()
                .enumerate()
                .map(|(i, raw)| json_pointer_value(raw, ptr, row_offset + i + 1))
                .collect::<Result<Vec<_>>>()?;
            pointed_refs = pointed.iter().map(|s| s.as_str()).collect();
            &pointed_refs
        }
    };

    // trim -> replace -> na -> strip. Borrowed until something actually
    // changes, so a clean column costs no allocations at all.
    let cleaned: Vec<Option<Cow<str>>> = values
        .iter()
        .enumerate()
        .map(|(i, raw)| {
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
                    let marker = if sign_guard { sign_marker(&v) } else { None };
                    let stripped = re.replace_all(&v, "").trim().to_string();
                    if ate_sign.is_none() && marker.is_some() && sign_marker(&stripped).is_none()
                    {
                        ate_sign = marker.map(|kind| (i, kind));
                    }
                    if stripped.is_empty() {
                        return None;
                    }
                    v = Cow::Owned(stripped);
                }
            }
            Some(v)
        })
        .collect();

    if let Some((i, kind)) = ate_sign {
        bail!(
            "row {}: `strip` removes the marker that makes {:?} negative, which would \
             silently read it as positive; write `negative = \"{}\"` to say what the \
             marker means and leave `strip` for the currency symbol",
            row_offset + i + 1,
            values[i].trim(),
            kind
        );
    }

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
        // The sign comes off first, so everything below sees an ordinary
        // unsigned number: grouping is checked on the digits, the separators
        // are swapped on the digits, and the decimal point is moved on the
        // digits. Re-attached at the end, which is sound because none of those
        // steps depends on the sign.
        let (neg, v) = match p.negative {
            Some(NegativeStyle::Parentheses) => match v
                .strip_prefix('(')
                .and_then(|inner| inner.strip_suffix(')'))
            {
                Some(inner) => (true, inner.trim()),
                None => (false, v),
            },
            Some(NegativeStyle::TrailingMinus) => match v.strip_suffix('-') {
                Some(inner) => (true, inner.trim_end()),
                None => (false, v),
            },
            None => (false, v),
        };
        // Two ways of saying the sign at once is not a value with a known
        // reading: `(-5)` is minus five to one author and plus five to
        // another, and picking is guessing.
        if neg && (v.starts_with('-') || v.starts_with('+')) {
            bail!("carries both a sign and a negative marker; one of the two is a mistake");
        }
        // Undeclared, the marker would otherwise surface as "invalid digit
        // found in string", which is true and useless. The value is right
        // here and so is the fix.
        if let (false, Some(kind)) = (neg, sign_marker(v)) {
            bail!(
                "looks like an accounting negative; declare `negative = \"{kind}\"` on \
                 this column to read the marker as a sign"
            );
        }
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
        Ok(if neg { format!("-{s}") } else { s.into_owned() })
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
            let round = p.round.unwrap_or(crate::spec::Rounding::HalfAway);
            let out = parse_all!(i128, |s: &str| parse_decimal(&numeric(s)?, *precision, *scale, round));
            let arr = Decimal128Array::from(out)
                .with_precision_and_scale(*precision, *scale)
                .context("decimal precision/scale")?;
            (ArrowType::Decimal128(*precision, *scale), Arc::new(arr))
        }
        DType::Date { format } => {
            let out = match p.epoch {
                Some(unit) => parse_all!(i32, |s: &str| {
                    // Truncating toward the epoch, so 1970-01-01T23:59 is
                    // still 1970-01-01 and a negative instant lands on the day
                    // that contains it rather than the one after.
                    let micros = epoch_micros(s, unit)?;
                    Ok::<i32, anyhow::Error>(micros.div_euclid(86_400_000_000) as i32)
                }),
                None => parse_all!(i32, |s: &str| parse_date_days(s, format)),
            };
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
            let out = match p.epoch {
                // An epoch is a count, not a rendering: it has no format to
                // parse and no timezone to place it in — it is already UTC.
                Some(unit) => parse_all!(i64, |s: &str| epoch_micros(s, unit)),
                None => parse_all!(i64, |s: &str| parse_timestamp_micros(s, format, offset)),
            };
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

/// Exact decimal parse to a scaled i128 mantissa. A value with more
/// fractional digits than `scale` is rounded half away from zero or refused,
/// as `round` says; trailing zeros are never excess, since dropping them
/// changes nothing.
fn parse_decimal(s: &str, precision: u8, scale: i8, round: crate::spec::Rounding) -> Result<i128> {
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
        if round == crate::spec::Rounding::Error && frac[scale_u..].bytes().any(|b| b != b'0') {
            bail!(
                "{s:?} has {} fractional digits, more than the declared scale {scale}; rounding is \
                 a value change, so it has to be declared — `round = \"half_away\"` in the \
                 sidecar, or OPTIONS(round = 'half_away') on the target column",
                frac_part.len()
            );
        }
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

/// Every workbook open goes through here: a compressed workbook is
/// materialised first, then `xlguard::preflight` bounds the container before
/// calamine allocates anything, then it is opened.
pub(crate) fn open_workbook(path: &Path, limits: &Limits) -> Result<Sheets<std::io::BufReader<std::fs::File>>> {
    let real = crate::fileio::materialize(path, limits.max_decompressed_bytes)?;
    let real = real.as_ref();
    crate::xlguard::preflight(real, limits)?;
    open_workbook_auto(real).with_context(|| format!("cannot open workbook {}", path.display()))
}

/// One open of a workbook, reporting the shape of every sheet.
pub fn excel_sheet_shapes(path: &Path, limits: Limits) -> Result<Vec<SheetShape>> {
    let mut wb = open_workbook(path, &limits)?;
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
    let mut wb = open_workbook(path, &limits)?;
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

/// One run of non-blank rows the split refused as a block: shorter than the
/// 3-row minimum, and therefore inside no member's window. Nothing reads
/// these lines once a window is applied, so they are reported — a run
/// dropped in silence turns a loud refusal into a quiet partial read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DroppedRun {
    /// 0-based, half-open, in the same index space as [`RowWindow`].
    pub start: u64,
    pub end: u64,
    /// Fields on its first row, counted the way the kept blocks' own first
    /// rows were counted (one shared delimiter for a text file, non-empty
    /// cells for a sheet). Equal counts is what makes a dropped run look
    /// like a table rather than a banner.
    pub width: usize,
}

/// What [`regions_of`] found: the blocks it kept, and the runs it did not.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Regions {
    /// The blocks, in file order, 1-based ordinals. Empty = no regions.
    pub windows: Vec<RowWindow>,
    /// Runs below the 3-row minimum, in file order. Always empty when
    /// `windows` is: with no window there is nothing a member fails to
    /// read, because the file is read whole.
    pub dropped: Vec<DroppedRun>,
    /// The first kept block's first-row width — what a `dropped` run's own
    /// width is compared against.
    pub block_width: usize,
}

impl Regions {
    /// The dropped runs shaped like the blocks that were kept: the same
    /// field count on their first row. A two-line banner over a three-field
    /// table is one field wide and is not one of these; a `Total;;1500`
    /// footer is three fields wide and is, which is correct — the 3-row
    /// minimum says a total line is not a *table*, and a person rules on
    /// whether those rows were data.
    pub fn table_shaped(&self) -> impl Iterator<Item = &DroppedRun> {
        let w = self.block_width;
        self.dropped.iter().filter(move |d| w > 0 && d.width == w)
    }
}

/// The stacked blocks of a text file (or of one sheet of a workbook), split
/// at runs of blank rows, in file order.
///
/// A block needs at least 3 rows to count as a region — a one- or two-line
/// title/date banner above a table is not itself a stacked table, which is
/// exactly the case `regions_titled.csv` exercises. A single block spanning
/// the whole file (or sheet) is not a region either: there is nothing to
/// choose between, so this returns an empty `Vec` the same way it would for
/// a file with no blank line in it at all — `compressed_plain.csv` is that
/// control fixture. "Spans the whole file" means exactly one non-blank run
/// exists, full stop — a leading or trailing blank line is padding, not a
/// second region, so it does not make an otherwise-single table look like
/// two blocks either.
///
/// For a text file (`sheet: None`) this streams the file and splits on raw
/// *lines*: a line is blank when it is empty after trimming, and its index
/// is the raw physical line number — the same counting `RowWindow`
/// documents for `region` on `src/spec.rs`, and the same counting both
/// executors do, a record's quoted newlines included.
///
/// For a workbook (`sheet: Some(name)`) this reads the named sheet's used
/// range instead (`open_workbook` + `checked_worksheet_range`) and splits
/// its rows: a row is blank when every cell in it renders as empty text
/// (`sample::render_cell`). The windows returned are row indices of that
/// used range, in the same half-open form.
pub fn regions_of(path: &Path, sheet: Option<&str>, limits: Limits) -> Result<Regions> {
    match sheet {
        Some(name) => {
            // A sheet is materialised by calamine regardless, so there is
            // nothing to stream here — the whole grid already exists.
            let mut wb = open_workbook(path, &limits)?;
            let range = checked_worksheet_range(&mut wb, name, &limits)?;
            let blanks: Vec<bool> = range
                .rows()
                .map(|row| row.iter().all(|c| render_cell(c).trim().is_empty()))
                .collect();
            // A sheet row's width is its count of non-empty cells: the
            // grid is rectangular, so its own arity says nothing about
            // whether a row is a table's header or a one-line banner.
            let widths: Vec<usize> = range
                .rows()
                .map(|row| row.iter().filter(|c| !render_cell(c).trim().is_empty()).count())
                .collect();
            Ok(blocks_from(&blanks, |row| widths.get(row as usize).copied().unwrap_or(0)))
        }
        // A text file is read line by line instead: `expand_units` calls
        // this on every plain member on every fit (including the
        // sidecar-reuse fast path), so materialising the whole file into a
        // `String` here — the old `read_text` call — turned an O(1)
        // sidecar-reuse check into an O(file) one. `regions_of_lines` keeps
        // only the run boundaries found so far, so memory here is O(runs),
        // not O(file).
        None => regions_of_lines(path, limits),
    }
}

/// [`regions_of`]'s text-file path: streams raw lines through a `BufReader`
/// over the materialised file (the same opener the streaming executor
/// uses), tracking only the current run and the runs already closed —
/// never the file's lines themselves. A line's index is its raw line
/// number, exactly as [`read_text`]'s whole-file split counts it: the final
/// line contributes no phantom trailing entry when the file ends in `\n`,
/// because `read_until` simply returns nothing on the next call.
///
/// Blankness does not need decoding: a line is blank when every byte in it,
/// after the line terminator is stripped, is ASCII whitespace (which is
/// exactly what trimming a `\r` and ordinary spaces/tabs means) — a
/// non-UTF-8 byte elsewhere in the line makes it non-blank, which is the
/// only thing that question needs to get right.
fn regions_of_lines(path: &Path, limits: Limits) -> Result<Regions> {
    let real = fileio::materialize(path, limits.max_decompressed_bytes)?;
    let file = std::fs::File::open(real.as_ref())
        .with_context(|| format!("cannot open {}", path.display()))?;
    let mut reader = std::io::BufReader::new(file);
    let mut runs: Vec<(u64, u64)> = Vec::new();
    // Each run's own first line, kept so a run's width can be counted once
    // the delimiter is known — which is only after the split has said which
    // runs are blocks. O(runs), not O(file).
    let mut heads: Vec<String> = Vec::new();
    let mut run_start: Option<u64> = None;
    let mut index: u64 = 0;
    let mut buf: Vec<u8> = Vec::new();
    loop {
        buf.clear();
        let n = std::io::BufRead::read_until(&mut reader, b'\n', &mut buf)
            .with_context(|| format!("reading {}", path.display()))?;
        if n == 0 {
            break;
        }
        let mut line = buf.as_slice();
        if line.last() == Some(&b'\n') {
            line = &line[..line.len() - 1];
        }
        if line.last() == Some(&b'\r') {
            line = &line[..line.len() - 1];
        }
        let blank = line.iter().all(|b| b.is_ascii_whitespace());
        match (blank, run_start) {
            (false, None) => {
                run_start = Some(index);
                heads.push(String::from_utf8_lossy(line).into_owned());
            }
            (true, Some(s)) => {
                runs.push((s, index));
                run_start = None;
            }
            _ => {}
        }
        index += 1;
    }
    if let Some(s) = run_start {
        runs.push((s, index));
    }
    // The delimiter is the one the blocks that survived the minimum are best
    // read with — never one guessed from the dropped runs themselves, or a
    // banner would get to choose how it is counted.
    let kept: Vec<&str> = runs
        .iter()
        .zip(&heads)
        .filter(|((s, e), _)| e - s >= 3)
        .map(|(_, h)| h.as_str())
        .collect();
    let delim = pick_block_delimiter(&kept);
    let measured: Vec<(u64, u64, usize)> = runs
        .iter()
        .zip(&heads)
        .map(|(&(s, e), h)| (s, e, field_count(h, delim)))
        .collect();
    Ok(windows_from_runs(measured))
}

/// The delimiter a set of block headers is best read with: whichever
/// candidate gives the most fields on most of them. Only the *relative*
/// answer matters — the same delimiter counts the kept blocks and the
/// dropped runs, so a comparison of the two is meaningful whatever it
/// picks; guessing per run is what would let a one-line banner claim a
/// block's shape.
fn pick_block_delimiter(heads: &[&str]) -> char {
    let mut best = (0usize, ',');
    for cand in [',', ';', '\t', '|'] {
        let mut counts: Vec<usize> = heads.iter().map(|h| field_count(h, cand)).collect();
        counts.sort_unstable();
        let modal = counts.get(counts.len() / 2).copied().unwrap_or(0);
        if modal > best.0 {
            best = (modal, cand);
        }
    }
    best.1
}

/// Fields on one line under one delimiter, quoting honoured (a `;` inside
/// `"Ost;Nord"` is not a boundary).
fn field_count(line: &str, delim: char) -> usize {
    if !delim.is_ascii() {
        return 1;
    }
    let mut rdr = csv::ReaderBuilder::new()
        .has_headers(false)
        .flexible(true)
        .delimiter(delim as u8)
        .from_reader(line.as_bytes());
    rdr.records().next().and_then(|r| r.ok()).map(|r| r.len()).unwrap_or(1)
}

/// Maximal runs of `false` (non-blank) in `blanks`, as half-open
/// `(start, end)` row ranges, in order.
fn raw_runs(blanks: &[bool]) -> Vec<(usize, usize)> {
    let mut runs = Vec::new();
    let mut start: Option<usize> = None;
    for (i, blank) in blanks.iter().enumerate() {
        match (*blank, start) {
            (false, None) => start = Some(i),
            (true, Some(s)) => {
                runs.push((s, i));
                start = None;
            }
            _ => {}
        }
    }
    if let Some(s) = start {
        runs.push((s, blanks.len()));
    }
    runs
}

/// The runs from [`raw_runs`], measured by `width_of` (the first row's
/// field count) and handed to [`windows_from_runs`].
fn blocks_from(blanks: &[bool], width_of: impl Fn(u64) -> usize) -> Regions {
    let runs = raw_runs(blanks)
        .into_iter()
        .map(|(s, e)| (s as u64, e as u64, width_of(s as u64)))
        .collect();
    windows_from_runs(runs)
}

/// The filtering rule, shared by the text path (which finds its runs by
/// streaming rather than by materialising a `blanks` vector first) and the
/// sheet path, so the two cannot drift apart on what counts as a region —
/// nor on what was thrown away to get there.
///
/// A run is kept only when at least 3 rows long — unless exactly one run
/// exists at all, in which case there is nothing to split and the answer is
/// "no regions", whatever the run's own length or position. "One block
/// spanning the whole file" means exactly one non-blank run exists, not
/// that the run happens to start at row 0 and end at the last row: a
/// leading or trailing blank line is padding, not a second region, and must
/// not turn a single table into one spurious window by making its one run
/// fall short of the file's own start or end.
///
/// Every run below the minimum comes back as a [`DroppedRun`]. Those lines
/// are inside no window, so with a window applied nothing reads them — and
/// a member that quietly answers for part of a file is the wrong value this
/// tool refuses. When no window is kept there is nothing dropped either:
/// the file is then read whole, so every line is read.
fn windows_from_runs(runs: Vec<(u64, u64, usize)>) -> Regions {
    if runs.len() == 1 {
        return Regions::default();
    }
    let (kept, short): (Vec<_>, Vec<_>) = runs.into_iter().partition(|(s, e, _)| e - s >= 3);
    if kept.is_empty() {
        return Regions::default();
    }
    Regions {
        block_width: kept[0].2,
        windows: kept
            .into_iter()
            .enumerate()
            .map(|(i, (s, e, _))| RowWindow { start: s, end: e, ordinal: (i + 1) as u32 })
            .collect(),
        dropped: short
            .into_iter()
            .map(|(start, end, width)| DroppedRun { start, end, width })
            .collect(),
    }
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
        use crate::spec::Rounding::{Error, HalfAway};
        assert_eq!(parse_decimal("1.005", 12, 2, HalfAway).unwrap(), 101);
        assert_eq!(parse_decimal("-1.005", 12, 2, HalfAway).unwrap(), -101);
        assert_eq!(parse_decimal("2.344", 12, 2, HalfAway).unwrap(), 234);
        assert_eq!(parse_decimal("1200.50", 12, 2, HalfAway).unwrap(), 120050);
        assert_eq!(parse_decimal("0", 12, 2, HalfAway).unwrap(), 0);
        assert!(parse_decimal("12345.67", 5, 2, HalfAway).is_err());
        assert!(parse_decimal("abc", 12, 2, HalfAway).is_err());
        assert!(parse_decimal("", 12, 2, HalfAway).is_err());
        // Under `error`, excess digits are refused — and trailing zeros are
        // not excess, since dropping them changes nothing.
        let e = parse_decimal("1.005", 12, 2, Error).unwrap_err();
        assert!(format!("{e}").contains("3 fractional digits"), "{e}");
        assert_eq!(parse_decimal("1.200", 12, 2, Error).unwrap(), 120);
        assert_eq!(parse_decimal("1.2", 12, 2, Error).unwrap(), 120);
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
            pointer: None,
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
            pointer: None,
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
