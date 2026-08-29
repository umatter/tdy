//! Number *shape* analysis: which character is the decimal point and which is
//! the thousands separator.
//!
//! This exists because the obvious approach — "try each convention, keep the
//! first that parses" — silently corrupts data. `1,5` parses fine if you
//! declare `,` a thousands separator: you get `15`. A German price column
//! becomes ten times too large with no error anywhere. So instead of asking
//! *can this parse*, we ask *what shape is this*, and only accept a
//! convention that every value in the column is consistent with.
//!
//! The rules, in the order they settle a column:
//!
//! 1. A separator that appears **more than once** in one value is a thousands
//!    separator (`1.234.567`).
//! 2. If a value contains **two different** separators, the rightmost is the
//!    decimal point and the other is the thousands separator (`1.234,56`).
//! 3. A separator followed by a group that is **not exactly three digits** is
//!    a decimal point (`1,5`, `12.75`, `1,2345`).
//! 4. An apostrophe is never a decimal point (Swiss `1'234.56`).
//! 5. A lone separator with exactly three digits after it (`1,234`) is
//!    genuinely ambiguous. It is resolved by the rest of the column if any
//!    other value settles it, and otherwise by convention — reported with
//!    `ambiguous = true` so the caller can lower confidence and say so in the
//!    spec's notes rather than pretend it knew.
//!
//! Grouping is also *enforced*: a thousands separator must group the integer
//! part in threes. [`check_grouping`] is what the executor uses to turn a
//! wrong spec into an error instead of a wrong number.

/// Characters that may act as a thousands separator.
const THOUSANDS_CANDIDATES: [char; 4] = ['.', ',', '\'', ' '];
/// Characters that may act as a decimal point.
const DECIMAL_CANDIDATES: [char; 2] = ['.', ','];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct NumericFormat {
    pub thousands: Option<char>,
    pub decimal: Option<char>,
    /// True when the column never contained a value that proves the
    /// convention (e.g. every value looks like `1,234`).
    pub ambiguous: bool,
}

/// What one value tells us about one separator character.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Role {
    Thousands,
    Decimal,
    /// Present, but this value alone cannot tell which.
    Unknown,
}

/// Split off a leading sign and surrounding whitespace.
///
/// A *trailing* minus ("1234-", the accounting convention) is deliberately not
/// accepted: nothing downstream can parse it, so calling such a column numeric
/// would type it as an integer and then fail on every row. Left as text, it is
/// at least readable — and a `replace` pair in the sidecar fixes it.
fn core(v: &str) -> Option<&str> {
    let v = v.trim();
    let v = v.strip_prefix('+').or_else(|| v.strip_prefix('-')).unwrap_or(v);
    if v.is_empty() {
        return None;
    }
    Some(v)
}

/// Analyse a single value. Returns None if it is not numeric-shaped at all.
fn roles(v: &str) -> Option<Vec<(char, Role)>> {
    let v = core(v)?;
    if v.is_empty() || !v.chars().next().is_some_and(|c| c.is_ascii_digit()) {
        // Must start with a digit: ".5" is unusual enough in exports that we
        // would rather fall through to text than guess.
        return None;
    }
    if !v
        .chars()
        .all(|c| c.is_ascii_digit() || THOUSANDS_CANDIDATES.contains(&c) || DECIMAL_CANDIDATES.contains(&c))
    {
        return None;
    }
    if v.chars().last().is_some_and(|c| !c.is_ascii_digit()) {
        // Trailing separator: "1,234." is not a number we will guess at.
        return None;
    }

    // Positions of every separator, in order.
    let mut seps: Vec<(usize, char)> = Vec::new();
    for (i, c) in v.char_indices() {
        if !c.is_ascii_digit() {
            seps.push((i, c));
        }
    }
    if seps.is_empty() {
        return Some(Vec::new()); // plain integer, tells us nothing
    }

    // Digit-run lengths between separators.
    let mut out: Vec<(char, Role)> = Vec::new();
    let distinct: Vec<char> = {
        let mut d: Vec<char> = seps.iter().map(|(_, c)| *c).collect();
        d.sort_unstable();
        d.dedup();
        d
    };

    if distinct.len() > 2 {
        return None; // three different separator characters: not a number
    }

    if distinct.len() == 2 {
        // Rule 2: rightmost separator is the decimal point.
        let (_, last_char) = *seps.last().unwrap();
        let other = *distinct.iter().find(|c| **c != last_char).unwrap();
        // The decimal point may only appear once.
        if seps.iter().filter(|(_, c)| *c == last_char).count() != 1 {
            return None;
        }
        if !DECIMAL_CANDIDATES.contains(&last_char) {
            return None; // an apostrophe is never a decimal point
        }
        if check_grouping(v, Some(other), Some(last_char)).is_err() {
            return None;
        }
        out.push((last_char, Role::Decimal));
        out.push((other, Role::Thousands));
        return Some(out);
    }

    // Exactly one distinct separator character.
    let sep = distinct[0];
    let count = seps.len();
    let after_last = v.len() - seps.last().unwrap().0 - seps.last().unwrap().1.len_utf8();

    if count > 1 {
        // Rule 1: repeated -> thousands, but only if it really groups in
        // threes. "12.34.5" is not a number at all.
        if !THOUSANDS_CANDIDATES.contains(&sep) || check_grouping(v, Some(sep), None).is_err() {
            return None;
        }
        out.push((sep, Role::Thousands));
    } else if after_last != 3 {
        // Rule 3: not a 3-digit group -> decimal point.
        if !DECIMAL_CANDIDATES.contains(&sep) {
            return None; // "12'5" is nothing
        }
        out.push((sep, Role::Decimal));
    } else if !DECIMAL_CANDIDATES.contains(&sep) {
        // Rule 4: apostrophe/space with a 3-group -> thousands, unambiguous.
        out.push((sep, Role::Thousands));
    } else {
        // Rule 5: "1,234" — genuinely ambiguous.
        out.push((sep, Role::Unknown));
    }
    Some(out)
}

/// Infer the numeric convention of a whole column.
///
/// Returns `None` when the values are not consistently numeric — the caller
/// should then fall back to text. A returned format with both fields `None`
/// means "plain numbers, no separators".
pub fn infer(values: &[&str]) -> Option<NumericFormat> {
    let mut any = false;
    let mut decimal: Option<char> = None;
    let mut thousands: Option<char> = None;
    // A *set*: a column can carry unresolved evidence about both characters
    // at once ("1.234" and "2,750" in the same column), and keeping only the
    // last one throws away the fact that settles the other.
    let mut unknown: std::collections::BTreeSet<char> = std::collections::BTreeSet::new();
    let mut all_plain = true;

    for v in values {
        let r = roles(v)?;
        any = true;
        if !r.is_empty() {
            all_plain = false;
        }
        for (c, role) in r {
            match role {
                Role::Decimal => {
                    if decimal.is_some_and(|d| d != c) {
                        return None; // two different decimal points in one column
                    }
                    if thousands == Some(c) {
                        return None; // same char used both ways
                    }
                    decimal = Some(c);
                }
                Role::Thousands => {
                    if thousands.is_some_and(|t| t != c) {
                        return None;
                    }
                    if decimal == Some(c) {
                        return None;
                    }
                    thousands = Some(c);
                }
                Role::Unknown => {
                    unknown.insert(c);
                }
            }
        }
    }
    if !any {
        return None;
    }
    if all_plain {
        return Some(NumericFormat { thousands: None, decimal: None, ambiguous: false });
    }

    let mut ambiguous = false;
    // Resolve what the column already knows before falling back to convention:
    // once one character is proved to be the decimal point, any *other*
    // separator in the column can only be grouping the thousands.
    let pending: Vec<char> = unknown
        .iter()
        .copied()
        .filter(|u| decimal != Some(*u) && thousands != Some(*u))
        .collect();
    match pending.as_slice() {
        [] => {}
        [u] => {
            if decimal.is_some() {
                thousands = Some(*u);
            } else if thousands.is_some() {
                decimal = Some(*u);
            } else {
                // Nothing in the column settles it. Convention: a comma
                // grouping exactly three digits is far more often a thousands
                // separator in exports; a dot is more often a decimal point.
                ambiguous = true;
                match u {
                    ',' => thousands = Some(*u),
                    _ => decimal = Some(*u),
                }
            }
        }
        // Two characters, each seen only in its ambiguous form and neither
        // settled by any other value: "1.234" and "1,234" side by side could
        // be Continental or Anglo and the column contains no way to tell.
        // Refusing to guess leaves it as text, which a person or the model
        // can fix; guessing would be a 1000x error half the time.
        _ => return None,
    }
    if decimal.is_some() && decimal == thousands {
        return None;
    }

    // A lone decimal '.' is the default parse; no need to declare it.
    if decimal == Some('.') {
        decimal = None;
    }
    Some(NumericFormat { thousands, decimal, ambiguous })
}

/// Normalise a value to a plain `[-]digits[.digits]` string under a known
/// convention, *verifying* the shape rather than blindly deleting characters.
///
/// This is what makes a wrong spec loud: with `thousands = ','`, the value
/// `1,5` is rejected instead of silently becoming `15`.
pub fn check_grouping(v: &str, thousands: Option<char>, decimal: Option<char>) -> Result<(), String> {
    let t = match thousands {
        Some(t) => t,
        None => return Ok(()),
    };
    // Grouping is only *evidence* for separators that could also be a decimal
    // point. An apostrophe or a space can never be one, so `1000'000.00` is
    // merely sloppy Swiss grouping with exactly one possible reading — while
    // `1,5` under a comma-thousands spec has two, and guessing between them is
    // how a price becomes ten times itself.
    if !DECIMAL_CANDIDATES.contains(&t) {
        return Ok(());
    }
    // When '.' is itself the grouping character, the value has no decimal
    // point to split on: "1.234.567" is one number, not one-point-something.
    let dec: Option<char> = match decimal {
        Some(d) => Some(d),
        None if t == '.' => None,
        None => Some('.'),
    };
    let s = match core(v) {
        Some(s) => s,
        None => return Ok(()),
    };
    // Integer part = everything before the decimal point.
    let int_part = match dec.and_then(|d| s.split_once(d)) {
        Some((i, frac)) => {
            if frac.contains(t) {
                return Err(format!(
                    "thousands separator {t:?} appears after the decimal point in {v:?}"
                ));
            }
            i
        }
        None => s,
    };
    if !int_part.contains(t) {
        return Ok(());
    }
    let groups: Vec<&str> = int_part.split(t).collect();
    let bad = groups[0].is_empty()
        || groups[0].len() > 3
        || groups[1..].iter().any(|g| g.len() != 3)
        || groups.iter().any(|g| !g.chars().all(|c| c.is_ascii_digit()));
    if bad {
        return Err(format!(
            "{v:?} is not grouped in threes for thousands separator {t:?} \
             (if {t:?} is the decimal point here, set decimal_separator instead)"
        ));
    }
    Ok(())
}

/// True when a string of digits should stay text rather than becoming an
/// integer: leading zeros carry meaning (postal codes, article numbers,
/// phone numbers) and `007 -> 7` is data loss.
pub fn has_significant_leading_zero(v: &str) -> bool {
    let v = v.trim();
    let v = v.strip_prefix(['+', '-']).unwrap_or(v);
    v.len() > 1 && v.starts_with('0') && v.chars().all(|c| c.is_ascii_digit())
}

/// Would this integer literal fit in an i64?
pub fn fits_i64(v: &str) -> bool {
    v.trim().trim_start_matches('+').parse::<i64>().is_ok()
}

/// Number of fractional digits under a convention, for picking a decimal scale.
pub fn frac_digits(v: &str, decimal: Option<char>) -> usize {
    frac_digits_with(v, decimal, None)
}

/// As [`frac_digits`], but told which character groups thousands — otherwise
/// the German integer `1.234` reads as three decimal places, and a column of
/// whole euros is typed `decimal(38, 3)`.
pub fn frac_digits_with(v: &str, decimal: Option<char>, thousands: Option<char>) -> usize {
    let dec = match decimal {
        Some(d) => d,
        // No declared decimal point: '.' is the default — unless '.' is doing
        // the grouping, in which case the value has no fractional part at all.
        None if thousands == Some('.') => return 0,
        None => '.',
    };
    if thousands == Some(dec) {
        return 0;
    }
    match core(v).and_then(|s| s.rsplit_once(dec)) {
        Some((_, frac)) if !frac.is_empty() && frac.chars().all(|c| c.is_ascii_digit()) => frac.len(),
        _ => 0,
    }
}

/// Is every value plainly integral (no separators, no fraction)?
pub fn all_integral(values: &[&str]) -> bool {
    values.iter().all(|v| {
        core(v)
            .map(|s| !s.is_empty() && s.chars().all(|c| c.is_ascii_digit()))
            .unwrap_or(false)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn inf(vs: &[&str]) -> Option<NumericFormat> {
        infer(vs)
    }

    #[test]
    fn german_decimal_comma_is_not_a_thousands_separator() {
        // The bug this module exists for: "1,5" must never mean 15.
        let f = inf(&["1,5", "2,75", "10,25"]).unwrap();
        assert_eq!(f.decimal, Some(','));
        assert_eq!(f.thousands, None);
        assert!(!f.ambiguous);
    }

    #[test]
    fn swiss_apostrophe_grouping() {
        let f = inf(&["1'234.56", "12'000.00", "999.99"]).unwrap();
        assert_eq!(f.thousands, Some('\''));
        assert_eq!(f.decimal, None); // '.' is the default
    }

    #[test]
    fn continental_dot_thousands_comma_decimal() {
        let f = inf(&["1.234,56", "999,00"]).unwrap();
        assert_eq!(f.thousands, Some('.'));
        assert_eq!(f.decimal, Some(','));
        assert!(!f.ambiguous);
    }

    #[test]
    fn anglo_comma_thousands() {
        let f = inf(&["1,234.56", "12,000.00"]).unwrap();
        assert_eq!(f.thousands, Some(','));
        assert_eq!(f.decimal, None);
    }

    #[test]
    fn repeated_separator_is_grouping() {
        let f = inf(&["1.234.567", "12.000"]).unwrap();
        assert_eq!(f.thousands, Some('.'));
        assert_eq!(f.decimal, None);
    }

    #[test]
    fn evidence_about_one_separator_settles_the_other() {
        // "12,75" proves the comma is the decimal point, which leaves the dot
        // in "1.234" no role but grouping. Reading it as 1.234 instead of
        // 1234 is a thousandfold error, and it must not depend on row order.
        for order in [
            ["1.234", "12,75", "2,750"],
            ["2,750", "12,75", "1.234"],
            ["12,75", "1.234", "2,750"],
        ] {
            let f = inf(&order).unwrap_or_else(|| panic!("{order:?} is readable"));
            assert_eq!(f.thousands, Some('.'), "{order:?}");
            assert_eq!(f.decimal, Some(','), "{order:?}");
        }
        // The mirror image, Anglo.
        let f = inf(&["1,234", "1.234", "1.5"]).unwrap();
        assert_eq!(f.thousands, Some(','));
        assert_eq!(f.decimal, None); // '.' is the default decimal point
    }

    #[test]
    fn two_separators_with_no_evidence_either_way_is_refused() {
        // Continental or Anglo? The column does not say, and guessing is a
        // 1000x error half the time.
        assert!(inf(&["1.234", "1,234"]).is_none());
    }

    #[test]
    fn one_unambiguous_value_settles_the_column() {
        // "1,5" proves the comma is a decimal point, so "1,234" is 1.234.
        let f = inf(&["1,234", "1,5"]).unwrap();
        assert_eq!(f.decimal, Some(','));
        assert_eq!(f.thousands, None);
        assert!(!f.ambiguous);
    }

    #[test]
    fn truly_ambiguous_column_is_flagged() {
        let f = inf(&["1,234", "5,678"]).unwrap();
        assert!(f.ambiguous);
        assert_eq!(f.thousands, Some(','));
    }

    #[test]
    fn plain_numbers_need_no_convention() {
        let f = inf(&["1", "22", "333"]).unwrap();
        assert_eq!(f, NumericFormat::default());
        let f = inf(&["1.5", "2.25"]).unwrap();
        assert_eq!(f.decimal, None);
        assert_eq!(f.thousands, None);
    }

    #[test]
    fn conflicting_column_is_rejected() {
        // '.' used as decimal in one value and as grouping in another.
        assert!(inf(&["1.5", "1.234.567"]).is_none());
    }

    #[test]
    fn non_numeric_rejected() {
        assert!(inf(&["abc"]).is_none());
        assert!(inf(&["1,2,3,4"]).is_none()); // does not group in threes
        assert!(inf(&["12.34.5"]).is_none()); // 3-then-1 grouping: not a number
        assert!(inf(&["1,234."]).is_none());
        assert!(inf(&[""]).is_none());
    }

    #[test]
    fn grouping_check_rejects_the_corrupting_case() {
        assert!(check_grouping("1,5", Some(','), None).is_err());
        assert!(check_grouping("1,234", Some(','), None).is_ok());
        assert!(check_grouping("1,234,567.89", Some(','), None).is_ok());
        assert!(check_grouping("12,34", Some(','), None).is_err());
        assert!(check_grouping("1'234.56", Some('\''), None).is_ok());
        // Sloppy but unambiguous: an apostrophe is never a decimal point, so
        // there is only one number this can be.
        assert!(check_grouping("1000'000.00", Some('\''), None).is_ok());
        assert!(check_grouping("12'34", Some('\''), None).is_ok());
        assert!(check_grouping("1.234,56", Some('.'), Some(',')).is_ok());
        assert!(check_grouping("-1'234.50", Some('\''), None).is_ok());
        // No separator present at all: nothing to check.
        assert!(check_grouping("1234.56", Some('\''), None).is_ok());
    }

    #[test]
    fn leading_zeros_stay_text() {
        assert!(has_significant_leading_zero("007"));
        assert!(has_significant_leading_zero("0123"));
        assert!(!has_significant_leading_zero("0"));
        assert!(!has_significant_leading_zero("10"));
        assert!(!has_significant_leading_zero("0.5"));
    }

    #[test]
    fn i64_boundaries() {
        assert!(fits_i64("9223372036854775807"));
        assert!(!fits_i64("9223372036854775808"));
        assert!(!fits_i64("99999999999999999999"));
    }

    #[test]
    fn fractional_digit_count() {
        assert_eq!(frac_digits("1234.56", None), 2);
        assert_eq!(frac_digits("1.234,5", Some(',')), 1);
        assert_eq!(frac_digits("1234", None), 0);
        // German integers: '.' groups, so there is no fractional part.
        assert_eq!(frac_digits_with("1.234", None, Some('.')), 0);
        assert_eq!(frac_digits_with("1.234.567", None, Some('.')), 0);
        assert_eq!(frac_digits_with("1'234.56", None, Some('\'')), 2);
    }

    #[test]
    fn negative_and_trailing_sign() {
        let f = inf(&["-1,5", "2,75"]).unwrap();
        assert_eq!(f.decimal, Some(','));
        // A trailing minus is not a number tdy can parse, so it is not one.
        assert!(inf(&["1234-", "5678"]).is_none());
    }

}
