//! The conformance gate, against the corpus and against the executor.
//!
//! `src/conform.rs`'s unit tests pin what each `Mismatch` means. This file
//! pins the thing the whole layer rests on:
//!
//! > `engine::schema_of(spec)` is the schema execution actually produces.
//!
//! If that ever stops holding, a spec can pass the gate and then emit
//! something else — which is worse than having no gate at all, because the
//! gate is what a user will trust. It is asserted here over every committed
//! fixture, on both executors, rather than reasoned about.

use std::path::{Path, PathBuf};

use datafusion::arrow::datatypes::{DataType as ArrowType, Field, Schema};
use tempfile::TempDir;

use tdy::config::Limits;
use tdy::conform::{compare, conforms, judge, Verdict};
use tdy::spec::*;
use tdy::target::Target;
use tdy::{engine, provider, sample, sniff, stream};

fn testdata() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("testdata")
}

/// Every data fixture in the tree, whatever its extension.
fn fixtures() -> Vec<PathBuf> {
    fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
        let Ok(rd) = std::fs::read_dir(dir) else { return };
        for e in rd.flatten() {
            let p = e.path();
            if p.is_dir() {
                if p.file_name().map(|n| n == "large" || n == "gen").unwrap_or(false) {
                    continue;
                }
                walk(&p, out);
            } else {
                let ext = p
                    .extension()
                    .map(|e| e.to_string_lossy().to_ascii_lowercase())
                    .unwrap_or_default();
                if matches!(
                    ext.as_str(),
                    "csv" | "tsv" | "txt" | "dat" | "log" | "ndjson" | "jsonl" | "json"
                        | "xlsx" | "xlsm" | "xls" | "ods"
                ) {
                    out.push(p);
                }
            }
        }
    }
    let mut v = Vec::new();
    walk(&testdata(), &mut v);
    v.sort();
    v
}

/// THE load-bearing assertion of this layer.
///
/// The gate proves a spec's shape without opening the file. That is only worth
/// anything if the shape it computes is the shape the executor emits — for
/// every fixture, and on both the streaming and materialising paths, since the
/// two build their batches through different code.
#[test]
fn the_derived_schema_is_the_schema_execution_produces() {
    let mut checked = 0usize;
    for p in fixtures() {
        let Ok(s) = sample::build(&p, 16 * 1024, Limits::default()) else { continue };
        let Ok(res) = sniff::sniff(&p, &s, Limits::default()) else { continue };
        let spec = res.spec;
        if spec.validate().is_err() {
            continue;
        }
        let Ok(derived) = engine::schema_of(&spec) else { continue };

        // The materialising path.
        if let Ok(batches) = engine::execute_batches(&spec, &p, Limits::default()) {
            assert_eq!(
                batches[0].schema().as_ref(),
                &derived,
                "{}: schema_of disagrees with the materialising executor",
                p.display()
            );
            checked += 1;
        }
        // …and the streaming one, which builds its batches separately.
        if stream::can_stream(&spec) {
            if let Ok(batches) = stream::execute_batches(&spec, &p, Limits::default()) {
                assert_eq!(
                    batches[0].schema().as_ref(),
                    &derived,
                    "{}: schema_of disagrees with the streaming executor",
                    p.display()
                );
            }
        }
    }
    assert!(checked > 40, "only {checked} fixtures were checked — the sweep proves little");
    eprintln!("schema_of verified against execution on {checked} fixtures");
}

/// A target's Arrow schema must not depend on anything a target cannot say.
/// The date format is the case that matters: it is a property of a file, it
/// differs between members, and if it reached the schema then twelve exports
/// with twelve formats could never land on one column.
#[test]
fn the_target_schema_is_invariant_to_per_file_formats() {
    let t = Target::parse(
        "CREATE TABLE s (d DATE NOT NULL, ts TIMESTAMP NOT NULL) WITH (files = 'x')",
    )
    .unwrap();
    let want = t.arrow_schema();

    for (df, tf) in [
        ("%d.%m.%Y", "%Y-%m-%d %H:%M:%S"),
        ("%Y-%m-%d", "%d/%m/%Y %H:%M"),
        ("%b %Y", "%Y-%m-%dT%H:%M:%S"),
    ] {
        let spec = spec_with(vec![
            ColumnSpec {
                name: "d".into(),
                source: None,
                dtype: DType::Date { format: df.into() },
                nullable: false,
                parse: ValueParsing::default(),
            },
            ColumnSpec {
                name: "ts".into(),
                source: None,
                dtype: DType::Timestamp { format: tf.into(), timezone: None },
                nullable: false,
                parse: ValueParsing::default(),
            },
        ]);
        assert!(
            conforms(&spec, &t).is_ok(),
            "formats ({df}, {tf}) broke conformance, but neither reaches the Arrow schema"
        );
        assert_eq!(engine::schema_of(&spec).unwrap(), want);
    }
}

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

/// The end-to-end promise: a spec that the gate accepts, executed on a real
/// file, produces exactly the declared schema — and the values are right.
#[test]
fn a_conforming_spec_really_produces_the_declared_dataset() {
    let dir = TempDir::new().unwrap();
    let p = dir.path().join("2025-01.csv");
    std::fs::write(
        &p,
        "Datum;Region;Betrag\n31.01.2025;Ost;1'234.50\n28.02.2025;West;987.25\n",
    )
    .unwrap();

    let target = Target::parse(
        "CREATE TABLE sales (
           month      DATE          NOT NULL,
           region     TEXT          NOT NULL,
           amount_chf DECIMAL(14,2) NOT NULL
         ) WITH (files = '2025-*.csv', date_order = 'dmy')",
    )
    .unwrap();

    let spec = ParseSpec {
        extraction: Extraction::Delimited {
            delimiter: ';',
            quote: Some('"'),
            escape: None,
            encoding: Some("utf-8".into()),
            comment: None,
            ragged: RaggedPolicy::PadNulls,
        },
        transforms: vec![Transform::PromoteHeader { rows: 1, join: " ".into() }],
        columns: vec![
            ColumnSpec {
                name: "month".into(),
                source: Some("Datum".into()),
                dtype: DType::Date { format: "%d.%m.%Y".into() },
                nullable: false,
                parse: ValueParsing::default(),
            },
            ColumnSpec {
                name: "region".into(),
                source: Some("Region".into()),
                dtype: DType::Utf8,
                nullable: false,
                parse: ValueParsing::default(),
            },
            ColumnSpec {
                name: "amount_chf".into(),
                source: Some("Betrag".into()),
                dtype: DType::Decimal { precision: 14, scale: 2 },
                nullable: false,
                parse: ValueParsing {
                    thousands_separator: Some('\''),
                    ..ValueParsing::default()
                },
            },
        ],
        confidence: Some(1.0),
        notes: vec![],
    };

    assert!(spec.validate().is_ok(), "{:?}", spec.validate());
    assert!(conforms(&spec, &target).is_ok(), "the gate rejected a conforming spec");

    let batch = provider::spec_to_batch(&spec, &p).expect("execute");
    assert_eq!(
        batch.schema().as_ref(),
        &target.arrow_schema(),
        "the gate and the executor disagree"
    );
    assert_eq!(batch.num_rows(), 2);
}

/// The gate's whole purpose: a spec that parses the file perfectly and reads
/// the wrong columns out of it must be refused. `check_spec` accepts this
/// today, which is why the target layer exists.
#[test]
fn a_spec_that_parses_the_file_but_reads_the_wrong_columns_is_refused() {
    let dir = TempDir::new().unwrap();
    let p = dir.path().join("x.csv");
    std::fs::write(&p, "Datum;Region;Betrag;Rabatt\n31.01.2025;Ost;1234.50;10\n").unwrap();

    let target = Target::parse(
        "CREATE TABLE sales (amount_chf DECIMAL(14,2) NOT NULL) WITH (files = 'x')",
    )
    .unwrap();

    // Reads `Rabatt` — a discount — and calls it the amount. It parses. It is
    // the wrong number, and only the declared name and type catch it.
    let wrong = ParseSpec {
        extraction: Extraction::Delimited {
            delimiter: ';',
            quote: Some('"'),
            escape: None,
            encoding: Some("utf-8".into()),
            comment: None,
            ragged: RaggedPolicy::PadNulls,
        },
        transforms: vec![Transform::PromoteHeader { rows: 1, join: " ".into() }],
        columns: vec![ColumnSpec {
            name: "rabatt".into(),
            source: Some("Rabatt".into()),
            dtype: DType::Int64,
            nullable: true,
            parse: ValueParsing::default(),
        }],
        confidence: Some(1.0),
        notes: vec![],
    };

    // It executes happily — that is the point.
    assert!(provider::spec_to_batch(&wrong, &p).is_ok());
    // And the gate refuses it.
    let errs = conforms(&wrong, &target).unwrap_err();
    assert_eq!(errs.len(), 2, "{errs:?}");
    let text: String = errs.iter().map(|m| m.message()).collect::<Vec<_>>().join("\n");
    assert!(text.contains("amount_chf"), "{text}");
    assert!(text.contains("rabatt"), "{text}");
}

/// A sniffed sidecar differs from a hand-written target in ways that say
/// nothing about the file — `sniff` hardcodes nullable and gives money
/// decimal(38, s). Reporting that as a contradiction on day one would be
/// noise, so the verdict separates "never fitted" from "contradicts".
#[test]
fn a_sniffed_sidecar_is_unfitted_rather_than_contradicting() {
    let dir = TempDir::new().unwrap();
    let p = dir.path().join("s.csv");
    std::fs::write(&p, "Datum;Region;Betrag\n31.01.2025;Ost;1234.50\n").unwrap();

    let s = sample::build(&p, 16 * 1024, Limits::default()).unwrap();
    let spec = sniff::sniff(&p, &s, Limits::default()).unwrap().spec;

    let target = Target::parse(
        "CREATE TABLE sales (
           month      DATE          NOT NULL,
           region     TEXT          NOT NULL,
           amount_chf DECIMAL(14,2) NOT NULL
         ) WITH (files = 's.csv')",
    )
    .unwrap();

    match judge(&spec, &target, false) {
        Verdict::Unfitted(m) => {
            assert!(!m.is_empty());
            // The sniffer names columns after the file, so nothing binds yet.
            assert!(m.iter().any(|x| x.message().contains("month")));
        }
        other => panic!("expected Unfitted, got {other:?}"),
    }
}

/// Order is part of the contract because `SELECT *` and a Parquet write both
/// depend on it — but a *missing* column must not also report every column it
/// displaces, or one fix looks like several problems.
#[test]
fn a_missing_column_does_not_cascade_into_order_complaints() {
    let want = Schema::new(vec![
        Field::new("a", ArrowType::Utf8, true),
        Field::new("b", ArrowType::Utf8, true),
        Field::new("c", ArrowType::Utf8, true),
        Field::new("d", ArrowType::Utf8, true),
    ]);
    let got = Schema::new(vec![
        Field::new("a", ArrowType::Utf8, true),
        Field::new("c", ArrowType::Utf8, true),
        Field::new("d", ArrowType::Utf8, true),
    ]);
    let errs = compare(&got, &want).unwrap_err();
    assert_eq!(errs.len(), 1, "one missing column, one message: {errs:?}");

    // With the same columns present, a genuine reordering is reported.
    let swapped = Schema::new(vec![
        Field::new("b", ArrowType::Utf8, true),
        Field::new("a", ArrowType::Utf8, true),
        Field::new("c", ArrowType::Utf8, true),
        Field::new("d", ArrowType::Utf8, true),
    ]);
    let errs = compare(&swapped, &want).unwrap_err();
    assert_eq!(errs.len(), 2, "{errs:?}");
}

/// `tdy check` is a CI gate, so it has to exit non-zero when it found a
/// problem. A gate that exits 0 on failure is decoration.
#[test]
fn the_check_command_exits_non_zero_when_a_file_does_not_conform() {
    let dir = TempDir::new().unwrap();
    let p = dir.path().join("s.csv");
    std::fs::write(&p, "Datum;Region;Betrag\n31.01.2025;Ost;1234.50\n").unwrap();
    let target = dir.path().join("sales.tdy.sql");
    std::fs::write(
        &target,
        "CREATE TABLE sales (month DATE NOT NULL) WITH (files = 's.csv');",
    )
    .unwrap();

    let sniffed = std::process::Command::new(env!("CARGO_BIN_EXE_tdy"))
        .args(["sniff", p.to_str().unwrap(), "--no-llm"])
        .output()
        .expect("run tdy");
    assert!(sniffed.status.success(), "{}", String::from_utf8_lossy(&sniffed.stderr));

    let out = std::process::Command::new(env!("CARGO_BIN_EXE_tdy"))
        .args(["check", target.to_str().unwrap(), "--against", p.to_str().unwrap()])
        .output()
        .expect("run tdy");
    assert!(!out.status.success(), "check passed a non-conforming sidecar");
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("UNFITTED"), "{text}");
    assert!(text.contains("month"), "{text}");
}

/// A target with no `--against` has nothing to check yet — it must say so
/// rather than exiting zero as if it had verified something.
#[test]
fn checking_a_target_with_nothing_to_check_says_so() {
    let dir = TempDir::new().unwrap();
    let target = dir.path().join("t.tdy.sql");
    std::fs::write(&target, "CREATE TABLE s (a TEXT) WITH (files = 'x/*.csv');").unwrap();

    let out = std::process::Command::new(env!("CARGO_BIN_EXE_tdy"))
        .args(["check", target.to_str().unwrap()])
        .output()
        .expect("run tdy");
    assert!(out.status.success());
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("nothing to check"), "{text}");
    assert!(text.contains("x/*.csv"), "the declared sources are not shown: {text}");
}

/// A target file that is not valid SQL, or not a target, fails loudly at the
/// place the user can fix it.
#[test]
fn a_broken_target_file_fails_with_a_readable_message() {
    let dir = TempDir::new().unwrap();
    for (name, body, needle) in [
        ("bad.tdy.sql", "CREATE TABL sales (a TEXT);", "not valid SQL"),
        ("empty.tdy.sql", "SELECT 1;", "must be a CREATE TABLE"),
        ("nofiles.tdy.sql", "CREATE TABLE s (a TEXT);", "files"),
    ] {
        let target = dir.path().join(name);
        std::fs::write(&target, body).unwrap();
        let out = std::process::Command::new(env!("CARGO_BIN_EXE_tdy"))
            .args(["check", target.to_str().unwrap()])
            .output()
            .expect("run tdy");
        assert!(!out.status.success(), "{name} was accepted");
        let err = String::from_utf8_lossy(&out.stderr);
        assert!(err.contains(needle), "{name}: expected {needle:?} in:\n{err}");
    }
}

/// The gate must go RED on drift, and this is the case that matters most: the
/// file's headers changed completely, so the blessed spec is one no query will
/// ever use — a non-frozen query re-sniffs and overwrites it, a frozen one
/// refuses outright. Reporting CONFORMS and exiting 0 here means going green
/// on exactly the change the gate exists to catch.
#[test]
fn a_stale_sidecar_fails_the_gate_rather_than_conforming() {
    let dir = TempDir::new().unwrap();
    let p = dir.path().join("s.csv");
    std::fs::write(&p, "month;region;amount\n31.01.2025;Ost;1234\n").unwrap();
    let target = dir.path().join("t.tdy.sql");
    std::fs::write(
        &target,
        "CREATE TABLE sales (month DATE NULL, region TEXT NULL, amount BIGINT NULL) \
         WITH (files = 's.csv');",
    )
    .unwrap();

    let run = |what: &str| {
        std::process::Command::new(env!("CARGO_BIN_EXE_tdy"))
            .args([what, p.to_str().unwrap()])
            .args(if what == "sniff" { vec!["--no-llm"] } else { vec![] })
            .output()
            .expect("run tdy")
    };
    assert!(run("sniff").status.success());

    let check = || {
        std::process::Command::new(env!("CARGO_BIN_EXE_tdy"))
            .args(["check", target.to_str().unwrap(), "--against", p.to_str().unwrap()])
            .output()
            .expect("run tdy")
    };

    // Fresh: this is the success path, which had no test at all.
    let ok = check();
    let text = String::from_utf8_lossy(&ok.stdout);
    assert!(ok.status.success(), "a conforming sidecar failed the gate:\n{text}");
    assert!(text.contains("CONFORMS"), "{text}");
    assert!(text.contains("1 of 1"), "{text}");

    // Now the export drifts: every header is renamed.
    std::fs::write(&p, "datum;gebiet;betrag\n31.01.2025;Ost;1234\n").unwrap();
    let drifted = check();
    let text = String::from_utf8_lossy(&drifted.stdout);
    assert!(
        !drifted.status.success(),
        "the gate went green on a stale sidecar:\n{text}"
    );
    assert!(text.contains("STALE"), "{text}");
    assert!(!text.contains("CONFORMS"), "a stale sidecar was still called conforming:\n{text}");
}

/// When a spec's own types cannot be built there is no schema to compare, and
/// saying "the target declares it" about columns the target never mentioned —
/// while discarding the only sentence that said what was wrong — is worse than
/// saying nothing.
#[test]
fn an_underivable_spec_reports_the_real_reason_not_a_fabricated_comparison() {
    let target = Target::parse(
        "CREATE TABLE sales (amount_chf DECIMAL(14,2) NOT NULL) WITH (files = 'x')",
    )
    .unwrap();

    // Decimal128 tops out at precision 38; 39 cannot be built.
    let spec = spec_with(vec![ColumnSpec {
        name: "amount_chf".into(),
        source: None,
        dtype: DType::Decimal { precision: 39, scale: 2 },
        nullable: false,
        parse: ValueParsing::default(),
    }]);

    let errs = conforms(&spec, &target).unwrap_err();
    assert_eq!(errs.len(), 1, "{errs:?}");
    let m = errs[0].message();
    assert!(m.contains("amount_chf"), "{m}");
    assert!(
        !m.contains("the target declares it"),
        "an underivable spec was reported as a missing target column: {m}"
    );
    assert!(
        m.contains("cannot be built"),
        "the real reason was discarded: {m}"
    );
}

/// A target that declares a zoned timestamp must be able to name the offset,
/// and a spec carrying that offset must conform to it. Before, no target could
/// express `Timestamp(_, Some(_))` at all, so such a spec could never conform
/// to anything — and the error pointed at the sidecar, which guaranteed it.
#[test]
fn a_zoned_timestamp_can_be_declared_and_conformed_to() {
    let target = Target::parse(
        "CREATE TABLE s (ts TIMESTAMP WITH TIME ZONE NOT NULL) \
         WITH (files = 'x', timezone = '+02:00')",
    )
    .unwrap();

    let spec = spec_with(vec![ColumnSpec {
        name: "ts".into(),
        source: None,
        dtype: DType::Timestamp {
            format: "%Y-%m-%d %H:%M:%S".into(),
            timezone: Some("+02:00".into()),
        },
        nullable: false,
        parse: ValueParsing::default(),
    }]);

    assert!(
        conforms(&spec, &target).is_ok(),
        "a spec declaring the same offset as the target did not conform: {:?}",
        conforms(&spec, &target)
    );

    // And a different offset is a real mismatch, not silently equal.
    let mut other = spec.clone();
    other.columns[0].dtype = DType::Timestamp {
        format: "%Y-%m-%d %H:%M:%S".into(),
        timezone: Some("+01:00".into()),
    };
    assert!(conforms(&other, &target).is_err(), "two different offsets conformed");
}
