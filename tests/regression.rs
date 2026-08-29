//! Regression tests: one per defect found while hardening tdy.
//!
//! Every test in this file failed before the fix it guards. They are written
//! against the *correct* behaviour, not the observed one, so a regression
//! shows up as a wrong number rather than as a changed snapshot.
//!
//! The governing rule is the one the design claims: **tdy never silently
//! produces a wrong value.** Where a file is genuinely ambiguous, the right
//! outcomes are (a) the right answer, or (b) a loud error — never (c) a
//! plausible-looking wrong number.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use datafusion::arrow::array::{Array, Float64Array, Int64Array, StringArray};
use tempfile::TempDir;

use tdy::config::{Backend, Config, Limits};
use tdy::provider;
use tdy::sample;
use tdy::sniff;
use tdy::spec::*;

fn cfg() -> Config {
    Config { backend: Backend::None, ..Config::default() }
}

fn write(dir: &TempDir, name: &str, body: &str) -> PathBuf {
    let p = dir.path().join(name);
    fs::write(&p, body).unwrap();
    p
}

fn write_bytes(dir: &TempDir, name: &str, body: &[u8]) -> PathBuf {
    let p = dir.path().join(name);
    fs::write(&p, body).unwrap();
    p
}

fn sniffed(path: &Path) -> sniff::SniffResult {
    let s = sample::build(path, 64 * 1024).unwrap();
    sniff::sniff(path, &s, Limits::default()).unwrap()
}

async fn query(sql: &str) -> Vec<datafusion::arrow::record_batch::RecordBatch> {
    provider::run_query(sql, &cfg(), false).await.unwrap().1
}

fn col_f64(b: &datafusion::arrow::record_batch::RecordBatch, i: usize) -> Vec<Option<f64>> {
    let a = b.column(i).as_any().downcast_ref::<Float64Array>().unwrap();
    (0..a.len()).map(|i| if a.is_null(i) { None } else { Some(a.value(i)) }).collect()
}

fn col_i64(b: &datafusion::arrow::record_batch::RecordBatch, i: usize) -> Vec<Option<i64>> {
    let a = b.column(i).as_any().downcast_ref::<Int64Array>().unwrap();
    (0..a.len()).map(|i| if a.is_null(i) { None } else { Some(a.value(i)) }).collect()
}

fn col_dec(b: &datafusion::arrow::record_batch::RecordBatch, i: usize) -> Vec<Option<i128>> {
    let a = b
        .column(i)
        .as_any()
        .downcast_ref::<datafusion::arrow::array::Decimal128Array>()
        .unwrap_or_else(|| panic!("column {i} is {:?}, expected an exact decimal", b.column(i).data_type()));
    (0..a.len()).map(|i| if a.is_null(i) { None } else { Some(a.value(i)) }).collect()
}

fn col_str(b: &datafusion::arrow::record_batch::RecordBatch, i: usize) -> Vec<Option<String>> {
    let a = b.column(i).as_any().downcast_ref::<StringArray>().unwrap();
    (0..a.len())
        .map(|i| if a.is_null(i) { None } else { Some(a.value(i).to_string()) })
        .collect()
}

// ---------------------------------------------------------------------------
// 1. The decimal-comma catastrophe
// ---------------------------------------------------------------------------

/// `1,5` in a German export is one and a half. Treating the comma as a
/// thousands separator turned it into 15 — a tenfold error, silently, in a
/// money column. This is the worst bug the audit found.
#[tokio::test]
async fn german_decimal_comma_is_not_multiplied_by_ten() {
    let dir = TempDir::new().unwrap();
    let p = write(&dir, "de.csv", "name;betrag\na;1,5\nb;2,75\nc;10,25\n");

    let spec = sniffed(&p).spec;
    let betrag = spec.columns.iter().find(|c| c.name == "betrag").unwrap();
    assert_eq!(
        betrag.parse.decimal_separator,
        Some(','),
        "comma must be recognised as the decimal point, got {:?}",
        betrag.parse
    );
    assert_eq!(betrag.parse.thousands_separator, None);

    // `betrag` is money, so it is read as an exact decimal: mantissas at
    // scale 2. The point of the test is the magnitude — 1,5 is one and a
    // half, not fifteen.
    let b = query(&format!("SELECT betrag FROM messy('{}')", p.display())).await;
    assert_eq!(col_dec(&b[0], 0), vec![Some(150), Some(275), Some(1025)]);
}

/// The same file shape, but the numbers are grouped: `1.234,56` is one
/// thousand two hundred thirty-four point five six.
#[tokio::test]
async fn continental_grouping_and_decimal_together() {
    let dir = TempDir::new().unwrap();
    let p = write(&dir, "cont.csv", "k;v\na;1.234,56\nb;999,00\nc;12.000,10\n");
    let b = query(&format!("SELECT v FROM messy('{}')", p.display())).await;
    // Money is exact: mantissas at scale 2.
    assert_eq!(col_dec(&b[0], 0), vec![Some(123_456), Some(99_900), Some(1_200_010)]);
}

/// Swiss apostrophes must keep working (this one was already right).
#[tokio::test]
async fn swiss_apostrophe_grouping_still_works() {
    let dir = TempDir::new().unwrap();
    let p = write(&dir, "ch.csv", "k;v\na;1'234.50\nb;12'000.00\n");
    let b = query(&format!("SELECT sum(v) s FROM messy('{}')", p.display())).await;
    assert_eq!(col_dec(&b[0], 0), vec![Some(1_323_450)]); // 13'234.50

}

/// A spec that declares the wrong separator must ERROR, not quietly delete
/// the character. This is what makes a hand-edited or model-written sidecar
/// safe to trust.
#[test]
fn wrong_thousands_separator_is_an_error_not_a_wrong_number() {
    let dir = TempDir::new().unwrap();
    // Semicolon-delimited so that "1,5" is a single field.
    let p = write(&dir, "y.csv", "k;v\na;1,5\n");
    let mut s = sniffed(&p).spec;
    // Force the wrong convention, exactly as a careless hand edit or a
    // confused model would.
    for c in &mut s.columns {
        if c.name == "v" {
            c.dtype = DType::Float64;
            c.parse.thousands_separator = Some(',');
            c.parse.decimal_separator = None;
        }
    }
    let err = match provider::spec_to_batch(&s, &p) {
        Err(e) => e,
        Ok(b) => panic!("expected an error, got {b:?}"),
    };
    let msg = format!("{err:#}");
    assert!(
        msg.contains("thousands") || msg.contains("grouped"),
        "expected a grouping error, got: {msg}"
    );
}

// ---------------------------------------------------------------------------
// 2. Header mapping: columns must carry their own data
// ---------------------------------------------------------------------------

/// Two columns called `a`: the sniffer de-duplicated the *output* names but
/// pointed both at the first source column, so column two's data vanished and
/// column one's was duplicated.
#[tokio::test]
async fn duplicate_header_names_keep_their_own_data() {
    let dir = TempDir::new().unwrap();
    let p = write(&dir, "dup.csv", "a,a,b\n1,2,3\n4,5,6\n");
    let b = query(&format!("SELECT * FROM messy('{}')", p.display())).await;
    let batch = &b[0];
    assert_eq!(batch.num_columns(), 3);
    assert_eq!(col_i64(batch, 0), vec![Some(1), Some(4)]);
    assert_eq!(col_i64(batch, 1), vec![Some(2), Some(5)], "second `a` column must be its own data");
    assert_eq!(col_i64(batch, 2), vec![Some(3), Some(6)]);
}

/// A blank header cell is normal in exports. It must not stop header
/// detection and push the header row into the data.
#[tokio::test]
async fn blank_header_cell_still_promotes_a_header() {
    let dir = TempDir::new().unwrap();
    let p = write(&dir, "blank.csv", "id,,value\n1,x,2\n3,y,4\n");
    let b = query(&format!("SELECT * FROM messy('{}')", p.display())).await;
    let batch = &b[0];
    let schema = batch.schema();
    let names: Vec<&str> = schema.fields().iter().map(|f| f.name().as_str()).collect();
    assert!(names.contains(&"id"), "header row must be promoted, got {names:?}");
    assert_eq!(batch.num_rows(), 2, "the header row must not appear as data");
}

/// Whatever the sniffer emits must execute. This is the invariant the design
/// claims; it was violated whenever promote_header rewrote a header name.
#[test]
fn every_sniffed_spec_executes() {
    let dir = TempDir::new().unwrap();
    let cases: &[(&str, &str)] = &[
        ("dup.csv", "a,a,b\n1,2,3\n"),
        ("blank.csv", "id,,value\n1,x,2\n"),
        ("gap.csv", "a,,\n1,2,3\n4,5,6\n"),
        ("titled.csv", "Report\nStand: 2026\n\nk;v\na;1\nb;2\n"),
        ("ragged.csv", "a,b,c\n1,2\n3,4,5,6\n"),
        ("onecol.csv", "value\n1\n2\n"),
        ("headeronly.csv", "a,b\n"),
        ("nums.csv", "1,2\n3,4\n"),
        ("uml.csv", "Grösse,Straße\n1,x\n"),
    ];
    for (name, body) in cases {
        let p = write(&dir, name, body);
        let spec = sniffed(&p).spec;
        spec.validate().unwrap_or_else(|e| panic!("{name}: invalid spec: {e:?}"));
        provider::spec_to_batch(&spec, &p)
            .unwrap_or_else(|e| panic!("{name}: sniffed spec does not execute: {e:#}"));
    }
}

// ---------------------------------------------------------------------------
// 3. Crashes
// ---------------------------------------------------------------------------

/// A one-row sheet whose only row starts with "Total" underflowed a usize in
/// the Excel sniffer and panicked.
#[test]
fn degenerate_inputs_never_panic() {
    let dir = TempDir::new().unwrap();
    let cases: &[(&str, &[u8])] = &[
        ("empty.csv", b""),
        ("newlines.csv", b"\n\n\n"),
        ("delims.csv", b",,,,\n,,,,\n"),
        ("header_only.csv", b"a,b\n"),
        ("one_cell.csv", b"x"),
        ("nul.csv", b"a,b\n1,\x00\n"),
        ("cr_only.csv", b"a,b\r1,2\r"),
        ("quotes.csv", b"\"\"\"\"\n"),
    ];
    for (name, body) in cases {
        let p = write_bytes(&dir, name, body);
        // Either a spec or an error — never a panic, and never a hang.
        let r = std::panic::catch_unwind(|| {
            let s = sample::build(&p, 64 * 1024)?;
            sniff::sniff(&p, &s, Limits::default())
        });
        assert!(r.is_ok(), "{name}: sniffing panicked");
    }
}

/// The Excel path, through the real binary so a panic shows up as exit 101.
#[test]
fn degenerate_excel_never_panics() {
    let dir = TempDir::new().unwrap();
    let script = dir.path().join("mk.py");
    fs::write(
        &script,
        r#"
from openpyxl import Workbook
import sys, os
d = sys.argv[1]
wb = Workbook(); wb.active.append(["Total", 5]); wb.save(os.path.join(d, "single_total.xlsx"))
wb = Workbook(); wb.save(os.path.join(d, "empty_sheet.xlsx"))
wb = Workbook(); ws = wb.active
ws.append(["a", "b"]); ws.append(["Summe", 1])
wb.save(os.path.join(d, "two_rows_total.xlsx"))
"#,
    )
    .unwrap();
    let ok = Command::new("python3")
        .arg(&script)
        .arg(dir.path())
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if !ok {
        eprintln!("skipping: python3/openpyxl unavailable");
        return;
    }
    for name in ["single_total.xlsx", "empty_sheet.xlsx", "two_rows_total.xlsx"] {
        let p = dir.path().join(name);
        let out = Command::new(env!("CARGO_BIN_EXE_tdy"))
            .args(["sniff", p.to_str().unwrap(), "--no-llm"])
            .output()
            .unwrap();
        assert_ne!(
            out.status.code(),
            Some(101),
            "{name} panicked:\n{}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
}

// ---------------------------------------------------------------------------
// 4. Typing that loses information
// ---------------------------------------------------------------------------

/// Postal codes, article numbers and phone numbers have leading zeros.
/// Typing them as integers destroys the value.
#[tokio::test]
async fn leading_zeros_stay_text() {
    let dir = TempDir::new().unwrap();
    let p = write(&dir, "plz.csv", "plz,ort\n8001,Zuerich\n0234,Test\n0567,Andere\n");
    let b = query(&format!("SELECT plz FROM messy('{}')", p.display())).await;
    assert_eq!(
        col_str(&b[0], 0),
        vec![Some("8001".into()), Some("0234".into()), Some("0567".into())]
    );
}

/// Integers too large for i64 must not silently become lossy floats.
#[test]
fn oversized_integers_do_not_become_lossy_floats() {
    let dir = TempDir::new().unwrap();
    let p = write(&dir, "big.csv", "id\n99999999999999999999\n99999999999999999998\n");
    let spec = sniffed(&p).spec;
    assert_eq!(
        spec.columns[0].dtype,
        DType::Utf8,
        "a 20-digit id must stay text, not become f64"
    );
}

/// `01/02/2025` is 1 February in Europe and 2 January in the US. The sniffer
/// may pick one, but it must not pretend it is sure.
#[test]
fn ambiguous_dates_lower_confidence_and_say_so() {
    let dir = TempDir::new().unwrap();
    let p = write(&dir, "d.csv", "d\n01/02/2025\n03/04/2025\n05/06/2025\n");
    let r = sniffed(&p);
    assert!(
        r.confidence < 0.8,
        "ambiguous date format must fall below the escalation threshold, got {}",
        r.confidence
    );
    assert!(
        r.spec.notes.iter().any(|n| n.to_lowercase().contains("ambiguous")),
        "expected a note about ambiguity, got {:?}",
        r.spec.notes
    );
}

/// An unambiguous day-first date (day > 12 somewhere in the column) must be
/// read day-first.
#[tokio::test]
async fn unambiguous_day_first_dates_are_read_day_first() {
    let dir = TempDir::new().unwrap();
    let p = write(&dir, "d.csv", "d\n13/02/2025\n01/02/2025\n");
    let spec = sniffed(&p).spec;
    assert_eq!(spec.columns[0].dtype, DType::Date { format: "%d/%m/%Y".into() });
    let b = query(&format!("SELECT d FROM messy('{}')", p.display())).await;
    assert_eq!(b[0].num_rows(), 2);
}

// ---------------------------------------------------------------------------
// 4b. Files that are not delimited at all
// ---------------------------------------------------------------------------

/// A column-aligned report has no delimiter, so delimiter detection reports
/// one field per line — which used to score 1.0 ("confident") and skip the
/// fixed-width attempt entirely.
#[tokio::test]
async fn a_clean_fixed_width_report_is_read_as_columns() {
    let dir = TempDir::new().unwrap();
    let p = write(
        &dir,
        "fw.txt",
        "NAME       AMOUNT  CITY\n\
         Mueller       100  Bern\n\
         Meier        2000  Zug\n\
         Rossi         -50  Lugano\n\
         Keller       1234  Basel\n",
    );
    let b = query(&format!("SELECT * FROM messy('{}')", p.display())).await;
    let batch = &b[0];
    assert_eq!(batch.num_columns(), 3, "expected three aligned columns");
    assert_eq!(batch.num_rows(), 4);
    assert_eq!(col_str(batch, 0)[0].as_deref(), Some("Mueller"));
    assert_eq!(col_i64(batch, 1), vec![Some(100), Some(2000), Some(-50), Some(1234)]);
}

/// An nginx access log is readable without any model: this is the whole point
/// of `backend = "none"` being the default.
#[tokio::test]
async fn an_access_log_is_queryable_without_a_model() {
    let dir = TempDir::new().unwrap();
    let p = write(
        &dir,
        "access.log",
        "192.168.1.1 - - [05/Jan/2026:10:00:01 +0100] \"GET /a HTTP/1.1\" 200 1234 \"-\" \"curl/8.0\"\n\
         10.0.0.7 - alice [05/Jan/2026:10:00:02 +0100] \"POST /b HTTP/1.1\" 201 55 \"-\" \"Mozilla/5.0\"\n\
         10.0.0.8 - - [05/Jan/2026:10:00:03 +0100] \"GET /c HTTP/1.1\" 404 0 \"-\" \"Mozilla/5.0\"\n\
         10.0.0.9 - - [05/Jan/2026:10:00:04 +0100] \"GET /d HTTP/1.1\" 200 12 \"-\" \"Go-http/2.0\"\n",
    );
    let b = query(&format!(
        "SELECT status, count(*) n FROM messy('{}') GROUP BY status ORDER BY status",
        p.display()
    ))
    .await;
    assert_eq!(col_i64(&b[0], 0), vec![Some(200), Some(201), Some(404)]);
    assert_eq!(col_i64(&b[0], 1), vec![Some(2), Some(1), Some(1)]);
}

/// A layout tdy cannot read must say so rather than confidently returning one
/// column of raw lines.
#[test]
fn an_unrecognised_layout_reports_low_confidence_with_advice() {
    let dir = TempDir::new().unwrap();
    let p = write(
        &dir,
        "report.txt",
        "Muster Handels AG                    Seite 1 von 1\n\
         Umsatzstatistik nach Kunde           Stand: 10.02.2026\n\
         \n\
         Kunde                   LD   Menge    Betrag\n\
         --------------------------------------------------\n\
         Region Ost\n\
         Mueller Transport AG    CH    1234   84'320.57\n\
         Baeckerei Steiner       CH      96   -8'450.23\n",
    );
    let r = sniffed(&p);
    assert!(
        r.confidence < 0.8,
        "an unread layout must fall below the escalation threshold, got {}",
        r.confidence
    );
    assert!(
        r.spec.notes.iter().any(|n| n.contains("no delimiter or column alignment")),
        "expected an actionable note, got {:?}",
        r.spec.notes
    );
}

// ---------------------------------------------------------------------------
// 4c. Reporting the truth about where things went wrong
// ---------------------------------------------------------------------------

/// Output is built in 64k-row batches. A parse error past the first batch
/// must still name the row a person would find in their file, not its
/// position inside an internal batch.
#[test]
fn a_parse_error_names_the_file_row_not_the_batch_row() {
    let dir = TempDir::new().unwrap();
    let mut body = String::from("id,v\n");
    for i in 1..=70_000 {
        // One bad value, well past the 65,536-row batch boundary.
        if i == 70_000 {
            body.push_str(&format!("{i},not-a-number\n"));
        } else {
            body.push_str(&format!("{i},{i}\n"));
        }
    }
    let p = write(&dir, "big.csv", &body);
    let mut spec = sniffed(&p).spec;
    for c in &mut spec.columns {
        if c.name == "v" {
            c.dtype = DType::Int64;
        }
    }
    let msg = format!("{:#}", provider::spec_to_batch(&spec, &p).unwrap_err());
    assert!(
        msg.contains("row 70000"),
        "error should name the file row; got: {msg}"
    );
}

/// UTF-16 text is *valid UTF-8* when it is ASCII underneath (a NUL byte is a
/// legal UTF-8 character), so a naive UTF-8 check hands back a string full of
/// NULs.
#[test]
fn utf16_is_not_mistaken_for_utf8() {
    let dir = TempDir::new().unwrap();
    let mut bytes: Vec<u8> = Vec::new();
    for ch in "id;name\n1;Zurich\n2;Bern\n".encode_utf16() {
        bytes.extend_from_slice(&ch.to_le_bytes());
    }
    let p = write_bytes(&dir, "u16.csv", &bytes);
    let spec = sniffed(&p).spec;
    // No column name and no text value may contain a NUL: that is what
    // reading UTF-16 as UTF-8 produces.
    for c in &spec.columns {
        assert!(!c.name.contains('\0'), "NUL in column name {:?}", c.name);
        assert!(!c.source_name().contains('\0'), "NUL in source {:?}", c.source_name());
    }
    let batch = provider::spec_to_batch(&spec, &p).unwrap();
    assert_eq!(batch.num_rows(), 2);
    let text: Vec<Option<String>> = (0..batch.num_columns())
        .filter(|i| {
            batch.column(*i).as_any().downcast_ref::<StringArray>().is_some()
        })
        .flat_map(|c| col_str(&batch, c))
        .collect();
    assert!(
        text.iter().flatten().any(|v| v == "Zurich"),
        "expected the decoded text, got {text:?}"
    );
    assert!(
        !text.iter().flatten().any(|v| v.contains('\0')),
        "NUL bytes leaked into the values: {text:?}"
    );
}

/// Stamping records "this spec matches this file". Recording that before
/// checking it would leave a fresh-looking sidecar that --frozen trusts and
/// that cannot parse anything.
#[test]
fn stamping_refuses_a_spec_that_cannot_parse_the_file() {
    let dir = TempDir::new().unwrap();
    let p = write(&dir, "s.csv", "a,b\n1,2\n");
    let out = tdy(&["sniff", p.to_str().unwrap(), "--no-llm"]);
    assert!(out.status.success());
    let sc = tdy::sidecar::sidecar_path(&p);
    let text = fs::read_to_string(&sc).unwrap();
    // Point a column at something that does not exist.
    let broken = text.replace("name = \"a\"", "name = \"a\"\nsource = \"nope\"");
    assert_ne!(broken, text, "test needs to actually modify the sidecar");
    fs::write(&sc, &broken).unwrap();

    let out = tdy(&["validate", p.to_str().unwrap(), "--stamp"]);
    assert!(!out.status.success(), "stamping a broken spec must fail");
    assert_eq!(
        fs::read_to_string(&sc).unwrap(),
        broken,
        "a refused stamp must not have rewritten the sidecar"
    );
}

/// A declared encoding that is wrong produces replacement characters. That is
/// silent corruption unless somebody says so.
#[test]
fn a_wrong_declared_encoding_is_reported() {
    let dir = TempDir::new().unwrap();
    // Bytes that are not valid UTF-8.
    let p = write_bytes(&dir, "w.csv", b"k,v\na,M\xfcller\n");
    let out = tdy(&["sniff", p.to_str().unwrap(), "--no-llm"]);
    assert!(out.status.success());
    // Force the wrong encoding into the sidecar and re-run.
    let sc = tdy::sidecar::sidecar_path(&p);
    let text = fs::read_to_string(&sc).unwrap();
    let forced = if text.contains("encoding =") {
        regex_replace_encoding(&text)
    } else {
        text.replace("format = \"delimited\"", "format = \"delimited\"\nencoding = \"utf-8\"")
    };
    fs::write(&sc, forced).unwrap();
    let out = tdy(&["validate", p.to_str().unwrap(), "--stamp"]);
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        err.contains("does not decode cleanly"),
        "expected a mojibake warning, got: {err}"
    );
}

fn regex_replace_encoding(text: &str) -> String {
    text.lines()
        .map(|l| if l.trim_start().starts_with("encoding =") { "encoding = \"utf-8\"" } else { l })
        .collect::<Vec<_>>()
        .join("\n")
}

// ---------------------------------------------------------------------------
// 5. The pre-pass must read the query, not grep it
// ---------------------------------------------------------------------------

/// A `messy()` call inside a comment is not a file reference. It used to make
/// the whole query fail with "file not found".
#[tokio::test]
async fn messy_inside_a_comment_is_not_a_file_reference() {
    let dir = TempDir::new().unwrap();
    let p = write(&dir, "real.csv", "x\n1\n2\n");
    let sql = format!(
        "SELECT sum(x) s FROM messy('{}') -- messy('/nonexistent/ghost.csv')",
        p.display()
    );
    let b = provider::run_query(&sql, &cfg(), false).await.unwrap().1;
    assert_eq!(col_i64(&b[0], 0), vec![Some(3)]);
}

/// Same for a path that appears inside a string literal.
#[tokio::test]
async fn messy_inside_a_string_literal_is_not_a_file_reference() {
    let dir = TempDir::new().unwrap();
    let p = write(&dir, "real.csv", "x,note\n1,a\n2,b\n");
    let sql = format!(
        "SELECT count(*) c FROM messy('{}') WHERE note <> 'messy(''/nonexistent/ghost.csv'')'",
        p.display()
    );
    let b = provider::run_query(&sql, &cfg(), false).await.unwrap().1;
    assert_eq!(col_i64(&b[0], 0), vec![Some(2)]);
}

// ---------------------------------------------------------------------------
// 6. --frozen is a guarantee, not a hint
// ---------------------------------------------------------------------------

/// Frozen mode must not write anything, even when the sidecar is stale.
#[tokio::test]
async fn frozen_mode_writes_nothing() {
    let dir = TempDir::new().unwrap();
    let p = write(&dir, "f.csv", "a,b\n1,2\n");
    let sql = format!("SELECT * FROM messy('{}')", p.display());
    // Create a sidecar, then make it stale.
    provider::run_query(&sql, &cfg(), false).await.unwrap();
    let sc = tdy::sidecar::sidecar_path(&p);
    let before = fs::read_to_string(&sc).unwrap();
    fs::write(&p, "a,b\n1,2\n3,4\n").unwrap();

    let err = provider::run_query(&sql, &cfg(), true).await.unwrap_err();
    assert!(format!("{err:#}").contains("--frozen"));
    assert_eq!(fs::read_to_string(&sc).unwrap(), before, "frozen run rewrote the sidecar");
}

// ---------------------------------------------------------------------------
// 7. CLI behaviour
// ---------------------------------------------------------------------------

fn tdy(args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_tdy")).args(args).output().unwrap()
}

#[test]
fn errors_exit_nonzero_and_go_to_stderr() {
    let out = tdy(&["query", "SELECT * FROM messy('/definitely/not/here.csv')"]);
    assert!(!out.status.success());
    assert_ne!(out.status.code(), Some(101), "must be an error, not a panic");
    assert!(out.stdout.is_empty(), "no partial results on stdout");
    assert!(!out.stderr.is_empty());
}

#[test]
fn directory_instead_of_file_is_a_clear_error() {
    let dir = TempDir::new().unwrap();
    let out = tdy(&["sniff", dir.path().to_str().unwrap(), "--no-llm"]);
    assert!(!out.status.success());
    assert_ne!(out.status.code(), Some(101));
}

#[test]
fn schema_command_emits_valid_json_schema() {
    let out = tdy(&["schema"]);
    assert!(out.status.success());
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert!(v.get("properties").is_some() || v.get("$ref").is_some());
}

/// Every output format must actually write the file it was given. `--format
/// table -o results.csv` used to print to the terminal, write nothing, and
/// exit 0.
#[test]
fn every_output_format_writes_the_file_it_was_given() {
    let dir = TempDir::new().unwrap();
    let p = write(&dir, "a.csv", "k,v\na,1\nb,2\n");
    let sql = format!("SELECT * FROM messy('{}')", p.display());

    for (args, name) in [
        (vec!["--format", "csv"], "out.csv"),
        (vec!["--format", "json"], "out.json"),
        (vec!["--format", "parquet"], "out.parquet"),
        (vec!["--format", "table"], "out.txt"),
        (vec![], "byext.csv"),
        (vec![], "byext.parquet"),
    ] {
        let out_path = dir.path().join(name);
        let mut argv: Vec<String> =
            vec!["query".into(), sql.clone(), "-o".into(), out_path.display().to_string()];
        argv.extend(args.iter().map(|a| a.to_string()));
        let out = Command::new(env!("CARGO_BIN_EXE_tdy"))
            .args(&argv)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{name}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        let meta = fs::metadata(&out_path)
            .unwrap_or_else(|_| panic!("{name}: no file was written"));
        assert!(meta.len() > 0, "{name}: wrote an empty file");
        assert!(
            out.stdout.is_empty(),
            "{name}: results went to stdout as well as to the file"
        );
    }
}

/// `tdy validate` is the command that tells you whether a sidecar can be
/// trusted, so it has to be right about all three of its answers.
#[test]
fn validate_reports_fresh_stale_and_missing() {
    let dir = TempDir::new().unwrap();
    let p = write(&dir, "v.csv", "k,v\na,1\n");
    let path = p.to_str().unwrap();

    // No sidecar yet.
    let out = tdy(&["validate", path]);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("no sidecar"));

    // Fresh.
    assert!(tdy(&["sniff", path, "--no-llm"]).status.success());
    let out = tdy(&["validate", path]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert!(String::from_utf8_lossy(&out.stdout).contains("ok"));

    // Stale.
    fs::write(&p, "k,v\na,1\nb,2\n").unwrap();
    let out = tdy(&["validate", path]);
    assert!(!out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("stale"), "{err}");

    // ...and --stamp makes it fresh again, keeping the spec.
    let out = tdy(&["validate", path, "--stamp"]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert!(tdy(&["query", &format!("SELECT * FROM messy('{path}')"), "-f"]).status.success());
}

/// A preview caps *output* rows. Capping extraction instead meant a ten-row
/// preview of a file with a twelve-line title block had nothing left to
/// promote a header from.
#[test]
fn a_long_title_block_does_not_break_the_preview() {
    let dir = TempDir::new().unwrap();
    let mut body = String::new();
    for i in 0..12 {
        body.push_str(&format!("Report line {i}\n"));
    }
    body.push_str("k;v\n");
    for i in 0..50 {
        body.push_str(&format!("row{i};{i}\n"));
    }
    let p = write(&dir, "titled.csv", &body);
    let out = tdy(&["sniff", p.to_str().unwrap(), "--no-llm"]);
    assert!(
        out.status.success(),
        "sniff preview failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(String::from_utf8_lossy(&out.stdout).contains("row0"));
}

#[test]
fn parquet_to_stdout_is_refused() {
    let dir = TempDir::new().unwrap();
    let p = write(&dir, "a.csv", "x\n1\n");
    let out = tdy(&[
        "query",
        &format!("SELECT * FROM messy('{}')", p.display()),
        "--format",
        "parquet",
    ]);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("parquet"));
}
