//! temporary review probe — delete
use datafusion::arrow::datatypes::{DataType as ArrowType, Field, Schema};
use tdy::conform::{compare, conforms};
use tdy::spec::*;
use tdy::target::Target;
use tempfile::TempDir;

fn spec_with(columns: Vec<ColumnSpec>) -> ParseSpec {
    ParseSpec {
        extraction: Extraction::Delimited {
            delimiter: ',',
            quote: Some('"'),
            escape: None,
            encoding: None,
            comment: None,
            ragged: RaggedPolicy::PadNulls,
        },
        transforms: vec![Transform::PromoteHeader { rows: 1, join: " ".into() }],
        columns,
        confidence: Some(1.0),
        notes: vec![],
    }
}

#[test]
fn probe_timezone_cannot_be_declared() {
    for sql in [
        "CREATE TABLE s (ts TIMESTAMP WITH TIME ZONE) WITH (files='x')",
        "CREATE TABLE s (ts TIMESTAMPTZ) WITH (files='x')",
        "CREATE TABLE s (ts TIMESTAMP(6) WITH TIME ZONE) WITH (files='x')",
    ] {
        match Target::parse(sql) {
            Ok(t) => eprintln!("PARSED {sql} -> {:?}", t.columns[0].dtype),
            Err(e) => eprintln!("REFUSED {sql} -> {e:#}"),
        }
    }

    // a spec that carries an offset can never conform
    let t = Target::parse("CREATE TABLE s (ts TIMESTAMP NOT NULL) WITH (files='x')").unwrap();
    let spec = spec_with(vec![ColumnSpec {
        name: "ts".into(),
        source: None,
        dtype: DType::Timestamp { format: "%Y-%m-%d %H:%M:%S".into(), timezone: Some("+02:00".into()) },
        nullable: false,
        parse: ValueParsing::default(),
    }]);
    let errs = conforms(&spec, &t).unwrap_err();
    eprintln!("offset spec vs TIMESTAMP target: {}", errs[0].message());
}

#[test]
fn probe_timestamp_frame_of_reference() {
    let dir = TempDir::new().unwrap();
    let t = Target::parse("CREATE TABLE s (ts TIMESTAMP NOT NULL) WITH (files='x')").unwrap();

    // member A: naive local wall clock
    let a = dir.path().join("a.csv");
    std::fs::write(&a, "ts\n2025-01-31 10:00:00\n").unwrap();
    let spec_a = spec_with(vec![ColumnSpec {
        name: "ts".into(),
        source: None,
        dtype: DType::Timestamp { format: "%Y-%m-%d %H:%M:%S".into(), timezone: None },
        nullable: false,
        parse: ValueParsing::default(),
    }]);

    // member B: same wall clock, written with its offset
    let b = dir.path().join("b.csv");
    std::fs::write(&b, "ts\n2025-01-31 10:00:00+0200\n").unwrap();
    let spec_b = spec_with(vec![ColumnSpec {
        name: "ts".into(),
        source: None,
        dtype: DType::Timestamp { format: "%Y-%m-%d %H:%M:%S%z".into(), timezone: None },
        nullable: false,
        parse: ValueParsing::default(),
    }]);

    assert!(conforms(&spec_a, &t).is_ok(), "A does not conform");
    assert!(conforms(&spec_b, &t).is_ok(), "B does not conform");

    let ba = tdy::provider::spec_to_batch(&spec_a, &a).unwrap();
    let bb = tdy::provider::spec_to_batch(&spec_b, &b).unwrap();
    eprintln!("A schema {:?}", ba.schema());
    eprintln!("B schema {:?}", bb.schema());
    eprintln!("A values {:?}", ba.column(0));
    eprintln!("B values {:?}", bb.column(0));
}

#[test]
fn probe_duplicate_produced_names() {
    let want = Schema::new(vec![
        Field::new("a", ArrowType::Utf8, true),
        Field::new("b", ArrowType::Utf8, true),
    ]);
    let got = Schema::new(vec![
        Field::new("a", ArrowType::Utf8, true),
        Field::new("b", ArrowType::Utf8, true),
        Field::new("b", ArrowType::Int64, true),
    ]);
    eprintln!("dup-name compare: {:?}", compare(&got, &want));
}

#[test]
fn probe_norm_pairs() {
    for (x, y) in [
        ("Umsatz %", "Umsatz"),
        ("Umsatz%", "Umsatz %"),
        ("Betrag (CHF)", "Betrag CHF"),
        ("Betrag Rp.", "Betrag Rp"),
        ("amount_chf", "amount chf"),
        ("Q1", "Q 1"),
        ("Umsatz netto", "Umsatz-netto"),
        ("Betrag EUR", "Betrag CHF"),
    ] {
        eprintln!("{x:?} -> {:?} | {y:?} -> {:?}", tdy::target::norm(x), tdy::target::norm(y));
    }
}

#[test]
fn probe_stale_sidecar_check_exit_code() {
    let dir = TempDir::new().unwrap();
    let p = dir.path().join("s.csv");
    std::fs::write(&p, "month,region,amount_chf\n31.01.2025,Ost,1234.50\n").unwrap();

    // a hand-fitted sidecar that conforms
    let spec = spec_with(vec![
        ColumnSpec { name: "month".into(), source: None, dtype: DType::Date { format: "%d.%m.%Y".into() }, nullable: false, parse: ValueParsing::default() },
        ColumnSpec { name: "region".into(), source: None, dtype: DType::Utf8, nullable: false, parse: ValueParsing::default() },
        ColumnSpec { name: "amount_chf".into(), source: None, dtype: DType::Decimal { precision: 14, scale: 2 }, nullable: false, parse: ValueParsing::default() },
    ]);
    tdy::sidecar::save(
        &p,
        &spec,
        tdy::sidecar::ProvenanceInfo {
            method: tdy::spec::InferenceMethod::Manual,
            model: None,
            prompt_version: None,
            sampled_bytes: None,
        },
    )
    .unwrap();

    // now the file changes: the amount column is renamed and its unit changes
    std::fs::write(&p, "month,region,Betrag Rp\n31.01.2025,Ost,123450\n").unwrap();

    let target = dir.path().join("t.tdy.sql");
    std::fs::write(
        &target,
        "CREATE TABLE sales (month DATE NOT NULL, region TEXT NOT NULL, amount_chf DECIMAL(14,2) NOT NULL) WITH (files='s.csv');",
    )
    .unwrap();

    let out = std::process::Command::new(env!("CARGO_BIN_EXE_tdy"))
        .args(["check", target.to_str().unwrap(), "--against", p.to_str().unwrap()])
        .output()
        .unwrap();
    eprintln!("exit = {:?}", out.status.code());
    eprintln!("{}", String::from_utf8_lossy(&out.stdout));
    eprintln!("{}", String::from_utf8_lossy(&out.stderr));
}

#[test]
fn probe_verify_option_ignored() {
    let t = Target::parse("CREATE TABLE s (a TEXT) WITH (files='x', verify='full')").unwrap();
    eprintln!("verify = {:?}", t.verify);
}
