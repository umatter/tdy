//! ParseSpec — the contract between the sniffer, the LLM inferencer, and the
//! executor.
//!
//! Design invariants:
//!
//! 1. Single source of truth: these structs are what the executor
//!    deserializes AND what generates the JSON Schema used for
//!    grammar-constrained decoding (`schemars`).
//! 2. Envelope vs. body: the LLM emits only `ParseSpec`; the tool wraps it in
//!    `Sidecar` with the source fingerprint and provenance.
//! 3. Strictness as a feature: `deny_unknown_fields` everywhere, so
//!    hallucinated fields fail with precise messages fed back in the retry
//!    loop.
//! 4. Columns are a projection: no drop/rename ops; only listed columns
//!    survive, renamed via `source` -> `name`.
//! 5. [`ParseSpec::validate`] is a real gate, not a formality: every spec
//!    reaching the executor has passed it, whether it came from the sniffer,
//!    the model, or a hand-edited sidecar. Anything the executor would
//!    otherwise discover by panicking belongs here as a message.

use anyhow::{anyhow, bail, Result};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Version of the spec *format* (this schema), not of any one spec.
pub const SPEC_FORMAT_VERSION: u32 = 1;

// ---------------------------------------------------------------------------
// Envelope (tool-generated, never emitted by the model)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Sidecar {
    pub spec_version: u32,
    pub source: SourceFingerprint,
    pub provenance: Provenance,
    pub spec: ParseSpec,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SourceFingerprint {
    /// Path relative to the sidecar's location (survives repo relocation).
    pub path: String,
    /// blake3 of the full file; mismatch at query time = stale spec.
    pub blake3: String,
    pub bytes: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Provenance {
    pub method: InferenceMethod,
    pub tool_version: String,
    /// RFC 3339.
    pub created_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt_version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sampled_bytes: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InferenceMethod {
    /// Tier-1 deterministic sniffer succeeded on its own.
    Heuristic,
    /// Tier-2 model call produced (or refined) the spec.
    Llm,
    /// A human wrote or edited the sidecar.
    Manual,
}

// ---------------------------------------------------------------------------
// Body (emitted by sniffer or LLM; consumed by the executor)
// ---------------------------------------------------------------------------

/// Everything needed to turn one messy file into one tidy Arrow relation.
/// Applied strictly in order: extraction -> transforms -> column projection
/// and typed casting.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ParseSpec {
    pub extraction: Extraction,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub transforms: Vec<Transform>,
    /// Output columns. Acts as a projection: unlisted columns are dropped.
    pub columns: Vec<ColumnSpec>,
    /// Model self-assessment in [0, 1]; heuristic specs set it too.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub confidence: Option<f32>,
    /// Free-text caveats surfaced to the user; never machine-interpreted.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub notes: Vec<String>,
}

// ---------------------------------------------------------------------------
// Extraction: format-specific "get me a raw rectangle of strings"
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "format", rename_all = "snake_case", deny_unknown_fields)]
pub enum Extraction {
    /// CSV/TSV and friends, including ragged ones. Produces a headerless raw
    /// table; use a `promote_header` transform for the header row(s).
    Delimited {
        delimiter: char,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        quote: Option<char>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        escape: Option<char>,
        /// encoding_rs label, e.g. "utf-8", "windows-1252". None = detect.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        encoding: Option<String>,
        /// Lines starting with this char are skipped before parsing.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        comment: Option<char>,
        #[serde(default)]
        ragged: RaggedPolicy,
    },
    /// Via calamine. Merged cells surface as value-in-top-left + blanks;
    /// deliberately handled by `fill_down` / header fill-right, not here.
    /// Produces a headerless raw table like Delimited.
    Excel {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        sheet_name: Option<String>,
        /// 0-based; used only if `sheet_name` is unset. Both unset = sheet 0.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        sheet_index: Option<u32>,
        /// A1-style range, e.g. "A4:H200". None = used range.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        range: Option<String>,
    },
    /// Fixed-width dumps. Offsets are **character** positions per line after
    /// decoding, half-open [start, end) — the columns you would count in a
    /// monospace editor. (Byte positions would shift by one for every
    /// non-ASCII character earlier in the line, silently sliding every later
    /// field into its neighbour.) Produces named columns.
    FixedWidth {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        encoding: Option<String>,
        fields: Vec<FixedField>,
    },
    /// Log files / line-oriented text. One regex with *named* capture
    /// groups; each group becomes a column. Produces named columns.
    Lines {
        pattern: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        encoding: Option<String>,
        #[serde(default)]
        on_no_match: NoMatchPolicy,
    },
    /// JSON and NDJSON. Produces named columns (union of record keys;
    /// nested values are serialized back to JSON strings).
    Json {
        /// true = newline-delimited records (NDJSON / JSON Lines).
        #[serde(default)]
        lines: bool,
        /// RFC 6901 JSON Pointer to the array of records within the
        /// document (ignored when `lines` is true).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pointer: Option<String>,
    },
}

impl Extraction {
    pub fn format_name(&self) -> &'static str {
        match self {
            Extraction::Delimited { .. } => "delimited",
            Extraction::Excel { .. } => "excel",
            Extraction::FixedWidth { .. } => "fixed_width",
            Extraction::Lines { .. } => "lines",
            Extraction::Json { .. } => "json",
        }
    }

    /// The encoding label declared in the spec, if any.
    pub fn encoding(&self) -> Option<&str> {
        match self {
            Extraction::Delimited { encoding, .. }
            | Extraction::FixedWidth { encoding, .. }
            | Extraction::Lines { encoding, .. } => encoding.as_deref(),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct FixedField {
    pub name: String,
    /// Inclusive start, in characters, 0-based.
    pub start: u32,
    /// Exclusive end, in characters.
    pub end: u32,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum RaggedPolicy {
    /// Fail on the first row whose arity differs from the modal one (safe
    /// default).
    #[default]
    Error,
    /// Short rows padded with empty cells; long rows keep extras in
    /// overflow columns.
    PadNulls,
    /// Short rows padded; extra fields silently dropped.
    TruncateExtra,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum NoMatchPolicy {
    /// Non-matching lines are dropped (typical for logs with banners).
    #[default]
    Skip,
    Error,
}

// ---------------------------------------------------------------------------
// Transforms: ordered structural surgery on the raw string table
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
pub enum Transform {
    /// Drop leading/trailing rows (title blocks, "Total" footers).
    SkipRows {
        #[serde(default)]
        head: u32,
        #[serde(default)]
        tail: u32,
    },
    /// Use the next `rows` rows as the header.
    ///
    /// With `rows > 1` the *upper* rows are filled rightward first, because a
    /// horizontally merged title cell ("2025" spanning four month columns)
    /// leaves blanks to its right. The **last** header row is not filled: a
    /// blank there is a nameless column, not a merge, and inheriting its left
    /// neighbour's name would attach one column's label to another column's
    /// data. Cells are then joined top-to-bottom with `join`, skipping empties.
    PromoteHeader {
        rows: u32,
        #[serde(default = "default_header_join")]
        join: String,
    },
    /// Drop body rows matching a regex (repeated group headers, page
    /// breaks, subtotal lines). `column = None` tests the whole row joined
    /// with tabs.
    DropRowsMatching {
        pattern: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        column: Option<String>,
    },
    /// Propagate the last non-empty value downward: the cure for vertically
    /// merged cells and "category written once" layouts.
    FillDown { columns: Vec<String> },
    /// Wide -> long.
    Unpivot {
        id_columns: Vec<String>,
        value_columns: Vec<String>,
        variable_name: String,
        value_name: String,
    },
}

fn default_header_join() -> String {
    " ".to_string()
}

// ---------------------------------------------------------------------------
// Output columns: projection + typing + value-level parsing
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ColumnSpec {
    /// Output name (tidy, snake_case — the name SQL sees).
    pub name: String,
    /// Column name as it exists *after* extraction + transforms.
    /// None = same as `name`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    pub dtype: DType,
    #[serde(default = "default_true")]
    pub nullable: bool,
    #[serde(default, skip_serializing_if = "ValueParsing::is_default")]
    pub parse: ValueParsing,
}

impl ColumnSpec {
    /// The post-transform column this reads from.
    pub fn source_name(&self) -> &str {
        self.source.as_deref().unwrap_or(&self.name)
    }
}

fn default_true() -> bool {
    true
}

/// Maps 1:1 onto Arrow types. Deliberately small — a grammar-constrained
/// 8–30B model picks reliably from a short list.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum DType {
    Utf8,
    Bool,
    Int64,
    Float64,
    /// Exact decimals for money. Arrow Decimal128. Values with more
    /// fractional digits than `scale` are rounded half away from zero.
    Decimal { precision: u8, scale: i8 },
    /// chrono strftime format, e.g. "%d.%m.%Y". Arrow Date32.
    /// Month-year values ("%b %Y") are accepted and pinned to day 1.
    Date { format: String },
    /// chrono strftime format. Arrow Timestamp(microsecond).
    ///
    /// `timezone` declares the zone the written values are **in**; the stored
    /// instants are converted to UTC accordingly, which is what an Arrow
    /// timestamp with a timezone means. Only fixed offsets are accepted
    /// ("UTC", "Z", "+02:00", "-0500") — a named zone like "Europe/Zurich"
    /// would need a rule database to resolve daylight saving correctly, and
    /// guessing is how timestamps end up an hour wrong for half the year.
    /// If the format itself parses an offset (`%z`), that offset wins.
    Timestamp {
        format: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        timezone: Option<String>,
    },
}

/// String-level cleanup applied before the typed cast, in this order:
/// trim -> replace -> na check -> strip -> separators -> parse.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ValueParsing {
    /// Tokens treated as null, e.g. ["", "n/a", "–", "#N/A"].
    ///
    /// Matched **case-insensitively**, so listing `"NA"` also covers `na` and
    /// `Na`: a null token's casing is not a distinction anybody means, and a
    /// list that had to spell every one is a list nobody keeps complete.
    /// `sniff::is_na` folds case when it decides a token is missing, and the
    /// two must agree — they did not, and a column typed from a sample
    /// containing `NA` failed on a later `NULL`.
    ///
    /// Checked *before* `true_values`/`false_values`, so a token in both would
    /// read as missing and never as a boolean. `validate` refuses that rather
    /// than resolving it silently.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub na_values: Vec<String>,
    /// Literal substring replacements applied before parsing. The dumb,
    /// explicit fix for locale issues (e.g. "Mär" -> "Mar", "Dez" -> "Dec")
    /// — no locale tables shipped, everything auditable in the sidecar.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub replace: Vec<Replacement>,
    /// Regex removed from the value before parsing (currency symbols,
    /// trailing "%", footnote markers).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub strip: Option<String>,
    /// For numbers written as "1'234,56" and friends.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub decimal_separator: Option<char>,
    /// Must group the integer part in threes; a value that does not is an
    /// error, not a silently rewritten number.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thousands_separator: Option<char>,
    /// Move the decimal point by this many places before parsing: `-2` turns
    /// integer Rappen (`123450`) into francs (`1234.50`).
    ///
    /// **This changes the value**, which is why it exists as a declaration and
    /// is never inferred. A column of integer minor units parses perfectly and
    /// type-checks perfectly and is wrong by a factor of a hundred, and the
    /// error is invisible in any single row — so tdy will not decide this, and
    /// a spec that carries it needs a human's acceptance before it joins a
    /// dataset.
    ///
    /// It is an exact decimal-point move on the digit string, not a
    /// multiplication: no float is involved and nothing is rounded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub decimal_shift: Option<i8>,
    /// For Bool columns: e.g. ["ja", "yes", "1"] / ["nein", "no", "0"].
    /// Matched case-insensitively.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub true_values: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub false_values: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Replacement {
    pub from: String,
    pub to: String,
}

impl ValueParsing {
    pub fn is_default(&self) -> bool {
        self == &Self::default()
    }
}

// ---------------------------------------------------------------------------
// A1 ranges (here rather than in the engine so `validate` can reject a bad
// one before calamine asserts on it)
// ---------------------------------------------------------------------------

/// "A4:H200" -> ((3, 0), (199, 7)), 0-based inclusive.
pub fn parse_a1_range(s: &str) -> Result<((u32, u32), (u32, u32))> {
    let (a, b) = s
        .split_once(':')
        .ok_or_else(|| anyhow!("range must look like \"A4:H200\", got {s:?}"))?;
    let start = parse_a1_cell(a)?;
    let end = parse_a1_cell(b)?;
    if end.0 < start.0 || end.1 < start.1 {
        bail!(
            "range {s:?} runs backwards: the second cell must be below and to \
             the right of the first"
        );
    }
    Ok((start, end))
}

pub fn parse_a1_cell(s: &str) -> Result<(u32, u32)> {
    let s = s.trim().to_ascii_uppercase();
    let letters: String = s.chars().take_while(|c| c.is_ascii_alphabetic()).collect();
    let digits: String = s.chars().skip_while(|c| c.is_ascii_alphabetic()).collect();
    if letters.is_empty() || digits.is_empty() || letters.len() > 3 {
        bail!("invalid A1 cell reference {s:?}");
    }
    let mut col: u32 = 0;
    for ch in letters.chars() {
        col = col
            .checked_mul(26)
            .and_then(|c| c.checked_add(ch as u32 - 'A' as u32 + 1))
            .ok_or_else(|| anyhow!("column out of range in {s:?}"))?;
    }
    let row: u32 = digits
        .parse()
        .map_err(|_| anyhow!("invalid row in A1 reference {s:?}"))?;
    if row == 0 {
        bail!("A1 rows start at 1, got {s:?}");
    }
    Ok((row - 1, col - 1))
}

// ---------------------------------------------------------------------------
// Grammar + validation (the first two tiers of the retry loop; tier three is
// the dry run in engine.rs)
// ---------------------------------------------------------------------------

impl ParseSpec {
    /// JSON Schema handed to llama.cpp / vLLM as the decoding grammar
    /// (`response_format: json_schema`) and to the Anthropic backend as a
    /// forced tool's `input_schema`.
    pub fn json_schema() -> serde_json::Value {
        let schema = schemars::schema_for!(ParseSpec);
        serde_json::to_value(schema).expect("schema serializes")
    }

    /// Cross-field checks serde can't express. On failure the messages are
    /// fed back to the model verbatim.
    pub fn validate(&self) -> std::result::Result<(), Vec<String>> {
        let mut errs = Vec::new();

        if self.columns.is_empty() {
            errs.push("`columns` must not be empty".into());
        }
        let mut seen = std::collections::HashSet::new();
        for c in &self.columns {
            if c.name.trim().is_empty() {
                errs.push("a column has an empty `name`".into());
            }
            if !seen.insert(c.name.as_str()) {
                errs.push(format!("duplicate output column name `{}`", c.name));
            }
            if c.parse.decimal_separator.is_some()
                && c.parse.decimal_separator == c.parse.thousands_separator
            {
                errs.push(format!(
                    "column `{}`: decimal_separator equals thousands_separator",
                    c.name
                ));
            }
            if let Some(pat) = &c.parse.strip {
                if let Err(e) = regex::Regex::new(pat) {
                    errs.push(format!("column `{}`: `strip` is not a valid regex: {e}", c.name));
                }
            }
            // Moving a decimal point means nothing outside a number, and a
            // shift big enough to be a typo is more likely one than a
            // deliberate 40-place move.
            if let Some(shift) = c.parse.decimal_shift {
                if !matches!(c.dtype, DType::Decimal { .. } | DType::Float64 | DType::Int64) {
                    errs.push(format!(
                        "column `{}`: decimal_shift moves a decimal point, which means \
                         nothing for a {} column",
                        c.name,
                        dtype_name(&c.dtype)
                    ));
                }
                if !(-30..=30).contains(&shift) {
                    errs.push(format!(
                        "column `{}`: decimal_shift {shift} is out of range (-30..=30)",
                        c.name
                    ));
                }
                if matches!(c.dtype, DType::Int64) && shift < 0 {
                    // The shift moves the point right for a positive value and
                    // left for a negative one (`engine::shift_decimal_point`
                    // adds it to the integer part's length), so it is the
                    // *negative* direction that turns 1234 into 12.34 — a
                    // fraction an integer column cannot hold. The guard used
                    // to name the other direction, which both rejected the
                    // harmless case and let the lossy one through.
                    errs.push(format!(
                        "column `{}`: a negative decimal_shift on an integer column produces \
                         a fraction it cannot hold (shift {shift} turns 1234 into {}); \
                         declare it as DECIMAL",
                        c.name,
                        crate::engine::shift_decimal_point("1234", shift)
                    ));
                }
            }
            // A token cannot be both "missing" and a value. The executor
            // checks na_values first, so an overlap silently turns a declared
            // FALSE into a null — the exact shape of wrong answer a sidecar's
            // author would never see, since both readings produce a valid
            // column.
            for t in c.parse.true_values.iter().chain(c.parse.false_values.iter()) {
                if c.parse.na_values.iter().any(|na| na.eq_ignore_ascii_case(t)) {
                    errs.push(format!(
                        "column `{}`: {t:?} is in both na_values and true_values/false_values \
                         — it would read as missing, never as a boolean. Remove it from one.",
                        c.name
                    ));
                }
            }
            match &c.dtype {
                DType::Decimal { precision, scale } => {
                    if *precision == 0 || *precision > 38 {
                        errs.push(format!(
                            "column `{}`: decimal precision must be 1..=38 (got {precision})",
                            c.name
                        ));
                    } else if *scale < 0 || i16::from(*scale) > i16::from(*precision) {
                        errs.push(format!(
                            "column `{}`: decimal scale must be 0..={precision} (got {scale})",
                            c.name
                        ));
                    }
                }
                DType::Bool => {
                    let lower = |v: &String| v.to_ascii_lowercase();
                    let t: Vec<String> = c.parse.true_values.iter().map(lower).collect();
                    if c.parse.false_values.iter().map(lower).any(|v| t.contains(&v)) {
                        errs.push(format!(
                            "column `{}`: a token appears in both true_values and false_values",
                            c.name
                        ));
                    }
                }
                DType::Date { format } => {
                    if format.trim().is_empty() {
                        errs.push(format!("column `{}`: empty date format", c.name));
                    }
                }
                DType::Timestamp { format, timezone } => {
                    if format.trim().is_empty() {
                        errs.push(format!("column `{}`: empty timestamp format", c.name));
                    }
                    if let Some(tz) = timezone {
                        if parse_fixed_offset(tz).is_none() {
                            errs.push(format!(
                                "column `{}`: timezone {tz:?} is not a fixed offset. Use \
                                 \"UTC\", \"+02:00\" or \"-0500\"; named zones are not \
                                 resolved because daylight saving cannot be guessed from \
                                 the value alone.",
                                c.name
                            ));
                        }
                    }
                }
                _ => {}
            }
        }

        if let Some(label) = self.extraction.encoding() {
            if encoding_rs::Encoding::for_label(label.as_bytes()).is_none() {
                errs.push(format!(
                    "unknown encoding {label:?}; use an encoding_rs label such as \
                     \"utf-8\", \"windows-1252\" or \"utf-16le\" (an unrecognised \
                     label would otherwise be silently ignored and the encoding guessed)"
                ));
            }
        }

        match &self.extraction {
            Extraction::Delimited { delimiter, quote, escape, comment, .. } => {
                // Every one of these is handed to the CSV reader as a single
                // byte; a multi-byte character would be truncated into a
                // different, arbitrary one.
                for (label, ch) in [
                    ("delimiter", Some(*delimiter)),
                    ("quote", *quote),
                    ("escape", *escape),
                    ("comment", *comment),
                ] {
                    if let Some(c) = ch {
                        if !c.is_ascii() {
                            errs.push(format!(
                                "{label} must be a single ASCII character (got {c:?})"
                            ));
                        }
                    }
                }
                let mut specials: Vec<(&str, char)> = vec![("delimiter", *delimiter)];
                if let Some(q) = quote {
                    specials.push(("quote", *q));
                }
                if let Some(e) = escape {
                    specials.push(("escape", *e));
                }
                if let Some(c) = comment {
                    specials.push(("comment", *c));
                }
                for i in 0..specials.len() {
                    for j in i + 1..specials.len() {
                        if specials[i].1 == specials[j].1 {
                            errs.push(format!(
                                "{} and {} are both {:?}; they must differ",
                                specials[i].0, specials[j].0, specials[i].1
                            ));
                        }
                    }
                }
                if *delimiter == '\n' || *delimiter == '\r' {
                    errs.push("delimiter must not be a newline".into());
                }
            }
            Extraction::Excel { range, sheet_index, .. } => {
                if let Some(r) = range {
                    if let Err(e) = parse_a1_range(r) {
                        errs.push(format!("excel range: {e}"));
                    }
                }
                if let Some(i) = sheet_index {
                    if *i > 10_000 {
                        errs.push(format!("sheet_index {i} is out of any plausible range"));
                    }
                }
            }
            Extraction::FixedWidth { fields, .. } => {
                if fields.is_empty() {
                    errs.push("fixed_width needs at least one field".into());
                }
                let mut names = std::collections::HashSet::new();
                for f in fields {
                    if f.name.trim().is_empty() {
                        errs.push("a fixed_width field has an empty name".into());
                    }
                    if !names.insert(f.name.as_str()) {
                        errs.push(format!("duplicate fixed_width field name `{}`", f.name));
                    }
                    if f.end <= f.start {
                        errs.push(format!(
                            "fixed field `{}`: end ({}) must be greater than start ({})",
                            f.name, f.end, f.start
                        ));
                    }
                    if f.end > 1_000_000 {
                        errs.push(format!("fixed field `{}`: end is implausibly large", f.name));
                    }
                }
            }
            Extraction::Lines { pattern, .. } => match regex::Regex::new(pattern) {
                Ok(re) => {
                    if re.capture_names().flatten().next().is_none() {
                        errs.push(
                            "lines pattern must contain at least one named capture group, \
                             e.g. (?P<ip>\\S+)"
                                .into(),
                        );
                    }
                }
                Err(e) => errs.push(format!("lines pattern is not a valid regex: {e}")),
            },
            Extraction::Json { lines, pointer } => {
                if let Some(p) = pointer {
                    if *lines {
                        errs.push("json: `pointer` is meaningless when `lines` is true".into());
                    }
                    if !p.is_empty() && !p.starts_with('/') {
                        errs.push(format!(
                            "json pointer {p:?} must start with '/' (RFC 6901)"
                        ));
                    }
                }
            }
        }

        for t in &self.transforms {
            match t {
                Transform::PromoteHeader { rows, .. } => {
                    if *rows == 0 {
                        errs.push("promote_header: `rows` must be >= 1".into());
                    }
                    if *rows > 20 {
                        errs.push(format!(
                            "promote_header: {rows} header rows is implausible (max 20)"
                        ));
                    }
                }
                Transform::DropRowsMatching { pattern, .. } => {
                    if let Err(e) = regex::Regex::new(pattern) {
                        errs.push(format!("drop_rows_matching: invalid regex: {e}"));
                    }
                }
                Transform::FillDown { columns } => {
                    if columns.is_empty() {
                        errs.push("fill_down: `columns` must not be empty".into());
                    }
                }
                Transform::Unpivot {
                    id_columns,
                    value_columns,
                    variable_name,
                    value_name,
                } => {
                    if variable_name == value_name {
                        errs.push("unpivot: variable_name equals value_name".into());
                    }
                    if id_columns.iter().any(|c| value_columns.contains(c)) {
                        errs.push("unpivot: id_columns and value_columns overlap".into());
                    }
                    if value_columns.is_empty() {
                        errs.push("unpivot: value_columns must not be empty".into());
                    }
                    // The output header is id_columns + [variable, value]; a
                    // collision there would produce two columns with one name
                    // and silently resolve to the first.
                    for n in [variable_name, value_name] {
                        if id_columns.contains(n) {
                            errs.push(format!(
                                "unpivot: `{n}` is both an id column and an output name"
                            ));
                        }
                    }
                    let mut ids = std::collections::HashSet::new();
                    for c in id_columns {
                        if !ids.insert(c) {
                            errs.push(format!("unpivot: duplicate id column `{c}`"));
                        }
                    }
                }
                Transform::SkipRows { .. } => {}
            }
        }

        if let Some(c) = self.confidence {
            if !(0.0..=1.0).contains(&c) || c.is_nan() {
                errs.push("confidence must be within [0, 1]".into());
            }
        }

        if errs.is_empty() {
            Ok(())
        } else {
            Err(errs)
        }
    }
}

/// Parse the fixed offsets we accept in `DType::Timestamp::timezone`.
/// The type's name, for a message.
fn dtype_name(d: &DType) -> &'static str {
    match d {
        DType::Utf8 => "text",
        DType::Bool => "boolean",
        DType::Int64 => "integer",
        DType::Float64 => "float",
        DType::Decimal { .. } => "decimal",
        DType::Date { .. } => "date",
        DType::Timestamp { .. } => "timestamp",
    }
}

pub fn parse_fixed_offset(tz: &str) -> Option<chrono::FixedOffset> {
    let t = tz.trim();
    if t.eq_ignore_ascii_case("utc") || t.eq_ignore_ascii_case("z") || t.eq_ignore_ascii_case("gmt")
    {
        return chrono::FixedOffset::east_opt(0);
    }
    let (sign, rest) = match t.strip_prefix('+') {
        Some(r) => (1i32, r),
        None => (-1i32, t.strip_prefix('-')?),
    };
    let digits: String = rest.chars().filter(|c| c.is_ascii_digit()).collect();
    let (h, m) = match digits.len() {
        2 => (digits.parse::<i32>().ok()?, 0),
        4 => (digits[..2].parse::<i32>().ok()?, digits[2..].parse::<i32>().ok()?),
        _ => return None,
    };
    // Reject stray characters: only ':' is allowed as a separator.
    if rest.chars().any(|c| !c.is_ascii_digit() && c != ':') {
        return None;
    }
    if h > 23 || m > 59 {
        return None;
    }
    chrono::FixedOffset::east_opt(sign * (h * 3600 + m * 60))
}

#[cfg(test)]
mod tests {
    /// A token cannot be both "missing" and a value: the executor checks
    /// na_values first, so an overlap silently turns a declared FALSE into a
    /// null — a wrong value whose two readings both produce a valid column.
    #[test]
    fn a_token_may_not_be_both_missing_and_a_boolean() {
        let mut spec = minimal_bool_spec();
        spec.columns[0].parse.false_values = vec!["keine".into()];
        spec.columns[0].parse.na_values = vec!["KEINE".into(), "n/a".into()];
        let e = spec.validate().expect_err("the overlap must be refused");
        let text = format!("{e:?}");
        assert!(text.contains("keine") || text.contains("KEINE"), "{text}");

        spec.columns[0].parse.na_values = vec!["n/a".into()];
        spec.validate().expect("no overlap, no complaint");
    }

    /// `shift_decimal_point` adds the shift to the integer part's length, so a
    /// *negative* shift is the one that turns 1234 into 12.34. The guard used
    /// to name the other direction: it rejected the harmless multiply and let
    /// the lossy divide through.
    #[test]
    fn the_integer_decimal_shift_guard_names_the_lossy_direction() {
        assert_eq!(crate::engine::shift_decimal_point("1234", -2), "12.34");
        assert_eq!(crate::engine::shift_decimal_point("1234", 2), "123400");

        let mut spec = minimal_int_spec();
        spec.columns[0].parse.decimal_shift = Some(-2);
        let e = spec.validate().expect_err("a fraction does not fit in an integer column");
        assert!(format!("{e:?}").contains("12.34"), "{e:?}");

        spec.columns[0].parse.decimal_shift = Some(2);
        spec.validate().expect("multiplying an integer keeps it an integer");
    }

    fn minimal_int_spec() -> ParseSpec {
        ParseSpec {
            extraction: Extraction::Lines {
                pattern: "^(?P<n>.*)$".into(),
                encoding: None,
                on_no_match: NoMatchPolicy::default(),
            },
            transforms: vec![],
            columns: vec![ColumnSpec {
                name: "n".into(),
                source: None,
                dtype: DType::Int64,
                nullable: true,
                parse: ValueParsing::default(),
            }],
            confidence: None,
            notes: vec![],
        }
    }

    fn minimal_bool_spec() -> ParseSpec {
        let mut s = minimal_int_spec();
        s.columns[0].dtype = DType::Bool;
        s
    }

    use super::*;

    fn minimal_spec() -> ParseSpec {
        ParseSpec {
            extraction: Extraction::Delimited {
                delimiter: ',',
                quote: Some('"'),
                escape: None,
                encoding: None,
                comment: None,
                ragged: RaggedPolicy::Error,
            },
            transforms: vec![],
            columns: vec![ColumnSpec {
                name: "a".into(),
                source: None,
                dtype: DType::Utf8,
                nullable: true,
                parse: ValueParsing::default(),
            }],
            confidence: None,
            notes: vec![],
        }
    }

    fn errs(s: &ParseSpec) -> Vec<String> {
        s.validate().unwrap_err()
    }

    #[test]
    fn valid_minimal_spec_passes() {
        assert!(minimal_spec().validate().is_ok());
    }

    #[test]
    fn duplicate_columns_rejected() {
        let mut s = minimal_spec();
        s.columns.push(s.columns[0].clone());
        assert!(s.validate().is_err());
    }

    #[test]
    fn unpivot_overlap_rejected() {
        let mut s = minimal_spec();
        s.transforms.push(Transform::Unpivot {
            id_columns: vec!["x".into()],
            value_columns: vec!["x".into(), "y".into()],
            variable_name: "k".into(),
            value_name: "v".into(),
        });
        assert!(s.validate().is_err());
    }

    #[test]
    fn unpivot_output_name_colliding_with_an_id_column_rejected() {
        let mut s = minimal_spec();
        s.transforms.push(Transform::Unpivot {
            id_columns: vec!["region".into()],
            value_columns: vec!["jan".into()],
            variable_name: "region".into(),
            value_name: "v".into(),
        });
        assert!(errs(&s).iter().any(|e| e.contains("id column and an output name")));
    }

    #[test]
    fn non_ascii_csv_special_characters_rejected() {
        let mut s = minimal_spec();
        s.extraction = Extraction::Delimited {
            delimiter: '€',
            quote: Some('"'),
            escape: None,
            encoding: None,
            comment: None,
            ragged: RaggedPolicy::Error,
        };
        assert!(errs(&s).iter().any(|e| e.contains("ASCII")));
    }

    #[test]
    fn colliding_csv_special_characters_rejected() {
        let mut s = minimal_spec();
        s.extraction = Extraction::Delimited {
            delimiter: ',',
            quote: Some(','),
            escape: None,
            encoding: None,
            comment: None,
            ragged: RaggedPolicy::Error,
        };
        assert!(errs(&s).iter().any(|e| e.contains("must differ")));
    }

    #[test]
    fn fixed_width_bounds_are_checked() {
        let mut s = minimal_spec();
        s.extraction = Extraction::FixedWidth {
            encoding: None,
            fields: vec![
                FixedField { name: "a".into(), start: 10, end: 2 },
                FixedField { name: "a".into(), start: 0, end: 1 },
            ],
        };
        let e = errs(&s);
        assert!(e.iter().any(|m| m.contains("greater than start")));
        assert!(e.iter().any(|m| m.contains("duplicate")));
    }

    #[test]
    fn decimal_scale_bounds() {
        let mut s = minimal_spec();
        s.columns[0].dtype = DType::Decimal { precision: 5, scale: -1 };
        assert!(errs(&s).iter().any(|e| e.contains("scale")));
        s.columns[0].dtype = DType::Decimal { precision: 5, scale: 9 };
        assert!(errs(&s).iter().any(|e| e.contains("scale")));
        s.columns[0].dtype = DType::Decimal { precision: 39, scale: 2 };
        assert!(errs(&s).iter().any(|e| e.contains("precision")));
        s.columns[0].dtype = DType::Decimal { precision: 38, scale: 2 };
        assert!(s.validate().is_ok());
    }

    #[test]
    fn named_timezones_are_rejected_with_guidance() {
        let mut s = minimal_spec();
        s.columns[0].dtype = DType::Timestamp {
            format: "%Y-%m-%d".into(),
            timezone: Some("Europe/Zurich".into()),
        };
        assert!(errs(&s).iter().any(|e| e.contains("fixed offset")));
        s.columns[0].dtype = DType::Timestamp {
            format: "%Y-%m-%d".into(),
            timezone: Some("+02:00".into()),
        };
        assert!(s.validate().is_ok());
    }

    #[test]
    fn fixed_offsets_parse() {
        assert_eq!(parse_fixed_offset("UTC").unwrap().local_minus_utc(), 0);
        assert_eq!(parse_fixed_offset("Z").unwrap().local_minus_utc(), 0);
        assert_eq!(parse_fixed_offset("+02:00").unwrap().local_minus_utc(), 7200);
        assert_eq!(parse_fixed_offset("-0500").unwrap().local_minus_utc(), -18000);
        assert_eq!(parse_fixed_offset("+05").unwrap().local_minus_utc(), 18000);
        assert!(parse_fixed_offset("Europe/Zurich").is_none());
        assert!(parse_fixed_offset("+25:00").is_none());
        assert!(parse_fixed_offset("").is_none());
    }

    #[test]
    fn a1_ranges() {
        assert_eq!(parse_a1_range("A4:H200").unwrap(), ((3, 0), (199, 7)));
        assert_eq!(parse_a1_cell("AA1").unwrap(), (0, 26));
        assert!(parse_a1_range("H200:A4").is_err(), "backwards range must be rejected");
        assert!(parse_a1_range("A0:B2").is_err());
        assert!(parse_a1_range("nonsense").is_err());
        assert!(parse_a1_cell("AAAA1").is_err());
    }

    #[test]
    fn an_unknown_encoding_label_is_rejected() {
        let mut s = minimal_spec();
        s.extraction = Extraction::Delimited {
            delimiter: ',',
            quote: Some('"'),
            escape: None,
            encoding: Some("utf8x".into()),
            comment: None,
            ragged: RaggedPolicy::Error,
        };
        assert!(errs(&s).iter().any(|e| e.contains("unknown encoding")));
        s.extraction = Extraction::Delimited {
            delimiter: ',',
            quote: Some('"'),
            escape: None,
            encoding: Some("windows-1252".into()),
            comment: None,
            ragged: RaggedPolicy::Error,
        };
        assert!(s.validate().is_ok());
    }

    #[test]
    fn lines_pattern_needs_named_groups() {
        let mut s = minimal_spec();
        s.extraction = Extraction::Lines {
            pattern: r"^(\w+)$".into(),
            encoding: None,
            on_no_match: NoMatchPolicy::Skip,
        };
        assert!(errs(&s).iter().any(|e| e.contains("named capture group")));
    }

    #[test]
    fn json_pointer_shape_is_checked() {
        let mut s = minimal_spec();
        s.extraction = Extraction::Json { lines: false, pointer: Some("data".into()) };
        assert!(errs(&s).iter().any(|e| e.contains("RFC 6901")));
        s.extraction = Extraction::Json { lines: true, pointer: Some("/data".into()) };
        assert!(errs(&s).iter().any(|e| e.contains("meaningless")));
    }

    #[test]
    fn schema_roundtrip_json() {
        let s = minimal_spec();
        let j = serde_json::to_string(&s).unwrap();
        let back: ParseSpec = serde_json::from_str(&j).unwrap();
        assert!(back.validate().is_ok());
    }

    #[test]
    fn unknown_field_rejected() {
        let j = r#"{"extraction":{"format":"json","lines":true},"columns":[{"name":"a","dtype":{"type":"utf8"}}],"bogus":1}"#;
        assert!(serde_json::from_str::<ParseSpec>(j).is_err());
    }
}
