//! Is one member's money in a different unit from the pile's?
//!
//! The Rappen class: a month exported in cents beside eleven in francs
//! parses, type-checks and binds, and its total is a hundred times what it
//! should be with the error invisible in any single row. Nothing about the
//! bytes says so. What does say so is the *pile*: a member whose typical
//! value is ten times the others' is worth a person's look before it joins.
//!
//! Medians, not totals — a partial final month has a small total and an
//! ordinary median — and a threshold of ten, which catches a factor of a
//! hundred with room to spare and lets an ordinary busy month through. This
//! is a review reason, never a refusal: the number is put in front of a
//! person, and `--accept` records that they looked.

use std::collections::HashMap;

/// One member's typical absolute value per numeric column.
pub type Medians = HashMap<String, f64>;

/// A member whose typical value is out of scale with the pile's.
#[derive(Debug, Clone, PartialEq)]
pub struct Outlier {
    /// Index into the members handed to [`outliers`].
    pub member: usize,
    pub column: String,
    pub median: f64,
    pub pile_median: f64,
    /// `median / pile_median`, or its reciprocal when below one.
    pub ratio: f64,
}

/// The factor at which a member's typical value is a different unit rather
/// than a busy month: a hundred is the Rappen class, ten leaves room.
pub const THRESHOLD: f64 = 10.0;

/// Fewer members than this and no member can be called the odd one out.
pub const MIN_MEMBERS: usize = 3;

impl Outlier {
    /// The review reason: what was measured, against what, and the question
    /// a person has to answer.
    pub fn reason(&self) -> String {
        format!(
            "`{}`: this member's typical value (median {}) is {}× the pile's (median {}) — a \
             different unit? Accept only if these really are the same unit as the other members'",
            self.column,
            fmt(self.median),
            fmt(self.ratio),
            fmt(self.pile_median)
        )
    }
}

/// A number the way a person would write it in a sentence: no trailing
/// zeros, at most two decimals.
fn fmt(x: f64) -> String {
    let s = format!("{x:.2}");
    let s = s.trim_end_matches('0').trim_end_matches('.');
    if s.is_empty() || s == "-" { "0".to_string() } else { s.to_string() }
}

/// The median absolute value of every numeric column of a typed batch,
/// nulls ignored. A column with no values has no median and is absent.
pub fn medians(batch: &datafusion::arrow::record_batch::RecordBatch) -> Medians {
    use datafusion::arrow::array::{Array, Decimal128Array, Float64Array, Int64Array};
    use datafusion::arrow::datatypes::DataType;
    let mut out = Medians::new();
    for (i, field) in batch.schema().fields().iter().enumerate() {
        let col = batch.column(i);
        let mut vals: Vec<f64> = match field.data_type() {
            DataType::Int64 => {
                let a = col.as_any().downcast_ref::<Int64Array>().expect("Int64");
                (0..a.len()).filter(|&j| !a.is_null(j)).map(|j| (a.value(j) as f64).abs()).collect()
            }
            DataType::Float64 => {
                let a = col.as_any().downcast_ref::<Float64Array>().expect("Float64");
                (0..a.len()).filter(|&j| !a.is_null(j) && a.value(j).is_finite()).map(|j| a.value(j).abs()).collect()
            }
            DataType::Decimal128(_, scale) => {
                let a = col.as_any().downcast_ref::<Decimal128Array>().expect("Decimal128");
                let div = 10f64.powi(i32::from(*scale));
                (0..a.len()).filter(|&j| !a.is_null(j)).map(|j| (a.value(j) as f64 / div).abs()).collect()
            }
            _ => continue,
        };
        if let Some(m) = median(&mut vals) {
            out.insert(field.name().to_string(), m);
        }
    }
    out
}

fn median(vals: &mut [f64]) -> Option<f64> {
    if vals.is_empty() {
        return None;
    }
    vals.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let n = vals.len();
    Some(if n % 2 == 1 { vals[n / 2] } else { (vals[n / 2 - 1] + vals[n / 2]) / 2.0 })
}

/// Which members are out of scale with the pile, per column. The pile's
/// reference for a column is the median of the members' medians; a member
/// whose median is `threshold`× above or below it is an outlier. Needs at
/// least [`MIN_MEMBERS`] members carrying the column — with two, neither is
/// the odd one out. A column whose reference is zero is not judged.
pub fn outliers(members: &[Medians], threshold: f64) -> Vec<Outlier> {
    let mut columns: Vec<&String> = members.iter().flat_map(|m| m.keys()).collect();
    columns.sort();
    columns.dedup();
    let mut out = Vec::new();
    for column in columns {
        let mut present: Vec<(usize, f64)> =
            members.iter().enumerate().filter_map(|(i, m)| m.get(column).map(|v| (i, *v))).collect();
        if present.len() < MIN_MEMBERS {
            continue;
        }
        let mut vals: Vec<f64> = present.iter().map(|(_, v)| *v).collect();
        let Some(pile) = median(&mut vals) else { continue };
        if pile <= 0.0 {
            continue;
        }
        present.sort_by_key(|(i, _)| *i);
        for (i, v) in present {
            let ratio = if v >= pile { v / pile } else { pile / v };
            if ratio.is_finite() && ratio >= threshold {
                out.push(Outlier { member: i, column: column.clone(), median: v, pile_median: pile, ratio });
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn m(pairs: &[(&str, f64)]) -> Medians {
        pairs.iter().map(|(k, v)| (k.to_string(), *v)).collect()
    }

    #[test]
    fn a_member_a_hundred_times_the_pile_is_an_outlier_either_way() {
        let members = vec![m(&[("amount", 1100.0)]), m(&[("amount", 1200.0)]), m(&[("amount", 115000.0)])];
        let out = outliers(&members, 10.0);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].member, 2);
        assert_eq!(out[0].column, "amount");
        assert!((out[0].ratio - 115000.0 / 1200.0).abs() < 1e-9, "{}", out[0].ratio);

        let members = vec![m(&[("amount", 1100.0)]), m(&[("amount", 12.0)]), m(&[("amount", 1200.0)])];
        let out = outliers(&members, 10.0);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].member, 1);
        assert!(out[0].ratio > 90.0, "a member a hundred times *smaller* is the same question");
    }

    /// A busy month is not a different unit.
    #[test]
    fn nine_times_is_not_an_outlier_and_a_pile_of_two_is_not_judged() {
        let members = vec![m(&[("amount", 100.0)]), m(&[("amount", 110.0)]), m(&[("amount", 950.0)])];
        assert!(outliers(&members, 10.0).is_empty());
        let members = vec![m(&[("amount", 100.0)]), m(&[("amount", 100000.0)])];
        assert!(outliers(&members, 10.0).is_empty(), "with two members, which one is wrong?");
    }

    /// A column a member lacks a median for (all null, or absent) is skipped
    /// for that member and does not move the pile's reference.
    #[test]
    fn a_missing_median_is_skipped() {
        let members = vec![m(&[("amount", 100.0)]), m(&[]), m(&[("amount", 100.0)]), m(&[("amount", 10000.0)])];
        let out = outliers(&members, 10.0);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].member, 3);
    }

    #[test]
    fn the_reason_names_the_column_the_medians_and_the_factor() {
        let o = Outlier { member: 2, column: "amount".into(), median: 115000.0, pile_median: 1150.0, ratio: 100.0 };
        let r = o.reason();
        assert!(r.contains("`amount`") && r.contains("115000") && r.contains("1150") && r.contains("100"), "{r}");
        assert!(r.to_lowercase().contains("unit"), "{r}");
    }
}
