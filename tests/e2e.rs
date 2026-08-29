//! End-to-end tests. Everything here runs with backend = none: the LLM tier
//! is exercised structurally (schema generation, retry-loop plumbing) but
//! network inference is never required for the suite to pass.

use std::fs;
use std::path::{Path, PathBuf};

use datafusion::arrow::array::{Array, Date32Array, Decimal128Array, Float64Array, Int64Array, StringArray};
use tempfile::TempDir;

use tdy::config::{Backend, Config, Limits};
use tdy::provider::{self};
use tdy::sample;
use tdy::sidecar::{self, ProvenanceInfo};
use tdy::sniff;
use tdy::spec::*;

fn no_llm_config() -> Config {
    Config { backend: Backend::None, ..Config::default() }
}

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("testdata").join(name)
}

// ---------------------------------------------------------------------------
// The canonical messy Excel file, parsed with a hand-written spec — i.e. the
// exact artifact the LLM tier is meant to produce.
// ---------------------------------------------------------------------------

fn umsatz_spec() -> ParseSpec {
    ParseSpec {
        extraction: Extraction::Excel {
            sheet_name: Some("Umsatz".into()),
            sheet_index: None,
            range: None,
        },
        transforms: vec![
            Transform::SkipRows { head: 3, tail: 1 },
            Transform::PromoteHeader { rows: 2, join: " ".into() },
            Transform::FillDown { columns: vec!["Region".into()] },
            Transform::DropRowsMatching {
                pattern: "(?i)^zwischensumme".into(),
                column: Some("Region".into()),
            },
            Transform::Unpivot {
                id_columns: vec!["Region".into(), "Produkt".into()],
                value_columns: vec![
                    "2025 Jan".into(),
                    "2025 Feb".into(),
                    "2025 Mär".into(),
                    "2025 Dez".into(),
                ],
                variable_name: "monat_raw".into(),
                value_name: "umsatz_raw".into(),
            },
        ],
        columns: vec![
            ColumnSpec {
                name: "region".into(),
                source: Some("Region".into()),
                dtype: DType::Utf8,
                nullable: false,
                parse: ValueParsing::default(),
            },
            ColumnSpec {
                name: "produkt".into(),
                source: Some("Produkt".into()),
                dtype: DType::Utf8,
                nullable: false,
                parse: ValueParsing::default(),
            },
            ColumnSpec {
                name: "monat".into(),
                source: Some("monat_raw".into()),
                dtype: DType::Date { format: "%Y %b".into() },
                nullable: false,
                parse: ValueParsing {
                    replace: vec![
                        Replacement { from: "Mär".into(), to: "Mar".into() },
                        Replacement { from: "Okt".into(), to: "Oct".into() },
                        Replacement { from: "Dez".into(), to: "Dec".into() },
                    ],
                    ..Default::default()
                },
            },
            ColumnSpec {
                name: "umsatz_chf".into(),
                source: Some("umsatz_raw".into()),
                dtype: DType::Decimal { precision: 12, scale: 2 },
                nullable: false,
                parse: ValueParsing {
                    thousands_separator: Some('\''),
                    ..Default::default()
                },
            },
        ],
        confidence: Some(0.9),
        notes: vec![],
    }
}

#[test]
fn messy_excel_hand_spec() {
    let spec = umsatz_spec();
    spec.validate().expect("spec is valid");
    let batch = provider::spec_to_batch(&spec, &fixture("umsatz.xlsx")).unwrap();

    // 4 (region, produkt) pairs x 4 months; subtotal + total rows gone.
    assert_eq!(batch.num_rows(), 16);
    assert_eq!(batch.num_columns(), 4);

    let region = batch.column(0).as_any().downcast_ref::<StringArray>().unwrap();
    let monat = batch.column(2).as_any().downcast_ref::<Date32Array>().unwrap();
    let umsatz = batch.column(3).as_any().downcast_ref::<Decimal128Array>().unwrap();

    // Row 0: Ost / Widget / 2025-01-01 / 1200.50
    assert_eq!(region.value(0), "Ost");
    let epoch = chrono::NaiveDate::from_ymd_opt(1970, 1, 1).unwrap();
    let jan = chrono::NaiveDate::from_ymd_opt(2025, 1, 1).unwrap();
    assert_eq!(monat.value(0), (jan - epoch).num_days() as i32);
    assert_eq!(umsatz.value(0), 120050); // 1200.50 at scale 2

    // German month replace worked: row 2 is Mär -> 2025-03-01.
    let mar = chrono::NaiveDate::from_ymd_opt(2025, 3, 1).unwrap();
    assert_eq!(monat.value(2), (mar - epoch).num_days() as i32);

    // No subtotal contamination: total of all 16 values = 21_244.25.
    let sum: i128 = (0..16).map(|i| umsatz.value(i)).sum();
    assert_eq!(sum, 2_124_425);

    // Merged region cells were filled down: rows 4..8 are the second Ost
    // product (Gadget), not blanks.
    assert_eq!(region.value(4), "Ost");
    assert!(!region.is_null(4));
}

// ---------------------------------------------------------------------------
// Heuristic sniffing of a messy delimited file, then SQL end to end.
// ---------------------------------------------------------------------------

const MESSY_CSV: &str = "\
Kundenexport Muster AG
Stand;2026-01-05

kunde_id;name;kanton;umsatz;beitritt
1001;Meier AG;BE;12'345.50;2019-03-12
1002;Huber GmbH;ZH;8'700.00;2020-11-01
1003;Rossi SA;TI;n/a;2021-06-30
1004;Keller & Co;BE;15'000.25;2018-01-15
";

#[test]
fn sniff_messy_csv_structure() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("kunden.csv");
    fs::write(&path, MESSY_CSV).unwrap();

    let s = sample::build(&path, 16 * 1024).unwrap();
    let result = sniff::sniff(&path, &s, Limits::default()).unwrap();
    let spec = result.spec;
    spec.validate().unwrap();

    // Delimiter and title rows detected.
    match &spec.extraction {
        Extraction::Delimited { delimiter, .. } => assert_eq!(*delimiter, ';'),
        other => panic!("expected delimited extraction, got {other:?}"),
    }
    assert!(matches!(
        spec.transforms.first(),
        Some(Transform::SkipRows { head: 2, .. })
    ));
    assert!(spec
        .transforms
        .iter()
        .any(|t| matches!(t, Transform::PromoteHeader { rows: 1, .. })));

    // Types: id -> int, umsatz -> exact decimal with Swiss thousands sep, date.
    // Money goes to Decimal, not Float64: a franc amount that has been through
    // binary floating point is no longer the amount that was in the file.
    let by_name = |n: &str| spec.columns.iter().find(|c| c.name == n).unwrap();
    assert_eq!(by_name("kunde_id").dtype, DType::Int64);
    assert_eq!(by_name("umsatz").dtype, DType::Decimal { precision: 38, scale: 2 });
    assert_eq!(by_name("umsatz").parse.thousands_separator, Some('\''));
    assert!(by_name("umsatz").parse.na_values.contains(&"n/a".to_string()));
    assert_eq!(by_name("beitritt").dtype, DType::Date { format: "%Y-%m-%d".into() });
}

#[tokio::test]
async fn query_messy_csv_via_sql() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("kunden.csv");
    fs::write(&path, MESSY_CSV).unwrap();

    let cfg = no_llm_config();
    let sql = format!(
        "SELECT kanton, sum(umsatz) AS total, count(*) AS n \
         FROM messy('{}') GROUP BY kanton ORDER BY kanton",
        path.display()
    );
    let (_schema, batches) = provider::run_query(&sql, &cfg, false).await.unwrap();
    let batch = &batches[0];
    let kanton = batch.column(0).as_any().downcast_ref::<StringArray>().unwrap();
    let total = batch
        .column(1)
        .as_any()
        .downcast_ref::<datafusion::arrow::array::Decimal128Array>()
        .expect("money sums as an exact decimal");
    let n = batch.column(2).as_any().downcast_ref::<Int64Array>().unwrap();

    assert_eq!(kanton.value(0), "BE");
    // 12'345.50 + 15'000.25, exactly, at scale 2.
    assert_eq!(total.value(0), 2_734_575);
    assert_eq!(n.value(0), 2);
    assert_eq!(kanton.value(2), "ZH");

    // The pre-pass persisted a sidecar next to the file...
    let sc = sidecar::sidecar_path(&path);
    assert!(sc.exists());
    // ...so a frozen re-run works without any inference.
    let (_s2, b2) = provider::run_query(&sql, &cfg, true).await.unwrap();
    assert_eq!(b2[0].num_rows(), batch.num_rows());
}

#[tokio::test]
async fn frozen_without_sidecar_fails() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("plain.csv");
    fs::write(&path, "a,b\n1,2\n").unwrap();
    let sql = format!("SELECT * FROM messy('{}')", path.display());
    let err = provider::run_query(&sql, &no_llm_config(), true).await.unwrap_err();
    assert!(format!("{err:#}").contains("--frozen"));
}

#[tokio::test]
async fn stale_sidecar_is_reinferred() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("data.csv");
    fs::write(&path, "x,y\n1,10\n2,20\n").unwrap();
    let cfg = no_llm_config();
    let sql = format!("SELECT sum(y) AS s FROM messy('{}')", path.display());
    let (_sch, b) = provider::run_query(&sql, &cfg, false).await.unwrap();
    assert_eq!(
        b[0].column(0).as_any().downcast_ref::<Int64Array>().unwrap().value(0),
        30
    );
    // File changes -> hash mismatch -> re-sniff on the next run.
    fs::write(&path, "x,y\n1,10\n2,20\n3,30\n").unwrap();
    let (_sch, b) = provider::run_query(&sql, &cfg, false).await.unwrap();
    assert_eq!(
        b[0].column(0).as_any().downcast_ref::<Int64Array>().unwrap().value(0),
        60
    );
}

// ---------------------------------------------------------------------------
// Log files through the Lines extraction.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn log_file_with_lines_spec() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("app.log");
    fs::write(
        &path,
        "# started 2026-01-01\n\
         2026-01-05 10:00:01 INFO  login user=um\n\
         2026-01-05 10:00:07 WARN  slow query 1200ms\n\
         garbage line without timestamp\n\
         2026-01-05 10:01:00 ERROR db timeout\n",
    )
    .unwrap();

    let spec = ParseSpec {
        extraction: Extraction::Lines {
            pattern: r"^(?P<ts>\d{4}-\d{2}-\d{2} \d{2}:\d{2}:\d{2}) (?P<level>\w+)\s+(?P<message>.*)$".into(),
            encoding: None,
            on_no_match: NoMatchPolicy::Skip,
        },
        transforms: vec![],
        columns: vec![
            ColumnSpec {
                name: "ts".into(),
                source: None,
                dtype: DType::Timestamp { format: "%Y-%m-%d %H:%M:%S".into(), timezone: None },
                nullable: false,
                parse: ValueParsing::default(),
            },
            ColumnSpec {
                name: "level".into(),
                source: None,
                dtype: DType::Utf8,
                nullable: false,
                parse: ValueParsing::default(),
            },
            ColumnSpec {
                name: "message".into(),
                source: None,
                dtype: DType::Utf8,
                nullable: true,
                parse: ValueParsing::default(),
            },
        ],
        confidence: Some(1.0),
        notes: vec![],
    };
    // Persist as a manual sidecar, then query frozen (proves the whole
    // sidecar round trip: TOML serialize -> deserialize -> execute).
    sidecar::save(
        &path,
        &spec,
        ProvenanceInfo {
            method: InferenceMethod::Manual,
            model: None,
            prompt_version: None,
            sampled_bytes: None,
        },
    )
    .unwrap();

    let sql = format!(
        "SELECT level, count(*) AS n FROM messy('{}') GROUP BY level ORDER BY level",
        path.display()
    );
    let (_schema, batches) = provider::run_query(&sql, &no_llm_config(), true).await.unwrap();
    let batch = &batches[0];
    let level = batch.column(0).as_any().downcast_ref::<StringArray>().unwrap();
    let n = batch.column(1).as_any().downcast_ref::<Int64Array>().unwrap();
    assert_eq!(batch.num_rows(), 3); // ERROR, INFO, WARN; banner+garbage skipped
    assert_eq!(level.value(0), "ERROR");
    assert_eq!(n.value(0), 1);
}

// ---------------------------------------------------------------------------
// NDJSON sniffing.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn ndjson_sniff_and_query() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("events.ndjson");
    fs::write(
        &path,
        "{\"id\":1,\"who\":\"a\",\"score\":3.5}\n\
         {\"id\":2,\"who\":\"b\",\"score\":1.25,\"extra\":{\"k\":1}}\n\
         {\"id\":3,\"who\":\"a\",\"score\":2.0}\n",
    )
    .unwrap();

    let s = sample::build(&path, 16 * 1024).unwrap();
    let result = sniff::sniff(&path, &s, Limits::default()).unwrap();
    assert!(matches!(result.spec.extraction, Extraction::Json { lines: true, .. }));

    let sql = format!(
        "SELECT who, sum(score) AS s FROM messy('{}') GROUP BY who ORDER BY who",
        path.display()
    );
    let (_schema, batches) = provider::run_query(&sql, &no_llm_config(), false).await.unwrap();
    let batch = &batches[0];
    let s_col = batch.column(1).as_any().downcast_ref::<Float64Array>().unwrap();
    assert!((s_col.value(0) - 5.5).abs() < 1e-9);
}

// ---------------------------------------------------------------------------
// Schema generation for grammar-constrained decoding.
// ---------------------------------------------------------------------------

#[test]
fn json_schema_generates_and_names_key_fields() {
    let schema = ParseSpec::json_schema();
    let text = schema.to_string();
    for needle in ["extraction", "transforms", "columns", "promote_header", "unpivot", "decimal"] {
        assert!(text.contains(needle), "schema should mention {needle}");
    }
}
