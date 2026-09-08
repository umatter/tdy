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

fn three_pile() -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::TempDir::new().unwrap();
    std::fs::copy(fixture("regions_three.csv"), dir.path().join("report.csv")).unwrap();
    let t = dir.path().join("q.tdy.sql");
    std::fs::write(&t, "CREATE TABLE q (month DATE NOT NULL OPTIONS(matches='Datum'), region TEXT NOT NULL OPTIONS(matches='Region'), amount DECIMAL(14,2) NOT NULL OPTIONS(matches='Betrag')) WITH (files = '*.csv', date_order = 'dmy', provenance = 'true');").unwrap();
    (dir, t)
}

/// Three stacked tables become three members, every one waiting on a person,
/// accepted by name one at a time; the acceptance survives a refit.
#[test]
fn stacked_tables_become_region_members_that_wait_on_a_person() {
    let (dir, t) = three_pile();
    let out = tdy(&["fit", t.to_str().unwrap()]);
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "{text}{}", String::from_utf8_lossy(&out.stderr));
    for n in 1..=3 { assert!(text.contains(&format!("report.csv#{n}")), "{text}"); }
    // `render_pile_text` prints "REVIEW" twice per unaccepted member — once
    // as the status word, once as the "REVIEW: <reason>" detail line — the
    // same established format `tests/dataset.rs`'s single-member review
    // assertions check with `.contains`, so three members is six, not three.
    assert_eq!(text.matches("REVIEW").count(), 6, "{text}");
    assert!(text.contains("split at blank rows"), "{text}");
    let sql = format!("SELECT _member, sum(amount) AS total FROM dataset('{}') GROUP BY 1 ORDER BY 1", t.display());
    let out = tdy(&["query", &sql]);
    assert!(!out.status.success() && String::from_utf8_lossy(&out.stderr).contains("report.csv#1"));
    for n in 1..=3 {
        let out = tdy(&["fit", t.to_str().unwrap(), "--accept", &format!("report.csv#{n}")]);
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    }
    let out = tdy(&["query", &sql]);
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "{text}{}", String::from_utf8_lossy(&out.stderr));
    assert!(text.contains("600.00") && text.contains("1500.00") && text.contains("900.00"), "{text}");
    let out = tdy(&["fit", t.to_str().unwrap()]);
    assert!(out.status.success());
    let lock = std::fs::read_to_string(dir.path().join("q.tdy.lock")).unwrap();
    assert_eq!(lock.matches("accepted = true").count(), 3, "{lock}");
    assert_eq!(lock.matches("region = ").count(), 3, "{lock}");
    assert!(dir.path().join("report.csv#2.tdy.toml").exists() && !dir.path().join("report.csv.tdy.toml").exists());
}

/// A block of a different kind is a gap for the declared table; its sibling fits.
#[test]
fn a_summary_block_is_a_gap_not_merged_into_the_table_above_it() {
    let (dir, t) = three_pile();
    std::fs::copy(fixture("regions_summary.csv"), dir.path().join("report.csv")).unwrap();
    let out = tdy(&["fit", t.to_str().unwrap()]);
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(!out.status.success(), "{text}");
    assert!(text.contains("report.csv#1") && text.contains("report.csv#2"), "{text}");
    assert!(text.contains("GAP"), "{text}");
    assert!(!dir.path().join("q.tdy.lock").exists(), "no partial lock");
}

/// A title block above one table is one plain member with a note and no review.
#[test]
fn one_block_under_a_title_is_a_plain_member_without_review() {
    let (dir, t) = three_pile();
    std::fs::copy(fixture("regions_titled.csv"), dir.path().join("report.csv")).unwrap();
    let out = tdy(&["fit", t.to_str().unwrap()]);
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "{text}{}", String::from_utf8_lossy(&out.stderr));
    assert!(!text.contains("REVIEW") && !text.contains("#1"), "{text}");
    let sc = std::fs::read_to_string(dir.path().join("report.csv.tdy.toml")).unwrap();
    assert!(sc.contains("[spec.extraction.region]") || sc.contains("region = {"), "the window is in the plain sidecar: {sc}");
}

/// The sheet form: a block on a sheet of a workbook. A one-sheet workbook is
/// not a *sheet* member (discovery asks regions of the plain unit, whose
/// sheet is the workbook's only sheet), so the expected name is
/// `book.xlsx#2`, not `book.xlsx#Data#2` — only a sheet that was itself
/// expanded into a member gets `#Sheet#N`.
#[test]
fn stacked_tables_on_a_sheet_become_sheet_region_members() {
    let dir = tempfile::TempDir::new().unwrap();
    std::fs::copy(fixture("regions_three.xlsx"), dir.path().join("book.xlsx")).unwrap();
    let t = dir.path().join("q.tdy.sql");
    std::fs::write(&t, "CREATE TABLE q (month DATE NOT NULL OPTIONS(matches='Datum'), region TEXT NOT NULL OPTIONS(matches='Region'), amount DECIMAL(14,2) NOT NULL OPTIONS(matches='Betrag')) WITH (files = '*.xlsx', date_order = 'dmy');").unwrap();
    let out = tdy(&["fit", t.to_str().unwrap()]);
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "{text}{}", String::from_utf8_lossy(&out.stderr));
    assert!(text.contains("book.xlsx#2"), "{text}");
    assert!(dir.path().join("book.xlsx#2.tdy.toml").exists());
}

#[test]
fn a_region_member_can_be_excluded_by_reference() {
    let (dir, t) = three_pile();
    let ddl = std::fs::read_to_string(&t).unwrap().replace("files = '*.csv',", "files = '*.csv', exclude = 'report.csv#3',");
    std::fs::write(&t, ddl).unwrap();
    for n in 1..=2 { tdy(&["fit", t.to_str().unwrap(), "--accept", &format!("report.csv#{n}")]); }
    let lock = std::fs::read_to_string(dir.path().join("q.tdy.lock")).unwrap();
    assert!(lock.contains("region = 2") && !lock.contains("region = 3"), "{lock}");
}

/// The used range does not always start at the sheet's own A1. `regions_of`
/// returns windows relative to the used range, so turning a window into an
/// A1 address has to add the range's own `start()` back in — otherwise the
/// read still succeeds, just against the wrong sheet rows and columns
/// (blank margin, or a neighbouring block), which is exactly the silent
/// wrong-value failure this project refuses.
#[test]
fn a_sheet_region_is_addressed_from_the_used_ranges_own_origin() {
    let dir = tempfile::TempDir::new().unwrap();
    std::fs::copy(fixture("regions_three_offset.xlsx"), dir.path().join("book.xlsx")).unwrap();
    let t = dir.path().join("q.tdy.sql");
    std::fs::write(&t, "CREATE TABLE q (month DATE NOT NULL OPTIONS(matches='Datum'), region TEXT NOT NULL OPTIONS(matches='Region'), amount DECIMAL(14,2) NOT NULL OPTIONS(matches='Betrag')) WITH (files = '*.xlsx', date_order = 'dmy', provenance = 'true');").unwrap();
    let out = tdy(&["fit", t.to_str().unwrap()]);
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "{text}{}", String::from_utf8_lossy(&out.stderr));
    for n in 1..=3 { assert!(text.contains(&format!("book.xlsx#{n}")), "{text}"); }
    for n in 1..=3 {
        let out = tdy(&["fit", t.to_str().unwrap(), "--accept", &format!("book.xlsx#{n}")]);
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    }
    let sql = format!("SELECT _member, sum(amount) AS total FROM dataset('{}') GROUP BY 1 ORDER BY 1", t.display());
    let out = tdy(&["query", &sql]);
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "{text}{}", String::from_utf8_lossy(&out.stderr));
    assert!(text.contains("600.00") && text.contains("1500.00") && text.contains("900.00"), "{text}");
}

/// `regions_of` must stream a text file rather than materialise it — a
/// blank-row split runs on every plain member on every `tdy fit`, including
/// the sidecar-reuse fast path, so an O(file) read there would turn a
/// cheap freshness check into an expensive one. This proves the streaming
/// path's *correctness* on a large file (one block, no blank line, of
/// ~50 MB: still no regions) without a size assumption baked into the
/// assertion — cargo's test harness has no practical way to assert a peak-
/// RSS bound from inside the process being measured.
///
/// Ignored by default (building the fixture is real I/O on every run).
/// To confirm the *memory* claim by hand — that peak RSS stays flat and
/// does not track the file's ~50 MB — build the test binary and measure it
/// directly, not through `cargo test` (which would measure cargo's own
/// process):
///
/// ```text
/// cargo test --release --test regions --no-run
/// BIN=$(find target/release/deps -maxdepth 1 -name 'regions-*' -type f -executable | head -1)
/// /usr/bin/time -f "wall %es peak_rss %MkB" "$BIN" --ignored --exact \
///   regions_of_streams_a_large_file
/// ```
///
/// Compare the reported `peak_rss` against the same command run on a
/// `~5 MB` fixture (shrink `TARGET_BYTES` below) — a streaming split's
/// peak RSS should not move with the file size; the old whole-file
/// `read_text` path would show it growing roughly linearly.
#[test]
#[ignore]
fn regions_of_streams_a_large_file() {
    use std::io::Write;
    const TARGET_BYTES: usize = 50 * 1024 * 1024;
    let dir = tempfile::TempDir::new().unwrap();
    let p = dir.path().join("big.csv");
    let mut f = std::fs::File::create(&p).unwrap();
    writeln!(f, "a,b,c").unwrap();
    let row = "1,2,3\n";
    let rows = TARGET_BYTES / row.len();
    for _ in 0..rows {
        f.write_all(row.as_bytes()).unwrap();
    }
    drop(f);
    let w = tdy::engine::regions_of(&p, None, Limits::default()).unwrap();
    assert!(w.is_empty(), "one block, no blank line anywhere, is no region: {w:?}");
}
