use std::path::{Path, PathBuf};
use tdy::config::Limits;
use tdy::spec::RowWindow;

fn fixture(name: &str) -> PathBuf { Path::new(env!("CARGO_MANIFEST_DIR")).join("testdata").join(name) }
fn tdy(args: &[&str]) -> std::process::Output {
    std::process::Command::new(env!("CARGO_BIN_EXE_tdy")).args(args).env("TDY_BACKEND", "none").output().unwrap()
}

#[test]
fn stacked_blocks_are_found_at_blank_rows_in_file_order() {
    let w = tdy::engine::regions_of(&fixture("regions_three.csv"), None, Limits::default()).unwrap();
    assert_eq!(w, vec![RowWindow { start: 0, end: 4 }, RowWindow { start: 5, end: 9 }, RowWindow { start: 10, end: 14 }]);
    let w = tdy::engine::regions_of(&fixture("regions_three.xlsx"), Some("Data"), Limits::default()).unwrap();
    assert_eq!(w.len(), 3);
    assert!(tdy::engine::regions_of(&fixture("compressed_plain.csv"), None, Limits::default()).unwrap().is_empty(), "one block is no region");
    // A title block above one table: the split finds one proper block, not two.
    let w = tdy::engine::regions_of(&fixture("regions_titled.csv"), None, Limits::default()).unwrap();
    assert_eq!(w, vec![RowWindow { start: 3, end: 7 }]);
}

/// "One block spanning the whole file" means the file has exactly one
/// non-blank run at all — a leading or trailing blank line is padding, not
/// a second region, even though the sole run then no longer starts at line
/// 0 or end at the last line.
#[test]
fn leading_or_trailing_blank_lines_are_padding_not_a_region() {
    let dir = tempfile::TempDir::new().unwrap();
    for (name, content) in [("leading.csv", "\n\na\nb\nc\nd\n"), ("trailing.csv", "a\nb\nc\nd\n\n")] {
        let p = dir.path().join(name);
        std::fs::write(&p, content).unwrap();
        let w = tdy::engine::regions_of(&p, None, Limits::default()).unwrap();
        assert!(w.is_empty(), "{name}: expected no regions, got {w:?}");
    }
}

/// A lock naming three regions of one file, written by hand with
/// hand-written region sidecars, reads as three members with each block's
/// own sum — pinning `regions_three.csv`'s block-3 ground truth (900.00) by
/// a test rather than only a generator docstring.
#[test]
fn a_hand_written_lock_over_three_regions_reads_all_and_names_them() {
    use tdy::lockfile::{Lock, Member, LOCK_VERSION};
    use tdy::spec::{ColumnSpec, DType, Extraction, InferenceMethod, ParseSpec, RaggedPolicy, Transform, ValueParsing};
    let dir = tempfile::TempDir::new().unwrap();
    let f = dir.path().join("report.csv");
    std::fs::copy(fixture("regions_three.csv"), &f).unwrap();
    let t = dir.path().join("q.tdy.sql");
    std::fs::write(&t, "CREATE TABLE q (month DATE NOT NULL, region TEXT NOT NULL, amount DECIMAL(14,2) NOT NULL) WITH (files = '*.csv', date_order = 'dmy', provenance = 'true');").unwrap();
    let target = tdy::target::Target::load(&t).unwrap();
    let col = |name: &str, source: &str, dtype: DType| ColumnSpec { name: name.into(), source: Some(source.into()), dtype, nullable: false, parse: ValueParsing::default(), pointer: None };
    let spec = |w: RowWindow| ParseSpec {
        extraction: Extraction::Delimited { delimiter: ';', quote: Some('"'), escape: None, encoding: None, comment: None, ragged: RaggedPolicy::PadNulls, region: Some(w) },
        transforms: vec![Transform::PromoteHeader { rows: 1, join: " ".into() }],
        columns: vec![col("month", "Datum", DType::Date { format: "%d.%m.%Y".into() }), col("region", "Region", DType::Utf8), col("amount", "Betrag", DType::Decimal { precision: 14, scale: 2 })],
        confidence: Some(1.0), notes: vec![],
    };
    let prov = || tdy::sidecar::ProvenanceInfo { method: InferenceMethod::Manual, model: None, prompt_version: None, sampled_bytes: None };
    tdy::sidecar::save_member(&f, None, Some(1), &spec(RowWindow { start: 0, end: 4 }), prov()).unwrap();
    tdy::sidecar::save_member(&f, None, Some(2), &spec(RowWindow { start: 5, end: 9 }), prov()).unwrap();
    tdy::sidecar::save_member(&f, None, Some(3), &spec(RowWindow { start: 10, end: 14 }), prov()).unwrap();
    let (blake3, bytes) = tdy::sidecar::hash_file(&f).unwrap();
    let member = |n: u32| Member { path: "report.csv".into(), sheet: None, region: Some(n), blake3: blake3.clone(), bytes, spec_digest: tdy::lockfile::spec_digest_for(&f, None, Some(n)), review: None, accepted: false };
    Lock { lock_version: LOCK_VERSION, target: "q".into(), target_hash: tdy::lockfile::target_hash(&target), tool_version: "test".into(), created_at: "now".into(), members: vec![member(1), member(2), member(3)] }.save(&t).unwrap();
    let out = tdy(&["query", &format!("SELECT _member, sum(amount) AS total FROM dataset('{}') GROUP BY 1 ORDER BY 1", t.display())]);
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "{text}{}", String::from_utf8_lossy(&out.stderr));
    assert!(text.contains("report.csv#1") && text.contains("600.00"), "{text}");
    assert!(text.contains("report.csv#2") && text.contains("1500.00"), "{text}");
    assert!(text.contains("report.csv#3") && text.contains("900.00"), "{text}");
}
