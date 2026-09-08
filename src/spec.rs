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
    /// The sheet this spec is about, for a sheet member's sidecar
    /// (`<file>#<sheet>.tdy.toml`). Absent for a plain sidecar.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sheet: Option<String>,
    /// The region this spec is about, for a region member's sidecar
    /// (`<file>[#<sheet>]#<N>.tdy.toml`). Absent for a plain or sheet
    /// sidecar covering the whole file or sheet.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub region: Option<u32>,
    /// The compression the file was read through (`gzip`, `zstd`, `bzip2`,
    /// `xz`), when it was. `blake3` and `bytes` above are of the compressed
    /// file — the bytes the user has and the ones that arrive again next
    /// month — and this says so.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compressed: Option<String>,
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
        /// A block of raw records to read, applied before anything else
        /// (skip_rows, promote_header, every transform). None = the whole
        /// file.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        region: Option<RowWindow>,
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
        /// The 1-based ordinal of the stacked block `range` was narrowed to,
        /// written by `fit_region` alongside `range` itself. `RowWindow`
        /// cannot travel with an Excel extraction the way it does with
        /// `Delimited` (a sheet has no row-window field at all — `range`
        /// already says which rows), so the ordinal a `source_name` column
        /// needs is carried separately, and only when `range` came from
        /// splitting a sheet at blank rows.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        region_ordinal: Option<u32>,
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

/// A 0-based, half-open window over the file's raw physical lines:
/// `[start, end)`. Applied by a `Delimited` extraction before anything
/// else — skip_rows, promote_header, every transform all see only the
/// records inside it.
///
/// A record's index is the line its first byte is on. A blank line
/// consumes an index of its own even though the CSV reader never yields it
/// as a record — it is still a line the file has. A quoted newline inside
/// a record is not a boundary: it does not advance the index of the
/// *next* record beyond that record's own extent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct RowWindow {
    pub start: u64,
    pub end: u64,
    /// The block's 1-based index among the file's (or sheet's) stacked
    /// blocks, in file order. Required, not defaulted: 0 would read as a
    /// real ordinal rather than an absent one, and `source_name`'s
    /// `from = "region"` needs a true count to report.
    pub ordinal: u32,
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

/// Which way `fill_down` carries the last non-empty value.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum FillDirection {
    /// Downward: the value is written once at the top of its group, or the
    /// cell above was vertically merged.
    #[default]
    Down,
    /// Upward: the value is written at the *bottom* of its group.
    Up,
}

impl FillDirection {
    pub fn is_default(&self) -> bool {
        matches!(self, FillDirection::Down)
    }
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
    FillDown {
        columns: Vec<String>,
        /// Which way the last non-empty value travels. `down` is the
        /// merged-cell and written-once-at-the-top layout; `up` is the same
        /// layout with the label written at the *bottom* of its group, which
        /// French-language and some accounting exports do.
        #[serde(default, skip_serializing_if = "FillDirection::is_default")]
        direction: FillDirection,
    },
    /// Add a column holding a fact about *where* the data came from.
    ///
    /// The period a monthly export covers is very often only in its filename,
    /// and forty CSVs whose canton appears nowhere but their path are an
    /// ordinary pile. `Constant` can hand-write that per file, at the cost of
    /// the review gate firing on every member — which is the right gate for an
    /// arbitrary constant and the wrong one for a fact tdy can read off the
    /// path itself. This is *derived*, not invented, so it carries no review.
    ///
    /// A `pattern` that does not match is an error, not an empty column: a
    /// silently empty `jahr` on one member of twelve is exactly the outcome
    /// this exists to prevent.
    SourceName {
        /// The column to add. May only add, never shadow.
        name: String,
        from: SourcePart,
        /// Optional regex; its first capture group (or the whole match, when
        /// it has none) becomes the value instead of the whole part.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pattern: Option<String>,
    },
    /// Add a column the file does not have, holding `value` in every row.
    ///
    /// The empty string is the null fill: `""` reads as missing in every
    /// type, so a declared-absent column becomes a column of nulls. Never
    /// inferred — the sniffer cannot know a fact the file does not contain.
    /// The planner emits it only for a target column declared
    /// `if_missing = 'null'`, where the declaration itself is the
    /// authorisation; any other use is hand-written and gated by review,
    /// because a constant is data tdy is being told, not data it read.
    Constant { name: String, value: String },
    /// Flip the table: rows become columns and columns become rows.
    ///
    /// The cure for a file laid out for reading rather than for analysis —
    /// variables down the left, observations across the top, which is what
    /// every "print-friendly" export and every hand-built management sheet
    /// produces. Nothing else in the spec language can reach such a file:
    /// `unpivot` turns wide into long but cannot make the *first column* into
    /// the header.
    ///
    /// Takes no options, deliberately. After the flip, the values that were
    /// the first column are the first *row*, so `promote_header` does what it
    /// always does and the header a `matches` clause addresses is the file's
    /// own spelling of those labels. An option to fold the two steps into one
    /// would be a second way to say the same thing.
    ///
    /// Must come before `promote_header`: the two disagree about which
    /// direction the names run. Runs on the materialising executor, since the
    /// first output row cannot be emitted until the last input row is read.
    Transpose,
    /// Split one column into several, in place.
    ///
    /// The column named by `source` is **replaced** by the columns named in
    /// `into`, in that position, so the header keeps its shape and `columns`
    /// addresses the parts by name. Runs on the string table like every other
    /// transform, before anything is typed.
    ///
    /// The split is *total by construction*: a delimiter split stops after
    /// `into.len()` parts and puts the remainder in the last one, so a value
    /// can never yield more parts than there are names for it. It can yield
    /// **fewer** — a value with no separator at all — and `on_short` says what
    /// that means, defaulting to an error naming the row. Padding a short row
    /// silently is how a split loses the second half of every value that
    /// happened to contain no comma.
    SplitColumn {
        /// The post-transform column to split, by the name the file gives it.
        source: String,
        /// The columns it becomes. At least two; they replace `source`.
        into: Vec<String>,
        by: SplitBy,
        #[serde(default)]
        on_short: ShortSplit,
    },
    /// Wide -> long.
    Unpivot {
        id_columns: Vec<String>,
        value_columns: Vec<String>,
        variable_name: String,
        value_name: String,
    },
}

/// The unit of an integer timestamp.
///
/// `format = "%s"` already reads epoch **seconds** — chrono parses it and tdy
/// passes the format straight through — so this exists for the two scales it
/// does not: the millisecond epochs JavaScript and Java hand out, and the
/// microsecond ones some databases do. `1748736000` and `1748736000000` are
/// the same instant a thousand apart, and both are plausible integers, so the
/// scale is declared and never guessed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum EpochUnit {
    Seconds,
    Milliseconds,
    Microseconds,
}

/// Which part of a file's location `source_name` reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum SourcePart {
    /// The file name without its extension: `umsatz_2025.xlsx` -> `umsatz_2025`.
    FileStem,
    /// The file name with its extension.
    FileName,
    /// The sheet this table was read from. Only a workbook has one.
    Sheet,
    /// The whole path as written.
    Path,
    /// The 1-based ordinal of the stacked block this table was read from.
    /// Only an extraction narrowed to one region of a file or sheet has one.
    Region,
}

/// How `split_column` cuts a value.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum SplitBy {
    /// On a literal separator, at most `into.len() - 1` times: the remainder
    /// stays in the last part, which is what every `split(sep, maxsplit)` in
    /// every language does and what makes the operation total.
    Delimiter { value: String },
    /// At **character** offsets, like `fixed_width` and for the same reason:
    /// byte offsets slide every later field along by one for each non-ASCII
    /// character before them. `at = [4, 6]` yields three parts. A value that
    /// ends before an offset simply has empty parts from there on — a fixed
    /// layout with a short line is not a short *split*.
    Positions { at: Vec<u32> },
    /// One regex whose capture groups become the parts, in order. A value the
    /// pattern does not match is short.
    Regex { pattern: String },
}

/// What a value that yields fewer parts than there are names for it means.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ShortSplit {
    /// Refuse, naming the row and the value. The default, because a value that
    /// does not split is usually a spec that is wrong about the file.
    #[default]
    Error,
    /// The missing trailing parts are null. Declares that the tail is
    /// optional — `"Zürich"` beside `"Zürich, ZH"` — which is a claim about
    /// the data a person is entitled to make, and one that invents nothing:
    /// a part that is not there becomes missing, never a guess.
    Null,
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
    /// An RFC 6901 JSON Pointer into this column's value, applied before
    /// anything else reads it.
    ///
    /// `Extraction::Json` gives the union of every record's keys and
    /// serialises a nested value back to a JSON string — honest, since nothing
    /// is lost, and unreachable, since DataFusion has no JSON functions to
    /// open it downstream. This opens one level at a time, declaratively:
    /// `source = "addr"` with `pointer = "/city"` is a text column.
    ///
    /// A pointer that does not resolve is a null. One that resolves to an
    /// object or an array is an **error**: the column would silently become
    /// JSON text again, which is the state this exists to get out of.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pointer: Option<String>,
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

/// How a negative number is written, when it is not written with a leading `-`.
///
/// This exists because the alternative was a wrong number. Accounting exports
/// write a negative as `(1,234.50)` and mainframe extracts write it as
/// `1234.50-`; neither parses, so before this existed the only repair the spec
/// language offered was `strip`, which deletes the marker and yields a
/// **positive** value — a validated, fingerprinted spec producing money wrong
/// in its sign, invisibly. Saying what the marker *means* is the honest fix;
/// deleting it is not.
///
/// Never inferred. The sniffer notices the shape and says so (lowering
/// confidence), but a convention is a claim about what the file's author meant,
/// and `(5)` is a footnote marker at least as often as it is minus five.
/// What a decimal column does with a value that carries more fractional
/// digits than its scale. Unset means `half_away` — what every sidecar
/// written before this option existed did, and what a sniffed spec still
/// does, its note saying so. A fitted column that did not declare rounding
/// gets `error`, so a value the probe never saw cannot round silently: the
/// whole-file verification refuses it, naming the row.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Rounding {
    /// Round half away from zero (1.005 → 1.01, -1.005 → -1.01).
    HalfAway,
    /// Refuse the value, naming the row.
    Error,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum NegativeStyle {
    /// `(1,234.50)` is -1234.50. Accounting/finance exports, Excel's
    /// "Accounting" cell format, and every ledger printed since 1494.
    Parentheses,
    /// `1234.50-` is -1234.50. COBOL/SAP/mainframe sign-trailing output.
    TrailingMinus,
}

/// String-level cleanup applied before the typed cast, in this order:
/// trim -> replace -> na check -> strip -> sign -> separators -> parse.
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
    /// How this column writes a negative number, when not with a leading `-`.
    ///
    /// Applied after `strip` and before the separators, so a value may carry a
    /// currency symbol inside the marker (`(CHF 1'234.50)`): strip removes the
    /// symbol, this removes the marker and remembers the sign, and the
    /// separators then see an ordinary number.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub negative: Option<NegativeStyle>,
    /// See [`Rounding`]. Only meaningful on a `decimal` column.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub round: Option<Rounding>,
    /// Read this column as an integer count since 1970 in the given unit.
    ///
    /// Only on a `date` or `timestamp` column, and only alongside
    /// `format = "%s"` — the format and this option are two statements about
    /// how to read the same value, and a sidecar where they disagree
    /// (`%Y-%m-%d` beside `epoch = "milliseconds"`) says nothing true.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub epoch: Option<EpochUnit>,
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
            if c.parse.round.is_some() && !matches!(c.dtype, DType::Decimal { .. }) {
                errs.push(format!(
                    "column `{}`: `round` only applies to a decimal column — this one is {}",
                    c.name,
                    crate::commands::describe_dtype(&c.dtype)
                ));
            }
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
            if let Some(ptr) = &c.pointer {
                if !ptr.is_empty() && !ptr.starts_with('/') {
                    errs.push(format!(
                        "column `{}`: `pointer` is an RFC 6901 JSON Pointer and must start \
                         with `/` (or be empty for the whole value); {ptr:?} does not",
                        c.name
                    ));
                }
                if !matches!(self.extraction, Extraction::Json { .. }) {
                    errs.push(format!(
                        "column `{}`: `pointer` reads inside a JSON value, and this file is \
                         read as {}",
                        c.name,
                        self.extraction.format_name()
                    ));
                }
            }
            if let Some(unit) = c.parse.epoch {
                let fmt = match &c.dtype {
                    DType::Timestamp { format, .. } | DType::Date { format } => Some(format),
                    _ => None,
                };
                match fmt {
                    None => errs.push(format!(
                        "column `{}`: `epoch` counts time, which means nothing for a {} column",
                        c.name,
                        dtype_name(&c.dtype)
                    )),
                    Some(f) if f != "%s" => errs.push(format!(
                        "column `{}`: `epoch = {unit:?}` and `format = {f:?}` are two \
                         different claims about how to read this value. Write \
                         `format = \"%s\"` beside an epoch",
                        c.name
                    )),
                    Some(_) => {}
                }
            }
            // A sign convention means nothing outside a number: on a text
            // column it would silently do nothing, and on a date it would eat
            // a bracket somebody meant to keep.
            if c.parse.negative.is_some()
                && !matches!(c.dtype, DType::Decimal { .. } | DType::Float64 | DType::Int64)
            {
                errs.push(format!(
                    "column `{}`: `negative` says how a negative number is written, which                      means nothing for a {} column",
                    c.name,
                    dtype_name(&c.dtype)
                ));
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
            Extraction::Delimited { delimiter, quote, escape, comment, region, .. } => {
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
                if let Some(w) = region {
                    if w.start >= w.end {
                        errs.push(format!(
                            "region: start ({}) must be below end ({})",
                            w.start, w.end
                        ));
                    }
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
                Transform::FillDown { columns, .. } => {
                    if columns.is_empty() {
                        errs.push("fill_down: `columns` must not be empty".into());
                    }
                }
                Transform::SourceName { name, pattern, .. } => {
                    if name.trim().is_empty() {
                        errs.push("source_name: `name` must not be empty".into());
                    }
                    if let Some(p) = pattern {
                        match regex::Regex::new(p) {
                            Err(e) => errs
                                .push(format!("source_name `{name}`: invalid regex: {e}")),
                            Ok(re) if re.captures_len() > 2 => errs.push(format!(
                                "source_name `{name}`: the pattern has {} capture groups, but \
                                 one column takes one value; use a single group",
                                re.captures_len() - 1
                            )),
                            Ok(_) => {}
                        }
                    }
                }
                Transform::Transpose => {
                    // Flipping a table whose names are already established
                    // would turn the header into a column of data and leave
                    // the spec addressing names that no longer run that way.
                    if self.transforms.iter().take_while(|o| !matches!(o, Transform::Transpose)).any(
                        |o| matches!(o, Transform::PromoteHeader { .. }),
                    ) {
                        errs.push(
                            "transpose must come before promote_header: after the flip it is \
                             the first row that holds the names, and a header established \
                             beforehand runs the other way"
                                .into(),
                        );
                    }
                    if self.transforms.iter().filter(|o| matches!(o, Transform::Transpose)).count()
                        > 1
                    {
                        errs.push(
                            "two transposes are the table you started with; one of them is a \
                             mistake"
                                .into(),
                        );
                    }
                }
                Transform::SplitColumn { source, into, by, .. } => {
                    // Splitting into one part is a rename, and tdy has no
                    // rename: `source` -> `name` in `columns` is the only one.
                    if into.len() < 2 {
                        errs.push(format!(
                            "split_column `{source}`: `into` needs at least two names \
                             (splitting into one is a rename, which `columns` already does)"
                        ));
                    }
                    // Two parts landing on one name means one of them is
                    // silently discarded by the projection.
                    let mut seen = std::collections::BTreeSet::new();
                    for n in into {
                        if n.is_empty() {
                            errs.push(format!("split_column `{source}`: an `into` name is empty"));
                        } else if !seen.insert(n) {
                            errs.push(format!(
                                "split_column `{source}`: `{n}` appears twice in `into`"
                            ));
                        }
                    }
                    match by {
                        SplitBy::Delimiter { value } => {
                            if value.is_empty() {
                                errs.push(format!(
                                    "split_column `{source}`: the delimiter is empty, which \
                                     would split between every character"
                                ));
                            }
                        }
                        SplitBy::Positions { at } => {
                            // n cuts make n+1 parts; anything else means the
                            // author counted one of the two wrong.
                            if !at.is_empty() && at.len() + 1 != into.len() {
                                errs.push(format!(
                                    "split_column `{source}`: {} cut position(s) make {} \
                                     parts, but `into` names {}",
                                    at.len(),
                                    at.len() + 1,
                                    into.len()
                                ));
                            }
                            if at.is_empty() {
                                errs.push(format!(
                                    "split_column `{source}`: `at` must name at least one \
                                     cut position"
                                ));
                            }
                            if at.windows(2).any(|w| w[1] <= w[0]) {
                                errs.push(format!(
                                    "split_column `{source}`: cut positions must increase; \
                                     {at:?} does not"
                                ));
                            }
                        }
                        SplitBy::Regex { pattern } => match regex::Regex::new(pattern) {
                            Err(e) => errs.push(format!(
                                "split_column `{source}`: `pattern` is not a valid regex: {e}"
                            )),
                            Ok(re) => {
                                // captures_len() counts group 0, the whole match.
                                let groups = re.captures_len() - 1;
                                if groups != into.len() {
                                    errs.push(format!(
                                        "split_column `{source}`: the pattern has {groups} \
                                         capture group(s) but `into` names {} column(s)",
                                        into.len()
                                    ));
                                }
                            }
                        },
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
                Transform::Constant { name, .. } => {
                    if name.trim().is_empty() {
                        errs.push("constant: `name` must not be blank".into());
                    }
                    // Two constants by one name would silently resolve to the
                    // first; the executor also refuses a name the file already
                    // has, but that needs the header and belongs there.
                    let dup = self
                        .transforms
                        .iter()
                        .filter(|o| matches!(o, Transform::Constant { name: n, .. } if n == name))
                        .count();
                    if dup > 1 {
                        errs.push(format!("constant: `{name}` is declared twice"));
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
    /// Every way of miscounting a split, refused with the count named. These
    /// are all mistakes whose result would otherwise be a column quietly
    /// holding the wrong half of a value.
    #[test]
    fn a_split_that_does_not_add_up_is_refused() {
        let base = |by, into: Vec<&str>| {
            let mut spec = minimal_bool_spec();
            spec.transforms.push(Transform::SplitColumn {
                source: "x".into(),
                into: into.into_iter().map(String::from).collect(),
                by,
                on_short: ShortSplit::Error,
            });
            spec
        };
        let err = |s: ParseSpec, want: &str| {
            let e = format!("{:?}", s.validate().expect_err("must be refused"));
            assert!(e.contains(want), "expected {want:?} in {e}");
        };

        // One part is a rename, and `columns` already renames.
        err(base(SplitBy::Delimiter { value: ",".into() }, vec!["a"]), "at least two");
        // Two parts on one name: the projection would drop one silently.
        err(base(SplitBy::Delimiter { value: ",".into() }, vec!["a", "a"]), "twice");
        // Splitting on nothing splits between every character.
        err(base(SplitBy::Delimiter { value: String::new() }, vec!["a", "b"]), "delimiter is empty");
        // One cut makes two parts, not three.
        err(base(SplitBy::Positions { at: vec![4] }, vec!["a", "b", "c"]), "make 2");
        err(base(SplitBy::Positions { at: vec![6, 4] }, vec!["a", "b", "c"]), "must increase");
        // A pattern whose groups do not match the names it is given.
        err(base(SplitBy::Regex { pattern: "(a)(b)(c)".into() }, vec!["x", "y"]), "3 capture");
        err(base(SplitBy::Regex { pattern: "(".into() }, vec!["x", "y"]), "not a valid regex");

        base(SplitBy::Delimiter { value: ", ".into() }, vec!["a", "b"])
            .validate()
            .expect("two names, a real delimiter: nothing to complain about");
        base(SplitBy::Positions { at: vec![4, 6] }, vec!["a", "b", "c"])
            .validate()
            .expect("two cuts, three parts");
    }

    /// A sign convention on a text column would silently do nothing, and on a
    /// date it would eat a bracket somebody meant to keep.
    #[test]
    fn a_negative_style_is_refused_outside_a_number() {
        let mut spec = minimal_bool_spec();
        spec.columns[0].parse.negative = Some(NegativeStyle::Parentheses);
        let e = spec.validate().expect_err("`negative` means nothing for a boolean");
        assert!(format!("{e:?}").contains("negative"), "{e:?}");

        spec.columns[0].dtype = DType::Decimal { precision: 12, scale: 2 };
        spec.columns[0].parse.true_values.clear();
        spec.columns[0].parse.false_values.clear();
        spec.validate().expect("on a decimal it is exactly what it is for");
    }

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
                pointer: None,
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
                region: None,
            },
            transforms: vec![],
            columns: vec![ColumnSpec {
                name: "a".into(),
                source: None,
                dtype: DType::Utf8,
                nullable: true,
                parse: ValueParsing::default(),
                pointer: None,
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
            region: None,
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
            region: None,
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
            region: None,
        };
        assert!(errs(&s).iter().any(|e| e.contains("unknown encoding")));
        s.extraction = Extraction::Delimited {
            delimiter: ',',
            quote: Some('"'),
            escape: None,
            encoding: Some("windows-1252".into()),
            comment: None,
            ragged: RaggedPolicy::Error,
            region: None,
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

    #[test]
    fn a_row_window_must_be_forward() {
        let mut s = minimal_spec();
        if let Extraction::Delimited { region, .. } = &mut s.extraction {
            *region = Some(RowWindow { start: 5, end: 5, ordinal: 1 });
        }
        let errs = s.validate().unwrap_err();
        assert!(errs.iter().any(|e| e.contains("region") && e.contains("start")), "{errs:?}");
    }
}
