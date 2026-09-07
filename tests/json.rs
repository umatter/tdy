//! `--json`: the machine-readable face of sniff / fit / check.
//!
//! The contract: everything the text output says is in the JSON, structured —
//! a gap is a `kind` plus the fields a caller could act on (`tried`, the
//! file's `header`, the remedy in `message`), never prose to re-parse.

use std::path::{Path, PathBuf};
use std::process::Command;

use tempfile::TempDir;

use datafusion::arrow::array::{Array, Int64Array, StringArray};
use tdy::config::Limits;
use tdy::spec::*;

fn corpus() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("testdata").join("drifting_exports")
}

fn staged() -> TempDir {
    let dir = TempDir::new().unwrap();
    for e in std::fs::read_dir(corpus()).unwrap().flatten() {
        let p = e.path();
        let n = e.file_name().to_string_lossy().to_string();
        if p.is_file() && !n.ends_with(".tdy.toml") && !n.ends_with(".tdy.lock") {
            std::fs::copy(&p, dir.path().join(&n)).unwrap();
        }
    }
    dir
}

fn tdy(args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_tdy")).args(args).output().expect("run tdy")
}

fn json_of(out: &std::process::Output) -> serde_json::Value {
    serde_json::from_slice(&out.stdout).unwrap_or_else(|e| {
        panic!(
            "stdout is not JSON: {e}\n{}\n{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        )
    })
}

/// The full pile, structured: statuses, bindings, and — for the refused
/// members — problems with machine-usable fields.
#[test]
fn fit_json_reports_the_whole_pile_structured() {
    let dir = staged();
    let t = dir.path().join("sales.tdy.sql"); // no excludes: 3 members must fail
    let out = tdy(&["fit", t.to_str().unwrap(), "--json"]);
    assert!(!out.status.success(), "the unedited sales target has 3 refusals");
    let v = json_of(&out);

    assert_eq!(v["failed"], 3, "{v:#}");
    assert_eq!(v["fitted"], 9, "{v:#}");
    assert!(v.get("lock_written").is_none(), "no partial lock: {v:#}");

    let members = v["members"].as_array().unwrap();
    assert_eq!(members.len(), 12);

    let july = members.iter().find(|m| m["path"] == "2025-07.csv").unwrap();
    assert_eq!(july["status"], "gaps");
    let p = &july["problems"][0];
    assert_eq!(p["kind"], "no_candidate");
    assert!(p["tried"].as_array().unwrap().iter().any(|t| t == "Betrag"));
    assert!(p["header"].as_array().unwrap().iter().any(|h| h == "Betrag Rp."));

    let aug = members.iter().find(|m| m["path"] == "2025-08.csv").unwrap();
    assert_eq!(aug["problems"][0]["kind"], "ambiguous");

    let jan = members.iter().find(|m| m["path"] == "2025-01.csv").unwrap();
    assert_eq!(jan["status"], "fits");
    assert!(jan["sources"]
        .as_array()
        .unwrap()
        .iter()
        .any(|s| s["column"] == "amount" && s["source"] == "Betrag"));
}

/// A member behind the review gate says so in JSON, with the reason.
#[test]
fn fit_json_carries_the_review_gate() {
    let dir = staged();
    let t = dir.path().join("sales_ok.tdy.sql");
    let out = tdy(&["fit", t.to_str().unwrap(), "--json"]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let v = json_of(&out);
    assert_eq!(v["failed"], 0);
    assert!(v["lock_written"].is_string(), "{v:#}");

    // check --json on the ready dataset.
    let out = tdy(&["check", t.to_str().unwrap(), "--json"]);
    assert!(out.status.success());
    let v = json_of(&out);
    assert_eq!(v["ready"], true, "{v:#}");
    assert_eq!(v["members"].as_array().unwrap().len(), 9);
}

/// sniff --json: confidence, notes, and the full spec, one object.
#[test]
fn sniff_json_is_one_object_with_the_spec_inside() {
    let dir = staged();
    let f = dir.path().join("2025-01.csv");
    let out = tdy(&["sniff", f.to_str().unwrap(), "--no-llm", "--json"]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let v = json_of(&out);
    assert_eq!(v["method"], "heuristic");
    assert!(v["spec"]["columns"].as_array().unwrap().len() >= 3, "{v:#}");
    assert!(v["sidecar"].as_str().unwrap().ends_with(".tdy.toml"));
}

// ---------------------------------------------------------------------------
// A per-column JSON pointer (catalogue D6). `Extraction::Json` serialises a
// nested value back to a JSON string — honest, since nothing is lost, and
// unreachable, since DataFusion has no JSON functions to open it downstream.
// A pointer opens one level, declaratively.
// ---------------------------------------------------------------------------

#[test]
fn a_pointer_reads_a_field_out_of_a_nested_object() {
    let dir = TempDir::new().unwrap();
    let p = dir.path().join("n.ndjson");
    std::fs::write(
        &p,
        "{\"id\":1,\"addr\":{\"city\":\"Bern\",\"zip\":\"3000\"}}\n\
         {\"id\":2,\"addr\":{\"city\":\"Genève\",\"zip\":\"1201\"}}\n",
    )
    .unwrap();

    let col = |name: &str, source: Option<&str>, pointer: Option<&str>, dtype| ColumnSpec {
        name: name.into(),
        source: source.map(String::from),
        dtype,
        nullable: true,
        parse: ValueParsing::default(),
        pointer: pointer.map(String::from),
    };
    let spec = ParseSpec {
        extraction: Extraction::Json { lines: true, pointer: None },
        transforms: vec![],
        columns: vec![
            col("id", None, None, DType::Int64),
            col("city", Some("addr"), Some("/city"), DType::Utf8),
            // The same source column, opened twice at different depths.
            col("zip", Some("addr"), Some("/zip"), DType::Utf8),
        ],
        confidence: None,
        notes: vec![],
    };
    spec.validate().expect("pointers on a json extraction are valid");

    let t = tdy::engine::execute(&spec, &p, Limits::default()).unwrap();
    let city = t.column(1).as_any().downcast_ref::<StringArray>().unwrap();
    assert_eq!((city.value(0), city.value(1)), ("Bern", "Genève"));
    let zip = t.column(2).as_any().downcast_ref::<StringArray>().unwrap();
    assert_eq!((zip.value(0), zip.value(1)), ("3000", "1201"));
}

/// A key some records lack is missing, not an error — that is the ordinary
/// shape of a JSON export, and the union-of-keys rule already says so.
#[test]
fn a_pointer_that_does_not_resolve_is_a_null() {
    let dir = TempDir::new().unwrap();
    let p = dir.path().join("n.ndjson");
    std::fs::write(&p, "{\"a\":{\"x\":1}}\n{\"a\":{}}\n").unwrap();
    let spec = ParseSpec {
        extraction: Extraction::Json { lines: true, pointer: None },
        transforms: vec![],
        columns: vec![ColumnSpec {
            name: "x".into(),
            source: Some("a".into()),
            dtype: DType::Int64,
            nullable: true,
            parse: ValueParsing::default(),
            pointer: Some("/x".into()),
        }],
        confidence: None,
        notes: vec![],
    };
    let t = tdy::engine::execute(&spec, &p, Limits::default()).unwrap();
    let x = t.column(0).as_any().downcast_ref::<Int64Array>().unwrap();
    assert_eq!(x.value(0), 1);
    assert!(x.is_null(1), "an absent key is missing, not a failure");
}

/// A pointer that lands on an object or an array is an error: the column would
/// quietly go back to holding JSON text, which is the state a pointer is
/// declared to get out of.
#[test]
fn a_pointer_onto_a_container_is_refused() {
    let dir = TempDir::new().unwrap();
    let p = dir.path().join("n.ndjson");
    std::fs::write(&p, "{\"a\":{\"inner\":{\"deep\":1}}}\n").unwrap();
    let spec = ParseSpec {
        extraction: Extraction::Json { lines: true, pointer: None },
        transforms: vec![],
        columns: vec![ColumnSpec {
            name: "inner".into(),
            source: Some("a".into()),
            dtype: DType::Utf8,
            nullable: true,
            parse: ValueParsing::default(),
            pointer: Some("/inner".into()),
        }],
        confidence: None,
        notes: vec![],
    };
    let e = format!("{:#}", tdy::engine::execute(&spec, &p, Limits::default()).unwrap_err());
    assert!(e.contains("an object"), "{e}");
    assert!(e.contains("Point at a value inside it"), "and what to do instead: {e}");
}

/// A pointer only means something inside JSON, and `validate` says so before
/// anything is read.
#[test]
fn a_pointer_on_a_csv_is_refused_by_validate() {
    let spec = ParseSpec {
        extraction: Extraction::Delimited {
            delimiter: ',',
            quote: None,
            escape: None,
            encoding: None,
            comment: None,
            ragged: RaggedPolicy::Error,
        },
        transforms: vec![],
        columns: vec![ColumnSpec {
            name: "a".into(),
            source: None,
            dtype: DType::Utf8,
            nullable: true,
            parse: ValueParsing::default(),
            pointer: Some("/x".into()),
        }],
        confidence: None,
        notes: vec![],
    };
    let e = format!("{:?}", spec.validate().expect_err("a csv has no JSON to point into"));
    assert!(e.contains("read as delimited"), "{e}");
}

#[test]
fn json_members_carry_their_sheet() {
    let dir = tempfile::TempDir::new().unwrap();
    std::fs::copy(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("testdata/sheet_frames_two_fit.xlsx"),
        dir.path().join("2025.xlsx"),
    )
    .unwrap();
    let t = dir.path().join("monat.tdy.sql");
    std::fs::write(
        &t,
        "CREATE TABLE monat (month DATE NOT NULL OPTIONS(matches='Datum'), region TEXT NOT NULL OPTIONS(matches='Region'), amount DECIMAL(14,2) NOT NULL OPTIONS(matches='Betrag')) WITH (files = '*.xlsx', date_order = 'dmy');",
    )
    .unwrap();
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_tdy"))
        .args(["--json", "fit", t.to_str().unwrap(), "--dry-run"])
        .output()
        .unwrap();
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let members = v["members"].as_array().unwrap();
    assert_eq!(members.len(), 2, "{v:#}");
    assert_eq!(members[0]["path"], "2025.xlsx");
    assert_eq!(members[0]["sheet"], "Q1");
    assert_eq!(members[1]["sheet"], "Q2");
}
