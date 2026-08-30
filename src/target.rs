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
    ColumnOption, DataType as SqlType, ExactNumberInfo, Expr, SqlOption, Statement, Value,
};
use datafusion::sql::sqlparser::dialect::GenericDialect;
use datafusion::sql::sqlparser::parser::Parser;

use crate::spec::parse_fixed_offset;

/// One declared output column.
#[derive(Debug, Clone, PartialEq)]
pub struct TargetColumn {
    pub name: String,
    /// The Arrow type this column must have. Held as Arrow rather than as
    /// [`crate::spec::DType`] on purpose: `DType::Date` carries a per-file
    /// strftime format that a target must not pin, so comparing `DType`s would
    /// force the target to invent one. The Arrow type is exactly the part that
    /// is common to every file, which is exactly what a contract should hold.
    pub dtype: ArrowType,
    pub nullable: bool,
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

        let mut creates = statements.into_iter().filter_map(|s| match s {
            Statement::CreateTable(c) => Some(c),
            _ => None,
        });
        let create = creates
            .next()
            .ok_or_else(|| anyhow::anyhow!("no CREATE TABLE statement found in the target"))?;
        if creates.next().is_some() {
            anyhow::bail!(
                "a target declares exactly one dataset, but this file has more than one \
                 CREATE TABLE. Split them into one file each."
            );
        }

        let name = create.name.0.last().map(|i| i.value.clone()).unwrap_or_default();

        let mut columns = Vec::with_capacity(create.columns.len());
        let mut errs: Vec<String> = Vec::new();
        for c in &create.columns {
            let cname = c.name.value.clone();
            // SQL's default is nullable; NOT NULL and an explicit NULL both
            // say so outright.
            let mut nullable = true;
            for opt in &c.options {
                match opt.option {
                    ColumnOption::NotNull => nullable = false,
                    ColumnOption::Null => nullable = true,
                    _ => errs.push(format!(
                        "column `{cname}`: unsupported column option {}. A target declares a \
                         name, a type and a nullability; constraints are not enforced and \
                         would be a promise tdy cannot keep.",
                        opt.option
                    )),
                }
            }
            match arrow_type_of(&c.data_type) {
                Ok(dtype) => columns.push(TargetColumn { name: cname, dtype, nullable }),
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
        };
        for opt in &create.with_options {
            if let Err(e) = t.apply_option(opt) {
                errs.push(e);
            }
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
                     date_order, verify."
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
                if parse_fixed_offset(tz).is_none() {
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
        SqlType::Text | SqlType::String(_) | SqlType::Varchar(_) | SqlType::Char(_)
        | SqlType::CharVarying(_) | SqlType::Nvarchar(_) | SqlType::Clob(_) => ArrowType::Utf8,

        SqlType::Boolean | SqlType::Bool => ArrowType::Boolean,

        SqlType::BigInt(_) | SqlType::Int64 | SqlType::Int(_) | SqlType::Integer(_)
        | SqlType::SmallInt(_) | SqlType::TinyInt(_) => ArrowType::Int64,

        SqlType::Double(_) | SqlType::DoublePrecision | SqlType::Float64 | SqlType::Float(_)
        | SqlType::Real => ArrowType::Float64,

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

        SqlType::Timestamp(_, tz) => {
            use datafusion::sql::sqlparser::ast::TimezoneInfo;
            let zone = match tz {
                TimezoneInfo::None | TimezoneInfo::WithoutTimeZone => None,
                // `WITH TIME ZONE` says the values carry an offset but not
                // which one. tdy needs the offset itself to convert to UTC, so
                // this has to be spelled out rather than implied.
                TimezoneInfo::WithTimeZone | TimezoneInfo::Tz => {
                    return Err(
                        "TIMESTAMP WITH TIME ZONE does not say which offset. Declare it in \
                         the sidecar's `timezone` instead, or use TIMESTAMP for local \
                         wall-clock values."
                            .to_string(),
                    )
                }
            };
            ArrowType::Timestamp(TimeUnit::Microsecond, zone)
        }

        other => {
            return Err(format!(
                "unsupported type {other}. A target may declare: TEXT, BOOLEAN, BIGINT, \
                 DOUBLE, DECIMAL(p,s), DATE, TIMESTAMP."
            ))
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

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

    #[test]
    fn timestamp_with_time_zone_is_refused_for_not_saying_which() {
        let e = Target::parse("CREATE TABLE s (a TIMESTAMP WITH TIME ZONE) WITH (files='x')")
            .unwrap_err();
        assert!(format!("{e:#}").contains("does not say which offset"));
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

    /// A file that declared two datasets and had only its first read would be
    /// the quietest possible way to query the wrong data.
    #[test]
    fn more_than_one_create_table_is_refused() {
        let e = Target::parse(
            "CREATE TABLE a (x TEXT) WITH (files='1'); CREATE TABLE b (y TEXT) WITH (files='2');",
        )
        .unwrap_err();
        assert!(format!("{e:#}").contains("exactly one dataset"));
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
