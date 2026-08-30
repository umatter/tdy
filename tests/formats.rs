//! Format and transform semantics, pinned with hand-written specs.
//!
//! `fixed_width` and most transform combinations had no test at all. These
//! tests are the contract: they say what each knob *means*, so that changing
//! the implementation has to be a deliberate act.

use std::fs;
use std::path::PathBuf;
use std::process::Command;

use datafusion::arrow::array::{
    Array, BooleanArray, Date32Array, Decimal128Array, Float64Array, Int64Array, StringArray,
    TimestampMicrosecondArray,
};
use datafusion::arrow::record_batch::RecordBatch;
use tempfile::TempDir;

use tdy::provider::spec_to_batch;
use tdy::spec::*;

// ---------------------------------------------------------------------------
// helpers
// ---------------------------------------------------------------------------

fn dir_file(dir: &TempDir, name: &str, body: &str) -> PathBuf {
    let p = dir.path().join(name);
    fs::write(&p, body).unwrap();
    p
}

fn strings(b: &RecordBatch, i: usize) -> Vec<String> {
    let a = b.column(i).as_any().downcast_ref::<StringArray>().unwrap();
    (0..a.len())
        .map(|i| if a.is_null(i) { "<null>".to_string() } else { a.value(i).to_string() })
        .collect()
}

fn ints(b: &RecordBatch, i: usize) -> Vec<Option<i64>> {
    let a = b.column(i).as_any().downcast_ref::<Int64Array>().unwrap();
    (0..a.len()).map(|i| if a.is_null(i) { None } else { Some(a.value(i)) }).collect()
}

fn floats(b: &RecordBatch, i: usize) -> Vec<Option<f64>> {
    let a = b.column(i).as_any().downcast_ref::<Float64Array>().unwrap();
    (0..a.len()).map(|i| if a.is_null(i) { None } else { Some(a.value(i)) }).collect()
}

fn decimals(b: &RecordBatch, i: usize) -> Vec<Option<i128>> {
    let a = b.column(i).as_any().downcast_ref::<Decimal128Array>().unwrap();
    (0..a.len()).map(|i| if a.is_null(i) { None } else { Some(a.value(i)) }).collect()
}

fn col(name: &str, dtype: DType) -> ColumnSpec {
    ColumnSpec { name: name.into(), source: None, dtype, nullable: true, parse: ValueParsing::default() }
}

fn col_from(name: &str, source: &str, dtype: DType) -> ColumnSpec {
    ColumnSpec {
        name: name.into(),
        source: Some(source.into()),
        dtype,
        nullable: true,
        parse: ValueParsing::default(),
    }
}

fn spec(extraction: Extraction, transforms: Vec<Transform>, columns: Vec<ColumnSpec>) -> ParseSpec {
    ParseSpec { extraction, transforms, columns, confidence: None, notes: vec![] }
}

fn delim(d: char, ragged: RaggedPolicy) -> Extraction {
    Extraction::Delimited {
        delimiter: d,
        quote: Some('"'),
        escape: None,
        encoding: None,
        comment: None,
        ragged,
    }
}

// ---------------------------------------------------------------------------
// fixed_width
// ---------------------------------------------------------------------------

/// Offsets count CHARACTERS, not bytes. A fixed-width report is laid out by
/// what a human sees in a monospace font; an umlaut occupies one column there
/// but two bytes in UTF-8. Counting bytes shifts every field after the first
/// non-ASCII character — silently, into the neighbouring column.
#[test]
fn fixed_width_offsets_are_character_positions() {
    let dir = TempDir::new().unwrap();
    // columns:      0123456789|012
    let p = dir_file(&dir, "fw.txt", "Mueller   100\nMüller    200\nÖzil      -50\n");
    let s = spec(
        Extraction::FixedWidth {
            encoding: None,
            fields: vec![
                FixedField { name: "name".into(), start: 0, end: 10 },
                FixedField { name: "amount".into(), start: 10, end: 13 },
            ],
        },
        vec![],
        vec![col("name", DType::Utf8), col("amount", DType::Int64)],
    );
    let b = spec_to_batch(&s, &p).unwrap();
    assert_eq!(strings(&b, 0), vec!["Mueller", "Müller", "Özil"]);
    assert_eq!(ints(&b, 1), vec![Some(100), Some(200), Some(-50)]);
}

#[test]
fn fixed_width_short_lines_pad_rather_than_panic() {
    let dir = TempDir::new().unwrap();
    let p = dir_file(&dir, "short.txt", "abc\nabcdefghij\n");
    let s = spec(
        Extraction::FixedWidth {
            encoding: None,
            fields: vec![
                FixedField { name: "a".into(), start: 0, end: 3 },
                FixedField { name: "b".into(), start: 3, end: 20 },
            ],
        },
        vec![],
        vec![col("a", DType::Utf8), col("b", DType::Utf8)],
    );
    let b = spec_to_batch(&s, &p).unwrap();
    assert_eq!(strings(&b, 0), vec!["abc", "abc"]);
    assert_eq!(strings(&b, 1), vec!["<null>", "defghij"]);
}

// ---------------------------------------------------------------------------
// lines
// ---------------------------------------------------------------------------

#[test]
fn lines_skips_non_matching_by_default_and_errors_on_demand() {
    let dir = TempDir::new().unwrap();
    let body = "# banner\n2026-01-05 INFO up\nrubbish\n2026-01-05 WARN slow\n";
    let p = dir_file(&dir, "a.log", body);
    let pattern = r"^(?P<d>\d{4}-\d{2}-\d{2}) (?P<lvl>\w+) (?P<msg>.*)$";

    let skip = spec(
        Extraction::Lines {
            pattern: pattern.into(),
            encoding: None,
            on_no_match: NoMatchPolicy::Skip,
        },
        vec![],
        vec![col("lvl", DType::Utf8)],
    );
    let b = spec_to_batch(&skip, &p).unwrap();
    assert_eq!(strings(&b, 0), vec!["INFO", "WARN"]);

    let strict = spec(
        Extraction::Lines {
            pattern: pattern.into(),
            encoding: None,
            on_no_match: NoMatchPolicy::Error,
        },
        vec![],
        vec![col("lvl", DType::Utf8)],
    );
    let err = spec_to_batch(&strict, &p).unwrap_err();
    assert!(format!("{err:#}").contains("does not match"));
}

#[test]
fn lines_optional_groups_become_nulls_not_empty_strings() {
    let dir = TempDir::new().unwrap();
    let p = dir_file(&dir, "b.log", "GET /a 200\nGET /b\n");
    let s = spec(
        Extraction::Lines {
            pattern: r"^(?P<verb>\w+) (?P<path>\S+)(?: (?P<status>\d+))?$".into(),
            encoding: None,
            on_no_match: NoMatchPolicy::Skip,
        },
        vec![],
        vec![col("verb", DType::Utf8), col("status", DType::Int64)],
    );
    let b = spec_to_batch(&s, &p).unwrap();
    assert_eq!(ints(&b, 1), vec![Some(200), None]);
}

// ---------------------------------------------------------------------------
// json
// ---------------------------------------------------------------------------

#[test]
fn json_pointer_nested_values_and_missing_keys() {
    let dir = TempDir::new().unwrap();
    let p = dir_file(
        &dir,
        "doc.json",
        r#"{"meta":{"v":1},"data":[
            {"id":1,"tags":["a","b"],"nested":{"k":2}},
            {"id":2,"extra":"late"}
        ]}"#,
    );
    let s = spec(
        Extraction::Json { lines: false, pointer: Some("/data".into()) },
        vec![],
        vec![col("id", DType::Int64), col("tags", DType::Utf8), col("extra", DType::Utf8)],
    );
    let b = spec_to_batch(&s, &p).unwrap();
    assert_eq!(ints(&b, 0), vec![Some(1), Some(2)]);
    assert_eq!(strings(&b, 1), vec![r#"["a","b"]"#, "<null>"]);
    assert_eq!(strings(&b, 2), vec!["<null>", "late"]);
}

#[test]
fn ndjson_keeps_large_integers_exact() {
    let dir = TempDir::new().unwrap();
    let p = dir_file(
        &dir,
        "big.ndjson",
        "{\"id\":9007199254740993}\n{\"id\":9223372036854775807}\n",
    );
    let s = spec(
        Extraction::Json { lines: true, pointer: None },
        vec![],
        vec![col("id", DType::Int64)],
    );
    let b = spec_to_batch(&s, &p).unwrap();
    assert_eq!(ints(&b, 0), vec![Some(9007199254740993), Some(9223372036854775807)]);
}

// ---------------------------------------------------------------------------
// delimited edge cases
// ---------------------------------------------------------------------------

#[test]
fn ragged_policies_behave_as_documented() {
    let dir = TempDir::new().unwrap();
    let p = dir_file(&dir, "r.csv", "a,b,c\n1,2\n3,4,5,6\n");

    let strict = spec(delim(',', RaggedPolicy::Error), vec![], vec![col("col_1", DType::Utf8)]);
    assert!(spec_to_batch(&strict, &p).is_err(), "Error policy must reject ragged input");

    let pad = spec(
        delim(',', RaggedPolicy::PadNulls),
        vec![Transform::PromoteHeader { rows: 1, join: " ".into() }],
        vec![col("a", DType::Utf8), col("c", DType::Utf8)],
    );
    let b = spec_to_batch(&pad, &p).unwrap();
    assert_eq!(strings(&b, 0), vec!["1", "3"]);
    assert_eq!(strings(&b, 1), vec!["<null>", "5"]);

    let trunc = spec(
        delim(',', RaggedPolicy::TruncateExtra),
        vec![Transform::PromoteHeader { rows: 1, join: " ".into() }],
        vec![col("a", DType::Utf8)],
    );
    assert!(spec_to_batch(&trunc, &p).is_ok());
}

#[test]
fn quoted_fields_with_delimiters_and_newlines() {
    let dir = TempDir::new().unwrap();
    let p = dir_file(&dir, "q.csv", "id,note\n1,\"a,b\nsecond\"\n2,\"say \"\"hi\"\"\"\n");
    let s = spec(
        delim(',', RaggedPolicy::PadNulls),
        vec![Transform::PromoteHeader { rows: 1, join: " ".into() }],
        vec![col("id", DType::Int64), col("note", DType::Utf8)],
    );
    let b = spec_to_batch(&s, &p).unwrap();
    assert_eq!(b.num_rows(), 2);
    assert_eq!(strings(&b, 1), vec!["a,b\nsecond", "say \"hi\""]);
}

#[test]
fn comment_lines_are_skipped() {
    let dir = TempDir::new().unwrap();
    let p = dir_file(&dir, "c.csv", "# note\na,b\n1,2\n# tail\n");
    let s = spec(
        Extraction::Delimited {
            delimiter: ',',
            quote: Some('"'),
            escape: None,
            encoding: None,
            comment: Some('#'),
            ragged: RaggedPolicy::PadNulls,
        },
        vec![Transform::PromoteHeader { rows: 1, join: " ".into() }],
        vec![col("a", DType::Int64)],
    );
    let b = spec_to_batch(&s, &p).unwrap();
    assert_eq!(ints(&b, 0), vec![Some(1)]);
}

#[test]
fn declared_encoding_is_honoured() {
    let dir = TempDir::new().unwrap();
    let p = dir.path().join("latin.csv");
    // "name\nMüller\n" in windows-1252
    fs::write(&p, b"name\nM\xfcller\n").unwrap();
    let s = spec(
        Extraction::Delimited {
            delimiter: ',',
            quote: Some('"'),
            escape: None,
            encoding: Some("windows-1252".into()),
            comment: None,
            ragged: RaggedPolicy::PadNulls,
        },
        vec![Transform::PromoteHeader { rows: 1, join: " ".into() }],
        vec![col("name", DType::Utf8)],
    );
    let b = spec_to_batch(&s, &p).unwrap();
    assert_eq!(strings(&b, 0), vec!["Müller"]);
}

// ---------------------------------------------------------------------------
// transforms
// ---------------------------------------------------------------------------

#[test]
fn skip_rows_head_and_tail_then_multirow_header() {
    let dir = TempDir::new().unwrap();
    let p = dir_file(
        &dir,
        "t.csv",
        "Report\nStand 2026\n,2025,\nRegion,Jan,Feb\nOst,1,2\nWest,3,4\nTotal,4,6\n",
    );
    let s = spec(
        delim(',', RaggedPolicy::PadNulls),
        vec![
            Transform::SkipRows { head: 2, tail: 1 },
            Transform::PromoteHeader { rows: 2, join: " ".into() },
        ],
        vec![
            // "Region" sits in the lower header row and has no year above it;
            // the month columns inherit the horizontally merged "2025".
            col_from("region", "Region", DType::Utf8),
            col_from("jan", "2025 Jan", DType::Int64),
            col_from("feb", "2025 Feb", DType::Int64),
        ],
    );
    let b = spec_to_batch(&s, &p).unwrap();
    assert_eq!(strings(&b, 0), vec!["Ost", "West"]);
    assert_eq!(ints(&b, 1), vec![Some(1), Some(3)]);
    assert_eq!(ints(&b, 2), vec![Some(2), Some(4)]);
}

#[test]
fn fill_down_then_drop_rows_then_unpivot() {
    let dir = TempDir::new().unwrap();
    let p = dir_file(
        &dir,
        "u.csv",
        "region,produkt,jan,feb\nOst,A,1,2\n,B,3,4\nZwischensumme,,4,6\nWest,A,5,6\n",
    );
    let s = spec(
        delim(',', RaggedPolicy::PadNulls),
        vec![
            Transform::PromoteHeader { rows: 1, join: " ".into() },
            Transform::FillDown { columns: vec!["region".into()] },
            Transform::DropRowsMatching {
                pattern: "(?i)^zwischensumme$".into(),
                column: Some("region".into()),
            },
            Transform::Unpivot {
                id_columns: vec!["region".into(), "produkt".into()],
                value_columns: vec!["jan".into(), "feb".into()],
                variable_name: "monat".into(),
                value_name: "wert".into(),
            },
        ],
        vec![
            col("region", DType::Utf8),
            col("monat", DType::Utf8),
            col("wert", DType::Int64),
        ],
    );
    let b = spec_to_batch(&s, &p).unwrap();
    assert_eq!(b.num_rows(), 6, "3 kept rows x 2 value columns");
    assert_eq!(strings(&b, 0), vec!["Ost", "Ost", "Ost", "Ost", "West", "West"]);
    assert_eq!(strings(&b, 1), vec!["jan", "feb", "jan", "feb", "jan", "feb"]);
    assert_eq!(
        ints(&b, 2),
        vec![Some(1), Some(2), Some(3), Some(4), Some(5), Some(6)]
    );
}

#[test]
fn drop_rows_matching_without_a_column_tests_the_whole_row() {
    let dir = TempDir::new().unwrap();
    let p = dir_file(&dir, "d.csv", "a,b\n1,keep\n2,DROPME\n3,keep\n");
    let s = spec(
        delim(',', RaggedPolicy::PadNulls),
        vec![
            Transform::PromoteHeader { rows: 1, join: " ".into() },
            Transform::DropRowsMatching { pattern: "DROPME".into(), column: None },
        ],
        vec![col("a", DType::Int64)],
    );
    let b = spec_to_batch(&s, &p).unwrap();
    assert_eq!(ints(&b, 0), vec![Some(1), Some(3)]);
}

#[test]
fn a_column_that_does_not_exist_names_the_available_ones() {
    let dir = TempDir::new().unwrap();
    let p = dir_file(&dir, "n.csv", "a,b\n1,2\n");
    let s = spec(
        delim(',', RaggedPolicy::PadNulls),
        vec![Transform::PromoteHeader { rows: 1, join: " ".into() }],
        vec![col("nope", DType::Utf8)],
    );
    let msg = format!("{:#}", spec_to_batch(&s, &p).unwrap_err());
    assert!(msg.contains("nope"), "{msg}");
    assert!(msg.contains('a') && msg.contains('b'), "error should list available columns: {msg}");
}

// ---------------------------------------------------------------------------
// typed casting
// ---------------------------------------------------------------------------

#[test]
fn decimal_rounds_half_away_from_zero_in_both_directions() {
    let dir = TempDir::new().unwrap();
    let p = dir_file(&dir, "dec.csv", "v\n1.005\n-1.005\n2.344\n-2.346\n");
    let s = spec(
        delim(',', RaggedPolicy::PadNulls),
        vec![Transform::PromoteHeader { rows: 1, join: " ".into() }],
        vec![col("v", DType::Decimal { precision: 12, scale: 2 })],
    );
    let b = spec_to_batch(&s, &p).unwrap();
    assert_eq!(decimals(&b, 0), vec![Some(101), Some(-101), Some(234), Some(-235)]);
}

#[test]
fn decimal_overflowing_the_declared_precision_is_an_error() {
    let dir = TempDir::new().unwrap();
    let p = dir_file(&dir, "over.csv", "v\n12345.67\n");
    let s = spec(
        delim(',', RaggedPolicy::PadNulls),
        vec![Transform::PromoteHeader { rows: 1, join: " ".into() }],
        vec![col("v", DType::Decimal { precision: 5, scale: 2 })],
    );
    let msg = format!("{:#}", spec_to_batch(&s, &p).unwrap_err());
    assert!(msg.contains("exceeds") || msg.contains("precision"), "{msg}");
}

#[test]
fn month_year_dates_pin_to_the_first_of_the_month() {
    let dir = TempDir::new().unwrap();
    let p = dir_file(&dir, "m.csv", "d\n2025 Jan\n2025 Mar\n");
    let s = spec(
        delim(',', RaggedPolicy::PadNulls),
        vec![Transform::PromoteHeader { rows: 1, join: " ".into() }],
        vec![col("d", DType::Date { format: "%Y %b".into() })],
    );
    let b = spec_to_batch(&s, &p).unwrap();
    let a = b.column(0).as_any().downcast_ref::<Date32Array>().unwrap();
    let epoch = chrono::NaiveDate::from_ymd_opt(1970, 1, 1).unwrap();
    let jan = chrono::NaiveDate::from_ymd_opt(2025, 1, 1).unwrap();
    assert_eq!(a.value(0), (jan - epoch).num_days() as i32);
}

#[test]
fn an_impossible_date_is_an_error_with_the_row_number() {
    let dir = TempDir::new().unwrap();
    let p = dir_file(&dir, "bad.csv", "d\n2024-02-10\n2024-02-30\n");
    let s = spec(
        delim(',', RaggedPolicy::PadNulls),
        vec![Transform::PromoteHeader { rows: 1, join: " ".into() }],
        vec![col("d", DType::Date { format: "%Y-%m-%d".into() })],
    );
    let msg = format!("{:#}", spec_to_batch(&s, &p).unwrap_err());
    assert!(msg.contains("row 2"), "error should point at the offending row: {msg}");
}

#[test]
fn value_cleanup_order_is_trim_replace_na_strip() {
    let dir = TempDir::new().unwrap();
    let p = dir_file(&dir, "v.csv", "v\n  CHF 1'200.00 \nk.A.\nCHF 0.00\n");
    let s = ParseSpec {
        extraction: delim(';', RaggedPolicy::PadNulls),
        transforms: vec![Transform::PromoteHeader { rows: 1, join: " ".into() }],
        columns: vec![ColumnSpec {
            name: "v".into(),
            source: None,
            dtype: DType::Decimal { precision: 12, scale: 2 },
            nullable: true,
            parse: ValueParsing {
                na_values: vec!["k.A.".into()],
                strip: Some(r"^CHF\s*".into()),
                thousands_separator: Some('\''),
                ..Default::default()
            },
        }],
        confidence: None,
        notes: vec![],
    };
    let b = spec_to_batch(&s, &p).unwrap();
    assert_eq!(decimals(&b, 0), vec![Some(120000), None, Some(0)]);
}

#[test]
fn booleans_use_declared_tokens_case_insensitively() {
    let dir = TempDir::new().unwrap();
    let p = dir_file(&dir, "b.csv", "flag\nJa\nNEIN\nja\n");
    let s = ParseSpec {
        extraction: delim(';', RaggedPolicy::PadNulls),
        transforms: vec![Transform::PromoteHeader { rows: 1, join: " ".into() }],
        columns: vec![ColumnSpec {
            name: "flag".into(),
            source: None,
            dtype: DType::Bool,
            nullable: true,
            parse: ValueParsing {
                true_values: vec!["ja".into()],
                false_values: vec!["nein".into()],
                ..Default::default()
            },
        }],
        confidence: None,
        notes: vec![],
    };
    let b = spec_to_batch(&s, &p).unwrap();
    let a = b.column(0).as_any().downcast_ref::<BooleanArray>().unwrap();
    assert_eq!(
        (0..3).map(|i| a.value(i)).collect::<Vec<_>>(),
        vec![true, false, true]
    );
}

#[test]
fn a_null_in_a_non_nullable_column_is_an_error() {
    let dir = TempDir::new().unwrap();
    let p = dir_file(&dir, "nn.csv", "k;v\na;1\nb;\n");
    let s = ParseSpec {
        extraction: delim(';', RaggedPolicy::PadNulls),
        transforms: vec![Transform::PromoteHeader { rows: 1, join: " ".into() }],
        columns: vec![ColumnSpec {
            name: "v".into(),
            source: None,
            dtype: DType::Int64,
            nullable: false,
            parse: ValueParsing::default(),
        }],
        confidence: None,
        notes: vec![],
    };
    let msg = format!("{:#}", spec_to_batch(&s, &p).unwrap_err());
    assert!(msg.contains("non-nullable"), "{msg}");
}

/// A timestamp column labelled with a timezone must describe the same instant
/// it names. Arrow timestamps with a timezone are UTC instants, so a naive
/// "10:00" in +02:00 is 08:00 UTC. Anything else labels the data wrongly.
#[test]
fn timestamps_with_a_fixed_offset_are_converted_to_utc() {
    let dir = TempDir::new().unwrap();
    let p = dir_file(&dir, "ts.csv", "t\n2026-01-05 10:00:00\n");
    let s = spec(
        delim(';', RaggedPolicy::PadNulls),
        vec![Transform::PromoteHeader { rows: 1, join: " ".into() }],
        vec![col(
            "t",
            DType::Timestamp { format: "%Y-%m-%d %H:%M:%S".into(), timezone: Some("+02:00".into()) },
        )],
    );
    let b = spec_to_batch(&s, &p).unwrap();
    let a = b.column(0).as_any().downcast_ref::<TimestampMicrosecondArray>().unwrap();
    let expected = chrono::NaiveDate::from_ymd_opt(2026, 1, 5)
        .unwrap()
        .and_hms_opt(8, 0, 0)
        .unwrap()
        .and_utc()
        .timestamp_micros();
    assert_eq!(a.value(0), expected);
}

// ---------------------------------------------------------------------------
// excel
// ---------------------------------------------------------------------------

fn make_xlsx(dir: &TempDir, script: &str) -> bool {
    let s = dir.path().join("mk.py");
    fs::write(&s, script).unwrap();
    Command::new("python3")
        .arg(&s)
        .arg(dir.path())
        .status()
        .map(|st| st.success())
        .unwrap_or(false)
}

#[test]
fn excel_sheet_selection_and_a1_range() {
    let dir = TempDir::new().unwrap();
    let ok = make_xlsx(
        &dir,
        r#"
import sys, os
from openpyxl import Workbook
d = sys.argv[1]
wb = Workbook()
cover = wb.active; cover.title = "Cover"; cover.append(["ignore me"])
data = wb.create_sheet("Daten")
data.append(["junk", "junk"])
data.append(["k", "v"])
data.append(["a", 1])
data.append(["b", 2])
wb.save(os.path.join(d, "sheets.xlsx"))
"#,
    );
    if !ok {
        eprintln!("skipping: python3/openpyxl unavailable");
        return;
    }
    let p = dir.path().join("sheets.xlsx");

    let by_name = spec(
        Extraction::Excel { sheet_name: Some("Daten".into()), sheet_index: None, range: None },
        vec![
            Transform::SkipRows { head: 1, tail: 0 },
            Transform::PromoteHeader { rows: 1, join: " ".into() },
        ],
        vec![col("k", DType::Utf8), col("v", DType::Int64)],
    );
    let b = spec_to_batch(&by_name, &p).unwrap();
    assert_eq!(strings(&b, 0), vec!["a", "b"]);

    let by_index = spec(
        Extraction::Excel { sheet_name: None, sheet_index: Some(1), range: None },
        vec![
            Transform::SkipRows { head: 1, tail: 0 },
            Transform::PromoteHeader { rows: 1, join: " ".into() },
        ],
        vec![col("v", DType::Int64)],
    );
    assert_eq!(ints(&spec_to_batch(&by_index, &p).unwrap(), 0), vec![Some(1), Some(2)]);

    let ranged = spec(
        Extraction::Excel {
            sheet_name: Some("Daten".into()),
            sheet_index: None,
            range: Some("A2:B4".into()),
        },
        vec![Transform::PromoteHeader { rows: 1, join: " ".into() }],
        vec![col("v", DType::Int64)],
    );
    assert_eq!(ints(&spec_to_batch(&ranged, &p).unwrap(), 0), vec![Some(1), Some(2)]);
}

#[test]
fn excel_missing_sheet_names_the_available_ones() {
    let dir = TempDir::new().unwrap();
    let ok = make_xlsx(
        &dir,
        r#"
import sys, os
from openpyxl import Workbook
wb = Workbook(); wb.active.title = "Only"; wb.active.append(["a"])
wb.save(os.path.join(sys.argv[1], "one.xlsx"))
"#,
    );
    if !ok {
        eprintln!("skipping excel_missing_sheet_names_the_available_ones: python3/openpyxl unavailable");
        return;
    }
    let p = dir.path().join("one.xlsx");
    let s = spec(
        Extraction::Excel { sheet_name: Some("Nope".into()), sheet_index: None, range: None },
        vec![],
        vec![col("col_1", DType::Utf8)],
    );
    let msg = format!("{:#}", spec_to_batch(&s, &p).unwrap_err());
    assert!(msg.contains("Only"), "error should list available sheets: {msg}");
}

#[test]
fn excel_error_cells_are_visible_not_silently_empty() {
    let dir = TempDir::new().unwrap();
    let ok = make_xlsx(
        &dir,
        r##"
import sys, os
from openpyxl import Workbook
wb = Workbook(); ws = wb.active
ws.append(["v"]); ws.append(["#DIV/0!"]); ws.append([1])
wb.save(os.path.join(sys.argv[1], "err.xlsx"))
"##,
    );
    if !ok {
        eprintln!("skipping excel_error_cells_are_visible_not_silently_empty: python3/openpyxl unavailable");
        return;
    }
    let p = dir.path().join("err.xlsx");
    let s = spec(
        Extraction::Excel { sheet_name: None, sheet_index: None, range: None },
        vec![Transform::PromoteHeader { rows: 1, join: " ".into() }],
        vec![col("v", DType::Utf8)],
    );
    let b = spec_to_batch(&s, &p).unwrap();
    let got = strings(&b, 0);
    assert!(
        got[0].to_ascii_uppercase().contains("DIV") || got[0].contains("ERR"),
        "an error cell must be visible in the output, got {got:?}"
    );
}

// ---------------------------------------------------------------------------
// float parsing under separator conventions
// ---------------------------------------------------------------------------

#[test]
fn float_with_declared_separators() {
    let dir = TempDir::new().unwrap();
    let p = dir_file(&dir, "f.csv", "v\n1.234,56\n-99,00\n");
    let s = ParseSpec {
        extraction: delim(';', RaggedPolicy::PadNulls),
        transforms: vec![Transform::PromoteHeader { rows: 1, join: " ".into() }],
        columns: vec![ColumnSpec {
            name: "v".into(),
            source: None,
            dtype: DType::Float64,
            nullable: true,
            parse: ValueParsing {
                thousands_separator: Some('.'),
                decimal_separator: Some(','),
                ..Default::default()
            },
        }],
        confidence: None,
        notes: vec![],
    };
    let b = spec_to_batch(&s, &p).unwrap();
    assert_eq!(floats(&b, 0), vec![Some(1234.56), Some(-99.0)]);
}

/// What a `number-columns-repeated` run *means*, pinned with a hand-written
/// spec so the answer does not depend on what tier 1 makes of the file.
///
/// LibreOffice writes any run of identical or empty cells as a single
/// element with a repeat count, so this shape is ordinary rather than
/// exotic. If the run were ignored instead of expanded, the trailing 5 would
/// land in column B and every column after the gap would be silently wrong —
/// a table that still parses and still sums, to the wrong total.
#[test]
fn an_ods_repeated_cell_run_expands_to_the_columns_it_claims() {
    let p = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("testdata")
        .join("legacy_formats_ods_sparse.ods");
    assert!(p.exists(), "missing fixture — run `python3 gen_fixtures.py`");

    let s = spec(
        Extraction::Excel { sheet_name: Some("Sparse".into()), sheet_index: None, range: None },
        vec![Transform::PromoteHeader { rows: 1, join: " ".into() }],
        ["a", "b", "c", "d", "e"].into_iter().map(|n| col(n, DType::Utf8)).collect(),
    );
    let b = spec_to_batch(&s, &p).unwrap();

    assert_eq!(b.num_rows(), 3, "number-rows-repeated did not expand to two rows");
    // The sparse row: 1, a three-cell empty run, then 5 in column E.
    assert_eq!(strings(&b, 0), vec!["1", "1", "1"]);
    assert_eq!(strings(&b, 1), vec!["<null>", "2", "2"]);
    assert_eq!(strings(&b, 2), vec!["<null>", "3", "3"]);
    assert_eq!(strings(&b, 3), vec!["<null>", "4", "4"]);
    assert_eq!(strings(&b, 4), vec!["5", "5", "5"], "the run swallowed column E");
}
