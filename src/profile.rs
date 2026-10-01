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
use std::path::Path;

use anyhow::{anyhow, bail, Context, Result};
use serde::Serialize;

use crate::config::Limits;
use crate::spec::{Extraction, ParseSpec, RowWindow, Transform};

/// Distinct values tracked per column. Past this the count is a floor and
/// no top values are given: an approximate top five is a number nobody can
/// check.
pub const MAX_DISTINCT: usize = 10_000;
/// Shapes listed per column; the rest are one `(other)` row.
pub const MAX_SHAPES: usize = 64;
/// The most frequent values listed per column.
pub const TOP: usize = 5;
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
    /// Which frame was read: `sidecar` (a fresh one), or `sniffed (…)` with
    /// the reason no sidecar was used.
    pub frame: String,
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
/// resolved the way every single-file tool resolves one
/// (`sidecar::resolve_ref`).
pub fn profile_file(path: &Path, req: &Request, limits: Limits) -> Result<Profile> {
    let (file, ref_sheet, ref_region) = crate::sidecar::resolve_ref(path)?;
    let sheet = match (ref_sheet, req.sheet.clone()) {
        (Some(a), Some(b)) if a != b => {
            bail!("{} names sheet {a:?} but --sheet says {b:?}", path.display())
        }
        (a, b) => a.or(b),
    };
    let (frame, source) = match ref_region {
        Some(r) => {
            if req.rows.is_some() {
                bail!("{} already names a block; drop --rows", path.display());
            }
            match crate::sidecar::load_member(&file, sheet.as_deref(), Some(r))? {
                crate::sidecar::SidecarStatus::Fresh(sc) => (sc.spec, "sidecar".to_string()),
                _ => bail!(
                    "{} has no fresh sidecar to say which rows it is; name the block with \
                     --rows START-END (what its title shows), or re-run `tdy fit`",
                    path.display()
                ),
            }
        }
        None => frame_for(&file, sheet.as_deref(), req.rows, limits)?,
    };
    let mut p = profile(&file, &frame, limits, ProfileOpts { head: req.head })?;
    p.path = path.display().to_string();
    p.frame = source;
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
/// `rows` names a block (1-based, inclusive): a fresh region sidecar whose
/// window is exactly that block, or else the frame `fit` reads a block
/// with (`fit::region_frame`).
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
        let window = RowWindow { start: a - 1, end: b, ordinal: 1 };
        if sheet.is_none() {
            if workbook {
                bail!("--rows on a workbook needs --sheet: rows are counted within one sheet");
            }
            for r in crate::sidecar::region_sidecars(path, None) {
                if let Ok(crate::sidecar::SidecarStatus::Fresh(sc)) =
                    crate::sidecar::load_member(path, None, Some(r))
                {
                    if let Extraction::Delimited { region: Some(w), .. } = &sc.spec.extraction {
                        if (w.start, w.end) == (window.start, window.end) {
                            return Ok((sc.spec, "sidecar".into()));
                        }
                    }
                }
            }
        }
        let open = match sheet {
            Some(s) => Some(crate::sniff::OpenSheet::open(path, s, limits)?),
            None => None,
        };
        let spec = crate::fit::region_frame(
            path,
            open.as_ref(),
            window,
            crate::sniff::SniffOpts { verify: false },
            limits,
        )
        .map_err(|e| anyhow!("{e}"))?;
        return Ok((spec, "sniffed (the block's own frame, as fit reads it; not saved)".into()));
    }
    let why = match crate::sidecar::load_member(path, sheet, None) {
        Ok(crate::sidecar::SidecarStatus::Fresh(sc)) => return Ok((sc.spec, "sidecar".into())),
        Ok(crate::sidecar::SidecarStatus::Stale(_)) => "the sidecar is stale".to_string(),
        Ok(crate::sidecar::SidecarStatus::Absent) => "no sidecar".to_string(),
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
    Ok(tally.finish(path.display().to_string(), sheet, window))
}

/// The running counts of every column.
struct Tally {
    head: Option<u64>,
    rows: u64,
    complete: bool,
    /// Each header name's `na_values`, from the frame's column that reads it.
    na: HashMap<String, Vec<String>>,
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
    /// Values whose shape arrived after [`MAX_DISTINCT`] shapes were already
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
            frame: String::new(),
            rows: self.rows,
            complete: self.complete,
            columns,
        }
    }
}

impl crate::stream::FramedSink for Tally {
    fn header(&mut self, header: &[String], origin: &[String]) -> Result<()> {
        self.columns = header
            .iter()
            .enumerate()
            .map(|(i, h)| Acc {
                name: origin.get(i).cloned().unwrap_or_else(|| h.clone()),
                na: self.na.get(h).cloned().unwrap_or_default(),
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
        } else if self.shapes.len() < MAX_DISTINCT {
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
