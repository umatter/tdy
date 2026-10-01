//! What a column holds, as evidence for a person.
//!
//! Per column of the *framed raw table* — the strings `fit` binds against,
//! after the frame's `transpose`/`skip_rows`/`promote_header`, a region
//! window or a sheet `range`, and before any body transform or cast — how
//! many values, how many distinct, the smallest and largest, the most
//! frequent, and the *shapes* they take ([`shape`]). Read over the whole
//! file by default: a profile of the head is the lie this exists to remove.
//!
//! It infers nothing and writes nothing. No sidecar, lock or target is
//! touched, and nothing in tdy reads a profile to change a spec: a profile
//! is evidence, and the declaration stays the person's sentence to write.
//! See docs/design/2026-10-01-profiling.md.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Context, Result};
use serde::Serialize;

use crate::config::Limits;
use crate::member::MemberRef;
use crate::spec::{Extraction, ParseSpec, RowWindow, Transform};

/// Distinct values tracked per column. Past this the count is a floor and
/// no top values are given: an approximate top five is a number nobody can
/// check.
pub const MAX_DISTINCT: usize = 10_000;
/// Shapes listed per column; the rest are one `(other)` row.
pub const MAX_SHAPES: usize = 64;
/// The most frequent values listed per column.
pub const TOP: usize = 5;
/// Distinct shapes tracked per column. A shape first seen past this is
/// counted only in `(other)`, and the column says its shapes are not
/// complete.
pub const MAX_SHAPE_TRACK: usize = 10_000;
/// The pattern of the row that stands for every shape past [`MAX_SHAPES`].
pub const OTHER_SHAPES: &str = "(other)";

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Profile {
    /// The file as the caller named it.
    pub path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sheet: Option<String>,
    /// The block profiled, when the frame reads one region of a text file —
    /// 0-based, half-open raw lines, as `RowWindow` always is.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub window: Option<RowWindow>,
    /// The A1 range read, for a workbook frame that declares one (a sheet
    /// block, or `--rows` on a sheet): rows in the sheet's own numbering.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub range: Option<String>,
    /// The record array read, for a JSON document.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pointer: Option<String>,
    /// Which frame was read: `sidecar` (a fresh one), or `sniffed (…)` with
    /// the reason no sidecar was used.
    pub frame: String,
    /// The other record arrays the document holds, when the sniffer chose
    /// among several and no sidecar or `--pointer` settled it.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub pointer_candidates: Vec<String>,
    /// What the heading adds about the read: other record arrays the
    /// document holds, stacked tables a file read whole holds.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub notes: Vec<String>,
    /// Body rows profiled.
    pub rows: u64,
    /// False when `--head` stopped the read before the end of the table.
    pub complete: bool,
    pub columns: Vec<ColumnProfile>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ColumnProfile {
    /// The file's own spelling (`header_origin`), not the sanitized one:
    /// two columns the file calls `Betrag` are both `Betrag` here.
    pub name: String,
    /// 1-based, in file order.
    pub position: usize,
    pub non_empty: u64,
    pub empty: u64,
    pub distinct: Distinct,
    /// Of the trimmed non-empty strings, by byte order.
    pub min: Option<String>,
    pub max: Option<String>,
    /// The [`TOP`] most frequent values, most frequent first, ties by value;
    /// empty when `distinct` is a floor.
    pub top: Vec<(String, u64)>,
    /// Every shape, most frequent first (ties by pattern), at most
    /// [`MAX_SHAPES`] plus one [`OTHER_SHAPES`] row for the rest.
    pub shapes: Vec<Shape>,
    /// False when a shape first seen past the [`MAX_SHAPE_TRACK`] bound
    /// was counted only in `(other)`: then nobody knows which shape is
    /// most frequent, and no renderer says one is.
    pub shapes_complete: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Distinct {
    Exact(u64),
    AtLeast(u64),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Shape {
    pub pattern: String,
    pub count: u64,
    /// The first value seen with this shape.
    pub example: String,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct ProfileOpts {
    /// Profile only the first N body rows; the profile then says it is not
    /// the whole file.
    pub head: Option<u64>,
}

/// What a person asks to have profiled: a file, optionally one sheet,
/// optionally one block of rows, optionally only its head.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Request {
    pub sheet: Option<String>,
    /// A block, as a region member's title counts it: 1-based, inclusive
    /// (`--rows 6-9`).
    pub rows: Option<(u64, u64)>,
    pub head: Option<u64>,
    /// The record array of a JSON document to read (`/q2`), in place of
    /// the one the sidecar or the sniffer chose.
    pub pointer: Option<String>,
}

/// Parse `--rows`' `START-END` (1-based, inclusive; an en dash, as the
/// workbench's member title prints it, is read too).
pub fn parse_rows(s: &str) -> Result<(u64, u64)> {
    let bad = || anyhow!("--rows wants START-END, 1-based and inclusive (e.g. 6-9), not {s:?}");
    let (a, b) = s.split_once(['-', '\u{2013}']).ok_or_else(bad)?;
    let a: u64 = a.trim().parse().map_err(|_| bad())?;
    let b: u64 = b.trim().parse().map_err(|_| bad())?;
    if a == 0 || b < a {
        return Err(bad());
    }
    Ok((a, b))
}

/// Profile `path` as `req` asks: find the frame ([`frame_for`]), then read
/// the framed raw table ([`profile`]). The one function every door calls.
///
/// `path` may also be a member reference — `book.xlsx#Q1`, `report.csv#2` —
/// split by [`resolve`].
pub fn profile_file(path: &Path, req: &Request, limits: Limits) -> Result<Profile> {
    let (file, sheet, region) = resolve(path, None)?;
    let mut p = profile_member(&file, sheet, region, req, limits)?;
    p.path = path.display().to_string();
    Ok(p)
}

/// Split a typed file or member reference into the data file, the sheet
/// and the region it names.
///
/// A plain file wins; then a member a sidecar declares
/// (`sidecar::resolve_ref`'s rule); then a split into a data file that
/// exists plus a sheet (of a workbook) or a region — which
/// [`profile_member`] then reads, or, for a region with no sidecar to say
/// which rows it is, refuses by name rather than as a missing file. Two
/// readings are refused, naming both.
///
/// With `root`, every candidate data file is confined to it *before* any
/// sidecar beside it is read, and the answer is confined again.
pub fn resolve(path: &Path, root: Option<&Path>) -> Result<(PathBuf, Option<String>, Option<u32>)> {
    let confined = |f: &Path| -> Result<PathBuf> {
        match root {
            Some(r) => crate::fileio::confine(f, r),
            None => Ok(f.to_path_buf()),
        }
    };
    if path.is_file() {
        return Ok((confined(path)?, None, None));
    }
    let inside = |f: &Path| f.is_file() && root.is_none_or(|r| crate::fileio::confine(f, r).is_ok());
    let text = path.to_string_lossy().into_owned();
    let declared = MemberRef::resolve(&text, |m| {
        let f = Path::new(&m.path);
        inside(f) && crate::sidecar::declares_member(f, m.sheet.as_deref(), m.region)
    });
    let split = match declared {
        Ok(Some(m)) => Some(m),
        Err(several) => bail!("{text} could mean {} — name the file and the sheet unambiguously", MemberRef::names(&several)),
        Ok(None) => match MemberRef::resolve(&text, |m| {
            let f = Path::new(&m.path);
            (m.sheet.is_some() || m.region.is_some())
                && inside(f)
                && (m.sheet.is_none() || crate::sample::guess_format(f) == crate::sample::FormatGuess::Excel)
        }) {
            Ok(m) => m,
            Err(several) => {
                bail!("{text} could mean {} — name the file and the sheet unambiguously", MemberRef::names(&several))
            }
        },
    };
    match split {
        Some(m) => Ok((confined(Path::new(&m.path))?, m.sheet, m.region)),
        None => match root {
            // Confinement's own sentence: a split whose data file exists but
            // lies outside is "outside" (no sidecar beside it was read);
            // anything else is the path's own "outside" or "does not exist".
            Some(r) => {
                let outside = match MemberRef::resolve(&text, |m| {
                    (m.sheet.is_some() || m.region.is_some()) && Path::new(&m.path).is_file()
                }) {
                    Ok(Some(m)) => Some(m),
                    Err(mut several) => several.pop(),
                    Ok(None) => None,
                };
                let probe = outside.map(|m| PathBuf::from(m.path)).unwrap_or_else(|| path.to_path_buf());
                Err(crate::fileio::confine(&probe, r).err().unwrap_or_else(|| anyhow!("{text} is not a file")))
            }
            None => bail!("{text} does not exist (not a file, nor a sheet or block of one)"),
        },
    }
}

/// [`profile_file`] for a member already resolved: the data file, the
/// sheet and the region its reference named.
pub fn profile_member(
    file: &Path,
    ref_sheet: Option<String>,
    ref_region: Option<u32>,
    req: &Request,
    limits: Limits,
) -> Result<Profile> {
    let sheet = match (ref_sheet, req.sheet.clone()) {
        (Some(a), Some(b)) if a != b => {
            bail!("{} names sheet {a:?} but --sheet says {b:?}", file.display())
        }
        (a, b) => a.or(b),
    };
    let (mut frame, mut source) = match ref_region {
        Some(r) => {
            let name = MemberRef { path: file.display().to_string(), sheet: sheet.clone(), region: Some(r) }.name();
            if req.rows.is_some() {
                bail!("{name} already names its block; drop --rows");
            }
            match crate::sidecar::load_member(file, sheet.as_deref(), Some(r)) {
                Ok(crate::sidecar::SidecarStatus::Fresh(sc)) => (sc.spec, "sidecar".to_string()),
                other => {
                    let why = match other {
                        Ok(crate::sidecar::SidecarStatus::Stale(_)) => " (its sidecar is stale)".to_string(),
                        Err(e) => format!(" (its sidecar was refused: {e:#})"),
                        _ => String::new(),
                    };
                    bail!(
                        "no fresh sidecar for {name}{why} — name the block with --rows A-B (the rows \
                         its title shows{}), or re-run `tdy fit`",
                        if sheet.is_some() { ", with --sheet" } else { "" }
                    )
                }
            }
        }
        None => frame_for(file, sheet.as_deref(), req.rows, limits)?,
    };
    let mut notes = Vec::new();
    let mut candidates = Vec::new();
    if let Extraction::Json { lines: false, pointer } = &mut frame.extraction {
        match &req.pointer {
            Some(want) => {
                *pointer = Some(want.clone());
                source.push_str("; record array from --pointer");
            }
            None if source != "sidecar" => {
                let all = crate::sniff::json_record_pointers(file, limits);
                if all.len() > 1 {
                    candidates = all;
                }
            }
            None => {}
        }
    } else if let Some(want) = &req.pointer {
        bail!("--pointer {want:?} picks a record array of a JSON document; {} is not read as one", file.display());
    }
    // A text file holding several stacked tables, read whole, counts each
    // table's header as data: say so, and name the flag that reads one.
    if let Extraction::Delimited { region: None, .. } = &frame.extraction {
        if let Ok(r) = crate::engine::regions_of(file, None, limits) {
            if r.windows.len() > 1 {
                let spans: Vec<String> =
                    r.windows.iter().map(|w| format!("{}\u{2013}{}", w.start + 1, w.end)).collect();
                notes.push(format!(
                    "this file holds {} stacked tables (rows {}), read whole here with their headers \
                     as data; --rows A-B reads one",
                    r.windows.len(),
                    spans.join(", ")
                ));
            }
        }
    }
    let mut p = profile(file, &frame, limits, ProfileOpts { head: req.head })?;
    p.frame = source;
    p.notes = notes;
    p.pointer_candidates = candidates;
    Ok(p)
}

/// Which frame to read `path` with, and a word on where it came from.
///
/// A fresh sidecar's spec when the member has one; otherwise the sniffer's
/// own frame, heuristics only and without the whole-file type check (the
/// frame is all that is kept) — never the model, and never written to
/// disk. A refused member is the screen that most needs a profile and has
/// no sidecar; this is the rule `console::raw_head` follows.
///
/// `rows` names a block, 1-based and inclusive: physical lines of a text
/// file, or — with `sheet` — the sheet's own A1 row numbers, what Excel and
/// a sheet block's `range` show. A fresh sidecar that reads exactly that
/// block is used; otherwise the frame `fit` reads a block with
/// (`fit::region_frame`).
pub fn frame_for(
    path: &Path,
    sheet: Option<&str>,
    rows: Option<(u64, u64)>,
    limits: Limits,
) -> Result<(ParseSpec, String)> {
    let workbook = crate::sample::guess_format(path) == crate::sample::FormatGuess::Excel;
    if let Some(s) = sheet {
        if !workbook {
            bail!("--sheet {s:?} applies to workbooks; {} is not one", path.display());
        }
    }
    if let Some((a, b)) = rows {
        return match sheet {
            Some(s) => sheet_block_frame(path, s, (a, b), limits),
            None if workbook => {
                bail!("--rows on a workbook needs --sheet: rows are counted within one sheet")
            }
            None => text_block_frame(path, (a, b), limits),
        };
    }
    let why = match crate::sidecar::load_member(path, sheet, None) {
        Ok(crate::sidecar::SidecarStatus::Fresh(sc)) => return Ok((sc.spec, "sidecar".into())),
        Ok(crate::sidecar::SidecarStatus::Stale(_)) => "the sidecar is stale".to_string(),
        Ok(crate::sidecar::SidecarStatus::Absent) => absent_why(path, sheet),
        Err(e) => format!("the sidecar was refused: {e:#}"),
    };
    let spec = match sheet {
        Some(s) => crate::sniff::frame_excel_sheet(path, s, limits)
            .with_context(|| format!("framing sheet {s:?} of {}", path.display()))?,
        None => {
            let sample = crate::sample::build(path, 16 * 1024, limits)
                .with_context(|| format!("sampling {}", path.display()))?;
            crate::sniff::sniff_opts(path, &sample, limits, crate::sniff::SniffOpts { verify: false })
                .with_context(|| format!("framing {}", path.display()))?
                .spec
        }
    };
    Ok((spec, format!("sniffed ({why}; heuristics only, not saved)")))
}

/// "No sidecar", naming the member sidecars that do exist beside the file:
/// a workbook expanded into sheets, a file or sheet split into blocks.
fn absent_why(path: &Path, sheet: Option<&str>) -> String {
    let regions = crate::sidecar::region_sidecars(path, sheet);
    let regions: Vec<String> = regions.iter().map(|r| format!("#{r}")).collect();
    let sheets = if sheet.is_none() { crate::sidecar::sheet_sidecars(path) } else { Vec::new() };
    let whole = if sheet.is_some() { "sheet" } else if sheets.is_empty() { "file" } else { "workbook" };
    let mut has = Vec::new();
    if !sheets.is_empty() {
        has.push(format!("sheet sidecars: {}", sheets.join(", ")));
    }
    if !regions.is_empty() {
        has.push(format!("region sidecars: {}", regions.join(", ")));
    }
    if has.is_empty() {
        "no sidecar".into()
    } else {
        format!("no sidecar for the whole {whole}; {}", has.join("; "))
    }
}

/// `--rows A-B` of a text file: physical lines, refused past the end with
/// the file's own length.
fn text_block_frame(path: &Path, (a, b): (u64, u64), limits: Limits) -> Result<(ParseSpec, String)> {
    let lines = count_lines(path, limits)?;
    if b > lines {
        bail!(
            "--rows {a}-{b} reaches past the end of {} ({lines} lines)",
            path.display()
        );
    }
    let window = RowWindow { start: a - 1, end: b, ordinal: 1 };
    // A fresh sidecar that reads exactly this block: a region member's, or
    // a plain one whose file is one block with padding around it.
    let plain = std::iter::once(None);
    let regions = crate::sidecar::region_sidecars(path, None).into_iter().map(Some);
    for r in plain.chain(regions) {
        if let Ok(crate::sidecar::SidecarStatus::Fresh(sc)) = crate::sidecar::load_member(path, None, r) {
            if let Extraction::Delimited { region: Some(w), .. } = &sc.spec.extraction {
                if (w.start, w.end) == (window.start, window.end) {
                    return Ok((sc.spec, "sidecar".into()));
                }
            }
        }
    }
    let spec = crate::fit::region_frame(path, None, window, crate::sniff::SniffOpts { verify: false }, limits)
        .map_err(|e| anyhow!("{e}"))?;
    Ok((spec, "sniffed (the block's own frame, as fit reads it; not saved)".into()))
}

/// `--sheet S --rows A-B`: the sheet's own A1 rows, refused outside its
/// used range (which is named), then the block's A1 range — a fresh sheet
/// block sidecar's when one reads exactly it.
fn sheet_block_frame(path: &Path, sheet: &str, (a, b): (u64, u64), limits: Limits) -> Result<(ParseSpec, String)> {
    let open = crate::sniff::OpenSheet::open(path, sheet, limits)?;
    let (r0, c0) = open.range.start().unwrap_or((0, 0));
    let height = open.range.height() as u64;
    let (first, last) = (u64::from(r0) + 1, u64::from(r0) + height);
    if height == 0 || a < first || b > last {
        bail!(
            "--rows {a}-{b} is outside sheet {sheet:?}'s used range, rows {first}\u{2013}{last}: a \
             sheet's rows are its own A1 row numbers"
        );
    }
    let window = RowWindow { start: a - first, end: b - u64::from(r0), ordinal: 1 };
    let a1 = crate::fit::block_a1((r0, c0), open.range.width(), window);
    for r in crate::sidecar::region_sidecars(path, Some(sheet)) {
        if let Ok(crate::sidecar::SidecarStatus::Fresh(sc)) = crate::sidecar::load_member(path, Some(sheet), Some(r)) {
            if let Extraction::Excel { range: Some(got), .. } = &sc.spec.extraction {
                if *got == a1 {
                    return Ok((sc.spec, "sidecar".into()));
                }
            }
        }
    }
    let spec = crate::fit::region_frame(path, Some(&open), window, crate::sniff::SniffOpts { verify: false }, limits)
        .map_err(|e| anyhow!("{e}"))?;
    Ok((spec, "sniffed (the block's own frame, as fit reads it; not saved)".into()))
}

/// Physical lines in a text file, counted as `RowWindow` counts them,
/// streamed.
fn count_lines(path: &Path, limits: Limits) -> Result<u64> {
    use std::io::BufRead;
    let real = crate::fileio::materialize(path, limits.max_decompressed_bytes)?;
    let f = std::fs::File::open(real.as_ref() as &Path).with_context(|| format!("cannot open {}", path.display()))?;
    let mut r = std::io::BufReader::new(f);
    let (mut n, mut buf) = (0u64, Vec::new());
    loop {
        buf.clear();
        if r.read_until(b'\n', &mut buf)? == 0 {
            return Ok(n);
        }
        n += 1;
    }
}

/// The frame's framing half: its transforms up to and including the last
/// `transpose`/`skip_rows`/`promote_header` — the same split
/// `engine::apply_spec_transforms` makes — and nothing after.
fn framing_of(spec: &ParseSpec) -> ParseSpec {
    let k = spec
        .transforms
        .iter()
        .rposition(|t| {
            matches!(t, Transform::Transpose | Transform::SkipRows { .. } | Transform::PromoteHeader { .. })
        })
        .map_or(0, |i| i + 1);
    let mut f = spec.clone();
    f.transforms.truncate(k);
    f
}

/// Profile the framed raw table `frame` reads from `path`.
///
/// Text formats the streaming executor can read go through
/// `stream::framed_rows` — the executor's own reader, one row in memory —
/// so memory is O(columns × caps), not O(file). Anything else (Excel, a
/// JSON document, a `transpose`) materialises within `[limits]`, as every
/// other reader of it does.
pub fn profile(path: &Path, frame: &ParseSpec, limits: Limits, opts: ProfileOpts) -> Result<Profile> {
    let framing = framing_of(frame);
    let mut tally = Tally::new(frame, opts.head);
    if crate::stream::enabled() && crate::stream::can_stream(&framing) {
        crate::stream::framed_rows(&framing, path, limits, &mut tally)?;
    } else {
        let mut table =
            crate::engine::extract(&framing.extraction, path, &crate::engine::ExtractOpts::full(limits))
                .with_context(|| format!("extracting {}", path.display()))?;
        crate::engine::apply_spec_transforms(&mut table, &framing.transforms)?;
        table.ensure_header()?;
        let header = table.header.clone().unwrap_or_default();
        let origin = table.header_origin.clone().unwrap_or_else(|| header.clone());
        use crate::stream::FramedSink as _;
        tally.header(&header, &origin)?;
        for row in &table.rows {
            if !tally.row(row)? {
                break;
            }
        }
    }
    let sheet = match &frame.extraction {
        Extraction::Excel { sheet_name, .. } => sheet_name.clone(),
        _ => None,
    };
    let window = match &frame.extraction {
        Extraction::Delimited { region, .. } => *region,
        _ => None,
    };
    let mut p = tally.finish(path.display().to_string(), sheet, window);
    match &frame.extraction {
        Extraction::Excel { range, .. } => p.range.clone_from(range),
        Extraction::Json { pointer, .. } => p.pointer.clone_from(pointer),
        _ => {}
    }
    Ok(p)
}

/// The running counts of every column.
struct Tally {
    head: Option<u64>,
    rows: u64,
    complete: bool,
    /// Each frame column's source and `na_values`, resolved to a position
    /// when the header is known.
    na: Vec<(String, Vec<String>)>,
    columns: Vec<Acc>,
}

/// One column's running counts. Every field is bounded: `values` by
/// [`MAX_DISTINCT`] (and dropped past it), `shapes` likewise.
struct Acc {
    name: String,
    na: Vec<String>,
    non_empty: u64,
    empty: u64,
    /// `None` once more than [`MAX_DISTINCT`] distinct values were seen.
    values: Option<HashMap<String, u64>>,
    min: Option<String>,
    max: Option<String>,
    /// pattern -> (count, first example).
    shapes: HashMap<String, (u64, String)>,
    /// Values whose shape arrived after [`MAX_SHAPE_TRACK`] shapes were already
    /// tracked: counted, but only as `(other)`.
    untracked: u64,
}

impl Tally {
    fn new(frame: &ParseSpec, head: Option<u64>) -> Tally {
        let na = frame
            .columns
            .iter()
            .filter(|c| !c.parse.na_values.is_empty())
            .map(|c| (c.source_name().to_string(), c.parse.na_values.clone()))
            .collect();
        Tally { head, rows: 0, complete: true, na, columns: Vec::new() }
    }

    fn finish(self, path: String, sheet: Option<String>, window: Option<RowWindow>) -> Profile {
        let columns = self.columns.into_iter().enumerate().map(|(i, a)| a.finish(i + 1)).collect();
        Profile {
            path,
            sheet,
            window,
            range: None,
            pointer: None,
            frame: "given by the caller".to_string(),
            notes: Vec::new(),
            pointer_candidates: Vec::new(),
            rows: self.rows,
            complete: self.complete,
            columns,
        }
    }
}

impl crate::stream::FramedSink for Tally {
    fn header(&mut self, header: &[String], origin: &[String]) -> Result<()> {
        // `na_values` by the position a frame column reads — the header is
        // deduplicated, so a name is one position — and the union when two
        // columns read the same one.
        let mut na: Vec<Vec<String>> = vec![Vec::new(); header.len()];
        for (source, values) in &self.na {
            if let Some(i) = header.iter().position(|h| h == source) {
                na[i].extend(values.iter().cloned());
            }
        }
        self.columns = header
            .iter()
            .enumerate()
            .map(|(i, h)| Acc {
                name: origin.get(i).cloned().unwrap_or_else(|| h.clone()),
                na: std::mem::take(&mut na[i]),
                non_empty: 0,
                empty: 0,
                values: Some(HashMap::new()),
                min: None,
                max: None,
                shapes: HashMap::new(),
                untracked: 0,
            })
            .collect();
        Ok(())
    }

    /// Count one row; `false` once `--head` is satisfied and this row was
    /// one too many (so a file of exactly N rows is still complete).
    fn row(&mut self, row: &[String]) -> Result<bool> {
        if self.head.is_some_and(|h| self.rows >= h) {
            self.complete = false;
            return Ok(false);
        }
        self.rows += 1;
        for (i, acc) in self.columns.iter_mut().enumerate() {
            acc.add(row.get(i).map(String::as_str).unwrap_or(""));
        }
        Ok(true)
    }
}

impl Acc {
    fn add(&mut self, raw: &str) {
        // Trimmed as every cast trims, and a declared missing-value token is
        // empty, case-insensitively, as the executor reads it.
        let v = raw.trim();
        if v.is_empty() || self.na.iter().any(|na| na.eq_ignore_ascii_case(v)) {
            self.empty += 1;
            return;
        }
        self.non_empty += 1;
        if self.min.as_deref().is_none_or(|m| v < m) {
            self.min = Some(v.to_string());
        }
        if self.max.as_deref().is_none_or(|m| v > m) {
            self.max = Some(v.to_string());
        }
        if let Some(values) = &mut self.values {
            if let Some(n) = values.get_mut(v) {
                *n += 1;
            } else if values.len() < MAX_DISTINCT {
                values.insert(v.to_string(), 1);
            } else {
                // A floor from here on, and the map is freed: its counts
                // could no longer give a top five anyone can check.
                self.values = None;
            }
        }
        let pattern = shape(v);
        if let Some((n, _)) = self.shapes.get_mut(&pattern) {
            *n += 1;
        } else if self.shapes.len() < MAX_SHAPE_TRACK {
            self.shapes.insert(pattern, (1, v.to_string()));
        } else {
            self.untracked += 1;
        }
    }

    fn finish(self, position: usize) -> ColumnProfile {
        let (distinct, top) = match self.values {
            Some(values) => {
                let distinct = Distinct::Exact(values.len() as u64);
                let mut all: Vec<(String, u64)> = values.into_iter().collect();
                all.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
                all.truncate(TOP);
                (distinct, all)
            }
            None => (Distinct::AtLeast(MAX_DISTINCT as u64), Vec::new()),
        };
        let mut shapes: Vec<Shape> = self
            .shapes
            .into_iter()
            .map(|(pattern, (count, example))| Shape { pattern, count, example })
            .collect();
        shapes.sort_by(|a, b| b.count.cmp(&a.count).then_with(|| a.pattern.cmp(&b.pattern)));
        if shapes.len() > MAX_SHAPES || self.untracked > 0 {
            let rest = shapes.split_off(shapes.len().min(MAX_SHAPES));
            let example = rest.first().map(|s| s.example.clone()).unwrap_or_default();
            let count = rest.iter().map(|s| s.count).sum::<u64>() + self.untracked;
            shapes.push(Shape { pattern: OTHER_SHAPES.to_string(), count, example });
        }
        ColumnProfile {
            name: self.name,
            position,
            non_empty: self.non_empty,
            empty: self.empty,
            distinct,
            min: self.min,
            max: self.max,
            top,
            shapes,
            shapes_complete: self.untracked == 0,
        }
    }
}

/// A digit run longer than this is `9+`: an account number must not make
/// one shape per length.
const MAX_DIGIT_RUN: usize = 8;

/// The class one character falls into, for [`shape`].
#[derive(Clone, Copy, PartialEq, Eq)]
enum Class {
    Digit,
    Upper,
    Lower,
    Space,
    Other(char),
}

fn class(c: char) -> Class {
    if c.is_ascii_digit() {
        Class::Digit
    } else if c.is_alphabetic() {
        // A letter with no case (CJK, say) reads as lowercase: it is a
        // letter, and the shape has two letter symbols, not three.
        if c.is_uppercase() { Class::Upper } else { Class::Lower }
    } else if c.is_whitespace() {
        Class::Space
    } else {
        Class::Other(c)
    }
}

/// The shape of one trimmed value — Potter's Wheel's idea, in the form the
/// design page fixes: an ASCII digit is `9` and a run of digits keeps its
/// length up to [`MAX_DIGIT_RUN`] (`9+` beyond); a letter is `A` or `a`, a
/// run of one case longer than one collapsing to `A+`/`a+`; a run of
/// whitespace is one space; every other character is itself.
///
/// So `2025-01-28` is `9999-99-99`, `28.01.2025` is `99.99.9999`, `Bern` is
/// `Aa+` and `1'234.50` is `9'999.99`.
pub fn shape(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    let mut chars = value.chars().peekable();
    while let Some(c) = chars.next() {
        let k = class(c);
        let mut run = 1usize;
        if !matches!(k, Class::Other(_)) {
            while chars.peek().is_some_and(|&n| class(n) == k) {
                chars.next();
                run += 1;
            }
        }
        match k {
            Class::Digit if run > MAX_DIGIT_RUN => out.push_str("9+"),
            Class::Digit => out.extend(std::iter::repeat_n('9', run)),
            Class::Upper => out.push_str(if run > 1 { "A+" } else { "A" }),
            Class::Lower => out.push_str(if run > 1 { "a+" } else { "a" }),
            Class::Space => out.push(' '),
            Class::Other(c) => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shape_over_the_design_examples() {
        assert_eq!(shape("2025-01-28"), "9999-99-99");
        assert_eq!(shape("28.01.2025"), "99.99.9999");
        assert_eq!(shape("Bern"), "Aa+");
        assert_eq!(shape("CHF"), "A+");
        assert_eq!(shape("1'234.50"), "9'999.99");
        assert_eq!(shape("Zürich"), "Aa+");
    }

    #[test]
    fn a_digit_run_keeps_its_length_up_to_eight() {
        assert_eq!(shape("12345678"), "99999999");
        assert_eq!(shape("123456789"), "9+");
        assert_eq!(shape("CH12345678901"), "A+9+");
        assert_eq!(shape("7"), "9");
    }

    #[test]
    fn single_letters_whitespace_and_other_characters() {
        assert_eq!(shape("a"), "a");
        assert_eq!(shape("A"), "A");
        assert_eq!(shape("aB"), "aA");
        assert_eq!(shape("St. Gallen"), "Aa. Aa+");
        assert_eq!(shape("a \t b"), "a a");
        assert_eq!(shape("K100000"), "A999999");
        assert_eq!(shape("-12.5%"), "-99.9%");
    }
}
