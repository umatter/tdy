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

use datafusion::arrow::array::{Array, Int64Array, StringArray};
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
    let s = sample::build(path, 64 * 1024, Limits::default()).unwrap();
    sniff::sniff(path, &s, Limits::default()).unwrap()
}

async fn query(sql: &str) -> Vec<datafusion::arrow::record_batch::RecordBatch> {
    provider::run_query(sql, &cfg(), false).await.unwrap().1
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
            let s = sample::build(&p, 64 * 1024, Limits::default())?;
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

/// A valid-UTF-8 file whose tail sample begins mid-character was recorded as
/// windows-1252 and every accented value came back as mojibake, at confidence
/// 0.80 with no warning. Found by the 2026-09-03 corpus audit.
///
/// Fixture: testdata/torn_tail_utf8.csv (testdata/gen/15_audit_defects.py).
#[test]
fn utf8_file_with_a_torn_tail_sample_is_not_mojibake() {
    let p = fixture("torn_tail_utf8.csv");
    // The production sniff path samples `cfg.sample_bytes` (default 16 KiB,
    // giving a 4 KiB tail) — not the 64 KiB the `sniffed()` helper above
    // uses, which would swallow this 43 KiB fixture whole and never read a
    // separate tail at all.
    let sample_bytes = Config::default().sample_bytes;
    let sample = sample::build(&p, sample_bytes, Limits::default()).unwrap();
    let spec = sniff::sniff(&p, &sample, Limits::default()).unwrap().spec;
    assert_eq!(
        spec.extraction.encoding(),
        Some("utf-8"),
        "torn tail sample was mistaken for another encoding: {:?}",
        spec.extraction
    );
    let batch = provider::spec_to_batch(&spec, &p).unwrap();
    let text = col_str(&batch, 1); // "name" column
    assert!(
        text.iter().flatten().any(|v| v.contains('à') || v.contains('ò')),
        "expected accented text to survive decoding, got {text:?}"
    );
    assert!(
        !text.iter().flatten().any(|v| v.contains('\u{c3}')),
        "mojibake (stray 0xC3 lead byte rendered as text) in output: {text:?}"
    );
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

// ---------------------------------------------------------------------------
// Declared spreadsheet geometry (src/xlguard.rs)
// ---------------------------------------------------------------------------

fn fixture(rel: &str) -> PathBuf {
    let p = Path::new(env!("CARGO_MANIFEST_DIR")).join("testdata").join(rel);
    assert!(p.exists(), "missing fixture {rel} — run `python3 gen_fixtures.py`");
    p
}

/// A spreadsheet declares its geometry, so a few hundred bytes can ask for
/// tens of gigabytes — and every other limit in tdy is checked against a
/// table that already exists, which is far too late.
///
/// Measured before the guard: this 899-byte file reached 4.8 GB and aborted
/// the process under a 3 GB cap. An abort is the one failure mode tdy is not
/// allowed to have. It must be a sentence instead.
#[test]
fn a_tiny_ods_declaring_a_huge_grid_is_refused_not_allocated() {
    let p = fixture("declared_size_ods_declared_grid.ods");
    assert!(p.metadata().unwrap().len() < 10_000, "fixture is meant to be tiny");

    let err = sample::build(&p, 64 * 1024, Limits::default())
        .expect_err("a billion-cell sheet was accepted");
    let msg = format!("{err:#}");
    assert!(msg.contains("1000000020"), "error does not name the declared size: {msg}");
    assert!(msg.contains("max_cells"), "error does not say which knob raises it: {msg}");
}

/// The same for the xlsx reader, which reports `<dimension>` before it
/// builds the grid. One value in the far corner of a sheet is an ordinary
/// accident, not an attack, so it has to fail politely rather than abort.
#[test]
fn a_phantom_xlsx_range_is_refused_before_the_grid_is_built() {
    let p = fixture("declared_size_xlsx_phantom_grid.xlsx");
    let err = sample::build(&p, 64 * 1024, Limits::default())
        .err()
        .map(|e| format!("{e:#}"))
        .or_else(|| {
            let s = sample::build(&p, 64 * 1024, Limits::default()).ok()?;
            sniff::sniff(&p, &s, Limits::default()).err().map(|e| format!("{e:#}"))
        })
        .expect("a 100M-cell phantom range was accepted");
    assert!(err.contains("100000000"), "error does not name the declared size: {err}");
    assert!(err.contains("max_cells"), "error does not say which knob raises it: {err}");
}

/// THE CONTROL, and the reason the guard counts *valued* cells rather than
/// declared ones: LibreOffice pads every sheet it writes out to the full
/// 1,048,576-row grid, so a naive check would refuse almost every .ods in
/// existence. This file declares over a billion cell positions and contains
/// three rows of data.
#[test]
fn libreoffice_full_grid_padding_is_not_mistaken_for_a_huge_sheet() {
    let p = fixture("declared_size_ods_padded_like_libreoffice.ods");
    let s = sample::build(&p, 64 * 1024, Limits::default())
        .expect("an ordinary padded .ods was refused — the guard is unusable");
    let spec = sniff::sniff(&p, &s, Limits::default()).unwrap().spec;
    let b = provider::spec_to_batch(&spec, &p).expect("padded .ods did not execute");
    assert_eq!(b.num_rows(), 3, "padding leaked into the data");

    let stadt = b.column(0).as_any().downcast_ref::<StringArray>().unwrap();
    assert_eq!(
        (0..stadt.len()).map(|i| stadt.value(i)).collect::<Vec<_>>(),
        vec!["Bern", "Chur", "Sion"]
    );
}

/// The limit has to be a real knob, not just a wall: raising it must let a
/// file through, and lowering it must stop one that would otherwise pass.
#[test]
fn the_cell_limit_is_a_knob_in_both_directions() {
    let p = fixture("declared_size_ods_padded_like_libreoffice.ods");
    let tight = Limits { max_cells: 4, ..Limits::default() };
    assert!(
        sample::build(&p, 64 * 1024, tight).is_err(),
        "a 9-cell sheet passed a 4-cell limit"
    );

    let big = fixture("declared_size_ods_declared_grid.ods");
    let generous = Limits { max_cells: 2_000_000_000, ..Limits::default() };
    // Not executed — that would really allocate it. Only the gate is checked.
    assert!(
        tdy::xlguard::preflight(&big, &generous).is_ok(),
        "raising max_cells did not lift the refusal"
    );
    assert!(tdy::xlguard::preflight(&big, &Limits::default()).is_err());
}

/// A zip container is also bounded by what it *decompresses to*, which is
/// the complementary failure: the geometry scan would see nothing wrong with
/// a sheet whose sharedStrings.xml is a gigabyte.
#[test]
fn a_zip_container_is_bounded_by_its_uncompressed_size() {
    let p = fixture("declared_size_ods_padded_like_libreoffice.ods");
    let tiny = Limits { max_file_bytes: 64, ..Limits::default() };
    let err = tdy::xlguard::preflight(&p, &tiny).expect_err("uncompressed size was not checked");
    let msg = format!("{err:#}");
    assert!(msg.contains("expands to"), "unhelpful error: {msg}");
    assert!(msg.contains("max_file_bytes"), "error does not name the knob: {msg}");
}

// ---------------------------------------------------------------------------
// When the first 500 rows lie
// ---------------------------------------------------------------------------
//
// Four shapes reduced from real files that made tdy die mid-query, found by
// sweeping `corpus/` — 9,881 files from twenty-six public data-wrangling
// repositories. That corpus is gitignored and CI never sees it, so the shapes
// live in `testdata/` and these tests are what protect the fixes.
//
// In every case tdy's old behaviour was *correct* — it errored, naming the
// row, rather than inventing a value. But a tool that refuses one real CSV in
// ten is not one anybody reaches for, and "correctly refused" is not "works".

/// `NA` at row 901 in a column that is an integer for the 900 before it.
///
/// tdy already knew `NA` means missing — it is in `NA_TOKENS` — but only
/// declared it when it happened to *see* one inside the 500-row type sample.
/// So the identical file with the `NA` near the top read fine and this one
/// died. One file behaving two ways depending on where a token sits is worse
/// than either answer.
#[tokio::test]
async fn a_missing_marker_past_the_type_sample_is_still_a_missing_marker() {
    let p = fixture("late_surprise_na_after_the_sample.csv");
    let b = query(&format!(
        "SELECT count(*) n, count(children) nonnull, sum(children) total \
         FROM messy('{}')",
        p.display()
    ))
    .await;
    assert_eq!(col_i64(&b[0], 0), vec![Some(1000)], "rows");
    assert_eq!(col_i64(&b[0], 1), vec![Some(999)], "the NA row must be null, not missing");
    assert_eq!(col_i64(&b[0], 2), vec![Some(999)], "sum");
}

/// The same vocabulary, in a casing the file chose. `is_na` folds case when
/// the sniffer decides a token is missing; the executor did not, so a spec
/// carrying the canonical `na` failed on a file containing `NULL`.
#[tokio::test]
async fn missing_markers_match_whatever_case_the_file_used() {
    let dir = TempDir::new().unwrap();
    let p = write(&dir, "case.csv", "id,v\n1,1\n2,NULL\n3,N/A\n4,nan\n5,5\n");
    let b = query(&format!(
        "SELECT count(v) nonnull, sum(v) total FROM messy('{}')",
        p.display()
    ))
    .await;
    assert_eq!(col_i64(&b[0], 0), vec![Some(2)], "NULL/N/A/nan must all be null");
    assert_eq!(col_i64(&b[0], 1), vec![Some(6)]);
}

/// An id that is digits for 700 rows and then is not. No vocabulary fixes
/// this — the value is data — so the only honest answer is that the column is
/// text, decided by checking the guess against the whole file rather than
/// discovering it at row 701 of somebody's query.
#[tokio::test]
async fn a_type_that_the_sample_got_wrong_is_widened_not_discovered_later() {
    let p = fixture("late_surprise_id_turns_alphanumeric.csv");
    let res = sniffed(&p);
    let station = res
        .spec
        .columns
        .iter()
        .find(|c| c.name == "station_id")
        .expect("station_id");
    assert_eq!(station.dtype, DType::Utf8, "the sample's guess was not corrected");

    // And the note names the value, its row, and how many of how many — which
    // is what tells a reader "two strays" apart from "wrong type".
    let note = res
        .spec
        .notes
        .iter()
        .find(|n| n.contains("station_id"))
        .unwrap_or_else(|| panic!("no note explaining the widening: {:?}", res.spec.notes));
    assert!(note.contains("TA1309000067"), "{note}");
    assert!(note.contains("of 1000"), "the denominator is missing: {note}");

    let b = query(&format!("SELECT count(*) FROM messy('{}')", p.display())).await;
    assert_eq!(col_i64(&b[0], 0), vec![Some(1000)]);
}

/// A header from a *second* export, worded differently, sitting at row 501.
///
/// tdy must not drop it. Dropping rows because they fail to parse is exactly
/// the silent data loss this project refuses — the row could be data. So the
/// column widens, the note names the row, and a human who agrees it is a
/// stray adds a `drop_rows_matching`.
#[tokio::test]
async fn a_differently_worded_header_mid_file_is_reported_never_dropped() {
    let p = fixture("late_surprise_second_export_header.csv");
    let res = sniffed(&p);
    let amount = res.spec.columns.iter().find(|c| c.name == "amount").unwrap();
    assert_eq!(amount.dtype, DType::Utf8);
    assert!(
        res.spec.notes.iter().any(|n| n.contains("Total") && n.contains("row 501")),
        "{:?}",
        res.spec.notes
    );

    // 1001 rows: the junk one is still there, because tdy cannot prove it is junk.
    let b = query(&format!("SELECT count(*) FROM messy('{}')", p.display())).await;
    assert_eq!(col_i64(&b[0], 0), vec![Some(1001)], "a row was silently dropped");
}

/// The same shape, but the repeat is byte-identical to the real header — and
/// *that* tdy can settle without judgement, because a row reproducing the
/// header exactly is provably not data. It is dropped, and the numeric columns
/// keep their types.
///
/// The pair is the point: both look like "a header in the middle of the file",
/// and tdy does automatically only the one it can prove.
#[tokio::test]
async fn an_identical_repeated_header_is_dropped_and_the_types_survive() {
    let p = fixture("late_surprise_repeated_header.csv");
    let res = sniffed(&p);
    let amount = res.spec.columns.iter().find(|c| c.name == "amount").unwrap();
    assert_eq!(amount.dtype, DType::Int64, "the repeat was not dropped, so the type widened");
    assert!(
        res.spec.transforms.iter().any(|t| matches!(t, Transform::DropRowsMatching { .. })),
        "no drop transform was emitted: {:?}",
        res.spec.transforms
    );

    let b = query(&format!(
        "SELECT count(*) n, sum(amount) total FROM messy('{}')",
        p.display()
    ))
    .await;
    assert_eq!(col_i64(&b[0], 0), vec![Some(1000)], "the repeated header is still in the data");
    assert_eq!(col_i64(&b[0], 1), vec![Some(600500)]);
}

/// `--quick` skips the check, and says so where it matters: in the sidecar,
/// which is what a colleague reads six months later.
#[test]
fn quick_skips_verification_and_records_that_it_did() {
    let p = fixture("late_surprise_id_turns_alphanumeric.csv");
    let dir = TempDir::new().unwrap();
    let staged = dir.path().join("f.csv");
    std::fs::copy(&p, &staged).unwrap();

    let out = Command::new(env!("CARGO_BIN_EXE_tdy"))
        .args(["sniff", staged.to_str().unwrap(), "--no-llm", "--quick"])
        .output()
        .expect("run tdy");
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("NOT checked against the whole file"), "{text}");
    // Unverified, so the sample's wrong guess survives — which is exactly the
    // behaviour the flag is opting into.
    assert!(text.contains("int64"), "the unverified guess is not visible: {text}");
}

/// `sheet_grid` reads a bounded window of a sheet — 20 rows x 12 columns for
/// `.show`'s raw view — and used to hand that window back indistinguishable
/// from a whole small sheet. A reader who takes twelve columns for the file's
/// full header writes a `matches` clause for a column that is not the one
/// they saw: a wrong answer arrived at silently, which is the failure this
/// project exists to prevent. The clip is now marked *in the grid*, where
/// every renderer (console text, the TUI's raw panel) shows it for free.
#[test]
fn a_clipped_sheet_grid_says_that_it_was_clipped() {
    let p = fixture("umsatz.xlsx");
    let sheet = tdy::engine::excel_sheet_shapes(&p, Limits::default()).unwrap()[0].name.clone();

    // The whole sheet fits inside the real cap: no markers anywhere.
    let full = tdy::engine::sheet_grid(&p, &sheet, Limits::default(), 20, 12).unwrap();
    assert!(
        !full.iter().any(|r| r.iter().any(|c| c == "…")),
        "an unclipped read must carry no marker: {full:?}"
    );

    // The same read clipped to 2x2: every emitted row ends in a `…` cell
    // (columns were cut) and a final `…` row says the rows were too.
    let clipped = tdy::engine::sheet_grid(&p, &sheet, Limits::default(), 2, 2).unwrap();
    assert_eq!(clipped.len(), 3, "2 rows plus the marker row: {clipped:?}");
    assert_eq!(clipped[0].len(), 3, "2 cells plus the marker cell: {clipped:?}");
    assert_eq!(clipped[0][2], "…");
    assert_eq!(clipped[1][2], "…");
    assert_eq!(clipped[2], vec!["…".to_string()]);
    // The cells that *were* read come back unchanged — the marker is an
    // addition, never a replacement.
    assert_eq!(clipped[0][..2], full[0][..2]);
}

/// A single-line file has no header — promoting its only line returned an
/// EMPTY table with no warning. Found by the 2026-09-03 corpus audit.
#[test]
fn a_single_line_file_keeps_its_only_row() {
    let dir = TempDir::new().unwrap();
    let f = write(&dir, "one_line.txt", "1.0.1\n");
    let spec = sniffed(&f).spec;
    let t = tdy::engine::execute(&spec, &f, Limits::default()).unwrap();
    assert_eq!(t.num_rows(), 1, "the file's only datum was consumed as a header");
}

/// Every row structurally identical = no header. Promoting row 1 dropped a
/// record into the column name.
#[test]
fn a_homogeneous_single_column_list_keeps_its_first_row() {
    let dir = TempDir::new().unwrap();
    let f = write(&dir, "paths.txt", "a/b/one.csv\na/b/two.csv\na/b/three.csv\na/b/four.csv\n");
    let spec = sniffed(&f).spec;
    let t = tdy::engine::execute(&spec, &f, Limits::default()).unwrap();
    assert_eq!(t.num_rows(), 4, "row 1 was consumed as a header");
}

/// "total" as a routine category value in the last row is not a summary row.
/// tdy silently deleted a real record. Found by the 2026-09-03 corpus audit.
#[test]
fn a_frequent_category_value_is_not_a_footer() {
    let dir = TempDir::new().unwrap();
    let mut s = String::from("state,subsector,sales\n");
    for i in 0..40 {
        s.push_str(&format!("S{i},retail,{}\n", 100 + i));
        s.push_str(&format!("S{i},total,{}\n", 200 + i));
    }
    let f = write(&dir, "category_total.csv", &s);
    let spec = sniffed(&f).spec;
    let t = tdy::engine::execute(&spec, &f, Limits::default()).unwrap();
    assert_eq!(t.num_rows(), 80, "a routine 'total' category row was dropped as a footer");
}

/// A frequent label that only clusters *after* the sniffer's row-count probe
/// window (`PROBE_ROWS` = 2000) must still be corroborated from the file's
/// real tail, not from a head-only probe table that never reaches it. A
/// 2,600-row file where the last 100 rows share a "total" category read as 0
/// occurrences from the head alone, and the last row was silently dropped as
/// a footer — the same class of bug as `a_frequent_category_value_is_not_a_footer`,
/// just moved past the probe window. Found by the 2026-09-04 fix-round review.
#[test]
fn a_late_clustered_category_beyond_the_probe_window_is_not_a_footer() {
    let dir = TempDir::new().unwrap();
    let mut s = String::from("state,subsector,sales\n");
    for i in 0..2500 {
        s.push_str(&format!("S{i},retail,{}\n", 100 + i));
    }
    for i in 0..100 {
        s.push_str(&format!("S{i},total,{}\n", 200 + i));
    }
    let f = write(&dir, "late_category.csv", &s);
    let spec = sniffed(&f).spec;
    let t = tdy::engine::execute(&spec, &f, Limits::default()).unwrap();
    assert_eq!(
        t.num_rows(),
        2600,
        "the last row of a late-clustered 'total' category was dropped as a footer"
    );
}

/// A column whose values all appear after the probe window was typed text
/// while identical sibling columns were numeric, and whole-file verification
/// never corrected it (it only widens). Found by the 2026-09-03 corpus audit:
/// `individual_results_df.csv` types `p7` as text while `p1`-`p6` (identical
/// shape) are whole numbers, because `p7`'s only non-null values sit at rows
/// 21,513-21,655, past `PROBE_ROWS`.
#[test]
fn a_column_that_starts_late_is_still_typed() {
    let dir = TempDir::new().unwrap();
    let mut s = String::from("a,b\n");
    for i in 0..2500 {
        s.push_str(&format!("{i},NA\n"));
    }
    for i in 0..50 {
        s.push_str(&format!("{i},{}\n", i % 9));
    }
    let f = write(&dir, "late_column.csv", &s);
    let spec = sniffed(&f).spec;
    let b = spec.columns.iter().find(|c| c.name == "b").expect("column b");
    assert!(
        !matches!(b.dtype, DType::Utf8),
        "late-starting numeric column stayed text: {:?}",
        b.dtype
    );
}

/// A late-starting fractional column must never narrow to `Float64`: money is
/// exactly a consistently-scaled fractional column, `guess_type` sends that
/// shape to `Decimal` (never `Float64`) precisely because money must not go
/// through binary floating point, and a bounded per-value tracker cannot tell
/// `Decimal` from an ordinary float without the whole sample in hand. Found
/// by fix-round-1 review of `a_column_that_starts_late_is_still_typed`: the
/// first cut of the narrowing fix resolved to `Float64` and rendered 135.1
/// instead of 135.10. The safe outcome is staying `Utf8` — an explicit,
/// loud refusal to sum it — never a plausible wrong number.
#[test]
fn a_late_starting_money_column_does_not_narrow_to_float() {
    let dir = TempDir::new().unwrap();
    let mut s = String::from("a,b\n");
    for i in 0..2500 {
        s.push_str(&format!("{i},NA\n"));
    }
    for i in 0..50 {
        s.push_str(&format!("{i},{:.2}\n", 100.0 + i as f64 + 0.10));
    }
    let f = write(&dir, "late_money.csv", &s);
    let r = sniffed(&f);
    let b = r.spec.columns.iter().find(|c| c.name == "b").expect("column b");
    assert_eq!(
        b.dtype,
        DType::Utf8,
        "a late-starting two-decimal column narrowed to a lossy type: {:?}",
        b.dtype
    );
    assert!(
        r.spec.notes.iter().any(|n| n.contains("column `b`") && n.contains("fractional")),
        "no note explains why the fractional column was left as text: {:?}",
        r.spec.notes
    );
}

/// As above, with every value the same repeating decimal (`0.10`) — the
/// reviewer's second measurement, where `sum(x)` on the `Float64` narrowing
/// returned `100.00000000000088` instead of `100.00`. Pinned separately
/// because a single repeated value is the case most likely to look
/// "consistent enough" to a naive narrower.
#[tokio::test]
async fn a_late_starting_repeated_decimal_does_not_narrow_to_float() {
    let dir = TempDir::new().unwrap();
    let mut s = String::from("a,b\n");
    for i in 0..2500 {
        s.push_str(&format!("{i},NA\n"));
    }
    for i in 0..1000 {
        s.push_str(&format!("{i},0.10\n"));
    }
    let f = write(&dir, "late_repeated_decimal.csv", &s);
    let r = sniffed(&f);
    let b = r.spec.columns.iter().find(|c| c.name == "b").expect("column b");
    assert_eq!(
        b.dtype,
        DType::Utf8,
        "a repeated late-starting decimal narrowed to a lossy type: {:?}",
        b.dtype
    );
    // The safe fallback (text) must actually refuse to be summed, rather than
    // DataFusion silently coercing Utf8 to a float on its own — a loud error
    // is correct here, a quiet wrong number is not.
    let err = provider::run_query(
        &format!("SELECT sum(b) s FROM messy('{}')", f.display()),
        &cfg(),
        false,
    )
    .await;
    assert!(err.is_err(), "sum() over the text-kept column should be refused, not coerced");
}

/// A legend/footnote block below the data was read as data rows at high
/// confidence with no warning. Found by the 2026-09-03 corpus audit.
#[test]
fn a_trailing_prose_block_is_noticed() {
    let dir = tempfile::tempdir().unwrap();
    let f = dir.path().join("footnote_block.csv");
    let mut s = String::from("size,breweries,barrels\n");
    for i in 0..12 { s.push_str(&format!("band {i},{},{}\n", i * 3, i * 1000)); }
    s.push_str("\nLegend\n");
    s.push_str("1) Number of Breweries - Count of brewery premises reporting operations.\n");
    s.push_str("2) Size - Based on Annual Production as reported on the operations report.\n");
    std::fs::write(&f, &s).unwrap();
    let r = sniffed(&f);
    assert!(r.confidence < 0.8, "confidence {:?}", r.confidence);
    assert!(r.spec.notes.iter().any(|n| n.contains("trailing")),
            "no note about the trailing block: {:?}", r.spec.notes);
}

/// The header repeated mid-file between sections was read as data.
#[test]
fn a_repeated_header_row_is_noticed() {
    let dir = tempfile::tempdir().unwrap();
    let f = dir.path().join("repeated_header.csv");
    let mut s = String::from("year,lowest,second\n");
    for i in 0..10 { s.push_str(&format!("{},{},{}\n", 2000 + i, i * 10, i * 20)); }
    s.push_str("year,lowest,second\n");
    for i in 0..10 { s.push_str(&format!("{},{},{}\n", 2010 + i, i * 11, i * 21)); }
    std::fs::write(&f, &s).unwrap();
    let r = sniffed(&f);
    assert!(r.spec.notes.iter().any(|n| n.contains("repeat")),
            "no note about the repeated header: {:?}", r.spec.notes);
}

/// A spreadsheet cell with an embedded newline is one table row, not two.
///
/// The 2026-09-03 corpus audit flagged `ttb_monthly_stats_2018-12.xlsx` cell
/// A10 (`"Manufacture\nOf Beer (In Barrels)"`) as splitting into two output
/// rows, with the numeric values attaching to the first fragment and an
/// orphan blank row following. Investigating against both the real corpus
/// file and this reduction (`testdata/xl_cell_newline.xlsx`, generated by
/// `testdata/gen/15_audit_defects.py`):
/// `extract_excel` builds one `RawTable` row per calamine `Range` row via
/// `render_cell`, which clones a cell's string verbatim, `\n` included —
/// there is no code path that re-splits row text on embedded newlines. Both
/// `tdy::engine::execute` and the full `messy()` query path already return
/// the correct row count and keep the embedded newline inside the one field
/// it belongs to; this pins that as a regression test rather than leaving it
/// undocumented.
#[test]
fn a_newline_inside_a_cell_is_not_a_row_boundary() {
    let f = Path::new("testdata/xl_cell_newline.xlsx");
    let spec = sniffed(f).spec;
    let t = tdy::engine::execute(&spec, f, Limits::default()).unwrap();
    assert_eq!(t.num_rows(), 3, "an embedded newline split a row");
    let label = t.column(0).as_any().downcast_ref::<StringArray>().unwrap();
    assert_eq!(label.value(1), "Manufacture\nOf Beer (In Barrels)");
}

/// Sibling columns sharing a currency number format must all type as an
/// exact decimal, never `float64` — and never by rounding a genuine value
/// to get there.
///
/// The 2026-09-03 corpus audit found `PCA_Report_FY16Q3.xlsx` typing one
/// currency column as `decimal(2)` and six structurally identical ones as
/// `float64`, so a `SUM` over the float columns carried extra IEEE-754
/// noise the sheet itself does not show. calamine 0.36 never exposes a
/// cell's number format, so `xlmoney` reads `xl/styles.xml` and the sheet
/// XML directly out of the zip instead. `testdata/xl_money_siblings.xlsx`
/// (generated by `testdata/gen/15_audit_defects.py`) reproduces the real
/// file's two distinct value-level failure modes for the one currency
/// format `"$"#,##0.00"`, all four of whose data columns carry it:
/// `added`'s values are clean but `render_cell`'s shortest-round-trip
/// Display drops a trailing zero on one of them (100000.10 -> "100000.1"),
/// which used to defeat the heuristic's `consistent`-scale check; `amount_a`,
/// `amount_b` and `amount_c` carry genuine IEEE-754 noise already baked into
/// their stored doubles (values borrowed verbatim from the real file's
/// Total/Rehabilitation columns), which used to push the apparent scale
/// past the heuristic's 1..=4 window.
///
/// All four must still become `Decimal`, and — the load-bearing half of
/// this test — every sampled value must survive verbatim: `Decimal`'s scale
/// is the *widest* fractional part any sampled value in that column
/// actually has, so a noisy value pads the clean ones with zeros rather
/// than the noisy one ever being rounded to match them.
#[test]
fn currency_formatted_columns_all_become_decimal() {
    let f = Path::new("testdata/xl_money_siblings.xlsx");
    let spec = sniffed(f).spec;
    let money: Vec<&ColumnSpec> = spec
        .columns
        .iter()
        .filter(|c| c.name == "added" || c.name.starts_with("amount"))
        .collect();
    assert_eq!(money.len(), 4, "expected added/amount_a/amount_b/amount_c: {spec:#?}");
    for c in &money {
        assert!(
            matches!(c.dtype, DType::Decimal { .. }),
            "{} typed {:?}; every currency-formatted sibling must agree",
            c.name,
            c.dtype
        );
    }
    // agency_name has no currency format at all and must not be swept in by
    // a column-index bug in the tally.
    let name_col = spec.columns.iter().find(|c| c.name == "agency_name").unwrap();
    assert_eq!(name_col.dtype, DType::Utf8);

    let t = tdy::engine::execute(&spec, f, Limits::default()).unwrap();
    // The noisiest value the file actually contains (borrowed verbatim from
    // the real file's Total column, "255871181.1100003" at scale 9) must
    // come back as exactly that scaled mantissa — proof nothing was rounded
    // to get here, not merely that *some* value round-tripped.
    let idx = t.schema().fields().iter().position(|f| f.name() == "amount_c").unwrap();
    let amount_c =
        t.column(idx).as_any().downcast_ref::<datafusion::arrow::array::Decimal128Array>().unwrap();
    assert_eq!(amount_c.value(0), 255_871_181_110_000_300, "a noisy value was rounded away");
}

/// A short footnote line sitting between two long ones must not stop the
/// trailing-block scan from crossing it.
///
/// The 2026-09-04 audit re-run found `trailing_prose_block` failing on the
/// real `ttb_brewery_size_2011/2017/2018.xlsx` files for exactly this shape:
/// a legend block containing a short line ("5) Only for CY 2010...", well
/// under the 40-character prose bar and not matching the legend/footnote
/// lead-in pattern) between two long ones. The first version of the
/// detector required every row it walked to itself look like prose, so it
/// stopped at the short line and never reached the two qualifying rows it
/// needs to fire.
#[test]
fn a_short_line_inside_the_trailing_block_does_not_break_the_scan() {
    let dir = tempfile::tempdir().unwrap();
    let f = dir.path().join("short_line_in_block.csv");
    let mut s = String::from("size,breweries,barrels\n");
    for i in 0..12 { s.push_str(&format!("band {i},{},{}\n", i * 3, i * 1000)); }
    s.push_str("\nLegend\n");
    s.push_str("1) Number of Breweries - Count of brewery premises reporting operations.\n");
    s.push_str("2) Short note.\n");
    s.push_str("3) Size - Based on Annual Production as reported on the operations report.\n");
    std::fs::write(&f, &s).unwrap();
    let r = sniffed(&f);
    assert!(r.confidence < 0.8, "confidence {:?}", r.confidence);
    assert!(r.spec.notes.iter().any(|n| n.contains("trailing")),
            "no note about the trailing block: {:?}", r.spec.notes);
}
