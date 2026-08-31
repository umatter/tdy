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
use std::io::BufRead;
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

/// Cells per output batch, the bound that actually keeps a batch small.
///
/// `BATCH_ROWS` alone does not: a row is as wide as the file, so 65,536 rows
/// of a 1,000-column file is 65 million strings. At roughly 40 bytes a cell
/// this is about 40 MB of raw text in flight, and for any file up to 16
/// columns it works out wider than `BATCH_ROWS`, so the usual case keeps
/// exactly the batches it had.
const BATCH_CELLS: usize = 1 << 20;
use crate::spec::{
    ColumnSpec, Extraction, FixedField, NoMatchPolicy, ParseSpec, RaggedPolicy, Transform,
    ValueParsing,
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
        // NDJSON records are independent lines, so they stream; a JSON
        // *array* is one document and has to be parsed whole before any
        // record exists. A pointer only applies to the array form.
        Extraction::Json { lines: true, pointer: None } => true,
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


/// Where a streaming source gets its bytes.
///
/// A UTF-8 file is read straight off disk through a buffer, which is what
/// keeps memory independent of its size. Anything else is decoded whole
/// first, because deciding the encoding correctly needs the whole file — the
/// `enc_late_1252_byte.csv` fixture is ASCII for 12 KB and then is not, and a
/// UTF-16 file is only recognisable from its BOM and byte pattern. Those are
/// legacy exports and small in practice; correctness is worth more there than
/// a constant factor of memory.
/// Below this, the extra read that [`streamable_as_utf8`] costs is not worth
/// the memory it would save, so an undeclared encoding just decodes whole.
const VALIDATE_ABOVE_BYTES: u64 = 8 * 1024 * 1024;

/// Whether the file can be read as UTF-8 incrementally, deciding it exactly
/// as the whole-file decoder would.
///
/// `sample::decode_owned(bytes, None)` treats a file as UTF-8 when it is not
/// UTF-16 and is valid UTF-8 throughout, and detects an encoding otherwise.
/// That judgement needs the whole file — a sniffed spec leaves `encoding`
/// unset precisely because an ASCII-only *sample* proves nothing about the
/// rest, which is what `enc_late_1252_byte.csv` exists to demonstrate. But it
/// does not need the whole file *in memory*: this reads it in chunks and
/// keeps only an incomplete trailing sequence, so it costs one pass and a
/// fixed buffer.
fn streamable_as_utf8(path: &Path) -> Result<bool> {
    use std::io::Read;
    const CHUNK: usize = 256 * 1024;
    let mut f = std::fs::File::open(path)
        .with_context(|| format!("cannot read {}", path.display()))?;
    let mut buf = vec![0u8; CHUNK];
    let mut pending: Vec<u8> = Vec::with_capacity(CHUNK + 8);
    let mut first = true;
    loop {
        let n = f.read(&mut buf).with_context(|| format!("reading {}", path.display()))?;
        if n == 0 {
            break;
        }
        if first {
            first = false;
            if crate::sample::utf16_flavour(&buf[..n]).is_some() {
                return Ok(false);
            }
        }
        pending.extend_from_slice(&buf[..n]);
        match std::str::from_utf8(&pending) {
            Ok(_) => pending.clear(),
            Err(e) => {
                // A sequence merely cut off by the chunk boundary is fine; a
                // malformed one is not.
                if e.error_len().is_some() {
                    return Ok(false);
                }
                let keep = e.valid_up_to();
                pending.drain(..keep);
            }
        }
    }
    // Anything left is a sequence the file ended in the middle of.
    Ok(pending.is_empty())
}

fn open_input(
    path: &Path,
    encoding: Option<&str>,
    opts: &ExtractOpts,
) -> Result<Box<dyn BufRead + Send>> {
    let utf8 = match encoding {
        Some(l) => encoding_rs::Encoding::for_label(l.as_bytes())
            .map(|e| e == encoding_rs::UTF_8)
            .unwrap_or(false),
        // Unset means "decide from the whole file". Worth a validating pass
        // only when the file is big enough for the saving to matter.
        None => {
            std::fs::metadata(path).map(|m| m.len()).unwrap_or(0) >= VALIDATE_ABOVE_BYTES
                && streamable_as_utf8(path)?
        }
    };
    if !utf8 {
        let text = crate::engine::read_text(path, encoding, opts)?;
        return Ok(Box::new(std::io::Cursor::new(text.into_bytes())));
    }

    let meta = std::fs::metadata(path)
        .with_context(|| format!("cannot stat {}", path.display()))?;
    if meta.len() > opts.limits.max_file_bytes {
        bail!(
            "{} is {:.1} GB, above the {:.1} GB limit \
             (raise [limits].max_file_bytes in the config if you really mean it)",
            path.display(),
            meta.len() as f64 / 1e9,
            opts.limits.max_file_bytes as f64 / 1e9
        );
    }
    let f = std::fs::File::open(path)
        .with_context(|| format!("cannot read {}", path.display()))?;
    let mut r = std::io::BufReader::with_capacity(256 * 1024, f);
    // A BOM is not data. The whole-file decoder strips it; so must this, or
    // the first column comes back named "\u{feff}region".
    {
        let head = r.fill_buf().context("reading the start of the file")?;
        if head.starts_with(&[0xEF, 0xBB, 0xBF]) {
            r.consume(3);
        }
    }
    Ok(Box::new(r))
}

/// Bytes to text the way the whole-file decoder does it: invalid sequences
/// become U+FFFD rather than an error, so a file with one bad byte still
/// parses and the damage is visible in the value instead of stopping the run.
///
/// Borrows when the bytes are already valid UTF-8, which is almost always, so
/// the common path costs no allocation.
fn text_of(bytes: &[u8]) -> std::borrow::Cow<'_, str> {
    String::from_utf8_lossy(bytes)
}

/// One row at a time out of the input, per extraction format.
///
/// The formats that stream are the ones whose rows are independent: a
/// delimited record, a log line, a slice of a fixed-width line. JSON is not
/// here because a document has to be parsed whole before its records exist.
enum Source {
    Delimited { rdr: csv::Reader<Box<dyn BufRead + Send>>, rec: csv::ByteRecord },
    Lines {
        rdr: Box<dyn BufRead + Send>,
        buf: Vec<u8>,
        re: regex::Regex,
        names: Vec<String>,
        on_no_match: NoMatchPolicy,
        line_no: usize,
    },
    Fixed { rdr: Box<dyn BufRead + Send>, buf: Vec<u8>, fields: Vec<FixedField>, chars: Vec<char> },
    /// NDJSON. `header` comes from [`discover_ndjson`]; a record's values are
    /// emitted in that order, with a missing key an empty string, exactly as
    /// the materialising path does it.
    Ndjson { rdr: Box<dyn BufRead + Send>, buf: Vec<u8>, header: Vec<String>, line_no: usize },
}

/// Read one line, without its terminator. `Ok(false)` at end of input.
///
/// Mirrors `str::lines`: a trailing "\r\n" and a trailing "\n" both end a
/// line and neither survives into the data.
fn read_line(rdr: &mut dyn BufRead, buf: &mut Vec<u8>) -> Result<bool> {
    buf.clear();
    let n = rdr.read_until(b'\n', buf).context("reading a line")?;
    if n == 0 {
        return Ok(false);
    }
    if buf.last() == Some(&b'\n') {
        buf.pop();
        if buf.last() == Some(&b'\r') {
            buf.pop();
        }
    }
    Ok(true)
}

/// The column names an extraction supplies by itself, if any.
///
/// Pure, and separate from opening the file on purpose: the driver needs to
/// know whether a width has to be discovered *before* it decides how many
/// sources to open, and opening one to find out held a second decoded copy of
/// the file alive through the counting pass.
fn header_of(extraction: &Extraction) -> Result<Option<Vec<String>>> {
    Ok(match extraction {
        Extraction::Delimited { .. } => None,
        Extraction::Lines { pattern, .. } => {
            let re = compile(pattern, "lines pattern")?;
            let names: Vec<String> =
                re.capture_names().flatten().map(|s| s.to_string()).collect();
            if names.is_empty() {
                bail!("lines pattern must contain named capture groups, e.g. (?P<ip>\\S+)");
            }
            Some(names)
        }
        Extraction::FixedWidth { fields, .. } => {
            Some(fields.iter().map(|f| f.name.clone()).collect())
        }
        // Discovered by a pass over the file, not derivable from the spec.
        Extraction::Json { .. } => None,
        other => bail!("internal: {} is not a streaming source", other.format_name()),
    })
}

/// The header of an NDJSON file, and how many records it has.
///
/// Mirrors `engine::extract_json`: the union of every record's keys in
/// first-seen order, and a file that mixes objects with scalars is refused
/// rather than flattened, because there is no single tabular reading of one.
/// Finding that out needs a pass over the file — the union is not knowable
/// from a prefix, and guessing it from one would silently drop a column that
/// only appears late.
fn discover_ndjson(path: &Path, opts: &ExtractOpts) -> Result<(Vec<String>, usize)> {
    let mut rdr = open_input(path, None, opts)?;
    let mut buf = Vec::with_capacity(4096);
    let mut header: Vec<String> = Vec::new();
    let mut seen = std::collections::HashSet::new();
    let mut records = 0usize;
    let mut objects = 0usize;
    let mut line_no = 0usize;

    while read_line(rdr.as_mut(), &mut buf)? {
        line_no += 1;
        let line = text_of(&buf);
        if line.trim().is_empty() {
            continue;
        }
        let parsed: serde_json::Value = match serde_json::from_str(line.as_ref()) {
            Ok(v) => v,
            Err(e) => {
                // "Truncated last line" is a different diagnosis from "broken
                // record", and only knowable once nothing follows it.
                let last = !any_more_content(rdr.as_mut(), &mut buf)?;
                if last && e.is_eof() {
                    bail!(
                        "line {} is a truncated JSON record — the file looks like it was \
                         cut mid-write. Complete or remove the last line; tdy will not \
                         silently drop a partial record.",
                        line_no
                    );
                }
                bail!("invalid JSON on line {}: {e}", line_no);
            }
        };
        records += 1;
        if let serde_json::Value::Object(map) = &parsed {
            objects += 1;
            for k in map.keys() {
                if seen.insert(k.clone()) {
                    header.push(k.clone());
                }
            }
        }
    }

    if objects > 0 && objects != records {
        bail!(
            "{} of {} JSON records are not objects; a records array must be all \
             objects (or all scalars)",
            records - objects,
            records
        );
    }
    if objects == 0 {
        header = vec!["value".to_string()];
    }
    Ok((header, records))
}

/// Whether any further non-blank line exists. Consumes the rest of the input,
/// so only call it on the way to an error.
fn any_more_content(rdr: &mut dyn BufRead, buf: &mut Vec<u8>) -> Result<bool> {
    while read_line(rdr, buf)? {
        if !text_of(buf).trim().is_empty() {
            return Ok(true);
        }
    }
    Ok(false)
}

impl Source {
    fn open(
        path: &Path,
        extraction: &Extraction,
        opts: &ExtractOpts,
        discovered: Option<&[String]>,
    ) -> Result<(Self, Option<Vec<String>>)> {
        let input = open_input(path, extraction.encoding(), opts)?;
        Ok(match extraction {
            Extraction::Delimited { .. } => (
                Source::Delimited {
                    rdr: reader_for(extraction).from_reader(input),
                    rec: csv::ByteRecord::new(),
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
                        rdr: input,
                        buf: Vec::with_capacity(1024),
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
                        rdr: input,
                        buf: Vec::with_capacity(1024),
                        fields: fields.clone(),
                        chars: Vec::new(),
                    },
                    Some(header),
                )
            }
            Extraction::Json { .. } => {
                let header = discovered
                    .map(|h| h.to_vec())
                    .ok_or_else(|| anyhow!("internal: NDJSON opened without a header"))?;
                let names = header.clone();
                (
                    Source::Ndjson {
                        rdr: input,
                        buf: Vec::with_capacity(4096),
                        header,
                        line_no: 0,
                    },
                    Some(names),
                )
            }
            other => bail!("internal: {} is not a streaming source", other.format_name()),
        })
    }

    /// The *width* of the next row, without building it.
    ///
    /// The counting pass only ever needed the arity, and materialising a
    /// `Vec<String>` per row to throw it away immediately cost about 100 MB
    /// of resident memory on a 3M-row file — freed, but not returned to the
    /// OS, which is the same thing as far as the machine is concerned.
    fn next_width(&mut self) -> Result<Option<usize>> {
        match self {
            Source::Delimited { rdr, rec } => match rdr.read_byte_record(rec) {
                Ok(true) => Ok(Some(rec.len())),
                Ok(false) => Ok(None),
                Err(e) => Err(anyhow!("{e}")),
            },
            Source::Lines { rdr, buf, re, names, on_no_match, line_no } => {
                while read_line(rdr.as_mut(), buf)? {
                    *line_no += 1;
                    let line = text_of(buf);
                    if line.trim().is_empty() {
                        continue;
                    }
                    if re.is_match(line.as_ref()) {
                        return Ok(Some(names.len()));
                    }
                    match on_no_match {
                        NoMatchPolicy::Skip => continue,
                        NoMatchPolicy::Error => {
                            bail!("line {} does not match the pattern: {:?}", line_no, line)
                        }
                    }
                }
                Ok(None)
            }
            Source::Fixed { rdr, buf, fields, .. } => {
                while read_line(rdr.as_mut(), buf)? {
                    if text_of(buf).trim().is_empty() {
                        continue;
                    }
                    return Ok(Some(fields.len()));
                }
                Ok(None)
            }
            Source::Ndjson { rdr, buf, header, .. } => {
                while read_line(rdr.as_mut(), buf)? {
                    if text_of(buf).trim().is_empty() {
                        continue;
                    }
                    return Ok(Some(header.len()));
                }
                Ok(None)
            }
        }
    }

    /// The next row, or None at end of input. Rows the format skips (blank
    /// lines, non-matching lines under `on_no_match = "skip"`) are consumed
    /// here so the caller only ever sees data.
    fn next_row(&mut self) -> Result<Option<Vec<String>>> {
        match self {
            Source::Delimited { rdr, rec } => match rdr.read_byte_record(rec) {
                Ok(true) => Ok(Some(rec.iter().map(|b| text_of(b).into_owned()).collect())),
                Ok(false) => Ok(None),
                Err(e) => Err(anyhow!("{e}")),
            },
            Source::Lines { rdr, buf, re, names, on_no_match, line_no } => {
                while read_line(rdr.as_mut(), buf)? {
                    *line_no += 1;
                    let line = text_of(buf);
                    if line.trim().is_empty() {
                        continue;
                    }
                    match re.captures(line.as_ref()) {
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
            Source::Fixed { rdr, buf, fields, chars } => {
                while read_line(rdr.as_mut(), buf)? {
                    let line = text_of(buf);
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
            Source::Ndjson { rdr, buf, header, line_no } => {
                while read_line(rdr.as_mut(), buf)? {
                    *line_no += 1;
                    let line = text_of(buf);
                    if line.trim().is_empty() {
                        continue;
                    }
                    let v: serde_json::Value = serde_json::from_str(line.as_ref())
                        .map_err(|e| anyhow!("invalid JSON on line {}: {e}", line_no))?;
                    return Ok(Some(match &v {
                        serde_json::Value::Object(map) => header
                            .iter()
                            .map(|k| map.get(k).map(crate::engine::json_scalar).unwrap_or_default())
                            .collect(),
                        other => vec![crate::engine::json_scalar(other)],
                    }));
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

fn measure(source: &mut Source, limits: &Limits, path: &Path) -> Result<Shape> {
    let mut widths: HashMap<usize, usize> = HashMap::new();
    let mut rows = 0usize;
    let mut max_width = 0usize;
    let mut cells: u64 = 0;
    loop {
        let Some(w) = source
            .next_width()
            .with_context(|| format!("measuring {}", path.display()))?
        else {
            break;
        };
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
fn first_odd_row(
    path: &Path,
    extraction: &Extraction,
    opts: &ExtractOpts,
    modal: usize,
) -> Option<(usize, usize)> {
    let (mut src, _) = Source::open(path, extraction, opts, None).ok()?;
    let mut i = 0usize;
    while let Ok(Some(w)) = src.next_width() {
        if w != modal {
            return Some((i, w));
        }
        i += 1;
    }
    None
}

/// Run `spec` over `path` without materialising the file, collecting the
/// batches.
///
/// Convenient, and still bounded by the *output*: for a query that does not
/// need every row in memory at once, prefer [`execute_with`], which hands each
/// batch over as it is produced and lets the caller drop it.
///
/// Callers must check [`can_stream`] first; this returns an error rather than
/// guessing if handed a shape it does not implement.
pub fn execute_batches(spec: &ParseSpec, path: &Path, limits: Limits) -> Result<Vec<RecordBatch>> {
    let mut out = Vec::new();
    execute_with(spec, path, limits, |b| {
        out.push(b);
        Ok(())
    })?;
    Ok(out)
}

/// Which of a spec's columns fail to parse *somewhere* in the file.
///
/// The reason this exists: a type inferred from the first 500 rows is a guess
/// about the other 118,890. Real exports break it routinely — a station id
/// that is digits until row 708 and `TA1309000067` after, an incident number
/// that gains a `-18112015` suffix at row 4067 — and the result was a spec
/// that validated, conformed, and died mid-query.
///
/// So the guess is *checked*. Every column is read as a raw string (which
/// cannot fail), and each declared type is tried against those strings with
/// `build_column_at` — the executor's own function — batch by batch. Nothing
/// aborts on the first failure: the caller wants every column that is wrong,
/// not the first.
///
/// Memory stays bounded, which is what makes this affordable at all: before
/// the streaming work a full verification pass would have cost eight times the
/// file.
/// What a full read of the file says about a guessed spec.
#[derive(Debug, Default)]
pub struct Verification {
    /// (column index, the parse error) for every column whose declared type
    /// does not hold somewhere in the file.
    pub failing: Vec<(usize, String)>,
    /// For each failing column, the offending values and the rows they are on
    /// — at most a few, because the point is to name the problem rather than
    /// to list it. How many there are matters as much as what they are: one
    /// bad value in 40,000 rows is a stray, and thousands are a wrong type.
    pub offenders: Vec<(usize, Vec<(usize, String)>, usize)>,
    /// Rows read. The denominator that turns "5 values are not integers" into
    /// either "5 strays in 40,000" or "5 of 5, so it is simply not an integer
    /// column".
    pub rows: usize,
    /// Body rows identical to the header.
    ///
    /// Concatenated exports do this — `cat jan.csv feb.csv > year.csv` leaves
    /// February's header sitting in the middle of the data — and the result is
    /// one text row that poisons every numeric column in the file.
    pub repeated_header_rows: usize,
}

pub fn failing_columns(
    spec: &ParseSpec,
    path: &Path,
    limits: Limits,
) -> Result<Vec<(usize, String)>> {
    Ok(verify(spec, path, limits)?.failing)
}

/// Read the whole file and report what the guess got wrong.
///
/// Two paths, because the answer is almost always "nothing". The fast one
/// simply *runs the spec*: if every column types cleanly over the whole file
/// there is nothing to report, and that costs one ordinary read. Only when
/// something fails is the expensive analysis worth doing — reading every
/// column as raw text and trying each declared type against it, to find every
/// bad column rather than the first, and the values responsible.
///
/// Without the split this cost 16 s on a 141 MB file against 2.9 s for the
/// same file's `count(*)`, because it did the typing work twice for every
/// file whether or not anything was wrong.
pub fn verify(spec: &ParseSpec, path: &Path, limits: Limits) -> Result<Verification> {
    // A text column cannot fail to be text, so verifying one is pure work.
    // Projecting them away is free on a file of numbers and a large saving on
    // the wide mostly-textual exports that are most of real data.
    let mut probe = spec.clone();
    probe.columns.retain(|c| c.dtype != crate::spec::DType::Utf8);
    if probe.columns.is_empty() {
        return Ok(Verification::default());
    }

    let mut rows = 0usize;
    let clean = if can_stream(&probe) {
        execute_with(&probe, path, limits, |b| {
            rows += b.num_rows();
            Ok(())
        })
        .is_ok()
    } else {
        match crate::engine::execute_batches(&probe, path, limits) {
            Ok(bs) => {
                rows = bs.iter().map(|b| b.num_rows()).sum();
                true
            }
            Err(_) => false,
        }
    };
    if clean {
        return Ok(Verification { rows, ..Verification::default() });
    }
    analyse(spec, path, limits)
}

/// The slow path: which columns are wrong, and which values made them so.
fn analyse(spec: &ParseSpec, path: &Path, limits: Limits) -> Result<Verification> {
    use datafusion::arrow::array::{Array, StringArray};

    // Read every column as an untouched string: no na_values, no strip, no
    // separators — the raw post-transform text the real parse would see.
    let mut probe = spec.clone();
    for c in &mut probe.columns {
        c.dtype = crate::spec::DType::Utf8;
        c.nullable = true;
        c.parse = ValueParsing::default();
    }

    let mut failed: Vec<Option<String>> = vec![None; spec.columns.len()];
    let mut bad: Vec<(Vec<(usize, String)>, usize)> =
        vec![(Vec::new(), 0); spec.columns.len()];
    let mut repeats = 0usize;
    let mut row_base = 0usize;
    // The header cell each column reads from, which is what a repeated header
    // row would contain.
    let sources: Vec<String> =
        spec.columns.iter().map(|c| c.source_name().to_string()).collect();

    let mut check = |batch: RecordBatch| -> Result<()> {
        let cols: Vec<Option<&StringArray>> = (0..batch.num_columns())
            .map(|i| batch.column(i).as_any().downcast_ref::<StringArray>())
            .collect();

        // A row is a repeated header when *every* column holds its own name,
        // byte for byte. Two rules, and both matter:
        //
        // Every column, not one — a `name` column containing the value "name"
        // must not look like a header row.
        //
        // Byte for byte, not trimmed — because the caller turns this count
        // into a transform that deletes rows, and a detection that is more
        // permissive than the deletion (or vice versa) means deleting
        // something nobody proved was a header. Whatever is counted here must
        // be exactly what the emitted pattern matches.
        if spec.columns.len() > 1 {
            for r in 0..batch.num_rows() {
                let all = cols.iter().enumerate().all(|(i, a)| {
                    a.map(|a| !a.is_null(r) && a.value(r) == sources[i]).unwrap_or(false)
                });
                if all {
                    repeats += 1;
                }
            }
        }

        for (i, col) in spec.columns.iter().enumerate() {
            let Some(arr) = cols.get(i).copied().flatten() else { continue };
            let values: Vec<&str> =
                (0..arr.len()).map(|r| if arr.is_null(r) { "" } else { arr.value(r) }).collect();
            if build_column_at(col, &values, row_base).is_ok() {
                continue;
            }
            if failed[i].is_none() {
                failed[i] = Some(
                    build_column_at(col, &values, row_base)
                        .err()
                        .map(|e| format!("{e:#}"))
                        .unwrap_or_default(),
                );
            }
            // Only now, and only for this column, is it worth paying for a
            // per-row scan: it turns "this column is not an integer" into
            // "these two values are not, and here is where they are".
            for (r, v) in values.iter().enumerate() {
                if build_column_at(col, std::slice::from_ref(v), row_base + r).is_err() {
                    bad[i].1 += 1;
                    if bad[i].0.len() < 3 {
                        bad[i].0.push((row_base + r + 1, (*v).to_string()));
                    }
                    // A column where *everything* fails is a wrong guess, not
                    // a set of strays, and counting every one of a million
                    // rows adds nothing a reader would use.
                    if bad[i].1 >= 500 {
                        break;
                    }
                }
            }
        }
        row_base += batch.num_rows();
        Ok(())
    };

    if can_stream(&probe) {
        execute_with(&probe, path, limits, check)?;
    } else {
        for b in crate::engine::execute_batches(&probe, path, limits)? {
            check(b)?;
        }
    }

    let offenders = bad
        .into_iter()
        .enumerate()
        .filter(|(_, (v, _))| !v.is_empty())
        .map(|(i, (v, n))| (i, v, n))
        .collect();
    Ok(Verification {
        failing: failed.into_iter().enumerate().filter_map(|(i, e)| e.map(|e| (i, e))).collect(),
        offenders,
        rows: row_base,
        repeated_header_rows: repeats,
    })
}

/// The driver: calls `sink` with each batch as it is built, and never holds
/// more than one batch of raw strings plus whatever the sink keeps.
///
/// A sink that drops its batch after consuming it makes the whole pipeline
/// O(batch) in memory rather than O(file) — that is what the streaming table
/// provider does, and it is the difference between "big files are cheaper"
/// and "file size stops mattering".
pub fn execute_with(
    spec: &ParseSpec,
    path: &Path,
    limits: Limits,
    mut sink: impl FnMut(RecordBatch) -> Result<()>,
) -> Result<()> {
    if !can_stream(spec) {
        bail!("internal: stream::execute_with called on an unstreamable spec");
    }
    let opts = ExtractOpts::full(limits);
    let ragged = match &spec.extraction {
        Extraction::Delimited { ragged, .. } => *ragged,
        _ => RaggedPolicy::PadNulls,
    };

    // NDJSON's header is the union of every record's keys, so it comes from a
    // pass over the file rather than from the spec.
    let (provided_header, ndjson_rows) = match &spec.extraction {
        Extraction::Json { .. } => {
            let (h, n) = discover_ndjson(path, &opts)?;
            (Some(h), Some(n))
        }
        other => (header_of(other)?, None),
    };

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
        // Scoped so the counting source — and, for an encoding that cannot be
        // decoded incrementally, its copy of the file — is dropped before the
        // body source is opened. Holding both at once cost a second full copy
        // of the file, which is how a 987 MB CSV reached 2 GB.
        let (mut counting, _) =
            Source::open(path, &spec.extraction, &opts, provided_header.as_deref())?;
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
                        .then(|| first_odd_row(path, &spec.extraction, &opts, shape.modal_width))
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

    let total_rows =
        shape.as_ref().map(|s| s.rows).or(ndjson_rows).unwrap_or(usize::MAX);
    let mut plan = Plan::build(spec, target_width, total_rows)?;
    let (mut source, _) =
        Source::open(path, &spec.extraction, &opts, provided_header.as_deref())?;
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
    let mut schema: Option<Arc<Schema>> = None;
    let mut batches_emitted = 0usize;
    // A batch is bounded by *cells*, not rows. 65,536 rows of a 1,000-column
    // file is 65 million strings — a 134 MB file measured at 4.2 GB before
    // this, because width was the one dimension nothing bounded. For anything
    // up to 16 columns this is still exactly BATCH_ROWS, so the common case is
    // unchanged.
    let batch_rows = (BATCH_CELLS / target_width.max(1)).clamp(1, BATCH_ROWS);
    let mut chunk: Vec<Vec<String>> = Vec::with_capacity(batch_rows.min(1024));
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
        while chunk.len() >= batch_rows {
            let rest = chunk.split_off(batch_rows);
            flush(&plan, &chunk, emitted, &mut schema, &mut batches_emitted, &mut sink)?;
            emitted += chunk.len();
            chunk = rest;
        }
    }
    // `..=` in spirit: an empty table still produces one empty batch, because
    // a query over a file with a header and no rows must still have a schema.
    flush(&plan, &chunk, emitted, &mut schema, &mut batches_emitted, &mut sink)?;
    Ok(())
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

#[allow(clippy::too_many_arguments)]
fn flush(
    plan: &Plan,
    rows: &[Vec<String>],
    row_offset: usize,
    schema: &mut Option<Arc<Schema>>,
    emitted: &mut usize,
    sink: &mut impl FnMut(RecordBatch) -> Result<()>,
) -> Result<()> {
    if !rows.is_empty() || *emitted == 0 {
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
        *emitted += 1;
        sink(RecordBatch::try_new(s, arrays).context("assembling record batch")?)?;
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
