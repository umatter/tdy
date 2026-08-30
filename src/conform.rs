//! Proving a spec lands on a declared target, before reading a byte.
//!
//! This is the gate the current design does not have. `check_spec` proves
//! "this spec parses the head of this file" — a spec that parses the file into
//! entirely the wrong columns passes it, is written to disk, and is queried.
//! With a target declared, a much stronger and much cheaper thing is provable:
//!
//! > this spec produces **exactly** these columns, with exactly these types and
//! > nullabilities, for every row it will ever emit, on both executors.
//!
//! It is cheap because [`crate::engine::schema_of`] is a pure function of
//! `spec.columns` — it builds every column over *zero* rows, so it opens no
//! file and its answer is the schema of every batch the spec can produce. The
//! comparison is then field-for-field equality, in microseconds, with no I/O.
//!
//! # What this does not prove
//!
//! Shape, not values. A per-row parse failure, a grouping violation, a
//! two-digit `%Y`, a null arriving in a NOT NULL column — none of those are
//! visible here, and all of them are caught per row, loudly, naming the row, at
//! execution. Letting "conforms" sound like a whole-file guarantee would be a
//! new way to be quietly wrong, so every caller says which half it proved.

use datafusion::arrow::datatypes::{DataType as ArrowType, Schema};

use crate::spec::ParseSpec;
use crate::target::Target;

/// One reason a spec does not land on a target.
///
/// Deliberately a list rather than a bool: a user fixing a twelve-file dataset
/// wants every problem in one pass, and "does not conform" is not an
/// actionable sentence.
#[derive(Debug, Clone, PartialEq)]
pub enum Mismatch {
    /// The target declares a column the spec does not produce.
    Missing { column: String, dtype: ArrowType },
    /// The spec produces a column the target does not declare.
    Extra { column: String, dtype: ArrowType },
    /// Both have the column, with different types.
    Type { column: String, want: ArrowType, got: ArrowType },
    /// Both have the column with the same type, but disagree on nullability.
    Nullability { column: String, want: bool, got: bool },
    /// Both declare the same set of columns, in different orders.
    Order { column: String, want: usize, got: usize },
    /// The spec's own type could not be built, so it produces no schema to
    /// compare. Carries the real reason rather than inventing a comparison.
    Underivable { column: String, reason: String },
}

impl Mismatch {
    /// The sentence a user reads. Says what is wrong and, where there is one,
    /// the edit that fixes it.
    pub fn message(&self) -> String {
        match self {
            Mismatch::Missing { column, dtype } => format!(
                "`{column}`: the target declares it ({}), the spec does not produce it",
                render(dtype)
            ),
            Mismatch::Extra { column, dtype } => format!(
                "`{column}`: the spec produces it ({}), the target does not declare it. \
                 A target is a contract, so an undeclared column is dropped rather than \
                 added — remove it from the spec's `columns`, or declare it.",
                render(dtype)
            ),
            Mismatch::Type { column, want, got } => format!(
                "`{column}`: the target declares {}, the spec produces {}",
                render(want),
                render(got)
            ),
            Mismatch::Nullability { column, want, got } => {
                let (w, g) = (nn(*want), nn(*got));
                format!("`{column}`: the target declares {w}, the spec is {g}")
            }
            Mismatch::Underivable { column, reason } => format!(
                "`{column}`: the spec's own type cannot be built, so it produces no schema \
                 to compare — {reason}"
            ),
            Mismatch::Order { column, want, got } => format!(
                "`{column}`: the target puts it at position {}, the spec at {}. \
                 Column order is part of the contract, because `SELECT *` and a Parquet \
                 write both depend on it.",
                want + 1,
                got + 1
            ),
        }
    }
}

fn nn(nullable: bool) -> &'static str {
    if nullable {
        "nullable"
    } else {
        "NOT NULL"
    }
}

/// Arrow's Display is not what a user wrote, so say it back in their language.
fn render(t: &ArrowType) -> String {
    match t {
        ArrowType::Utf8 => "TEXT".into(),
        ArrowType::Boolean => "BOOLEAN".into(),
        ArrowType::Int64 => "BIGINT".into(),
        ArrowType::Float64 => "DOUBLE".into(),
        ArrowType::Decimal128(p, s) => format!("DECIMAL({p},{s})"),
        ArrowType::Date32 => "DATE".into(),
        ArrowType::Timestamp(_, None) => "TIMESTAMP".into(),
        ArrowType::Timestamp(_, Some(tz)) => format!("TIMESTAMP (offset {tz})"),
        other => format!("{other}"),
    }
}

/// Does this spec produce exactly this target?
///
/// Returns every disagreement, not the first.
pub fn conforms(spec: &ParseSpec, target: &Target) -> Result<(), Vec<Mismatch>> {
    let produced = match crate::engine::schema_of(spec) {
        Ok(s) => s,
        // A spec whose own types cannot be built produces no schema, so there
        // is nothing to compare. Report *that*, with the reason the engine
        // gave — which already names the offending column. Synthesising a
        // Missing per spec column, as this first did, printed "the target
        // declares it" about columns the target may never have mentioned, and
        // threw away the only sentence that said what was actually wrong.
        Err(e) => {
            let reason = format!("{e:#}");
            let column = spec
                .columns
                .iter()
                .map(|c| c.name.as_str())
                .find(|n| reason.contains(*n))
                .unwrap_or("?")
                .to_string();
            return Err(vec![Mismatch::Underivable { column, reason }]);
        }
    };
    compare(&produced, &target.arrow_schema())
}

/// Field-for-field comparison of two schemas, as the contract defines it.
pub fn compare(produced: &Schema, wanted: &Schema) -> Result<(), Vec<Mismatch>> {
    let mut out = Vec::new();

    // Position is only worth comparing once both sides agree on *which*
    // columns exist. A missing column shifts every column after it, and
    // reporting each of those shifts would turn one fix into a wall of
    // consequences — the same reason a type mismatch does not also report
    // nullability below.
    let same_columns = produced.fields().len() == wanted.fields().len()
        && wanted
            .fields()
            .iter()
            .all(|w| produced.fields().iter().any(|g| g.name() == w.name()));

    for (want_i, want) in wanted.fields().iter().enumerate() {
        match produced.fields().iter().position(|g| g.name() == want.name()) {
            None => out.push(Mismatch::Missing {
                column: want.name().clone(),
                dtype: want.data_type().clone(),
            }),
            Some(got_i) => {
                let got = produced.field(got_i);
                if got.data_type() != want.data_type() {
                    out.push(Mismatch::Type {
                        column: want.name().clone(),
                        want: want.data_type().clone(),
                        got: got.data_type().clone(),
                    });
                } else if got.is_nullable() != want.is_nullable() {
                    // Only worth saying once the type agrees; otherwise a
                    // single wrong column produces two lines that read like
                    // two problems.
                    out.push(Mismatch::Nullability {
                        column: want.name().clone(),
                        want: want.is_nullable(),
                        got: got.is_nullable(),
                    });
                } else if same_columns && got_i != want_i {
                    out.push(Mismatch::Order {
                        column: want.name().clone(),
                        want: want_i,
                        got: got_i,
                    });
                }
            }
        }
    }

    for got in produced.fields() {
        if !wanted.fields().iter().any(|w| w.name() == got.name()) {
            out.push(Mismatch::Extra {
                column: got.name().clone(),
                dtype: got.data_type().clone(),
            });
        }
    }

    if out.is_empty() {
        Ok(())
    } else {
        Err(out)
    }
}

/// What `tdy check` reports about one sidecar.
///
/// Three-way rather than two-way, and that is not hedging. `sniff` hardcodes
/// `nullable: true` and gives money `decimal(38, s)`, so an ordinary sniffed
/// sidecar will differ from a hand-written target in ways that say nothing
/// about the file. Reporting "this was never fitted to a target" separately
/// from "this contradicts the target" is what makes the verdict useful on day
/// one instead of a wall of noise.
#[derive(Debug, Clone, PartialEq)]
pub enum Verdict {
    /// Produces the target exactly.
    Conforms,
    /// Was fitted to this target and no longer matches it.
    Contradicts(Vec<Mismatch>),
    /// Was never fitted to any target — a sniffed or hand-written spec that
    /// happens not to match. The mismatches are still reported, as guidance.
    Unfitted(Vec<Mismatch>),
}

impl Verdict {
    pub fn label(&self) -> &'static str {
        match self {
            Verdict::Conforms => "CONFORMS",
            Verdict::Contradicts(_) => "CONTRADICTS",
            Verdict::Unfitted(_) => "UNFITTED",
        }
    }

    pub fn is_ok(&self) -> bool {
        matches!(self, Verdict::Conforms)
    }

    pub fn mismatches(&self) -> &[Mismatch] {
        match self {
            Verdict::Conforms => &[],
            Verdict::Contradicts(m) | Verdict::Unfitted(m) => m,
        }
    }
}

/// Judge one spec against a target.
///
/// `fitted` says whether this spec was produced *for* this target — until
/// `tdy fit` exists nothing is, so every non-conforming spec is `Unfitted`.
pub fn judge(spec: &ParseSpec, target: &Target, fitted: bool) -> Verdict {
    match conforms(spec, target) {
        Ok(()) => Verdict::Conforms,
        Err(m) if fitted => Verdict::Contradicts(m),
        Err(m) => Verdict::Unfitted(m),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::spec::{ColumnSpec, DType, Extraction, RaggedPolicy, Transform, ValueParsing};
    use crate::target::Target;

    fn col(name: &str, dtype: DType, nullable: bool) -> ColumnSpec {
        ColumnSpec {
            name: name.into(),
            source: None,
            dtype,
            nullable,
            parse: ValueParsing::default(),
        }
    }

    fn spec_of(columns: Vec<ColumnSpec>) -> ParseSpec {
        ParseSpec {
            extraction: Extraction::Delimited {
                delimiter: ',',
                quote: Some('"'),
                escape: None,
                encoding: None,
                comment: None,
                ragged: RaggedPolicy::PadNulls,
            },
            transforms: vec![Transform::PromoteHeader { rows: 1, join: " ".into() }],
            columns,
            confidence: Some(1.0),
            notes: vec![],
        }
    }

    fn target_of(sql: &str) -> Target {
        Target::parse(sql).unwrap_or_else(|e| panic!("{e:#}"))
    }

    const SALES: &str = "CREATE TABLE sales (
        month      DATE          NOT NULL,
        region     TEXT          NOT NULL,
        amount_chf DECIMAL(14,2) NOT NULL
    ) WITH (files = 'x.csv')";

    fn conforming() -> Vec<ColumnSpec> {
        vec![
            col("month", DType::Date { format: "%d.%m.%Y".into() }, false),
            col("region", DType::Utf8, false),
            col("amount_chf", DType::Decimal { precision: 14, scale: 2 }, false),
        ]
    }

    #[test]
    fn a_spec_that_lands_on_the_target_conforms() {
        assert!(conforms(&spec_of(conforming()), &target_of(SALES)).is_ok());
    }

    /// The point of holding the target as an Arrow type: twelve files with
    /// twelve date formats all land on one DATE column.
    #[test]
    fn the_date_format_is_a_property_of_the_file_not_the_contract() {
        let t = target_of(SALES);
        for fmt in ["%d.%m.%Y", "%Y-%m-%d", "%m/%d/%Y", "%b %Y"] {
            let mut cols = conforming();
            cols[0].dtype = DType::Date { format: fmt.into() };
            assert!(
                conforms(&spec_of(cols), &t).is_ok(),
                "format {fmt} broke conformance, but it does not reach the Arrow schema"
            );
        }
    }

    /// One missing column is one problem. It shifts every column after it, and
    /// reporting each shift as well would turn one fix into a wall of
    /// consequences.
    #[test]
    fn a_missing_column_is_named_once_without_cascading_order_noise() {
        let mut cols = conforming();
        cols.remove(1);
        let errs = conforms(&spec_of(cols), &target_of(SALES)).unwrap_err();
        assert_eq!(errs.len(), 1, "{errs:?}");
        assert!(matches!(&errs[0], Mismatch::Missing { column, .. } if column == "region"));
        assert!(errs[0].message().contains("does not produce it"));
    }

    #[test]
    fn an_undeclared_column_is_named_as_extra() {
        let mut cols = conforming();
        cols.push(col("kundennummer", DType::Int64, true));
        let errs = conforms(&spec_of(cols), &target_of(SALES)).unwrap_err();
        assert_eq!(errs.len(), 1);
        assert!(matches!(&errs[0], Mismatch::Extra { column, .. } if column == "kundennummer"));
        assert!(errs[0].message().contains("contract"));
    }

    /// The failure this whole layer exists to catch: a spec that parses the
    /// file perfectly and produces money as a float.
    #[test]
    fn money_as_a_float_contradicts_a_decimal_target() {
        let mut cols = conforming();
        cols[2].dtype = DType::Float64;
        let errs = conforms(&spec_of(cols), &target_of(SALES)).unwrap_err();
        assert_eq!(errs.len(), 1);
        let m = errs[0].message();
        assert!(m.contains("DECIMAL(14,2)") && m.contains("DOUBLE"), "{m}");
    }

    /// decimal(38,2) and decimal(14,2) are different Arrow types, and a
    /// sniffed sidecar produces the former for money.
    #[test]
    fn a_decimal_of_the_wrong_precision_does_not_conform() {
        let mut cols = conforming();
        cols[2].dtype = DType::Decimal { precision: 38, scale: 2 };
        let errs = conforms(&spec_of(cols), &target_of(SALES)).unwrap_err();
        assert!(matches!(&errs[0], Mismatch::Type { column, .. } if column == "amount_chf"));
    }

    #[test]
    fn nullability_is_part_of_the_contract() {
        let mut cols = conforming();
        cols[1].nullable = true;
        let errs = conforms(&spec_of(cols), &target_of(SALES)).unwrap_err();
        assert_eq!(errs.len(), 1);
        let m = errs[0].message();
        assert!(m.contains("NOT NULL") && m.contains("nullable"), "{m}");
    }

    /// A wrong type and a wrong nullability on one column is one problem, not
    /// two: reporting both makes a single fix look like two.
    #[test]
    fn a_type_mismatch_does_not_also_report_nullability() {
        let mut cols = conforming();
        cols[2].dtype = DType::Float64;
        cols[2].nullable = true;
        let errs = conforms(&spec_of(cols), &target_of(SALES)).unwrap_err();
        assert_eq!(errs.len(), 1, "{errs:?}");
        assert!(matches!(errs[0], Mismatch::Type { .. }));
    }

    /// Order matters because `SELECT *` and a Parquet write both depend on it.
    #[test]
    fn column_order_is_part_of_the_contract() {
        let mut cols = conforming();
        cols.swap(0, 1);
        let errs = conforms(&spec_of(cols), &target_of(SALES)).unwrap_err();
        assert_eq!(errs.len(), 2, "both moved columns are reported: {errs:?}");
        assert!(errs.iter().all(|e| matches!(e, Mismatch::Order { .. })));
        assert!(errs[0].message().contains("position"));
    }

    #[test]
    fn every_disagreement_is_reported_not_just_the_first() {
        let cols = vec![
            col("month", DType::Utf8, true),
            col("was_ist_das", DType::Int64, true),
        ];
        let errs = conforms(&spec_of(cols), &target_of(SALES)).unwrap_err();
        // month: wrong type. region, amount_chf: missing. was_ist_das: extra.
        assert_eq!(errs.len(), 4, "{errs:?}");
    }

    #[test]
    fn the_verdict_separates_never_fitted_from_contradicts() {
        let mut cols = conforming();
        cols[2].dtype = DType::Float64;
        let t = target_of(SALES);
        assert!(matches!(judge(&spec_of(cols.clone()), &t, false), Verdict::Unfitted(_)));
        assert!(matches!(judge(&spec_of(cols), &t, true), Verdict::Contradicts(_)));
        assert!(judge(&spec_of(conforming()), &t, true).is_ok());
    }

    /// The proof has to be about what the executor will really emit, not about
    /// a parallel reading of the spec. If these ever diverge the gate is
    /// worthless, so it is asserted rather than assumed.
    #[test]
    fn conformance_agrees_with_what_execution_actually_produces() {
        let dir = tempfile::TempDir::new().unwrap();
        let p = dir.path().join("s.csv");
        std::fs::write(&p, "month,region,amount_chf\n31.01.2025,Ost,1234.50\n").unwrap();

        let spec = spec_of(conforming());
        let t = target_of(SALES);
        assert!(conforms(&spec, &t).is_ok(), "the gate says it does not conform");

        let batch = crate::provider::spec_to_batch(&spec, &p).expect("execute");
        assert_eq!(
            batch.schema().as_ref(),
            &t.arrow_schema(),
            "the gate and the executor disagree about what this spec produces"
        );
    }
}
