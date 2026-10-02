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
         {\"id\":12345678901234567890124,\"amount\":0.000000000000000001}\n",
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
        "0.000000000000000001",
    ] {
        assert!(text.contains(want), "{want} missing:\n{text}");
    }
    let sum = tdy(&["query", &format!("SELECT CAST(sum(amount) AS VARCHAR) s FROM dataset('{}')", t.display())]);
    assert!(out(&sum).contains("1234567882.623456789012345679"), "{}", out(&sum));
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
    let text = out(&fit);
    assert!(!fit.status.success(), "{text}");
    assert!(
        text.contains("cannot parse \"12345678901234567890123\"") && text.contains("too large"),
        "the refusal names the value and why: {text}"
    );

    let t = write(&dir, "u.tdy.sql", "CREATE TABLE u (amount DECIMAL(38,10) NOT NULL) WITH (files = '*.ndjson')");
    let fit = tdy(&["fit", t.to_str().unwrap()]);
    let text = out(&fit);
    assert!(!fit.status.success(), "{text}");
    assert!(text.contains("fractional digits") && text.contains("round = 'half_away'"), "{text}");
}

// ---------------------------------------------------------------------------
// a literal outside a double's range
// ---------------------------------------------------------------------------

fn float_spec(extraction: Extraction) -> ParseSpec {
    let mut x = text_col("x");
    x.dtype = DType::Float64;
    let mut s = spec(extraction, vec![x]);
    if matches!(s.extraction, Extraction::Delimited { .. }) {
        s.transforms = vec![Transform::PromoteHeader { rows: 1, join: " ".into() }];
    }
    s
}

fn csv() -> Extraction {
    Extraction::Delimited {
        delimiter: ';',
        quote: Some('"'),
        escape: None,
        encoding: None,
        comment: None,
        ragged: RaggedPolicy::Error,
        region: None,
    }
}

/// `1E400` is no double: reading it as `inf` (or `1e-400` as `0`) is a
/// plausible wrong number. Both executors refuse it, naming the row, in
/// NDJSON and in CSV alike.
#[test]
fn a_float_outside_a_doubles_range_is_refused_on_both_executors() {
    use tdy::config::Limits;
    let dir = TempDir::new().unwrap();
    for (bad, name, body, extraction) in [
        ("1E400", "o.ndjson", "{\"x\":1.5}\n{\"x\":1E400}\n", Extraction::Json { lines: true, pointer: None, record: false }),
        ("1e-400", "u.ndjson", "{\"x\":1.5}\n{\"x\":1e-400}\n", Extraction::Json { lines: true, pointer: None, record: false }),
        ("1E400", "o.csv", "x\n1.5\n1E400\n", csv()),
        ("1e-400", "u.csv", "x\n1.5\n1e-400\n", csv()),
    ] {
        let p = write(&dir, name, body);
        let s = float_spec(extraction);
        let engine = tdy::engine::execute_batches(&s, &p, Limits::default()).expect_err(name);
        let streamed = tdy::stream::execute_batches(&s, &p, Limits::default()).expect_err(name);
        for e in [engine, streamed] {
            let msg = format!("{e:#}");
            assert!(msg.contains("row 2") && msg.contains(bad) && msg.contains("outside a double's range"), "{name}: {msg}");
        }
    }
    // Zeros written as zeros are zeros.
    let p = write(&dir, "z.ndjson", "{\"x\":0.0}\n{\"x\":0e0}\n{\"x\":-0.0}\n{\"x\":0.000}\n");
    let b = spec_to_batch(&float_spec(Extraction::Json { lines: true, pointer: None, record: false }), &p).unwrap();
    assert_eq!(b.num_rows(), 4);
}

/// Under a DOUBLE target the same file is a gap naming the row, not a fit.
#[test]
fn a_float_outside_a_doubles_range_is_a_gap_under_a_double_target() {
    let dir = TempDir::new().unwrap();
    write(&dir, "a.ndjson", "{\"x\":1.5}\n{\"x\":1E400}\n");
    let t = write(&dir, "t.tdy.sql", "CREATE TABLE t (x DOUBLE) WITH (files = '*.ndjson')");
    let fit = tdy(&["fit", t.to_str().unwrap()]);
    let text = out(&fit);
    assert!(!fit.status.success(), "{text}");
    assert!(text.contains("row 2") && text.contains("1E400") && text.contains("outside a double's range"), "{text}");
}

/// serde_json without `float_roundtrip` reads a 17-digit literal through
/// `significand as f64` and one multiply or divide by a power of ten — two
/// roundings — and lands one ULP off for about one in ten such values
/// (`0.09743057599473337`, from a random 100 MB export). tdy used to serve that
/// double; the literal now reaches the Float64 cast as written and is parsed
/// correctly rounded. A correction, and the one place float64 data moves.
#[test]
fn a_seventeen_digit_double_is_correctly_rounded() {
    use datafusion::arrow::array::Float64Array;
    let lit = "0.09743057599473337";
    let serde: f64 = serde_json::from_str(lit).unwrap();
    let exact: f64 = lit.parse().unwrap();
    assert_ne!(serde.to_bits(), exact.to_bits(), "the case this pins: serde_json is one ULP off");
    let dir = TempDir::new().unwrap();
    let p = write(&dir, "r.ndjson", &format!("{{\"x\":{lit}}}\n"));
    let mut x = text_col("x");
    x.dtype = DType::Float64;
    for executor in ["engine", "stream"] {
        let s = spec(Extraction::Json { lines: true, pointer: None, record: false }, vec![x.clone()]);
        let batches = if executor == "engine" {
            tdy::engine::execute_batches(&s, &p, tdy::config::Limits::default()).unwrap()
        } else {
            tdy::stream::execute_batches(&s, &p, tdy::config::Limits::default()).unwrap()
        };
        let got = batches[0].column(0).as_any().downcast_ref::<Float64Array>().unwrap().value(0);
        assert_eq!(got.to_bits(), exact.to_bits(), "{executor}: {got:?}");
    }
}

/// A plain literal stays plain: `0.00000123` would print as `1.23e-6`, the
/// same number, but a DECIMAL column refuses exponent form — on main too.
#[test]
fn a_small_decimal_written_plainly_lands_in_a_decimal_column() {
    let dir = TempDir::new().unwrap();
    write(&dir, "a.ndjson", "{\"x\":0.00000123,\"big\":10000000000000000}\n{\"x\":0.5,\"big\":7}\n");
    let t = write(
        &dir,
        "t.tdy.sql",
        "CREATE TABLE t (x DECIMAL(16,8) NOT NULL, big BIGINT NOT NULL) WITH (files = '*.ndjson')",
    );
    let fit = tdy(&["fit", t.to_str().unwrap()]);
    assert!(fit.status.success(), "{}", out(&fit));
    let q = tdy(&["query", &format!("SELECT CAST(x AS VARCHAR) x, big FROM dataset('{}') ORDER BY big DESC", t.display())]);
    let text = out(&q);
    assert!(text.contains("0.00000123") && text.contains("10000000000000000"), "{text}");

    let u = write(&dir, "u.tdy.sql", "CREATE TABLE u (big TEXT NOT NULL) WITH (files = '*.ndjson')");
    assert!(tdy(&["fit", u.to_str().unwrap()]).status.success());
    let q = tdy(&["query", &format!("SELECT big FROM dataset('{}')", u.display())]);
    assert!(out(&q).contains("| 10000000000000000 |"), "{}", out(&q));
}

// ---------------------------------------------------------------------------
// typing: never float64 for a value a double cannot hold
// ---------------------------------------------------------------------------

/// The reviewer's pile: one document's `v` is past 64 bits, the other's is
/// 0.5. The CSV draft of the same pile says TEXT; the JSON draft said DOUBLE
/// and served 1.2345678901234568e22.
#[test]
fn a_draft_never_declares_double_for_a_value_a_double_cannot_hold() {
    let dir = TempDir::new().unwrap();
    let a = write(&dir, "a.json", "{\"id\":1,\"v\":12345678901234567890123}");
    let b = write(&dir, "b.json", "{\"id\":2,\"v\":0.5}");
    let sql = tdy::draft::draft_target(&[a, b], tdy::config::Limits::default()).unwrap();
    let v = sql.lines().find(|l| l.trim_start().starts_with("v ")).unwrap_or_else(|| panic!("{sql}"));
    assert!(v.contains("TEXT"), "{sql}");
}

/// `amount_lossy` (19 significant digits) sniffs as an exact decimal and
/// queries as written.
#[test]
fn a_long_json_decimal_sniffs_as_an_exact_decimal() {
    let dir = TempDir::new().unwrap();
    let p = write(&dir, "d.ndjson", "{\"x\":1234567.891234567891}\n{\"x\":2.5}\n");
    let q = tdy(&["query", &format!("SELECT CAST(x AS VARCHAR) x FROM messy('{}')", p.display())]);
    let text = out(&q);
    assert!(text.contains("1234567.891234567891"), "{text}");
}

/// 2^53 + 1 beside a fraction, CSV and NDJSON: served as written, not as
/// 9007199254740992.0; an exponent literal a double cannot hold is text.
#[test]
fn a_sixteen_digit_value_a_double_cannot_hold_is_not_float64() {
    let dir = TempDir::new().unwrap();
    let c = write(&dir, "c.csv", "x\n9007199254740993\n0.5\n");
    let n = write(&dir, "n.ndjson", "{\"x\":9007199254740993}\n{\"x\":0.5}\n");
    let e = write(&dir, "e.ndjson", "{\"x\":1.2345678901234567891e5}\n{\"x\":2.5}\n");
    for (p, want) in [(&c, "9007199254740993"), (&n, "9007199254740993"), (&e, "1.2345678901234567891e5")] {
        let q = tdy(&["query", &format!("SELECT CAST(x AS VARCHAR) x FROM messy('{}')", p.display())]);
        // stdout only: the sniff's note names the double it avoided.
        let text = String::from_utf8_lossy(&q.stdout).to_string();
        assert!(text.contains(want) && !text.contains("9007199254740992"), "{}: {}", p.display(), out(&q));
    }
}

/// A file of `n` floats, one per row under a header `x`, with data row
/// `at` (1-based) replaced: far enough in that the sniffer's sample does not
/// see it, so only the whole-file verification can.
fn floats_with(dir: &TempDir, name: &str, n: usize, at: usize, odd: &str, ndjson: bool) -> PathBuf {
    let mut body = if ndjson { String::new() } else { String::from("x\n") };
    for i in 1..=n {
        let v = if i == at { odd.to_string() } else { format!("{}.{}", i % 977, (i * 7919) % 1000) };
        if ndjson {
            body.push_str(&format!("{{\"x\":{v}}}\n"));
        } else {
            body.push_str(&v);
            body.push('\n');
        }
    }
    write(dir, name, &body)
}

fn sniffed(p: &Path) -> serde_json::Value {
    let o = tdy(&["--json", "sniff", "--no-llm", "--force", p.to_str().unwrap()]);
    assert!(o.status.success(), "{}", out(&o));
    serde_json::from_slice(&o.stdout).unwrap()
}

/// Past the sample, a value outside a double's range widens the column with
/// the cast's own words, not "not a number".
#[test]
fn a_late_value_outside_a_doubles_range_is_named_as_such() {
    let dir = TempDir::new().unwrap();
    let p = floats_with(&dir, "late.csv", 20_000, 10_001, "1e-400", false);
    let v = sniffed(&p);
    let notes = v["notes"].to_string();
    assert!(notes.contains("are outside a double's range") && notes.contains("row 10001"), "{notes}");
    assert!(!notes.contains("not a number"), "{notes}");
}

fn column_type(v: &serde_json::Value, name: &str) -> serde_json::Value {
    v["spec"]["columns"].as_array().unwrap().iter().find(|c| c["name"] == name).unwrap()["dtype"].clone()
}

/// The sample cannot see row 10,001 of 20,000, so the sniffer's guess is
/// float64; the whole-file verification reads every literal and widens the
/// column when one would come back from a double as a different number —
/// DECIMAL when every value fits, naming the row and the value.
#[test]
fn a_late_literal_a_double_cannot_hold_widens_the_column() {
    let dir = TempDir::new().unwrap();
    for (name, odd, ndjson) in [
        ("long.csv", "12345678901234567890.123", false),
        ("twoto53.csv", "9007199254740993", false),
        ("long.ndjson", "12345678901234567890.123", true),
        ("twoto53.ndjson", "9007199254740993", true),
    ] {
        let p = floats_with(&dir, name, 20_000, 10_001, odd, ndjson);
        let v = sniffed(&p);
        assert_eq!(column_type(&v, "x"), serde_json::json!({"type": "decimal", "precision": 38, "scale": 3}), "{name}: {v}");
        let notes = v["notes"].to_string();
        assert!(notes.contains("row 10001") && notes.contains(odd), "{name}: {notes}");
        let q = tdy(&["query", "--frozen", &format!("SELECT CAST(x AS VARCHAR) x FROM messy('{}') WHERE x > 1000", p.display())]);
        assert!(String::from_utf8_lossy(&q.stdout).contains(odd), "{name}: {}", out(&q));
    }
    // --quick skips the whole-file read, and says so.
    let p = floats_with(&dir, "quick.csv", 20_000, 10_001, "9007199254740993", false);
    let o = tdy(&["--json", "sniff", "--no-llm", "--force", "--quick", p.to_str().unwrap()]);
    let v: serde_json::Value = serde_json::from_slice(&o.stdout).unwrap();
    assert_eq!(column_type(&v, "x"), serde_json::json!({"type": "float64"}));
    assert!(v["notes"].to_string().contains("NOT checked against the whole file"), "{v}");
}
