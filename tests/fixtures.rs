//! Value-level assertions against the committed hard fixtures.
//!
//! `tests/adversarial.rs` proves tdy never panics on these files. That is a
//! low bar: a parser that returns nothing at all also never panics. This file
//! is the other half — for a curated set of the nastiest fixtures it asserts
//! the *numbers*, so a regression that quietly changes an answer fails here
//! rather than shipping.
//!
//! Everything runs in-process through the heuristic tier only (no sidecars
//! are written, no model is consulted), which is also a standing check that
//! `backend = "none"` is genuinely useful on real files.

use std::path::{Path, PathBuf};

use datafusion::arrow::array::{
    Array, BooleanArray, Decimal128Array, Float64Array, Int64Array, StringArray,
};
use datafusion::arrow::record_batch::RecordBatch;

use tdy::config::Limits;
use tdy::provider::spec_to_batch;
use tdy::spec::{DType, Extraction, ParseSpec};
use tdy::{sample, sniff};

fn fixture(rel: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("testdata").join(rel)
}

/// Sniff and execute a fixture with heuristics only.
fn read(rel: &str) -> (ParseSpec, RecordBatch) {
    let p = fixture(rel);
    assert!(p.exists(), "missing fixture {rel} — run `python3 gen_fixtures.py`");
    let s = sample::build(&p, 16 * 1024).unwrap_or_else(|e| panic!("{rel}: sampling: {e:#}"));
    let spec = sniff::sniff(&p, &s, Limits::default())
        .unwrap_or_else(|e| panic!("{rel}: sniffing: {e:#}"))
        .spec;
    spec.validate()
        .unwrap_or_else(|e| panic!("{rel}: sniffed an invalid spec: {e:?}"));
    let batch = spec_to_batch(&spec, &p).unwrap_or_else(|e| panic!("{rel}: executing: {e:#}"));
    (spec, batch)
}

fn field(b: &RecordBatch, name: &str) -> usize {
    b.schema()
        .fields()
        .iter()
        .position(|f| f.name() == name)
        .unwrap_or_else(|| {
            panic!(
                "no column `{name}`; have {:?}",
                b.schema().fields().iter().map(|f| f.name().clone()).collect::<Vec<_>>()
            )
        })
}

fn strs(b: &RecordBatch, name: &str) -> Vec<Option<String>> {
    let a = b.column(field(b, name)).as_any().downcast_ref::<StringArray>().unwrap();
    (0..a.len())
        .map(|i| (!a.is_null(i)).then(|| a.value(i).to_string()))
        .collect()
}

fn ints(b: &RecordBatch, name: &str) -> Vec<Option<i64>> {
    let a = b.column(field(b, name)).as_any().downcast_ref::<Int64Array>().unwrap();
    (0..a.len()).map(|i| (!a.is_null(i)).then(|| a.value(i))).collect()
}

fn floats(b: &RecordBatch, name: &str) -> Vec<Option<f64>> {
    let a = b.column(field(b, name)).as_any().downcast_ref::<Float64Array>().unwrap();
    (0..a.len()).map(|i| (!a.is_null(i)).then(|| a.value(i))).collect()
}

fn decs(b: &RecordBatch, name: &str) -> Vec<Option<i128>> {
    let a = b.column(field(b, name)).as_any().downcast_ref::<Decimal128Array>().unwrap();
    (0..a.len()).map(|i| (!a.is_null(i)).then(|| a.value(i))).collect()
}

fn dtype(spec: &ParseSpec, name: &str) -> DType {
    spec.columns
        .iter()
        .find(|c| c.name == name)
        .unwrap_or_else(|| {
            panic!(
                "no column `{name}` in spec; have {:?}",
                spec.columns.iter().map(|c| c.name.clone()).collect::<Vec<_>>()
            )
        })
        .dtype
        .clone()
}

/// Numeric total of a column whatever its numeric type, for sum assertions.
fn total(b: &RecordBatch, name: &str) -> f64 {
    let col = b.column(field(b, name));
    if let Some(a) = col.as_any().downcast_ref::<Float64Array>() {
        return (0..a.len()).filter(|i| !a.is_null(*i)).map(|i| a.value(i)).sum();
    }
    if let Some(a) = col.as_any().downcast_ref::<Decimal128Array>() {
        let scale = 10f64.powi(a.scale() as i32);
        return (0..a.len())
            .filter(|i| !a.is_null(*i))
            .map(|i| a.value(i) as f64 / scale)
            .sum();
    }
    if let Some(a) = col.as_any().downcast_ref::<Int64Array>() {
        return (0..a.len()).filter(|i| !a.is_null(*i)).map(|i| a.value(i) as f64).sum();
    }
    panic!("column `{name}` is {:?}, not numeric", col.data_type())
}

// ---------------------------------------------------------------------------
// Encodings
// ---------------------------------------------------------------------------

/// The same table in four encodings must produce the same numbers and the
/// same text — including the BOM variant, whose first column must not be
/// named "\u{feff}region".
#[test]
fn the_encoding_family_all_read_identically() {
    for f in [
        "enc_family_utf8.csv",
        "enc_family_utf8_bom.csv",
        "enc_family_cp1252.csv",
        "enc_family_latin1.csv",
    ] {
        let (spec, b) = read(f);
        assert_eq!(b.num_rows(), 6, "{f}");
        assert!((total(&b, "umsatz") - 4261.59).abs() < 1e-6, "{f}: {}", total(&b, "umsatz"));
        assert_eq!(strs(&b, "stadt")[0].as_deref(), Some("Zürich"), "{f}");
        assert_eq!(spec.columns[0].name, "region", "{f}: a BOM leaked into a column name");
        assert!(
            !spec.columns.iter().any(|c| c.source_name().starts_with('\u{feff}')),
            "{f}: a BOM leaked into a source name"
        );
    }
}

/// The encoding trap: a file that is pure ASCII for 12 KB and then contains a
/// single windows-1252 byte. A guess frozen from the sample would mangle it.
#[test]
fn a_late_non_ascii_byte_is_still_decoded_correctly() {
    let (spec, b) = read("enc_late_1252_byte.csv");
    assert_eq!(b.num_rows(), 1200);
    let kunde = strs(&b, "kunde");
    assert_eq!(
        kunde[676].as_deref(),
        Some("O’Brien & Co"),
        "the single non-ASCII byte past the sample window was mis-decoded"
    );
    // Zero-padded ids keep their zeros.
    assert_eq!(dtype(&spec, "id"), DType::Utf8);
    assert_eq!(strs(&b, "id")[676].as_deref(), Some("00676"));
}

/// A NUL byte inside a field must not truncate it.
#[test]
fn a_nul_byte_does_not_truncate_a_field() {
    let (_, b) = read("enc_nul_byte.csv");
    let n = strs(&b, "notiz");
    assert_eq!(n[1].as_ref().map(|s| s.chars().count()), Some(5), "got {:?}", n[1]);
    assert_eq!(n[2].as_ref().map(|s| s.chars().count()), Some(5), "got {:?}", n[2]);
}

/// Mixed line endings must not leave a carriage return glued to a value.
#[test]
fn mixed_line_endings_do_not_corrupt_values() {
    let (_, b) = read("enc_mixed_eol.csv");
    assert_eq!(b.num_rows(), 6);
    assert!((total(&b, "umsatz") - 2100.0).abs() < 1e-9);
}

/// Two headers that look identical but differ by Unicode normalisation are
/// two columns, and each must carry its own data.
#[test]
fn normalisation_twins_stay_separate_columns() {
    let (_, b) = read("enc_normalization.csv");
    assert_eq!(ints(&b, "groesse")[0], Some(10));
    assert_eq!(ints(&b, "gro_sse")[0], Some(1));
}

// ---------------------------------------------------------------------------
// Delimited torture
// ---------------------------------------------------------------------------

#[test]
fn bom_and_crlf_together() {
    let (_, b) = read("csv_torture/csv_torture_bom_crlf.csv");
    assert_eq!(b.num_rows(), 6);
    assert!((total(&b, "umsatz") - 9149.90).abs() < 1e-6);
    assert_eq!(strs(&b, "region")[0].as_deref(), Some("Zürich"));
}

#[test]
fn quoted_fields_keep_their_delimiters_newlines_and_quotes() {
    let (_, b) = read("csv_torture/csv_torture_quoted.csv");
    let r = strs(&b, "bemerkung");
    assert_eq!(r[0].as_deref(), Some("Rabatt, 10% auf Artikel A"));
    assert_eq!(r[1].as_deref(), Some("Zeile1\nZeile2"));
    assert_eq!(r[2].as_deref(), Some("Er sagte \"Hallo, Welt\" und ging"));
}

/// A file with exactly one data row still has one data row — not zero (the
/// header eaten as data) and not two (the header counted as data).
#[test]
fn a_single_data_row_survives() {
    let (_, b) = read("csv_torture/csv_torture_one_row.csv");
    assert_eq!(b.num_rows(), 1);
    assert_eq!(strs(&b, "region")[0].as_deref(), Some("Ticino"));
}

#[test]
fn pipe_delimited_with_german_booleans() {
    let (spec, b) = read("csv_torture/csv_torture_pipe.psv");
    assert!(matches!(spec.extraction, Extraction::Delimited { delimiter: '|', .. }));
    let lager = b
        .column(field(&b, "lager"))
        .as_any()
        .downcast_ref::<BooleanArray>()
        .expect("ja/nein must type as boolean");
    assert!(lager.value(0));
    assert!(!lager.value(2));
}

/// A "Total" line at the end of a file is a summary, not a record: leaving it
/// in doubles every sum computed from the table.
#[test]
fn a_trailing_total_row_is_dropped() {
    let (spec, b) = read("csv_torture/csv_torture_total_footer.csv");
    assert!(
        spec.transforms.iter().any(|t| matches!(
            t,
            tdy::spec::Transform::SkipRows { tail, .. } if *tail == 1
        )),
        "expected a skip_rows tail, got {:?}",
        spec.transforms
    );
    let regions = strs(&b, "region");
    assert!(
        !regions.iter().flatten().any(|r| r.eq_ignore_ascii_case("total")),
        "the Total row survived into the data: {regions:?}"
    );
}

// ---------------------------------------------------------------------------
// Numbers: the whole point
// ---------------------------------------------------------------------------

/// German decimal commas, on a real export.
#[test]
fn german_decimal_commas_are_read_as_decimals() {
    let (spec, b) = read("swiss_german_finance/swiss_german_finance_03_dezimalkomma.csv");
    let menge = spec.columns.iter().find(|c| c.name.starts_with("menge")).unwrap();
    assert_eq!(menge.parse.decimal_separator, Some(','));
    assert_eq!(menge.parse.thousands_separator, None);
    // Mehl 1,5 kg — one and a half, not fifteen.
    let v = total(&b, &menge.name);
    assert!(v < 30.0, "the kg column sums to {v}: a comma was treated as grouping");
}

/// A column that mixes Swiss, German and Anglo conventions cannot be typed by
/// anyone. Refusing to guess is the correct answer.
#[test]
fn a_column_of_mixed_conventions_stays_text() {
    let (spec, b) = read("swiss_german_finance/swiss_german_finance_06_gemischte_konvention.csv");
    assert_eq!(dtype(&spec, "betrag"), DType::Utf8);
    let v = strs(&b, "betrag");
    assert_eq!(v[0].as_deref(), Some("1'234.56"));
    assert_eq!(v[1].as_deref(), Some("1.234,56"));
}

/// Impossible dates must not be typed as dates — that would turn a bad file
/// into a failing query instead of a text column the user can inspect.
#[test]
fn impossible_dates_are_not_typed_as_dates() {
    let (spec, _) = read("adversarial/adversarial_dates.csv");
    assert_eq!(dtype(&spec, "iso_ok"), DType::Date { format: "%Y-%m-%d".into() });
    assert_eq!(dtype(&spec, "iso_invalid"), DType::Utf8, "2024-02-30 is not a date");
    assert_eq!(dtype(&spec, "de_leap"), DType::Utf8, "29.02.2023 is not a date");
}

// ---------------------------------------------------------------------------
// JSON
// ---------------------------------------------------------------------------

#[test]
fn a_wrapped_records_array_is_found() {
    let (spec, b) = read("json_shapes/json_shapes_wrapped.json");
    match &spec.extraction {
        Extraction::Json { lines, pointer } => {
            assert!(!lines);
            assert_eq!(pointer.as_deref(), Some("/data"));
        }
        other => panic!("expected json extraction, got {other:?}"),
    }
    assert_eq!(b.num_rows(), 3);
}

#[test]
fn ndjson_row_counts_are_exact() {
    let (_, b) = read("json_shapes/json_shapes_trailing_newline.ndjson");
    assert_eq!(b.num_rows(), 4, "a trailing newline is not a record");
    assert_eq!(ints(&b, "id"), vec![Some(1), Some(2), Some(3), Some(4)]);
}

#[test]
fn nested_json_values_survive_as_json_text() {
    let (_, b) = read("json_shapes/json_shapes_nested.ndjson");
    let geo = strs(&b, "geo");
    assert_eq!(geo[0].as_deref(), Some(r#"{"lat":47.3769,"lon":8.5417}"#));
    assert_eq!(geo[2], None);
}

/// Keys that appear only in later records still become columns.
#[test]
fn heterogeneous_ndjson_keys_all_become_columns() {
    let (_, b) = read("json_shapes/json_shapes_hetero_keys.ndjson");
    for name in ["amount", "id", "region"] {
        let _ = field(&b, name);
    }
    assert!(b.num_columns() >= 5, "got {} columns", b.num_columns());
}

// ---------------------------------------------------------------------------
// Excel
// ---------------------------------------------------------------------------

/// A workbook whose first sheets are a cover page and a legend. Picking a
/// sheet by size or by position both get this wrong; the data sheet is the
/// one with quantities in it.
#[test]
fn the_data_sheet_is_chosen_over_a_cover_page_and_a_legend() {
    let (spec, b) = read("excel_nightmares_cover_sheet.xlsx");
    match &spec.extraction {
        Extraction::Excel { sheet_name, .. } => {
            assert_eq!(sheet_name.as_deref(), Some("Bewegungen"), "wrong sheet chosen")
        }
        other => panic!("expected an excel extraction, got {other:?}"),
    }
    assert_eq!(b.num_rows(), 24);
    let _ = field(&b, "buchungs_nr");
    assert!((total(&b, "betrag_chf") - 56_469.00).abs() < 1e-6);
}

/// A 3000-row sheet with a "Total" row at the end. Leaving that row in
/// doubles every sum computed from the sheet — and the row is well past any
/// probe window, which is exactly how it used to survive.
#[test]
fn a_total_row_at_the_end_of_a_long_sheet_is_dropped() {
    let (_, b) = read("excel_nightmares_3000_rows.xlsx");
    assert_eq!(b.num_rows(), 3000);
    assert_eq!(
        ints(&b, "id").into_iter().flatten().max(),
        Some(3000),
        "the Total row survived into the data"
    );
    assert!((total(&b, "betrag") - 7_627_533.57).abs() < 1e-4);
}

/// Excel dates arrive as typed cells; a contract sheet must read as dates,
/// and an id column with leading zeros must not.
#[test]
fn excel_dates_and_padded_ids_are_typed_correctly() {
    let (spec, b) = read("excel_nightmares_mixed_dates.xlsx");
    assert_eq!(dtype(&spec, "vertrag_nr"), DType::Utf8);
    assert_eq!(strs(&b, "vertrag_nr")[0].as_deref(), Some("00123"));
    assert!(
        matches!(dtype(&spec, "start"), DType::Date { .. }),
        "start is {:?}",
        dtype(&spec, "start")
    );
}

/// A merged multi-row header is what tier 2 is *for*. Tier 1 must say so
/// rather than shipping a confident wrong reading.
#[test]
fn a_merged_multirow_header_is_reported_as_uncertain() {
    let p = fixture("excel_nightmares_merged_header.xlsx");
    let s = sample::build(&p, 16 * 1024).unwrap();
    let r = sniff::sniff(&p, &s, Limits::default()).unwrap();
    assert!(
        r.confidence < 0.8,
        "a merged header must fall below the escalation threshold, got {}",
        r.confidence
    );
    assert!(!r.spec.notes.is_empty(), "and must say why");
}

// ---------------------------------------------------------------------------
// Line-oriented files: the heuristic tier must handle these unaided
// ---------------------------------------------------------------------------

#[test]
fn an_nginx_log_is_read_without_a_model() {
    let (spec, b) = read("logs_fixed_width_nginx_access.log");
    assert!(
        matches!(spec.extraction, Extraction::Lines { .. }),
        "expected a lines extraction, got {:?}",
        spec.extraction
    );
    assert!(b.num_rows() > 100, "only {} rows", b.num_rows());
    let status = ints(&b, "status");
    assert!(status.iter().flatten().any(|s| *s == 200));
    assert!(status.iter().all(|s| s.map(|v| (100..=599).contains(&v)).unwrap_or(true)));
}

#[test]
fn a_syslog_file_is_read_without_a_model() {
    let (spec, b) = read("logs_fixed_width_syslog.log");
    assert!(matches!(spec.extraction, Extraction::Lines { .. }));
    assert!(b.num_rows() > 5);
    let _ = field(&b, "host");
}

#[test]
fn a_java_application_log_skips_its_stack_traces() {
    let (spec, b) = read("logs_fixed_width_java_app.log");
    assert!(matches!(spec.extraction, Extraction::Lines { .. }));
    // Continuation lines ("\tat com.example...") are not log records.
    let level = strs(&b, "level");
    assert!(
        level.iter().flatten().all(|l| l.chars().all(|c| c.is_ascii_uppercase())),
        "a stack-trace line was parsed as a record: {level:?}"
    );
}
