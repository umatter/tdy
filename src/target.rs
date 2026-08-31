//! The dataset you want, declared in SQL.
//!
//! Everything else in tdy describes a *source*: this file is the shape of a
//! file you have. A target is the opposite — the shape of the data you want,
//! written by hand, checked into git, and the only statement of intent in the
//! system.
//!
//! ```sql
//! CREATE TABLE sales (
//!   month        DATE           NOT NULL,
//!   region       TEXT           NOT NULL,
//!   amount_chf   DECIMAL(14,2)  NOT NULL,
//!   discount_pct DOUBLE             NULL
//! )
//! WITH (
//!   files      = '2025-*.csv, 2025-*.xlsx',
//!   date_order = 'dmy'
//! );
//! ```
//!
//! It is real SQL, parsed by the same `sqlparser` DataFusion itself uses, so
//! the type vocabulary is SQL's and costs no dependency of its own. See
//! `docs/design/2026-08-30-target-schema.md` for why it is SQL rather than
//! another TOML file.
//!
//! # What a target may and may not say
//!
//! A target constrains exactly what reaches the Arrow schema: a name, a type,
//! a nullability. Nothing else, and the omissions are deliberate.
//!
//! **No strftime format.** A date format is a property of a *file* — twelve
//! monthly exports plausibly carry twelve of them — and it does not reach the
//! Arrow schema, so a target carrying one would be constraining something it
//! cannot check. Twelve formats land on one `DATE` column with no ceremony.
//! What replaces it, because "let the planner pick" is unacceptable when two
//! formats both parse, is the dataset-level `date_order` hint, which
//! constrains the planner's candidate set and nothing else.
//!
//! **No unit.** A unit label whose failure mode is silence is not a unit
//! system, it is a comment with a keyword — and `unit = 'CHF'` on a column no
//! file was ever checked against reads to a reviewer as a verified property.
//!
//! **A timezone, yes**, because that one *is* part of the Arrow type. The same
//! fixed-offset rule as a sidecar applies, enforced here as well: a target is
//! exactly where somebody will try to write `Europe/Zurich`, so the refusal has
//! to live in the parser rather than being discovered later.

use std::collections::HashSet;
use std::path::Path;

use anyhow::{Context, Result};
use datafusion::arrow::datatypes::{DataType as ArrowType, Field, Schema, TimeUnit};
use datafusion::sql::sqlparser::ast::{
    ColumnOption, DataType as SqlType, ExactNumberInfo, Expr, Ident, SqlOption, Statement, Value,
};
use datafusion::sql::sqlparser::dialect::GenericDialect;
use datafusion::sql::sqlparser::parser::Parser;

use crate::spec::parse_fixed_offset;

/// What one per-column `OPTIONS(...)` entry said.
enum ColOpt {
    Matches(Vec<String>),
    IfMissingNull,
}

/// A per-column `OPTIONS(...)` entry.
fn column_option(o: &SqlOption) -> std::result::Result<ColOpt, String> {
    let (key, value) = match o {
        SqlOption::KeyValue { key, value } => (key.value.to_ascii_lowercase(), value),
        other => return Err(format!("unsupported column option {other}; write `key = 'value'`")),
    };
    let text = literal_string(value)
        .ok_or_else(|| format!("column option `{key}` must be a quoted string"))?;
    match key.as_str() {
        "matches" => Ok(ColOpt::Matches(split_globs(&text))),
        "if_missing" => match text.to_ascii_lowercase().as_str() {
            // Only null. A default *value* would be data the file never
            // contained, invented at plan time, which is exactly the class of
            // step this tool gates behind a human; write it in the sidecar as
            // a `constant` transform instead, where review applies.
            "null" => Ok(ColOpt::IfMissingNull),
            other => Err(format!(
                "if_missing = {other:?} is not supported; the only declarable fallback is \
                 'null' (a constant value belongs in the sidecar, gated by review)"
            )),
        },
        other => Err(format!(
            "unknown column option `{other}`. Known: matches (header cells this column \
             may be read from), if_missing ('null' to fill the column with nulls in a \
             file that lacks it)."
        )),
    }
}

/// An identifier's name, folded the way SQL folds it.
///
/// Unquoted identifiers are case-insensitive in SQL and DataFusion lowercases
/// them when planning, and tdy's own output columns are lowercased by
/// `sniff::sanitize`. So a target written the natural way —
/// `CREATE TABLE sales (Betrag DECIMAL(14,2))`, copying the spelling off the
/// spreadsheet — has to fold too, or it could never match a column tdy
/// produces. A quoted `"Betrag"` keeps its case, which is exactly what quoting
/// means everywhere else in SQL.
fn ident_name(i: &Ident) -> String {
    match i.quote_style {
        Some(_) => i.value.clone(),
        None => i.value.to_lowercase(),
    }
}

fn first_line(s: &str) -> String {
    s.lines().next().unwrap_or("").chars().take(80).collect()
}

/// One declared output column.
#[derive(Debug, Clone, PartialEq)]
pub struct TargetColumn {
    pub name: String,
    /// Header cells this column may be read from, beyond its own name.
    ///
    /// Needed because a target names what you *want* — `amount_chf` — while
    /// the files are somebody else's exports and say `Betrag`, `Betrag CHF`,
    /// `Amount`. No amount of normalising bridges that; only a human saying
    /// so does. They are declared, versioned and reviewable, which is the
    /// point: the alternative is a planner guessing at synonyms.
    pub matches: Vec<String>,
    /// The Arrow type this column must have. Held as Arrow rather than as
    /// [`crate::spec::DType`] on purpose: `DType::Date` carries a per-file
    /// strftime format that a target must not pin, so comparing `DType`s would
    /// force the target to invent one. The Arrow type is exactly the part that
    /// is common to every file, which is exactly what a contract should hold.
    pub dtype: ArrowType,
    pub nullable: bool,
    /// `if_missing = 'null'`: a member that has no source for this column
    /// still fits, with the column null in every row.
    ///
    /// This is the *declared-absent* case — one export predates the column —
    /// and the declaration is what authorises it: the fill is written here,
    /// versioned and reviewed, so the planner is executing a decision rather
    /// than making one. Only valid on a nullable column, refused otherwise at
    /// parse, because a NOT NULL column of nulls could never conform anyway
    /// and the contradiction should be caught in the file that states it.
    pub if_missing_null: bool,
}

/// How a file's header cell is matched to a declared column name.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum MatchMode {
    /// Byte-for-byte.
    Exact,
    /// Case, whitespace and punctuation folded — `Betrag (CHF)` == `betrag chf`.
    #[default]
    Normalized,
}

/// Which reading of an all-numeric date is preferred when more than one parses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DateOrder {
    Dmy,
    Mdy,
    Ymd,
}

/// How much of a member is read before its plan is accepted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Verify {
    /// Every row. The honest default: shape is proved statically, but values
    /// are only proved by parsing them.
    #[default]
    Full,
    /// The bounded prefix `dry_run` already reads.
    Head,
}

/// A declared dataset: the columns, and where its members come from.
#[derive(Debug, Clone)]
pub struct Target {
    pub name: String,
    pub columns: Vec<TargetColumn>,
    /// Globs, relative to the directory holding the target file.
    pub files: Vec<String>,
    /// Globs subtracted from `files`.
    pub exclude: Vec<String>,
    pub match_mode: MatchMode,
    pub date_order: Option<DateOrder>,
    pub verify: Verify,
    /// Which character is the decimal point in these files.
    ///
    /// Not part of the Arrow type — like `date_order`, it constrains only the
    /// planner. It exists for the same reason: `1.234` is either one-point-two
    /// or one thousand two hundred and thirty-four, nothing in the column says
    /// which, and guessing is a thousandfold error nobody would notice.
    pub decimal_separator: Option<char>,
    /// The fixed offset every `TIMESTAMP WITH TIME ZONE` column carries.
    ///
    /// The offset is part of the Arrow type, so a target has to be able to say
    /// it — otherwise a spec that declares one could never conform to anything
    /// a target is able to express. SQL has no per-column syntax for a
    /// specific offset, so it is declared once for the dataset.
    pub timezone: Option<String>,
}

impl Target {
    /// Parse a target from SQL text.
    ///
    /// Accepts exactly one `CREATE TABLE` statement. Anything else is refused
    /// with a message naming what was found, because a file that silently
    /// declared only its first of two tables would be the quietest possible
    /// way to query the wrong dataset.
    pub fn parse(sql: &str) -> Result<Target> {
        let statements = Parser::parse_sql(&GenericDialect {}, sql)
            .map_err(|e| anyhow::anyhow!("{e}"))
            .context("the target is not valid SQL")?;

        // Exactly one statement, and it must be the declaration. Anything else
        // — a second CREATE TABLE, an ALTER, a DROP, a stray SELECT — is
        // refused rather than skipped. A target file whose second half was
        // silently ignored is the quietest possible way to query something
        // other than what the file appears to say.
        if statements.len() != 1 {
            anyhow::bail!(
                "a target is exactly one CREATE TABLE statement, but this file has {}. \
                 tdy does not execute SQL from a target — it only reads the declaration — \
                 so anything else here would be silently ignored.",
                statements.len()
            );
        }
        let create = match statements.into_iter().next() {
            Some(Statement::CreateTable(c)) => c,
            Some(other) => anyhow::bail!(
                "a target must be a CREATE TABLE statement; this file starts with:\n  {}",
                first_line(&other.to_string())
            ),
            None => anyhow::bail!("the target file is empty"),
        };

        // A CTAS or a LIKE/CLONE has no column list of its own to check
        // against, and executing the query is not something a target does.
        if create.query.is_some() {
            anyhow::bail!(
                "a target declares columns, it does not compute them: \
                 `CREATE TABLE … AS SELECT` is not a target. Write the column list out."
            );
        }
        if create.like.is_some() || create.clone.is_some() {
            anyhow::bail!(
                "`CREATE TABLE … LIKE`/`CLONE` copies a shape tdy cannot see. \
                 Write the column list out."
            );
        }
        // Table-level constraints land here rather than on a column, and were
        // being dropped in silence while the identical column-level clause was
        // refused. Both are promises tdy will not keep, so both are refused.
        if let Some(c) = create.constraints.first() {
            anyhow::bail!(
                "unsupported table constraint `{c}`. A target declares names, types and \
                 nullability; PRIMARY KEY, UNIQUE, CHECK and FOREIGN KEY are not enforced \
                 and would be a promise tdy cannot keep."
            );
        }

        let name = create
            .name
            .0
            .last()
            .map(ident_name)
            .unwrap_or_default();

        let mut columns = Vec::with_capacity(create.columns.len());
        let mut errs: Vec<String> = Vec::new();
        for c in &create.columns {
            let cname = ident_name(&c.name);
            // SQL's default is nullable; NOT NULL and an explicit NULL both
            // say so outright.
            let mut nullable = true;
            let mut matches: Vec<String> = Vec::new();
            let mut if_missing_null = false;
            for opt in &c.options {
                match &opt.option {
                    ColumnOption::NotNull => nullable = false,
                    ColumnOption::Null => nullable = true,
                    ColumnOption::Options(opts) => {
                        for o in opts {
                            match column_option(o) {
                                Ok(ColOpt::Matches(m)) => matches.extend(m),
                                Ok(ColOpt::IfMissingNull) => if_missing_null = true,
                                Err(e) => errs.push(format!("column `{cname}`: {e}")),
                            }
                        }
                    }
                    _ => errs.push(format!(
                        "column `{cname}`: unsupported column option {}. A target declares a \
                         name, a type and a nullability; constraints are not enforced and \
                         would be a promise tdy cannot keep.",
                        opt.option
                    )),
                }
            }
            if if_missing_null && !nullable {
                errs.push(format!(
                    "column `{cname}`: if_missing = 'null' on a NOT NULL column is a \
                     contradiction — a file without the column would produce nulls the \
                     column forbids"
                ));
            }
            match arrow_type_of(&c.data_type) {
                Ok(dtype) => columns.push(TargetColumn {
                    name: cname,
                    matches,
                    dtype,
                    nullable,
                    if_missing_null,
                }),
                Err(e) => errs.push(format!("column `{cname}`: {e}")),
            }
        }

        let mut t = Target {
            name,
            columns,
            files: Vec::new(),
            exclude: Vec::new(),
            match_mode: MatchMode::default(),
            date_order: None,
            verify: Verify::default(),
            timezone: None,
            decimal_separator: None,
        };
        // A target is hand-written and merge-conflict-prone. Two settings of
        // one option is a contradiction, and last-one-wins would resolve it
        // silently — the same reason `spec::validate` refuses duplicate column
        // names. `files` and `exclude` are lists and accumulate by design.
        let mut seen_opts: Vec<String> = Vec::new();
        for opt in &create.with_options {
            if let SqlOption::KeyValue { key, .. } = opt {
                let k = key.value.to_ascii_lowercase();
                if !matches!(k.as_str(), "files" | "exclude") {
                    if seen_opts.contains(&k) {
                        errs.push(format!(
                            "WITH option `{k}` is set more than once; remove one. \
                             (`files` and `exclude` may repeat — they are lists.)"
                        ));
                        continue;
                    }
                    seen_opts.push(k);
                }
            }
            if let Err(e) = t.apply_option(opt) {
                errs.push(e);
            }
        }

        // Resolve the placeholder left by `TIMESTAMP WITH TIME ZONE`, now that
        // the whole option list has been read.
        let zoned = t
            .columns
            .iter()
            .any(|c| matches!(&c.dtype, ArrowType::Timestamp(_, Some(z)) if z.as_ref() == TZ_PLACEHOLDER));
        match (&t.timezone, zoned) {
            (Some(tz), _) => {
                let tz = tz.clone();
                for c in &mut t.columns {
                    if let ArrowType::Timestamp(u, Some(z)) = &c.dtype {
                        if z.as_ref() == TZ_PLACEHOLDER {
                            c.dtype = ArrowType::Timestamp(*u, Some(tz.clone().into()));
                        }
                    }
                }
            }
            (None, true) => errs.push(
                "a TIMESTAMP WITH TIME ZONE column needs the offset it is in: add \
                 `WITH (timezone = '+02:00')`. The offset is part of the type, so a target \
                 that leaves it out is not saying which instant a value means."
                    .to_string(),
            ),
            (None, false) => {}
        }

        if !errs.is_empty() {
            anyhow::bail!("{}", errs.join("\n"));
        }
        t.validate().map_err(|e| anyhow::anyhow!("{}", e.join("\n")))?;
        Ok(t)
    }

    /// Read and parse a target file.
    pub fn load(path: &Path) -> Result<Target> {
        let sql = std::fs::read_to_string(path)
            .with_context(|| format!("cannot read target {}", path.display()))?;
        Target::parse(&sql).with_context(|| format!("in {}", path.display()))
    }

    fn apply_option(&mut self, opt: &SqlOption) -> std::result::Result<(), String> {
        let (key, value) = match opt {
            SqlOption::KeyValue { key, value } => (key.value.to_ascii_lowercase(), value),
            other => {
                return Err(format!(
                    "unsupported WITH option {other}; write `key = 'value'`"
                ))
            }
        };
        let text = literal_string(value).ok_or_else(|| {
            format!("WITH option `{key}` must be a quoted string, e.g. {key} = 'value'")
        })?;
        match key.as_str() {
            // A comma-separated list, because SQL has no array literal every
            // dialect agrees on. Repeating the option also works.
            "files" => self.files.extend(split_globs(&text)),
            "exclude" => self.exclude.extend(split_globs(&text)),
            "match" => {
                self.match_mode = match text.to_ascii_lowercase().as_str() {
                    "exact" => MatchMode::Exact,
                    "normalized" | "normalised" => MatchMode::Normalized,
                    other => return Err(format!("unknown match mode {other:?}; use 'exact' or 'normalized'")),
                }
            }
            "date_order" => {
                self.date_order = Some(match text.to_ascii_lowercase().as_str() {
                    "dmy" => DateOrder::Dmy,
                    "mdy" => DateOrder::Mdy,
                    "ymd" => DateOrder::Ymd,
                    other => return Err(format!("unknown date_order {other:?}; use 'dmy', 'mdy' or 'ymd'")),
                })
            }
            "decimal_separator" => {
                let mut cs = text.chars();
                match (cs.next(), cs.next()) {
                    (Some(c @ ('.' | ',')), None) => self.decimal_separator = Some(c),
                    _ => {
                        return Err(format!(
                            "decimal_separator must be '.' or ',', not {text:?}"
                        ))
                    }
                }
            }
            "timezone" => {
                if parse_fixed_offset(&text).is_none() {
                    return Err(format!(
                        "timezone {text:?} is not a fixed offset. Use \"UTC\", \"+02:00\" or \
                         \"-0500\"; named zones are not resolved because daylight saving \
                         cannot be guessed from the value alone."
                    ));
                }
                self.timezone = Some(text);
            }
            "verify" => {
                self.verify = match text.to_ascii_lowercase().as_str() {
                    "full" => Verify::Full,
                    "head" => Verify::Head,
                    other => return Err(format!("unknown verify mode {other:?}; use 'full' or 'head'")),
                }
            }
            other => {
                return Err(format!(
                    "unknown WITH option `{other}`. Known options: files, exclude, match, \
                     date_order, verify, timezone, decimal_separator."
                ))
            }
        }
        Ok(())
    }

    /// Everything the executor would otherwise discover by failing.
    ///
    /// A target is hand-written, so it is untrusted input in exactly the way a
    /// sidecar is, and the same rule applies: anything that would surface later
    /// as a confusing failure belongs here as a sentence.
    pub fn validate(&self) -> std::result::Result<(), Vec<String>> {
        let mut errs = Vec::new();

        if self.name.trim().is_empty() {
            errs.push("the dataset has no name".to_string());
        }
        if self.columns.is_empty() {
            errs.push("a target must declare at least one column".to_string());
        }

        let mut seen = HashSet::new();
        let mut seen_norm: Vec<(String, String)> = Vec::new();
        for c in &self.columns {
            if c.name.trim().is_empty() {
                errs.push("a column has an empty name".to_string());
            }
            if !seen.insert(c.name.as_str()) {
                errs.push(format!("duplicate column name `{}`", c.name));
            }
            // Two columns that differ only by case or punctuation cannot both
            // be matched under `match = 'normalized'`: whichever bound first
            // would take the other's data.
            let n = norm(&c.name);
            if let Some((other, _)) = seen_norm.iter().find(|(_, on)| *on == n) {
                if self.match_mode == MatchMode::Normalized {
                    errs.push(format!(
                        "columns `{other}` and `{}` are the same under `match = 'normalized'` \
                         (both normalise to {n:?}); rename one, or declare `match = 'exact'`",
                        c.name
                    ));
                }
            } else {
                seen_norm.push((c.name.clone(), n));
            }

            if let ArrowType::Decimal128(p, s) = c.dtype {
                if p == 0 || p > 38 {
                    errs.push(format!(
                        "column `{}`: decimal precision {p} is out of range; Arrow \
                         Decimal128 allows 1..=38",
                        c.name
                    ));
                }
                if s < 0 || s as u8 > p {
                    errs.push(format!(
                        "column `{}`: decimal scale {s} must be between 0 and the \
                         precision ({p})",
                        c.name
                    ));
                }
            }
            if let ArrowType::Timestamp(_, Some(tz)) = &c.dtype {
                if tz.as_ref() == TZ_PLACEHOLDER {
                    errs.push(format!(
                        "column `{}`: TIMESTAMP WITH TIME ZONE needs `WITH (timezone = …)`",
                        c.name
                    ));
                } else if parse_fixed_offset(tz).is_none() {
                    errs.push(format!(
                        "column `{}`: timezone {tz:?} is not a fixed offset. Use \"UTC\", \
                         \"+02:00\" or \"-0500\"; named zones are not resolved because \
                         daylight saving cannot be guessed from the value alone.",
                        c.name
                    ));
                }
            }
        }

        if self.files.is_empty() {
            errs.push(
                "no source files declared; add `WITH (files = 'exports/*.csv')`".to_string(),
            );
        }

        if errs.is_empty() {
            Ok(())
        } else {
            Err(errs)
        }
    }

    /// The Arrow schema every member must produce, field for field.
    ///
    /// This is the whole contract. A spec conforms when
    /// `engine::schema_of(spec)` equals this.
    pub fn arrow_schema(&self) -> Schema {
        Schema::new(
            self.columns
                .iter()
                .map(|c| Field::new(&c.name, c.dtype.clone(), c.nullable))
                .collect::<Vec<_>>(),
        )
    }
}

/// Fold a header cell or column name for `match = 'normalized'`.
///
/// Case, surrounding whitespace, runs of whitespace, and the punctuation that
/// spreadsheet headers acquire and lose between exports — `Betrag (CHF)` and
/// `betrag chf` are the same column, written twice by the same accounting
/// package in different months.
///
/// Deliberately *not* folded: anything that changes meaning rather than
/// spelling. `Umsatz %` keeps its percent sign, so it never collides with
/// `Umsatz` — a column of percentages and a column of francs matching each
/// other is precisely the quiet wrong number this tool exists to refuse.
pub fn norm(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut pending_space = false;
    for ch in s.trim().chars() {
        let c = if ch.is_whitespace() || matches!(ch, '(' | ')' | '[' | ']' | '_' | '-' | '.' | ',' | ':' | ';') {
            ' '
        } else {
            ch
        };
        if c == ' ' {
            pending_space = !out.is_empty();
            continue;
        }
        if pending_space {
            out.push(' ');
            pending_space = false;
        }
        out.extend(c.to_lowercase());
    }
    out
}

fn split_globs(s: &str) -> Vec<String> {
    s.split(',').map(|g| g.trim().to_string()).filter(|g| !g.is_empty()).collect()
}

fn literal_string(e: &Expr) -> Option<String> {
    match e {
        Expr::Value(Value::SingleQuotedString(s))
        | Expr::Value(Value::DoubleQuotedString(s))
        | Expr::Value(Value::EscapedStringLiteral(s)) => Some(s.clone()),
        Expr::Value(Value::Number(n, _)) => Some(n.clone()),
        Expr::Identifier(i) => Some(i.value.clone()),
        _ => None,
    }
}

/// SQL type -> the Arrow type a member must produce.
///
/// Only the types tdy can actually produce are accepted. A target that named
/// `INTERVAL` would parse as SQL and then be unsatisfiable by any spec, so it
/// is refused here with the list of what is available rather than failing
/// later as a conformance mismatch nobody can fix.
fn arrow_type_of(t: &SqlType) -> std::result::Result<ArrowType, String> {
    Ok(match t {
        // Unqualified string types: no promise beyond "text".
        SqlType::Text | SqlType::String(None) | SqlType::Varchar(None)
        | SqlType::Nvarchar(None) | SqlType::Clob(None) => ArrowType::Utf8,

        // A declared length is a constraint tdy does not enforce. Accepting it
        // would put a promise in the contract that nothing checks — the same
        // reason there is no `unit` keyword.
        SqlType::String(Some(_)) | SqlType::Varchar(Some(_)) | SqlType::Char(Some(_))
        | SqlType::CharVarying(Some(_)) | SqlType::Nvarchar(Some(_)) | SqlType::Clob(Some(_)) => {
            return Err(
                "a declared length is not enforced — tdy has one string type. Use TEXT."
                    .to_string(),
            )
        }
        SqlType::Char(None) | SqlType::CharVarying(None) => ArrowType::Utf8,

        SqlType::Boolean | SqlType::Bool => ArrowType::Boolean,

        SqlType::BigInt(None) | SqlType::Int64 => ArrowType::Int64,

        // Narrower integers are a range tdy would not enforce: a value of
        // 3_000_000_000 fits the Int64 it would really produce and violates
        // the INT that was declared. Silently widening is the failure this
        // module refuses everywhere else.
        SqlType::Int(_) | SqlType::Integer(_) | SqlType::SmallInt(_) | SqlType::TinyInt(_)
        | SqlType::MediumInt(_) | SqlType::BigInt(Some(_)) => {
            return Err(
                "tdy has one integer type, 64-bit, and would not enforce a narrower range. \
                 Use BIGINT."
                    .to_string(),
            )
        }

        SqlType::Double(ExactNumberInfo::None) | SqlType::DoublePrecision | SqlType::Float64 => {
            ArrowType::Float64
        }
        SqlType::Real | SqlType::Float(_) | SqlType::Double(_) => {
            return Err(
                "tdy has one floating type, 64-bit. Use DOUBLE — or DECIMAL(p,s) if the \
                 values are money, which is usually the right answer."
                    .to_string(),
            )
        }

        SqlType::Decimal(info) | SqlType::Numeric(info) | SqlType::Dec(info) => {
            let (p, s) = match info {
                ExactNumberInfo::PrecisionAndScale(p, s) => (*p, *s),
                ExactNumberInfo::Precision(p) => (*p, 0),
                // Money without a stated scale is a trap: DECIMAL alone means
                // "some precision" in every dialect and a different one in
                // each. Refusing costs the user eight characters and buys an
                // exact contract.
                ExactNumberInfo::None => {
                    return Err(
                        "DECIMAL needs an explicit precision and scale, e.g. DECIMAL(14,2). \
                         An unqualified DECIMAL means something different in every SQL \
                         dialect, and money is the wrong place to inherit a default."
                            .to_string(),
                    )
                }
            };
            if p == 0 || p > 38 {
                return Err(format!(
                    "decimal precision {p} is out of range; Arrow Decimal128 allows 1..=38"
                ));
            }
            if s > p {
                return Err(format!("decimal scale {s} is larger than its precision {p}"));
            }
            ArrowType::Decimal128(p as u8, s as i8)
        }

        SqlType::Date => ArrowType::Date32,

        SqlType::Timestamp(precision, tz) => {
            use datafusion::sql::sqlparser::ast::TimezoneInfo;
            // tdy stores microseconds. A declared precision that is not 6
            // would be a promise about resolution that nothing enforces.
            if let Some(p) = precision {
                if *p != 6 {
                    return Err(format!(
                        "TIMESTAMP({p}) is not enforced — tdy stores microseconds. \
                         Use TIMESTAMP, or TIMESTAMP(6)."
                    ));
                }
            }
            match tz {
                TimezoneInfo::None | TimezoneInfo::WithoutTimeZone => {
                    ArrowType::Timestamp(TimeUnit::Microsecond, None)
                }
                // `WITH TIME ZONE` says the values carry an offset but not
                // which one, and the offset is part of the Arrow type. The
                // dataset-level `timezone` option supplies it; without that,
                // refusing here is the only honest answer, since a spec that
                // declares an offset could otherwise never conform to
                // anything a target is able to say.
                TimezoneInfo::WithTimeZone | TimezoneInfo::Tz => {
                    ArrowType::Timestamp(TimeUnit::Microsecond, Some(TZ_PLACEHOLDER.into()))
                }
            }
        }

        other => {
            return Err(format!(
                "unsupported type {other}. A target may declare: TEXT, BOOLEAN, BIGINT, \
                 DOUBLE, DECIMAL(p,s), DATE, TIMESTAMP, TIMESTAMP WITH TIME ZONE."
            ))
        }
    })
}

/// Stands in for an offset between parsing a column's type and reading the
/// dataset-level `timezone` option, which may appear after the column list.
/// Never survives `Target::parse`: `validate()` refuses it.
const TZ_PLACEHOLDER: &str = "\u{0}pending";

#[cfg(test)]
mod tests {
    use super::*;

    /// `Result::expect_err` without requiring Target: Debug in the message.
    trait ExpectErrMsg {
        fn expect_err_msg(self, msg: &str) -> anyhow::Error;
    }
    impl ExpectErrMsg for Result<Target> {
        fn expect_err_msg(self, msg: &str) -> anyhow::Error {
            match self {
                Ok(_) => panic!("{msg}"),
                Err(e) => e,
            }
        }
    }

    fn t(sql: &str) -> Target {
        Target::parse(sql).unwrap_or_else(|e| panic!("{e:#}"))
    }

    const MIN: &str = "CREATE TABLE s (a TEXT) WITH (files = 'x.csv')";

    #[test]
    fn a_minimal_target_parses() {
        let g = t(MIN);
        assert_eq!(g.name, "s");
        assert_eq!(g.columns.len(), 1);
        assert_eq!(g.files, vec!["x.csv"]);
        assert!(g.columns[0].nullable, "SQL's default is nullable");
    }

    #[test]
    fn the_type_vocabulary_maps_onto_arrow() {
        let g = t("CREATE TABLE s (
            a TEXT, b BOOLEAN, c BIGINT, d DOUBLE, e DECIMAL(14,2), f DATE, g TIMESTAMP
        ) WITH (files = 'x.csv')");
        let got: Vec<ArrowType> = g.columns.iter().map(|c| c.dtype.clone()).collect();
        assert_eq!(
            got,
            vec![
                ArrowType::Utf8,
                ArrowType::Boolean,
                ArrowType::Int64,
                ArrowType::Float64,
                ArrowType::Decimal128(14, 2),
                ArrowType::Date32,
                ArrowType::Timestamp(TimeUnit::Microsecond, None),
            ]
        );
    }

    #[test]
    fn not_null_is_carried_through_to_the_schema() {
        let g = t("CREATE TABLE s (a TEXT NOT NULL, b TEXT NULL, c TEXT) WITH (files='x')");
        assert_eq!(
            g.columns.iter().map(|c| c.nullable).collect::<Vec<_>>(),
            vec![false, true, true]
        );
        let s = g.arrow_schema();
        assert!(!s.field(0).is_nullable());
        assert!(s.field(1).is_nullable());
    }

    /// Money without a scale means something different in every dialect, and
    /// inheriting a default here would be inheriting it for money.
    #[test]
    fn an_unqualified_decimal_is_refused() {
        let e = Target::parse("CREATE TABLE s (a DECIMAL) WITH (files='x')").unwrap_err();
        let m = format!("{e:#}");
        assert!(m.contains("DECIMAL(14,2)"), "unhelpful: {m}");
    }

    #[test]
    fn a_decimal_out_of_arrow_range_is_refused() {
        for bad in ["DECIMAL(0,0)", "DECIMAL(39,2)", "DECIMAL(4,7)"] {
            let sql = format!("CREATE TABLE s (a {bad}) WITH (files='x')");
            assert!(Target::parse(&sql).is_err(), "{bad} was accepted");
        }
    }

    /// The refusal has to live here, not only in the sidecar validator: a
    /// target is exactly where someone will try to write a named zone.
    #[test]
    fn a_named_timezone_is_refused_in_a_target_too() {
        // Reachable only through the struct, since the SQL surface cannot spell
        // a named zone — but validate() is the gate for both routes.
        let mut g = t(MIN);
        g.columns[0].dtype =
            ArrowType::Timestamp(TimeUnit::Microsecond, Some("Europe/Zurich".into()));
        let errs = g.validate().unwrap_err();
        assert!(
            errs.iter().any(|e| e.contains("daylight saving")),
            "{errs:?}"
        );
    }

    /// The offset is part of the Arrow type, so a target has to be able to
    /// say it — otherwise a spec that declares one could never conform to
    /// anything a target can express, and the refusal would point at a dead
    /// end.
    #[test]
    fn a_zoned_timestamp_needs_and_accepts_a_declared_offset() {
        let e = Target::parse("CREATE TABLE s (a TIMESTAMPTZ) WITH (files='x')").unwrap_err();
        assert!(format!("{e:#}").contains("timezone = "), "{e:#}");

        let g = t("CREATE TABLE s (a TIMESTAMPTZ) WITH (files='x', timezone='+02:00')");
        assert_eq!(
            g.columns[0].dtype,
            ArrowType::Timestamp(TimeUnit::Microsecond, Some("+02:00".into()))
        );
        // …and a named zone is refused here exactly as in a sidecar.
        let e = Target::parse(
            "CREATE TABLE s (a TIMESTAMPTZ) WITH (files='x', timezone='Europe/Zurich')",
        )
        .unwrap_err();
        assert!(format!("{e:#}").contains("daylight saving"), "{e:#}");
    }

    /// A declared precision tdy does not honour is a promise in the contract
    /// that nothing checks.
    #[test]
    fn a_timestamp_precision_other_than_microseconds_is_refused() {
        assert!(Target::parse("CREATE TABLE s (a TIMESTAMP(3)) WITH (files='x')").is_err());
        assert!(Target::parse("CREATE TABLE s (a TIMESTAMP(6)) WITH (files='x')").is_ok());
    }

    /// Silently widening a declared type is the same failure as silently
    /// widening a value: the contract says one thing and tdy does another.
    #[test]
    fn types_tdy_would_not_enforce_are_refused_with_the_spelling_that_works() {
        for (bad, needle) in [
            ("SMALLINT", "BIGINT"),
            ("INT", "BIGINT"),
            ("INTEGER", "BIGINT"),
            ("REAL", "DOUBLE"),
            ("FLOAT(24)", "DOUBLE"),
            ("VARCHAR(50)", "TEXT"),
            ("CHAR(3)", "TEXT"),
        ] {
            let sql = format!("CREATE TABLE s (a {bad}) WITH (files='x')");
            let e = Target::parse(&sql)
                .expect_err_msg(&format!("{bad} was silently accepted"));
            assert!(format!("{e:#}").contains(needle), "{bad}: {e:#}");
        }
        // The unqualified spellings are fine — they promise nothing extra.
        for good in ["TEXT", "VARCHAR", "BIGINT", "DOUBLE", "DOUBLE PRECISION"] {
            let sql = format!("CREATE TABLE s (a {good}) WITH (files='x')");
            assert!(Target::parse(&sql).is_ok(), "{good} was refused");
        }
    }

    #[test]
    fn an_unsupported_type_names_what_is_available() {
        let e = Target::parse("CREATE TABLE s (a INTERVAL) WITH (files='x')").unwrap_err();
        let m = format!("{e:#}");
        assert!(m.contains("DECIMAL(p,s)"), "{m}");
    }

    #[test]
    fn a_target_needs_source_files() {
        let e = Target::parse("CREATE TABLE s (a TEXT)").unwrap_err();
        assert!(format!("{e:#}").contains("files"));
    }

    #[test]
    fn duplicate_column_names_are_refused() {
        let e = Target::parse("CREATE TABLE s (a TEXT, a TEXT) WITH (files='x')").unwrap_err();
        assert!(format!("{e:#}").contains("duplicate"));
    }

    /// Under normalized matching these are one column, so a file's header cell
    /// could bind to either — whichever won would take the other's data.
    #[test]
    fn columns_colliding_under_normalization_are_refused() {
        let e = Target::parse("CREATE TABLE s (\"Betrag CHF\" TEXT, \"betrag (chf)\" TEXT) \
                               WITH (files='x')")
            .unwrap_err();
        assert!(format!("{e:#}").contains("normalized"), "{e:#}");
        // …and are fine when the user has asked for exact matching.
        assert!(Target::parse(
            "CREATE TABLE s (\"Betrag CHF\" TEXT, \"betrag (chf)\" TEXT) \
             WITH (files='x', match='exact')"
        )
        .is_ok());
    }

    #[test]
    fn options_are_parsed_and_unknown_ones_are_refused() {
        let g = t("CREATE TABLE s (a TEXT) WITH (
            files = '2025-*.csv, 2025-*.xlsx',
            exclude = '*-entwurf.csv',
            match = 'exact',
            date_order = 'dmy',
            verify = 'head')");
        assert_eq!(g.files, vec!["2025-*.csv", "2025-*.xlsx"]);
        assert_eq!(g.exclude, vec!["*-entwurf.csv"]);
        assert_eq!(g.match_mode, MatchMode::Exact);
        assert_eq!(g.date_order, Some(DateOrder::Dmy));
        assert_eq!(g.verify, Verify::Head);

        let e = Target::parse("CREATE TABLE s (a TEXT) WITH (files='x', colour='red')")
            .unwrap_err();
        assert!(format!("{e:#}").contains("Known options"));
        let e = Target::parse("CREATE TABLE s (a TEXT) WITH (files='x', date_order='xyz')")
            .unwrap_err();
        assert!(format!("{e:#}").contains("dmy"));
    }

    /// A target file whose second half was silently ignored is the quietest
    /// possible way to query something other than what the file says. That
    /// applies to *any* extra statement, not only a second CREATE TABLE —
    /// tdy does not execute SQL from a target, so anything else would vanish.
    #[test]
    fn anything_other_than_the_one_declaration_is_refused() {
        for extra in [
            "CREATE TABLE b (y TEXT) WITH (files='2');",
            "DROP TABLE other;",
            "ALTER TABLE s ADD COLUMN b TEXT;",
            "SELECT 1;",
        ] {
            let sql = format!("CREATE TABLE s (a TEXT) WITH (files='1'); {extra}");
            let e = Target::parse(&sql)
                .expect_err_msg(&format!("a trailing `{extra}` was silently dropped"));
            assert!(format!("{e:#}").contains("exactly one CREATE TABLE"), "{extra}: {e:#}");
        }
        // A statement that is not a declaration at all.
        let e = Target::parse("SELECT 1;").unwrap_err();
        assert!(format!("{e:#}").contains("must be a CREATE TABLE"), "{e:#}");
    }

    /// A table-level constraint was being dropped in silence while the
    /// identical column-level clause was refused.
    #[test]
    fn table_level_constraints_are_refused_like_column_level_ones() {
        for c in [
            "UNIQUE (a)",
            "PRIMARY KEY (a)",
            "CHECK (a <> '')",
            "FOREIGN KEY (a) REFERENCES o(z)",
        ] {
            let sql = format!("CREATE TABLE s (a TEXT, {c}) WITH (files='x')");
            let e = Target::parse(&sql)
                .expect_err_msg(&format!("`{c}` was silently accepted and dropped"));
            assert!(format!("{e:#}").contains("not enforced"), "{c}: {e:#}");
        }
    }

    /// A CTAS body would be computed, not declared, and tdy does not execute
    /// SQL from a target — so it must not look like it worked.
    #[test]
    fn a_computed_table_is_refused() {
        let e = Target::parse(
            "CREATE TABLE s (a TEXT) WITH (files='x') AS SELECT 1",
        );
        // Depending on the dialect this may fail to parse or parse with a
        // query; either way it must not succeed.
        assert!(e.is_err(), "a CTAS target was accepted");
    }

    /// SQL folds unquoted identifiers, and so does tdy's own column naming.
    /// A target written the natural way — copying the spelling off the
    /// spreadsheet — has to fold too, or it could never match.
    #[test]
    fn unquoted_identifiers_fold_like_sql_and_quoted_ones_do_not() {
        let g = t("CREATE TABLE Sales (Betrag DECIMAL(14,2), \"Betrag CHF\" TEXT) \
                   WITH (files='x', match='exact')");
        assert_eq!(g.name, "sales");
        assert_eq!(g.columns[0].name, "betrag", "an unquoted identifier kept its case");
        assert_eq!(g.columns[1].name, "Betrag CHF", "a quoted identifier lost its case");
    }

    /// A hand-written, merge-conflict-prone file that sets one option twice is
    /// a contradiction, and last-one-wins would resolve it silently.
    #[test]
    fn an_option_set_twice_is_refused_but_lists_may_repeat() {
        let e = Target::parse(
            "CREATE TABLE s (a TEXT) WITH (files='x', date_order='dmy', date_order='mdy')",
        )
        .unwrap_err();
        assert!(format!("{e:#}").contains("more than once"), "{e:#}");

        let g = t("CREATE TABLE s (a TEXT) WITH (files='a.csv', files='b.csv')");
        assert_eq!(g.files, vec!["a.csv", "b.csv"]);
    }

    #[test]
    fn a_constraint_is_refused_rather_than_half_enforced() {
        let e = Target::parse("CREATE TABLE s (a TEXT UNIQUE) WITH (files='x')").unwrap_err();
        assert!(format!("{e:#}").contains("not enforced"), "{e:#}");
    }

    #[test]
    fn normalization_folds_spelling_but_not_meaning() {
        assert_eq!(norm("Betrag (CHF)"), norm("betrag chf"));
        assert_eq!(norm("  Betrag   CHF "), "betrag chf");
        assert_eq!(norm("Umsatz_2025"), "umsatz 2025");
        assert_eq!(norm("Grüße"), "grüße");
        // The one that matters: a percentage is not an amount.
        assert_ne!(norm("Umsatz %"), norm("Umsatz"));
    }

    #[test]
    fn the_schema_is_the_contract() {
        let g = t("CREATE TABLE s (m DATE NOT NULL, v DECIMAL(14,2) NOT NULL) WITH (files='x')");
        let s = g.arrow_schema();
        assert_eq!(s.fields().len(), 2);
        assert_eq!(s.field(0).name(), "m");
        assert_eq!(s.field(0).data_type(), &ArrowType::Date32);
        assert_eq!(s.field(1).data_type(), &ArrowType::Decimal128(14, 2));
    }
}
