//! A JSON number keeps the digits it was written with.
//!
//! serde_json holds a number as a `u64`, an `i64` or an `f64`, so before tdy
//! read JSON data through its own reader (`src/jsondoc.rs`) an identifier
//! past `u64` came out as `1.2345678901234568e22` and a thirty-digit amount
//! lost its tail — a plausible wrong value, silently, on every JSON path.
//! These tests read such numbers down each path (a record array, one under a
//! `pointer`, NDJSON, a document read as one record, a column's own
//! `pointer` into a nested value) and then through `fit` and `dataset()`,
//! where the typing that was always right gets the digits it was owed: an
//! integer too large for `BIGINT` fits a declared `DECIMAL(38, 0)`, a long
//! decimal parses exactly into `DECIMAL(p, s)`, and more fractional digits
//! than the scale is still refused under the rounding rule.

use std::fs;
use std::path::{Path, PathBuf};

use datafusion::arrow::array::{Array, StringArray};
use datafusion::arrow::record_batch::RecordBatch;
use tempfile::TempDir;

use tdy::provider::spec_to_batch;
use tdy::spec::*;

const BIG: &str = "12345678901234567890123";
const LONG: &str = "0.123456789012345678901234567891";
const PAST_U64: &str = "100000000000000000000";

fn write(dir: &TempDir, name: &str, body: &str) -> PathBuf {
    let p = dir.path().join(name);
    fs::write(&p, body).unwrap();
    p
}

fn strings(b: &RecordBatch, i: usize) -> Vec<String> {
    let a = b.column(i).as_any().downcast_ref::<StringArray>().unwrap();
    (0..a.len()).map(|i| if a.is_null(i) { "<null>".to_string() } else { a.value(i).to_string() }).collect()
}

fn text_col(name: &str) -> ColumnSpec {
    ColumnSpec { name: name.into(), source: None, dtype: DType::Utf8, nullable: true, parse: ValueParsing::default(), pointer: None }
}

fn spec(extraction: Extraction, columns: Vec<ColumnSpec>) -> ParseSpec {
    ParseSpec { extraction, transforms: vec![], columns, confidence: Some(1.0), notes: vec![] }
}

/// One record, the same on every path: the numbers serde_json got wrong, the
/// ones it got right (which must not move by a byte), and a nested object.
fn record() -> String {
    format!(
        r#"{{"id":{BIG},"amount":{LONG},"past":{PAST_U64},"huge":1E400,"tiny":1e-400,"plain":1.5,"one":1.0,"kilo":1e3,"nested":{{"y":1.5,"x":{BIG}}}}}"#
    )
}

fn columns() -> Vec<ColumnSpec> {
    let mut x = text_col("x");
    x.source = Some("nested".into());
    x.pointer = Some("/x".into());
    ["id", "amount", "past", "huge", "tiny", "plain", "one", "kilo", "nested"]
        .into_iter()
        .map(text_col)
        .chain([x])
        .collect()
}

fn assert_digits_kept(p: &Path, extraction: Extraction, what: &str) {
    let b = spec_to_batch(&spec(extraction, columns()), p).unwrap_or_else(|e| panic!("{what}: {e:#}"));
    let row: Vec<String> = (0..b.num_columns()).map(|i| strings(&b, i)[0].clone()).collect();
    assert_eq!(
        row,
        [
            BIG,
            LONG,
            PAST_U64,
            "1E400",
            "1e-400",
            // What serde_json held exactly reads as it always has.
            "1.5",
            "1.0",
            "1000.0",
            &format!(r#"{{"x":{BIG},"y":1.5}}"#),
            BIG,
        ],
        "{what}"
    );
}

#[test]
fn a_record_array_keeps_a_numbers_digits() {
    let dir = TempDir::new().unwrap();
    let p = write(&dir, "a.json", &format!("[{}]", record()));
    assert_digits_kept(&p, Extraction::Json { lines: false, pointer: None, record: false }, "array");
}

#[test]
fn a_record_array_under_a_pointer_keeps_a_numbers_digits() {
    let dir = TempDir::new().unwrap();
    let p = write(&dir, "p.json", &format!(r#"{{"meta":{{"n":{BIG}}},"data":[{}]}}"#, record()));
    assert_digits_kept(&p, Extraction::Json { lines: false, pointer: Some("/data".into()), record: false }, "pointer");
}

#[test]
fn ndjson_keeps_a_numbers_digits() {
    let dir = TempDir::new().unwrap();
    let p = write(&dir, "n.ndjson", &format!("{}\n", record()));
    assert_digits_kept(&p, Extraction::Json { lines: true, pointer: None, record: false }, "ndjson");
}

#[test]
fn a_document_read_as_one_record_keeps_a_numbers_digits() {
    let dir = TempDir::new().unwrap();
    let p = write(&dir, "r.json", &record());
    assert_digits_kept(&p, Extraction::Json { lines: false, pointer: None, record: true }, "record");
}

/// A nested value reaches a column as compact JSON text, and a `pointer`
/// re-reads that text: the digits have to survive both the rendering and the
/// second read. A cell that arrives as JSON text in a CSV goes the same way.
#[test]
fn a_column_pointer_into_json_text_keeps_a_numbers_digits() {
    let dir = TempDir::new().unwrap();
    let p = write(&dir, "c.csv", &format!("k;doc\n1;{{\"v\":{LONG}}}\n"));
    let mut v = text_col("v");
    v.source = Some("doc".into());
    v.pointer = Some("/v".into());
    let s = spec(
        Extraction::Delimited {
            delimiter: ';',
            quote: Some('"'),
            escape: None,
            encoding: None,
            comment: None,
            ragged: RaggedPolicy::Error,
            region: None,
        },
        vec![v],
    );
    let s = ParseSpec { transforms: vec![Transform::PromoteHeader { rows: 1, join: " ".into() }], ..s };
    let b = spec_to_batch(&s, &p).unwrap_or_else(|e| panic!("{e:#}"));
    assert_eq!(strings(&b, 0), [LONG]);
}

// ---------------------------------------------------------------------------
// through fit and dataset()
// ---------------------------------------------------------------------------

fn tdy(args: &[&str]) -> std::process::Output {
    std::process::Command::new(env!("CARGO_BIN_EXE_tdy")).args(args).output().expect("run tdy")
}

fn out(o: &std::process::Output) -> String {
    format!("{}{}", String::from_utf8_lossy(&o.stdout), String::from_utf8_lossy(&o.stderr))
}

/// Two monthly exports whose identifiers are past `u64` and whose amounts
/// carry twenty-eight digits.
fn pile() -> TempDir {
    let dir = TempDir::new().unwrap();
    write(
        &dir,
        "jan.ndjson",
        "{\"id\":12345678901234567890123,\"amount\":1234567890.123456789012345678}\n\
         {\"id\":12345678901234567890124,\"amount\":0.25}\n",
    );
    write(&dir, "feb.ndjson", "{\"id\":98765432109876543210987,\"amount\":-7.5}\n");
    dir
}

#[test]
fn an_integer_past_bigint_fits_a_declared_decimal_and_a_long_decimal_lands_exactly() {
    let dir = pile();
    let t = write(
        &dir,
        "t.tdy.sql",
        "CREATE TABLE t (id DECIMAL(38,0) NOT NULL, amount DECIMAL(38,18) NOT NULL) WITH (files = '*.ndjson')",
    );
    let fit = tdy(&["fit", t.to_str().unwrap()]);
    assert!(fit.status.success(), "{}", out(&fit));
    let sql = format!(
        "SELECT CAST(id AS VARCHAR) id, CAST(amount AS VARCHAR) amount FROM dataset('{}') ORDER BY id",
        t.display()
    );
    let q = tdy(&["query", &sql]);
    let text = out(&q);
    assert!(q.status.success(), "{text}");
    for want in [
        "12345678901234567890123",
        "12345678901234567890124",
        "98765432109876543210987",
        "1234567890.123456789012345678",
        "0.250000000000000000",
    ] {
        assert!(text.contains(want), "{want} missing:\n{text}");
    }
    let sum = tdy(&["query", &format!("SELECT CAST(sum(amount) AS VARCHAR) s FROM dataset('{}')", t.display())]);
    assert!(out(&sum).contains("1234567882.873456789012345678"), "{}", out(&sum));
}

/// The same identifiers declared `TEXT` are their digits, not a double's.
#[test]
fn an_identifier_past_u64_declared_text_is_its_digits() {
    let dir = pile();
    let t = write(&dir, "t.tdy.sql", "CREATE TABLE t (id TEXT NOT NULL) WITH (files = '*.ndjson')");
    let fit = tdy(&["fit", t.to_str().unwrap()]);
    assert!(fit.status.success(), "{}", out(&fit));
    let q = tdy(&["query", &format!("SELECT id FROM dataset('{}') ORDER BY id", t.display())]);
    let text = out(&q);
    assert!(text.contains("| 12345678901234567890123 |"), "{text}");
    assert!(!text.contains("e22"), "{text}");
}

/// Typing is unchanged: past `BIGINT` is refused rather than wrapped or
/// rounded, and more fractional digits than the scale is a gap naming the
/// rounding the target did not declare.
#[test]
fn bigint_and_a_short_scale_are_still_refused() {
    let dir = pile();
    let t = write(&dir, "t.tdy.sql", "CREATE TABLE t (id BIGINT NOT NULL) WITH (files = '*.ndjson')");
    let fit = tdy(&["fit", t.to_str().unwrap()]);
    assert!(!fit.status.success(), "{}", out(&fit));

    let t = write(&dir, "u.tdy.sql", "CREATE TABLE u (amount DECIMAL(38,10) NOT NULL) WITH (files = '*.ndjson')");
    let fit = tdy(&["fit", t.to_str().unwrap()]);
    let text = out(&fit);
    assert!(!fit.status.success(), "{text}");
    assert!(text.contains("fractional digits") && text.contains("round = 'half_away'"), "{text}");
}
