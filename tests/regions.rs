use datafusion::arrow::array::{Array, StringArray};
use std::path::{Path, PathBuf};
use tdy::config::Limits;
use tdy::spec::RowWindow;

fn fixture(name: &str) -> PathBuf { Path::new(env!("CARGO_MANIFEST_DIR")).join("testdata").join(name) }
fn tdy_in(dir: &Path, args: &[&str]) -> std::process::Output {
    std::process::Command::new(env!("CARGO_BIN_EXE_tdy")).args(args).current_dir(dir).env("TDY_BACKEND", "none").output().unwrap()
}
fn tdy(args: &[&str]) -> std::process::Output {
    std::process::Command::new(env!("CARGO_BIN_EXE_tdy")).args(args).env("TDY_BACKEND", "none").output().unwrap()
}

#[test]
fn stacked_blocks_are_found_at_blank_rows_in_file_order() {
    let w = tdy::engine::regions_of(&fixture("regions_three.csv"), None, Limits::default()).unwrap();
    assert_eq!(w.windows, [RowWindow { start: 0, end: 4, ordinal: 1 }, RowWindow { start: 5, end: 9, ordinal: 2 }, RowWindow { start: 10, end: 14, ordinal: 3 }]);
    assert!(w.dropped.is_empty(), "nothing was discarded: {w:?}");
    let w = tdy::engine::regions_of(&fixture("regions_three.xlsx"), Some("Data"), Limits::default()).unwrap();
    assert_eq!(w.windows.len(), 3);
    assert!(tdy::engine::regions_of(&fixture("compressed_plain.csv"), None, Limits::default()).unwrap().windows.is_empty(), "one block is no region");
    // A title block above one table: the split finds one proper block, not two,
    // and says the banner is two lines nothing reads.
    let w = tdy::engine::regions_of(&fixture("regions_titled.csv"), None, Limits::default()).unwrap();
    assert_eq!(w.windows, [RowWindow { start: 3, end: 7, ordinal: 1 }]);
    assert_eq!(w.dropped, [tdy::engine::DroppedRun { start: 0, end: 2, width: 1, widest: 1 }]);
    assert_eq!(w.block_width, 3);
    assert_eq!(w.table_shaped().count(), 0, "a 1-field banner is not a table");
    // The same file with a 3-field run below the minimum: the run is dropped
    // and it IS table-shaped, so a person has to rule on it.
    let w = tdy::engine::regions_of(&fixture("regions_short_block.csv"), None, Limits::default()).unwrap();
    assert_eq!(w.windows, [RowWindow { start: 3, end: 7, ordinal: 1 }]);
    assert_eq!(w.dropped, [tdy::engine::DroppedRun { start: 0, end: 2, width: 3, widest: 3 }]);
    assert_eq!(w.table_shaped().count(), 1);
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
        assert!(w.windows.is_empty(), "{name}: expected no regions, got {w:?}");
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
    tdy::sidecar::save_member(&f, None, Some(1), &spec(RowWindow { start: 0, end: 4, ordinal: 1 }), prov()).unwrap();
    tdy::sidecar::save_member(&f, None, Some(2), &spec(RowWindow { start: 5, end: 9, ordinal: 2 }), prov()).unwrap();
    tdy::sidecar::save_member(&f, None, Some(3), &spec(RowWindow { start: 10, end: 14, ordinal: 3 }), prov()).unwrap();
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

/// A block of a different kind is never merged into the table above it,
/// and never dropped in silence either. It fails the gates, so it is not a
/// member: the table is the one block that passes — a plain member reading
/// only its own rows. But the recap has two fields on a row, which is data,
/// not a banner, so whether those lines belong to the dataset is a person's
/// call: the member waits, `dataset()` refuses it, and after `--accept` it
/// reads the table's 600.00 alone.
#[test]
fn a_summary_block_is_not_merged_and_waits_on_a_person() {
    let (dir, t) = three_pile();
    std::fs::copy(fixture("regions_summary.csv"), dir.path().join("report.csv")).unwrap();
    let out = tdy(&["fit", t.to_str().unwrap()]);
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "{text}{}", String::from_utf8_lossy(&out.stderr));
    assert!(!text.contains("report.csv#"), "the recap is not a member: {text}");
    assert!(text.contains("REVIEW: a run of 4 line(s) at lines 6–9 was not read"), "{text}");
    let sql = format!("SELECT sum(amount) AS total FROM dataset('{}')", t.display());
    let out = tdy(&["query", &sql]);
    assert!(!out.status.success(), "unaccepted: {}", String::from_utf8_lossy(&out.stdout));
    assert!(String::from_utf8_lossy(&out.stderr).contains("report.csv"), "{}", String::from_utf8_lossy(&out.stderr));
    let out = tdy(&["fit", t.to_str().unwrap(), "--accept", "report.csv"]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let q = query(&t, "SELECT count(*) AS n, sum(amount) AS total FROM DS");
    assert!(q.contains("| 3 | 600.00 |"), "block 1 alone: {q}");
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

/// A `source_name` column reading `from = "region"` holds the block's own
/// 1-based ordinal, in every row — the same fact `#N` names the member by,
/// now readable as data rather than only as a member name.
#[test]
fn source_name_from_region_puts_the_ordinal_in_a_column() {
    use tdy::spec::{ColumnSpec, DType, SourcePart, Transform, ValueParsing};
    let (dir, t) = three_pile();
    for n in 1..=3 {
        let out = tdy(&["fit", t.to_str().unwrap(), "--accept", &format!("report.csv#{n}")]);
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    }
    let f = dir.path().join("report.csv");
    let mut spec = match tdy::sidecar::load_member(&f, None, Some(2)).unwrap() {
        tdy::sidecar::SidecarStatus::Fresh(sc) => sc.spec,
        other => panic!("expected a fresh region-2 sidecar: {other:?}"),
    };
    spec.transforms.push(Transform::SourceName {
        name: "block".into(),
        from: SourcePart::Region,
        pattern: None,
    });
    spec.columns.push(ColumnSpec {
        name: "block".into(),
        source: None,
        dtype: DType::Utf8,
        nullable: false,
        parse: ValueParsing::default(),
        pointer: None,
    });
    let batches = tdy::engine::execute_batches(&spec, &f, Limits::default()).unwrap();
    let mut rows_seen = 0usize;
    for b in &batches {
        let idx = b.schema().index_of("block").unwrap();
        let col = b.column(idx).as_any().downcast_ref::<StringArray>().unwrap();
        for i in 0..col.len() {
            assert_eq!(col.value(i), "2", "every row of region 2 must read \"2\"");
            rows_seen += 1;
        }
    }
    assert!(rows_seen > 0, "expected at least one row");
}

/// The same transform on a plain (whole-file) spec has no region to read —
/// this must fail loudly naming `from = "region"`, not silently emit an
/// empty or wrong column.
#[test]
fn source_name_from_region_fails_without_a_region() {
    use tdy::spec::{ColumnSpec, DType, Extraction, RaggedPolicy, SourcePart, Transform, ValueParsing};
    let dir = tempfile::TempDir::new().unwrap();
    let f = dir.path().join("plain.csv");
    std::fs::write(&f, "a\n1\n2\n").unwrap();
    let spec = tdy::spec::ParseSpec {
        extraction: Extraction::Delimited {
            delimiter: ',',
            quote: None,
            escape: None,
            encoding: None,
            comment: None,
            ragged: RaggedPolicy::PadNulls,
            region: None,
        },
        transforms: vec![
            Transform::PromoteHeader { rows: 1, join: " ".into() },
            Transform::SourceName { name: "block".into(), from: SourcePart::Region, pattern: None },
        ],
        columns: vec![
            ColumnSpec { name: "a".into(), source: None, dtype: DType::Utf8, nullable: true, parse: ValueParsing::default(), pointer: None },
            ColumnSpec { name: "block".into(), source: None, dtype: DType::Utf8, nullable: false, parse: ValueParsing::default(), pointer: None },
        ],
        confidence: Some(1.0),
        notes: vec![],
    };
    let err = format!("{:#}", tdy::engine::execute_batches(&spec, &f, Limits::default()).unwrap_err());
    assert!(err.contains("from = \"region\""), "{err}");
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
    assert!(w.windows.is_empty(), "one block, no blank line anywhere, is no region: {w:?}");
}

/// A run the split discarded is data nothing reads. `regions_short_block.csv`
/// holds 1600.00 across two runs; the 2-line one is below the 3-row minimum,
/// so the window covers only the 1500.00 block. Reading that window and
/// reporting `fits` would answer 1500.00 for a file that holds 1600.00 —
/// quieter than the loud refusal the same file gets with no window at all.
/// So: the dropped run is named in the member's note and printed by the CLI,
/// and — because its first line is 3 fields wide exactly like the kept
/// block's — it waits on a person.
#[test]
fn a_dropped_run_that_looks_like_a_table_is_named_and_waits_on_a_person() {
    let (dir, t) = three_pile();
    std::fs::copy(fixture("regions_short_block.csv"), dir.path().join("report.csv")).unwrap();
    let out = tdy(&["fit", t.to_str().unwrap()]);
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "{text}{}", String::from_utf8_lossy(&out.stderr));
    assert!(text.contains("REVIEW"), "a discarded table-shaped run waits on a person: {text}");
    assert!(text.contains("lines 1–2"), "the note names the lines nothing read: {text}");
    assert!(text.contains("not read"), "{text}");

    let sql = format!("SELECT sum(amount) AS total FROM dataset('{}')", t.display());
    let out = tdy(&["query", &sql]);
    assert!(!out.status.success(), "unaccepted: {}", String::from_utf8_lossy(&out.stdout));
    assert!(String::from_utf8_lossy(&out.stderr).contains("report.csv"), "{}", String::from_utf8_lossy(&out.stderr));

    let out = tdy(&["fit", t.to_str().unwrap(), "--accept", "report.csv"]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let out = tdy(&["query", &sql]);
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "{text}{}", String::from_utf8_lossy(&out.stderr));
    assert!(text.contains("1500.00") && !text.contains("1600.00"), "the kept block's sum only: {text}");
}

/// A title banner is not table-shaped: one field where the block's header
/// has three. `regions_titled.csv` stays a plain, unreviewed member, and
/// the banner is still named as lines nothing read.
#[test]
fn a_dropped_run_that_is_not_table_shaped_asks_nothing() {
    let (dir, t) = three_pile();
    std::fs::copy(fixture("regions_titled.csv"), dir.path().join("report.csv")).unwrap();
    let out = tdy(&["fit", t.to_str().unwrap()]);
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "{text}{}", String::from_utf8_lossy(&out.stderr));
    assert!(!text.contains("REVIEW"), "a 1-field banner is not a table: {text}");
    assert!(text.contains("lines 1–2") && text.contains("not read"), "{text}");
    let _ = dir;
}

/// Fit the three-block pile and accept every member, so the lock is written
/// and the dataset queries. Returns the directory and the target.
fn three_pile_accepted() -> (tempfile::TempDir, PathBuf) {
    let (dir, t) = three_pile();
    for n in 1..=3 {
        let out = tdy(&["fit", t.to_str().unwrap(), "--accept", &format!("report.csv#{n}")]);
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    }
    (dir, t)
}

/// Rewrite one region sidecar's window in place, leaving everything else —
/// including `source.region` — exactly as `tdy fit` wrote it.
fn edit_window(dir: &Path, member: &str, start: u64, end: u64, ordinal: u32) {
    let p = dir.join(format!("{member}.tdy.toml"));
    let text = std::fs::read_to_string(&p).unwrap();
    let (before, rest) = text.split_once("[spec.extraction.region]").expect("a region window");
    let tail = rest.splitn(4, '\n').nth(4).unwrap_or("");
    let rest_after = rest
        .match_indices('\n')
        .nth(3)
        .map(|(i, _)| &rest[i + 1..])
        .unwrap_or(tail);
    std::fs::write(
        &p,
        format!("{before}[spec.extraction.region]\nstart = {start}\nend = {end}\nordinal = {ordinal}\n{rest_after}"),
    )
    .unwrap();
}

/// C2, part one: the ordinal a region sidecar's own window carries has to
/// agree with the region it is the sidecar *for*. `report.csv#2.tdy.toml`
/// whose window says ordinal 1 is a spec for some other block filed under
/// this member's name, and reading it would total block 1 twice.
#[test]
fn a_region_sidecar_whose_window_names_another_ordinal_is_refused() {
    let (dir, t) = three_pile_accepted();
    edit_window(dir.path(), "report.csv#2", 0, 4, 1);
    let f = dir.path().join("report.csv");
    let err = tdy::sidecar::load_member(&f, None, Some(2)).expect_err("the ordinals disagree");
    let msg = format!("{err:#}");
    assert!(msg.contains("report.csv#2"), "{msg}");
    assert!(msg.contains("ordinal"), "{msg}");

    let sql = format!("SELECT sum(amount) AS total FROM dataset('{}')", t.display());
    let out = tdy(&["query", &sql]);
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(!text.contains("2100.00"), "block 1 must never be read twice: {text}");
    assert!(!out.status.success(), "{text}");
    assert!(String::from_utf8_lossy(&out.stderr).contains("report.csv#2"), "{}", String::from_utf8_lossy(&out.stderr));
}

/// C2, part two: a hand-edited window that still names its own ordinal
/// passes every check the sidecar can make on its own — only the split
/// knows the block really starts at line 5. `tdy fit` must not reuse a spec
/// that reads a different block from the one this member is, or three
/// members read two blocks and the pile totals 2100.00 where the file
/// holds 3000.00.
#[test]
fn a_region_sidecar_whose_window_is_not_the_blocks_is_a_contradiction() {
    let (dir, t) = three_pile_accepted();
    edit_window(dir.path(), "report.csv#2", 0, 4, 2);
    // A person's spec says so: `method = "manual"`. A tool-written one whose
    // window disagrees is re-planned instead (see the renumbering tests).
    let sc = dir.path().join("report.csv#2.tdy.toml");
    let text = std::fs::read_to_string(&sc).unwrap().replace("method = \"heuristic\"", "method = \"manual\"");
    std::fs::write(&sc, text).unwrap();
    let out = tdy(&["fit", t.to_str().unwrap()]);
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(!out.status.success(), "{text}");
    assert!(text.contains("report.csv#2") && text.contains("CONTRADICTS"), "{text}");
    assert!(text.contains("lines 6–9"), "the block's own lines are named, 1-based: {text}");

    let sql = format!("SELECT sum(amount) AS total FROM dataset('{}')", t.display());
    let out = tdy(&["query", &sql]);
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(!text.contains("2100.00"), "block 1 must never be read twice: {text}");
    assert!(!out.status.success(), "{text}");
}

/// The single-file tools take a member reference and find its sidecar
/// through `sidecar::resolve_ref`. `report.csv#2.tdy.toml` is the sidecar
/// path of the sheet reading *and* of the region reading, so a fallback
/// that only asked whether the file existed saw two candidates and refused
/// every region member by name. A sidecar declares which member it is
/// about; one file cannot declare two.
#[test]
fn validate_and_check_can_name_a_region_member() {
    let (dir, t) = three_pile_accepted();
    let cwd = dir.path();
    let run = |args: &[&str]| {
        std::process::Command::new(env!("CARGO_BIN_EXE_tdy"))
            .args(args)
            .current_dir(cwd)
            .env("TDY_BACKEND", "none")
            .output()
            .unwrap()
    };
    let out = run(&["validate", "report.csv#2"]);
    let text = format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    assert!(!text.contains("could mean"), "one member, one reading: {text}");
    assert!(out.status.success(), "{text}");
    assert!(text.contains("region 2") || text.contains("#2"), "{text}");

    let out = run(&["check", t.to_str().unwrap(), "--against", "report.csv#2"]);
    let text = format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    assert!(!text.contains("could mean"), "one member, one reading: {text}");
    assert!(out.status.success(), "{text}");
}

/// CRLF line endings: `\r` is ASCII whitespace, so a `\r\n`-only line is
/// blank to the splitter and a `\r\n` terminator is one line, exactly as a
/// bare `\n` is. Two stacked blocks land on {0,4} and {5,9}, and both
/// executors read each block's own sum from them — this pins the behaviour
/// rather than leaving it to the reader of `is_ascii_whitespace`.
#[test]
fn a_crlf_file_splits_on_the_same_windows_and_both_executors_agree() {
    use tdy::spec::{ColumnSpec, DType, Extraction, ParseSpec, RaggedPolicy, Transform, ValueParsing};
    let dir = tempfile::TempDir::new().unwrap();
    let p = dir.path().join("crlf.csv");
    let body = [
        "Datum;Region;Betrag",
        "05.01.2025;Ost;190.00",
        "12.01.2025;West;200.00",
        "19.01.2025;Nord;210.00",
        "",
        "Datum;Region;Betrag",
        "05.02.2025;Ost;490.00",
        "12.02.2025;West;500.00",
        "19.02.2025;Nord;510.00",
    ]
    .join("\r\n")
        + "\r\n";
    std::fs::write(&p, &body).unwrap();
    let r = tdy::engine::regions_of(&p, None, Limits::default()).unwrap();
    assert_eq!(
        r.windows,
        [RowWindow { start: 0, end: 4, ordinal: 1 }, RowWindow { start: 5, end: 9, ordinal: 2 }]
    );
    assert!(r.dropped.is_empty(), "{r:?}");

    for (w, want) in [(r.windows[0], "190.00"), (r.windows[1], "490.00")] {
        let spec = ParseSpec {
            extraction: Extraction::Delimited {
                delimiter: ';', quote: Some('"'), escape: None, encoding: None, comment: None,
                ragged: RaggedPolicy::PadNulls, region: Some(w),
            },
            transforms: vec![Transform::PromoteHeader { rows: 1, join: " ".into() }],
            columns: vec![ColumnSpec {
                name: "Betrag".into(), source: None, dtype: DType::Decimal { precision: 14, scale: 2 },
                nullable: false, parse: ValueParsing::default(), pointer: None,
            }],
            confidence: Some(1.0),
            notes: vec![],
        };
        let render = |bs: &[datafusion::arrow::array::RecordBatch]| {
            datafusion::arrow::util::pretty::pretty_format_batches(bs).unwrap().to_string()
        };
        let a = render(&tdy::engine::execute_batches(&spec, &p, Limits::default()).unwrap());
        let b = render(&tdy::stream::execute_batches(&spec, &p, Limits::default()).unwrap());
        assert_eq!(a, b, "the executors must agree on block {}", w.ordinal);
        assert!(a.contains(want), "block {}: {a}", w.ordinal);
    }
}

/// `tdy fit TARGET FILE` reads the file as one table — the single-file
/// question has no answer for a file holding three. Its refusal says so and
/// names the command that does split it, instead of leaving a header read
/// as data looking like a type problem.
#[test]
fn a_single_file_fit_of_a_stacked_file_names_the_split_it_did_not_do() {
    let (dir, t) = three_pile();
    let out = tdy(&["fit", t.to_str().unwrap(), dir.path().join("report.csv").to_str().unwrap()]);
    let text = format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    assert!(!out.status.success(), "{text}");
    assert!(text.contains("3 stacked tables") || text.contains("3 tables"), "{text}");
    assert!(text.contains("tdy fit") && text.contains(&t.display().to_string()), "{text}");
}

/// A hand-edited sidecar the loader refuses is discarded and the member is
/// re-planned from the split's own window — which is right, since the window
/// is the one thing the sidecar cannot be trusted about. Doing it in silence
/// is what is wrong: nothing told the person their edit had no effect, and
/// the member reads exactly as it did before. So the refusal is a note.
#[test]
fn a_refused_sidecar_says_so_and_is_re_planned() {
    let (dir, t) = three_pile_accepted();
    // Only the ordinal: the window still names block 2's own lines, so this
    // is a sidecar that contradicts *itself*, which `load_member` refuses.
    edit_window(dir.path(), "report.csv#2", 5, 9, 1);
    let out = tdy(&["fit", t.to_str().unwrap()]);
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "{text}{}", String::from_utf8_lossy(&out.stderr));
    assert!(text.contains("sidecar refused:"), "the refusal is said out loud: {text}");
    assert!(text.contains("ordinal"), "and it carries the loader's own reason: {text}");
    assert!(text.contains("re-planned"), "and what happened instead: {text}");
    let row = text
        .lines()
        .find(|l| l.trim_start().starts_with("report.csv#2 "))
        .unwrap_or_else(|| panic!("no member row: {text}"));
    assert!(!row.contains("(existing spec)"), "re-planned, not reused: {row}");

    let sql = format!("SELECT sum(amount) AS total FROM dataset('{}')", t.display());
    let out = tdy(&["query", &sql]);
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "{text}{}", String::from_utf8_lossy(&out.stderr));
    assert!(text.contains("3000.00"), "{text}");
    let sql = format!("SELECT sum(amount) AS total FROM dataset('{}') WHERE _member = 'report.csv#2'", t.display());
    let out = tdy(&["query", &sql]);
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("1500.00"), "block 2 is block 2 again: {text}");
}

/// A member that reuses the *windowed* spec `tdy fit` wrote for it reads
/// one block, so the split's own wording is true of it: those lines are not
/// read. Reusing the sidecar must not change that text.
#[test]
fn a_windowed_reuse_keeps_the_split_s_own_wording() {
    let (dir, t) = three_pile();
    std::fs::copy(fixture("regions_short_block.csv"), dir.path().join("report.csv")).unwrap();
    assert!(tdy(&["fit", t.to_str().unwrap()]).status.success());
    let out = tdy(&["fit", t.to_str().unwrap()]);
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "{text}{}", String::from_utf8_lossy(&out.stderr));
    assert!(text.contains("(existing spec)"), "the second fit reuses it: {text}");
    assert!(text.contains("a run of 2 line(s) at lines 1–2 was not read"), "{text}");
    assert!(text.contains("accept only if those lines are not part of this dataset"), "{text}");
}

/// The same file with a hand-written *whole-file* spec. The split still
/// found the run, but this spec reads it — so the note that says nothing
/// read those lines, and the review question that asks whether they were
/// data, are both false of the spec in force. The facts are the same; the
/// question a person is asked is the other one, and the gate stays: a
/// whole-file read over a table-shaped extra block is exactly what someone
/// should have to look at.
#[test]
fn a_whole_file_spec_is_asked_the_other_question() {
    use tdy::spec::{ColumnSpec, DType, Extraction, InferenceMethod, ParseSpec, RaggedPolicy, Transform, ValueParsing};
    let (dir, t) = three_pile();
    let f = dir.path().join("report.csv");
    std::fs::copy(fixture("regions_short_block.csv"), &f).unwrap();
    let col = |name: &str, source: &str, dtype: DType| ColumnSpec {
        name: name.into(), source: Some(source.into()), dtype, nullable: false,
        parse: ValueParsing::default(), pointer: None,
    };
    let spec = ParseSpec {
        extraction: Extraction::Delimited {
            delimiter: ';', quote: Some('"'), escape: None, encoding: None, comment: None,
            ragged: RaggedPolicy::PadNulls, region: None,
        },
        transforms: vec![
            Transform::PromoteHeader { rows: 1, join: " ".into() },
            // Both runs carry the same header, and the file is read whole:
            // the repeat (and the blank line between them) is not data.
            Transform::DropRowsMatching { pattern: "^(Datum)?$".into(), column: Some("Datum".into()) },
        ],
        columns: vec![
            col("month", "Datum", DType::Date { format: "%d.%m.%Y".into() }),
            col("region", "Region", DType::Utf8),
            col("amount", "Betrag", DType::Decimal { precision: 14, scale: 2 }),
        ],
        confidence: Some(1.0),
        notes: vec![],
    };
    tdy::sidecar::save_member(&f, None, None, &spec, tdy::sidecar::ProvenanceInfo {
        method: InferenceMethod::Manual, model: None, prompt_version: None, sampled_bytes: None,
    })
    .unwrap();

    let out = tdy(&["fit", t.to_str().unwrap()]);
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "{text}{}", String::from_utf8_lossy(&out.stderr));
    assert!(text.contains("(hand-written spec)"), "{text}");
    assert!(!text.contains("was not read"), "this spec reads them: {text}");
    assert!(
        text.contains(
            "the split found a run of 2 line(s) at lines 1–2 outside the proper block; \
             this spec reads the whole file"
        ),
        "{text}"
    );
    assert!(
        text.contains(
            "this spec reads the whole file including a table-shaped run at lines 1–2 \
             — accept only if that is intended"
        ),
        "{text}"
    );

    let sql = format!("SELECT sum(amount) AS total FROM dataset('{}')", t.display());
    let out = tdy(&["query", &sql]);
    assert!(!out.status.success(), "unaccepted: {}", String::from_utf8_lossy(&out.stdout));

    let out = tdy(&["fit", t.to_str().unwrap(), "--accept", "report.csv"]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let out = tdy(&["query", &sql]);
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "{text}{}", String::from_utf8_lossy(&out.stderr));
    // 100.00 from the short run plus 490 + 500 + 510 from the kept block:
    // the whole file, which is what this spec reads.
    assert!(text.contains("1600.00"), "{text}");
}

/// `#` is how a member says which sheet or which block it is, and a file is
/// free to have one in its name. A file literally called `report.csv#2`
/// beside a `report.csv` the blank-row split reads as three tables gives two
/// members one name — and therefore one sidecar path, `report.csv#2.tdy.toml`,
/// which cannot be two specs. Until now that failed loud only at `--accept`,
/// after both had been fitted and one had overwritten the other's sidecar.
/// It is a fact about the pile, so the pile is refused whole: no member is
/// fitted, no sidecar is written, no lock exists.
#[test]
fn two_members_with_one_name_refuse_the_whole_pile() {
    let dir = tempfile::TempDir::new().unwrap();
    std::fs::copy(fixture("regions_three.csv"), dir.path().join("report.csv")).unwrap();
    std::fs::write(
        dir.path().join("report.csv#2"),
        "Datum;Region;Betrag\n05.03.2025;Ost;10.00\n12.03.2025;West;20.00\n",
    )
    .unwrap();
    let t = dir.path().join("q.tdy.sql");
    std::fs::write(&t, "CREATE TABLE q (month DATE NOT NULL OPTIONS(matches='Datum'), region TEXT NOT NULL OPTIONS(matches='Region'), amount DECIMAL(14,2) NOT NULL OPTIONS(matches='Betrag')) WITH (files = '*', date_order = 'dmy');").unwrap();

    let out = tdy(&["fit", t.to_str().unwrap()]);
    let text = format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    assert!(!out.status.success(), "{text}");
    assert!(text.contains("report.csv#2"), "{text}");
    // Both readings are named, not just the name they share.
    assert!(text.contains("the file") && text.contains("block 2"), "{text}");
    assert!(text.contains("rename"), "the remedy: {text}");
    assert!(!dir.path().join("q.tdy.lock").exists(), "no lock: {text}");
    assert!(!dir.path().join("report.csv#2.tdy.toml").exists(), "no sidecar: {text}");

    // A machine caller gets the same refusal as an object, not only as a
    // line on stderr.
    let out = tdy(&["--json", "fit", t.to_str().unwrap()]);
    let v: serde_json::Value =
        serde_json::from_slice(&out.stdout).unwrap_or_else(|e| panic!("{e}: {}", String::from_utf8_lossy(&out.stdout)));
    assert!(
        v["error"].as_str().unwrap_or_default().contains("report.csv#2"),
        "{v}"
    );
}

/// `exclude` is applied twice: once as a glob over files, in
/// `lockfile::resolve`, and once as an exact member reference, in
/// `expand_units`. One entry can match in both passes — `report.csv#2` is a
/// real file's name *and* block 2 of `report.csv` — and then it drops two
/// different things at once. Nothing said so: the pile reported `2 of 2
/// member(s) fit`, wrote the lock and exited 0, having lost a table. The
/// duplicate-name refusal cannot catch it, because both removals happen
/// before it looks.
#[test]
fn an_exclude_that_names_both_a_file_and_a_member_is_refused() {
    let dir = tempfile::TempDir::new().unwrap();
    std::fs::copy(fixture("regions_three.csv"), dir.path().join("report.csv")).unwrap();
    std::fs::write(
        dir.path().join("report.csv#2"),
        "Datum;Region;Betrag\n05.03.2025;Ost;10.00\n12.03.2025;West;20.00\n",
    )
    .unwrap();
    let t = dir.path().join("q.tdy.sql");
    let ddl = |opts: &str| format!("CREATE TABLE q (month DATE NOT NULL OPTIONS(matches='Datum'), region TEXT NOT NULL OPTIONS(matches='Region'), amount DECIMAL(14,2) NOT NULL OPTIONS(matches='Betrag')) WITH (files = '*', date_order = 'dmy'{opts});");
    std::fs::write(&t, ddl(", exclude = 'report.csv#2'")).unwrap();

    let out = tdy(&["fit", t.to_str().unwrap()]);
    let text = format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    assert!(!out.status.success(), "{text}");
    assert!(text.contains("the file `report.csv#2`"), "{text}");
    assert!(text.contains("block 2 of `report.csv`"), "{text}");
    assert!(text.contains("rename"), "the remedy: {text}");
    assert!(!dir.path().join("q.tdy.lock").exists(), "no lock: {text}");

    // The control: an exclude that names only a member still just removes
    // that member. Nothing about the ordinary case moves.
    std::fs::remove_file(dir.path().join("report.csv#2")).unwrap();
    std::fs::write(&t, ddl(", exclude = 'report.csv#3'")).unwrap();
    for n in 1..=2 {
        let out = tdy(&["fit", t.to_str().unwrap(), "--accept", &format!("report.csv#{n}")]);
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    }
    let lock = std::fs::read_to_string(dir.path().join("q.tdy.lock")).unwrap();
    assert!(lock.contains("region = 2") && !lock.contains("region = 3"), "{lock}");
}

/// An `exclude` entry containing `#` can be a plain file's own name rather
/// than a member reference. `lockfile::resolve_excluded`'s glob pass already
/// dropped `odd#name.csv` before `expand_units` ever runs, so the
/// member-reference pass — which finds no member called `odd#name.csv`,
/// because it was never a member at all — must not then say the entry
/// "removes no member". It already removed one, as a file.
#[test]
fn an_exclude_that_removed_a_file_by_glob_is_not_removes_no_member() {
    let dir = tempfile::TempDir::new().unwrap();
    std::fs::copy(fixture("regions_three.csv"), dir.path().join("report.csv")).unwrap();
    std::fs::write(dir.path().join("odd#name.csv"), "x,y\n1,2\n").unwrap();
    let t = dir.path().join("q.tdy.sql");
    std::fs::write(
        &t,
        "CREATE TABLE q (month DATE NOT NULL OPTIONS(matches='Datum'), region TEXT NOT NULL OPTIONS(matches='Region'), amount DECIMAL(14,2) NOT NULL OPTIONS(matches='Betrag')) WITH (files = '*', date_order = 'dmy', exclude = 'odd#name.csv');",
    )
    .unwrap();
    let out = tdy(&["fit", t.to_str().unwrap()]);
    let text = format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    assert!(out.status.success(), "{text}");
    for n in 1..=3 {
        assert!(text.contains(&format!("report.csv#{n}")), "{text}");
    }
    assert!(dir.path().join("q.tdy.lock").exists(), "lock written: {text}");
}

/// The control for the case above: an `exclude` entry with `#` that removes
/// nothing at all — not a file by glob, not a member by reference — is still
/// refused as a typo, exactly as before.
#[test]
fn an_exclude_naming_no_member_anywhere_is_still_refused() {
    let (dir, t) = three_pile();
    let ddl = std::fs::read_to_string(&t).unwrap().replace("files = '*.csv',", "files = '*.csv', exclude = 'report.csv#99',");
    std::fs::write(&t, ddl).unwrap();
    let out = tdy(&["fit", t.to_str().unwrap()]);
    let text = format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    assert!(!out.status.success(), "{text}");
    assert!(text.contains("removes no member"), "{text}");
    assert!(!dir.path().join("q.tdy.lock").exists(), "no lock: {text}");
}

/// The target a statistics export under a banner is meant to become: the
/// file's own header names, bound by `matches`.
fn banner_target(dir: &Path, files: &str) -> PathBuf {
    let t = dir.join("b.tdy.sql");
    std::fs::write(
        &t,
        format!(
            "CREATE TABLE b (state TEXT NOT NULL OPTIONS(matches='State'), y2008 BIGINT NOT NULL OPTIONS(matches='2008'), \
             y2009 BIGINT NOT NULL OPTIONS(matches='2009'), y2010 BIGINT NOT NULL OPTIONS(matches='2010')) \
             WITH (files = '{files}', provenance = 'true');"
        ),
    )
    .unwrap();
    t
}

/// Run `sql` with `DS` standing for the target's dataset.
fn query(t: &Path, sql: &str) -> String {
    let out = tdy(&["query", &sql.replace("DS", &format!("dataset('{}')", t.display()))]);
    let text = String::from_utf8_lossy(&out.stdout).into_owned();
    assert!(out.status.success(), "{text}{}", String::from_utf8_lossy(&out.stderr));
    text
}

/// Banner, header, blank, data, footnotes — the corpus's statistics-export
/// layout as text. Only the data block passes the gates, so the banner and
/// the footnotes are runs nothing read, not members that fit nothing; and
/// the one-line header the blank row cut off is adopted into the block's
/// window, so the columns bind by the file's own names rather than as
/// `col_N`. One plain member, no review.
#[test]
fn a_banner_and_footnotes_are_not_members_and_the_severed_header_is_adopted() {
    let dir = tempfile::TempDir::new().unwrap();
    std::fs::copy(fixture("regions_banner.csv"), dir.path().join("report.csv")).unwrap();
    // The draft reads the table by the file's own header, not positionally,
    // and skips the banner and the footnotes — one field per line is not a
    // table — saying so. The UNEDITED draft is the target.
    let out = tdy(&["draft", dir.path().join("report.csv").to_str().unwrap()]);
    let draft = String::from_utf8_lossy(&out.stdout).into_owned();
    assert!(out.status.success(), "{draft}");
    let has = |col: &str, m: &str| draft.lines().any(|l| l.trim_start().starts_with(&format!("{col} ")) && l.contains(&format!("matches = '{m}'")));
    assert!(has("state", "State") && has("c_2008", "2008"), "{draft}");
    assert!(!draft.contains("col_"), "no banner or footnote column: {draft}");
    assert!(draft.contains("block 1 (lines 1–6) skipped: one field per line"), "{draft}");
    assert!(draft.contains("block 3 (lines 16–18) skipped: one field per line"), "{draft}");
    let t = dir.path().join("b.tdy.sql");
    std::fs::write(&t, &draft).unwrap();
    let out = tdy(&["fit", t.to_str().unwrap()]);
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "{text}{}", String::from_utf8_lossy(&out.stderr));
    assert!(!text.contains("report.csv#"), "one plain member: {text}");
    assert!(!text.contains("REVIEW"), "a banner and footnotes are not table-shaped: {text}");
    assert!(text.contains("lines 1–6 was not read"), "the banner is named: {text}");
    assert!(text.contains("lines 16–18 was not read"), "the footnotes are named: {text}");
    assert!(!text.contains("lines 8–8"), "the header was adopted, not dropped: {text}");
    let sc = std::fs::read_to_string(dir.path().join("report.csv.tdy.toml")).unwrap();
    assert!(sc.contains("source = \"State\""), "bound by name: {sc}");
    let q = query(&t, "SELECT count(*) AS n, sum(c_2008) AS a, sum(c_2009) AS b, sum(c_2010) AS c FROM DS");
    assert!(q.contains("| 5 | 1500 | 1550 | 1600 |"), "{q}");
}

/// The same layout on two sheets: sheet discovery expands the workbook, and
/// each sheet is one table under a banner — a sheet member, not three
/// region members of which two fit nothing.
#[test]
fn a_banner_on_each_sheet_leaves_two_plain_sheet_members() {
    let dir = tempfile::TempDir::new().unwrap();
    std::fs::copy(fixture("regions_banner.xlsx"), dir.path().join("book.xlsx")).unwrap();
    let t = banner_target(dir.path(), "*.xlsx");
    let out = tdy(&["fit", t.to_str().unwrap()]);
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "{text}{}", String::from_utf8_lossy(&out.stderr));
    assert!(text.contains("book.xlsx#Premise") && text.contains("book.xlsx#Bottles"), "{text}");
    assert!(!text.contains("#Premise#") && !text.contains("#Bottles#"), "no region members: {text}");
    assert!(!text.contains("REVIEW"), "{text}");
    let q = query(&t, "SELECT _member, count(*) AS n, sum(y2008) AS a, sum(y2009) AS b, sum(y2010) AS c FROM DS GROUP BY 1 ORDER BY 1");
    assert!(q.contains("| book.xlsx#Bottles | 5 | 3000 | 3100 | 3200 |"), "{q}");
    assert!(q.contains("| book.xlsx#Premise | 5 | 1500 | 1550 | 1600 |"), "{q}");
}

/// When no block passes the gates the split has nothing to say: the file is
/// asked whole and refused with the ordinary gap report — no `#N` members,
/// no region notes, exactly as before regions existed.
#[test]
fn a_file_no_block_of_which_fits_gets_the_ordinary_gap_report() {
    let dir = tempfile::TempDir::new().unwrap();
    std::fs::copy(fixture("regions_three.csv"), dir.path().join("report.csv")).unwrap();
    let t = dir.path().join("q.tdy.sql");
    std::fs::write(&t, "CREATE TABLE q (kunde TEXT NOT NULL, menge BIGINT NOT NULL) WITH (files = '*.csv');").unwrap();
    let out = tdy(&["fit", t.to_str().unwrap()]);
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(!out.status.success(), "{text}");
    assert!(text.contains("report.csv") && text.contains("GAP"), "{text}");
    assert!(!text.contains("report.csv#"), "no region members: {text}");
    assert!(!text.contains("split at blank rows") && !text.contains("not read"), "no region notes: {text}");
}

/// A block that fails the gates but is as wide as the block that passed is
/// the one discarded run a person must rule on: it looks like a table.
#[test]
fn a_table_shaped_block_that_fails_the_gates_still_waits_on_a_person() {
    let (dir, t) = three_pile();
    let text = "Datum;Region;Betrag\n05.01.2025;Ost;190.00\n12.01.2025;West;200.00\n19.01.2025;Nord;210.00\n\n\
                Notiz;Stand;Vermerk\nfolgt;offen;siehe unten\nfolgt;offen;siehe unten\n";
    std::fs::write(dir.path().join("report.csv"), text).unwrap();
    let out = tdy(&["fit", t.to_str().unwrap()]);
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "{text}{}", String::from_utf8_lossy(&out.stderr));
    assert!(!text.contains("report.csv#"), "the failing block is not a member: {text}");
    assert!(text.contains("REVIEW: a run of 3 line(s) at lines 6–8 was not read"), "{text}");
    assert!(text.contains("REVIEW"), "a table-shaped discarded block waits on a person: {text}");
    let out = tdy(&["fit", t.to_str().unwrap(), "--accept", "report.csv"]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert!(query(&t, "SELECT sum(amount) AS total FROM DS").contains("| 600.00 |"));
}

/// The split's own answer for the banner layout: the one-line header the
/// blank row cut off is a dropped run directly above the block, as wide as
/// it — a candidate header (`severed_header`), adopted only by the framing
/// step and only for a block with no header of its own. Widths are counted
/// with the table's separator, not one a banner and a footnote block
/// out-voted it on. Gating then keeps the table alone, renumbered as
/// block 1, and the rest become dropped runs.
#[test]
fn a_severed_header_is_a_candidate_and_gating_keeps_the_table() {
    let want = [
        RowWindow { start: 0, end: 6, ordinal: 1 },
        RowWindow { start: 9, end: 14, ordinal: 2 },
        RowWindow { start: 15, end: 18, ordinal: 3 },
    ];
    let header = tdy::engine::DroppedRun { start: 7, end: 8, width: 4, widest: 4 };
    let r = tdy::engine::regions_of(&fixture("regions_banner.csv"), None, Limits::default()).unwrap();
    assert_eq!(r.windows, want);
    assert_eq!(r.dropped, [header], "{r:?}");
    assert_eq!(r.window_widths, [1, 4, 1]);
    assert_eq!(r.severed_header(1), Some(0));
    assert_eq!(r.severed_header(0), None);
    assert_eq!(r.severed_header(2), None, "the header sits above block 2, not below it");
    let x = tdy::engine::regions_of(&fixture("regions_banner.xlsx"), Some("Bottles"), Limits::default()).unwrap();
    assert_eq!(x.windows, want);
    assert_eq!(x.window_widths, [1, 4, 1]);

    let mut a = r.clone();
    a.adopt(1, 0);
    assert_eq!(a.windows[1], RowWindow { start: 7, end: 14, ordinal: 2 });
    assert!(a.dropped.is_empty());
    let g = a.gated(&[false, true, false]);
    assert_eq!(g.windows, [RowWindow { start: 7, end: 14, ordinal: 1 }]);
    assert_eq!(g.block_width, 4);
    assert_eq!(
        g.dropped,
        [tdy::engine::DroppedRun { start: 0, end: 6, width: 1, widest: 1 }, tdy::engine::DroppedRun { start: 15, end: 18, width: 1, widest: 1 }]
    );
    assert_eq!(g.table_shaped().count(), 0);
    assert_eq!(r.gated(&[false, false, false]), tdy::engine::Regions::default(), "none pass: read whole");

    // Header, blank, data and nothing else: one block and a candidate
    // header above it, decided by the framing step like any other.
    let dir = tempfile::TempDir::new().unwrap();
    let p = dir.path().join("h.csv");
    std::fs::write(&p, "a;b\n\n1;2\n3;4\n5;6\n").unwrap();
    let h = tdy::engine::regions_of(&p, None, Limits::default()).unwrap();
    assert_eq!(h.windows, [RowWindow { start: 2, end: 5, ordinal: 1 }]);
    assert_eq!(h.severed_header(0), Some(0));
}

/// "Table-shaped" is any row of the run holding two or more fields, not the
/// run's first row matching the block's first row. The block under a
/// one-cell title measures 1 wide by its first row, so a 72-cell row below
/// it matched nothing and went unreviewed (the corpus's
/// `infrastructure-data-may-2020.xlsx`, sheet `ch$stocks_and_USpop`, whose
/// "US population" row sits apart from the table).
#[test]
fn a_wide_row_under_a_one_cell_title_is_table_shaped() {
    let r = tdy::engine::Regions {
        windows: vec![RowWindow { start: 0, end: 109, ordinal: 1 }],
        dropped: vec![tdy::engine::DroppedRun { start: 111, end: 112, width: 72, widest: 72 }],
        block_width: 1,
        window_widths: vec![1],
        window_widest: vec![73],
    };
    assert_eq!(r.table_shaped().count(), 1);
    // A one-cell line over a wider table stays unreviewed; over a block
    // that is itself one cell wide on its first row it is as wide as the
    // table, which is the other rule.
    let one = vec![tdy::engine::DroppedRun { start: 111, end: 112, width: 1, widest: 1 }];
    let r = tdy::engine::Regions { dropped: one.clone(), block_width: 4, ..r };
    assert_eq!(r.table_shaped().count(), 0);
    let r = tdy::engine::Regions { dropped: one, block_width: 1, ..r };
    assert_eq!(r.table_shaped().count(), 1);
    // The same layout as text, through the split itself: the separator is
    // the table's, found from its widest row, not from the one-cell title.
    let dir = tempfile::TempDir::new().unwrap();
    let p = dir.path().join("t.csv");
    std::fs::write(&p, "Real net stock\nkind;y1;y2\nroads;1;2\nrails;3;4\n\nUS population;5;6\n").unwrap();
    let r = tdy::engine::regions_of(&p, None, Limits::default()).unwrap();
    assert_eq!(r.windows, [RowWindow { start: 0, end: 4, ordinal: 1 }]);
    assert_eq!(r.table_shaped().count(), 1, "{r:?}");
}

/// A lone data row as wide as the table, a blank line, then the table with
/// its own header: the table's header is its own, so nothing is adopted —
/// the lone row is a data-like run nothing read, and waits on a person.
/// (Before adoption asked whether the block had a header, the lone row was
/// adopted and the file refused on a header made of data — loud, but for
/// the wrong reason.)
#[test]
fn a_lone_row_above_a_headed_table_is_not_adopted() {
    let (_d, t) = three_pile();
    std::fs::write(
        _d.path().join("report.csv"),
        "05.01.2025;Ost;100.00\n\nDatum;Region;Betrag\n12.01.2025;West;200.00\n19.01.2025;Nord;210.00\n26.01.2025;Ost;190.00\n",
    )
    .unwrap();
    waits_then_reads(&t, "REVIEW: a run of 1 line(s) at lines 1–1 was not read", "amount", "600.00");
}

/// A blank row proves a boundary; only a header tells one block from the
/// next. Three headerless blocks of one width are indistinguishable from
/// one table with blank lines in it, so the file is read whole — one plain
/// member, no `#N`, no region notes — exactly as before regions.
#[test]
fn headerless_blocks_are_not_candidates_and_the_file_is_read_whole() {
    let dir = tempfile::TempDir::new().unwrap();
    let rows = |m: &str| format!("05.{m}.2025;Ost;190.00\n12.{m}.2025;West;200.00\n19.{m}.2025;Nord;210.00\n");
    std::fs::write(dir.path().join("report.csv"), format!("{}\n{}\n{}", rows("01"), rows("02"), rows("03"))).unwrap();
    let t = dir.path().join("q.tdy.sql");
    std::fs::write(&t, "CREATE TABLE q (col_1 TEXT, col_2 TEXT, col_3 TEXT) WITH (files = '*.csv', provenance = 'true');").unwrap();
    let out = tdy(&["fit", t.to_str().unwrap()]);
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "{text}{}", String::from_utf8_lossy(&out.stderr));
    assert!(!text.contains("report.csv#") && !text.contains("REVIEW"), "{text}");
    assert!(!text.contains("split at blank rows") && !text.contains("not read"), "no region notes: {text}");
    let q = query(&t, "SELECT count(*) AS n FROM DS");
    assert!(q.contains("| 9 "), "the whole file: {q}");
}

/// The corpus's ttb sheet, reduced. Against the drafted positional target
/// the banner and the two-cell footnote block used to pass by position
/// alone; neither has a header, so neither is a candidate, no block passes,
/// and the sheet is read whole through the frame the draft came from — the
/// pre-regions reading, unreviewed for a positional target. Against a
/// by-name target the table is
/// the one block that passes, bound by name, and the footnote block is a
/// data-like run nothing read, so it waits on a person.
#[test]
fn a_headerless_footnote_block_cannot_stand_in_for_the_sheet() {
    let dir = tempfile::TempDir::new().unwrap();
    std::fs::copy(fixture("regions_footnoted.xlsx"), dir.path().join("book.xlsx")).unwrap();
    let out = tdy(&["draft", dir.path().join("book.xlsx").to_str().unwrap()]);
    let t = dir.path().join("d.tdy.sql");
    std::fs::write(&t, out.stdout).unwrap();
    let out = tdy(&["fit", t.to_str().unwrap()]);
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "{text}{}", String::from_utf8_lossy(&out.stderr));
    assert!(!text.contains("book.xlsx#"), "{text}");
    assert!(!text.contains("not read") && !text.contains("split at blank rows"), "read whole: {text}");
    // No review, although the split saw a headed table the target cannot
    // bind: the drafted target is purely positional (`col_N`), which binds
    // by position by construction, so the failure is no evidence of a wrong
    // reading — the whole-sheet frame is the pre-regions reading. The
    // "this file is read whole" question is for by-name targets only
    // (`Target::names_a_column`); asked here it would always be answered yes.
    assert!(!text.contains("REVIEW"), "{text}");

    let dir = tempfile::TempDir::new().unwrap();
    std::fs::copy(fixture("regions_footnoted.xlsx"), dir.path().join("book.xlsx")).unwrap();
    let t = banner_target(dir.path(), "*.xlsx");
    let out = tdy(&["fit", t.to_str().unwrap()]);
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "{text}{}", String::from_utf8_lossy(&out.stderr));
    assert!(!text.contains("book.xlsx#"), "one member: {text}");
    assert!(text.contains("state<-\"State\""), "bound by name: {text}");
    assert!(text.contains("lines 16–18 was not read"), "the footnote block is named: {text}");
    assert!(text.contains("REVIEW"), "two cells a row is data-like: {text}");
    let out = tdy(&["fit", t.to_str().unwrap(), "--accept", "book.xlsx"]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let q = query(&t, "SELECT count(*) AS n, sum(y2008) AS a, sum(y2009) AS b, sum(y2010) AS c FROM DS");
    assert!(q.contains("| 5 | 1500 | 1550 | 1600 |"), "{q}");
}

/// A pile of one `report.csv` with `content`, under `ddl`.
fn pile(content: &str, ddl: &str) -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::TempDir::new().unwrap();
    std::fs::write(dir.path().join("report.csv"), content).unwrap();
    let t = dir.path().join("q.tdy.sql");
    std::fs::write(&t, ddl).unwrap();
    (dir, t)
}

/// Fit; the one member waits on a person with `reason`; `dataset()` refuses
/// it; after `--accept report.csv` the sum of `col` is `want`.
fn waits_then_reads(t: &Path, reason: &str, col: &str, want: &str) {
    let out = tdy(&["fit", t.to_str().unwrap()]);
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "{text}{}", String::from_utf8_lossy(&out.stderr));
    assert!(!text.contains("report.csv#"), "{text}");
    assert!(text.contains(reason), "{reason}: {text}");
    let sql = format!("SELECT sum({col}) AS total FROM dataset('{}')", t.display());
    let out = tdy(&["query", &sql]);
    assert!(!out.status.success(), "unaccepted: {}", String::from_utf8_lossy(&out.stdout));
    let out = tdy(&["fit", t.to_str().unwrap(), "--accept", "report.csv"]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let q = query(t, &format!("SELECT sum({col}) AS total FROM DS"));
    assert!(q.contains(&format!("| {want} |")), "{want}: {q}");
}

/// A one-column table's continuation is one field per row — exactly what a
/// banner is — but it is as wide as the table, so it is data-like: the
/// original same-width rule, OR'd with "any row holds two fields".
#[test]
fn a_one_column_tables_continuation_waits_on_a_person() {
    let ddl = "CREATE TABLE q (amount DECIMAL(14,2) NOT NULL OPTIONS(matches='Betrag')) WITH (files='*.csv');";
    let (_d, t) = pile("Betrag\n100.00\n200.00\n300.00\n400.00\n\n500.00\n600.00\n700.00\n", ddl);
    waits_then_reads(&t, "REVIEW: a run of 3 line(s) at lines 7–9 was not read", "amount", "1000.00");
    let (_d, t) = pile("Betrag\n100.00\n200.00\n300.00\n400.00\n\n500.00\n600.00\n", ddl);
    waits_then_reads(&t, "REVIEW: a run of 2 line(s) at lines 7–8 was not read", "amount", "1000.00");
}

/// A `"` in the middle of a field (12-inch pipe) is a character, not a
/// quote: the row has three fields and is data-like.
#[test]
fn a_mid_field_quote_does_not_hide_a_data_row() {
    let ddl = "CREATE TABLE q (item TEXT NOT NULL OPTIONS(matches='Item'), qty BIGINT NOT NULL OPTIONS(matches='Qty'), amount DECIMAL(14,2) NOT NULL OPTIONS(matches='Amount')) WITH (files='*.csv');";
    let (_d, t) = pile("Item;Qty;Amount\nRohr 10mm;1;10.00\nRohr 20mm;2;20.00\nRohr 30mm;3;30.00\n\nRohr 12\";5;60.00\n", ddl);
    waits_then_reads(&t, "REVIEW: a run of 1 line(s) at lines 6–6 was not read", "amount", "60.00");
}

/// Fit the renumbering pile (`file` copied as `name`) against `matches`;
/// accept members `1..=n`; return the dataset's total.
fn renumber_round(dir: &Path, name: &str, matches: &str, n: u32) -> (String, String) {
    let t = dir.join("q.tdy.sql");
    let ext = name.rsplit('.').next().unwrap();
    std::fs::write(&t, format!("CREATE TABLE q (month DATE NOT NULL OPTIONS(matches='Datum'), region TEXT NOT NULL OPTIONS(matches='Region'), amount DECIMAL(14,2) NOT NULL OPTIONS(matches='{matches}')) WITH (files = '*.{ext}', date_order = 'dmy');")).unwrap();
    let out = tdy(&["fit", t.to_str().unwrap()]);
    let fit = String::from_utf8_lossy(&out.stdout).into_owned();
    assert!(out.status.success(), "{fit}{}", String::from_utf8_lossy(&out.stderr));
    let lock = std::fs::read_to_string(dir.join("q.tdy.lock")).unwrap();
    assert_eq!(lock.matches("accepted = true").count(), 0, "nothing carried over a renumbering: {lock}");
    for i in 1..=n {
        let out = tdy(&["fit", t.to_str().unwrap(), "--accept", &format!("{name}#{i}")]);
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    }
    (fit, query(&t, "SELECT sum(amount) AS total FROM DS"))
}

/// The blocks that pass are numbered among themselves, so a declaration that
/// lets one more block through renumbers the rest, and the sidecar written
/// for `#1` now names `#2`'s rows. Reusing it read one block twice and lost
/// another (3300.00 where the file holds 2406.00). A tool-written sidecar
/// whose window is not its member's block is re-planned, saying so; nothing
/// accepted carries over, since the question asked changed with the count.
#[test]
fn a_renumbered_sheet_region_is_re_planned_not_reused() {
    let dir = tempfile::TempDir::new().unwrap();
    std::fs::copy(fixture("regions_renumber.xlsx"), dir.path().join("book.xlsx")).unwrap();
    let (_, total) = renumber_round(dir.path(), "book.xlsx", "Betrag", 2);
    assert!(total.contains("| 2400.00 |"), "{total}");
    let (fit, total) = renumber_round(dir.path(), "book.xlsx", "Betrag, Menge", 3);
    assert!(fit.contains("sidecar window was block 1") && fit.contains("re-planned"), "{fit}");
    assert!(!fit.contains("existing spec"), "{fit}");
    assert!(total.contains("| 2406.00 |"), "{total}");
}

/// The text twin: re-planned the same way rather than stuck on CONTRADICTS,
/// which would leave a person deleting tdy's own sidecars by hand.
#[test]
fn a_renumbered_text_region_is_re_planned_not_contradicted() {
    let dir = tempfile::TempDir::new().unwrap();
    std::fs::copy(fixture("regions_renumber.csv"), dir.path().join("report.csv")).unwrap();
    let (_, total) = renumber_round(dir.path(), "report.csv", "Betrag", 2);
    assert!(total.contains("| 2400.00 |"), "{total}");
    let (fit, total) = renumber_round(dir.path(), "report.csv", "Betrag, Menge", 3);
    assert!(fit.contains("sidecar window was block 1 (lines 6–9), the split now gives block 1 (lines 1–4); re-planned"), "{fit}");
    assert!(!fit.contains("CONTRADICTS"), "{fit}");
    assert!(total.contains("| 2406.00 |"), "{total}");
}

/// All-TEXT data under its own header, with a same-width line above it: the
/// block has a header of its own, so the line above is not adopted (it was,
/// and `Meier;Bern` became the header and `Name;City` a data row, with no
/// note and no review). It is a run nothing read, two fields wide.
#[test]
fn a_same_width_line_above_a_headed_text_table_is_not_its_header() {
    let ddl = "CREATE TABLE q (name TEXT NOT NULL OPTIONS(matches='Name'), city TEXT NOT NULL OPTIONS(matches='City')) WITH (files='*.csv', provenance='true');";
    let (dir, t) = pile("Meier;Bern\n\nName;City\nMuster;Zürich\nHuber;Genf\nKeller;Basel\n", ddl);
    let out = tdy(&["fit", t.to_str().unwrap()]);
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "{text}{}", String::from_utf8_lossy(&out.stderr));
    assert!(text.contains("name<-\"Name\"") && text.contains("city<-\"City\""), "bound by name: {text}");
    assert!(text.contains("REVIEW: a run of 1 line(s) at lines 1–1 was not read"), "{text}");
    tdy(&["fit", t.to_str().unwrap(), "--accept", "report.csv"]);
    let q = query(&t, "SELECT name FROM DS ORDER BY name");
    assert!(q.contains("| Huber ") && q.contains("| Keller ") && q.contains("| Muster "), "{q}");
    assert!(!q.contains("Name") && !q.contains("Meier"), "{q}");
    let _ = dir;
}

/// A two-field title over a headed table and a one-line source note under
/// it: one member bound by name; the title is data-like and reviewed.
#[test]
fn a_two_field_title_over_a_headed_table_is_reviewed_not_adopted() {
    let ddl = "CREATE TABLE q (state TEXT NOT NULL OPTIONS(matches='State'), amount DECIMAL(14,2) NOT NULL OPTIONS(matches='Amount')) WITH (files='*.csv', provenance='true');";
    let (_d, t) = pile("Sales report;Q1 2025\n\nState;Amount\nBern;100.00\nZug;200.00\nUri;300.00\n\nSource: FSO\n", ddl);
    waits_then_reads(&t, "REVIEW: a run of 1 line(s) at lines 1–1 was not read", "amount", "600.00");
}

/// A workbook where only one sheet holds the table is a plain member, and
/// its split must still read that sheet: "a workbook with several sheets"
/// gave no sheet to split, the banner was read as data, and the member was
/// refused where the same sheet alone fits.
#[test]
fn the_one_fitting_sheet_of_a_workbook_is_split_and_fits() {
    let dir = tempfile::TempDir::new().unwrap();
    std::fs::copy(fixture("regions_banner_notes.xlsx"), dir.path().join("book.xlsx")).unwrap();
    let t = banner_target(dir.path(), "*.xlsx");
    let out = tdy(&["fit", t.to_str().unwrap()]);
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "{text}{}", String::from_utf8_lossy(&out.stderr));
    assert!(!text.contains("book.xlsx#") && !text.contains("REVIEW"), "{text}");
    let q = query(&t, "SELECT count(*) AS n, sum(y2008) AS a, sum(y2009) AS b, sum(y2010) AS c FROM DS");
    assert!(q.contains("| 5 | 1500 | 1550 | 1600 |"), "{q}");
}

/// A block that binds none of the declared columns is not the table, even
/// when every column is declared absent-allowed: it "fit" by filling every
/// column with NULL, and became a member contributing rows of nothing.
#[test]
fn a_block_that_binds_no_declared_column_does_not_pass() {
    let ddl = "CREATE TABLE q (state TEXT OPTIONS(matches='State', if_missing='null'), amount DECIMAL(14,2) OPTIONS(matches='Amount', if_missing='null')) WITH (files='*.csv', provenance='true');";
    let (_d, t) = pile("State;Amount\nBern;100.00\nZug;200.00\nUri;300.00\n\nNote;Source\nrevised;FSO\nestimated;FSO\n", ddl);
    waits_then_reads(&t, "REVIEW: a run of 3 line(s) at lines 6–8 was not read", "amount", "600.00");
    let q = query(&t, "SELECT count(*) AS n FROM DS");
    assert!(q.contains("| 3 "), "{q}");
}

/// A one-column file with a blank line in it has no banner to skip: every
/// block is one field wide, so the draft says that and drafts the file whole.
#[test]
fn a_stacked_one_column_file_is_drafted_whole() {
    let (dir, _t) = pile("Betrag\n100.00\n200.00\n300.00\n400.00\n\n500.00\n600.00\n700.00\n", "");
    let out = tdy(&["draft", dir.path().join("report.csv").to_str().unwrap()]);
    let draft = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "{draft}");
    assert!(draft.contains("report.csv: all blocks one field wide; drafted whole"), "{draft}");
    assert!(!draft.contains("skipped"), "{draft}");
    assert!(draft.contains("matches = 'Betrag'"), "{draft}");
}

/// Draft, then fit the UNEDITED draft: the realistic route. A one-window
/// split with a line dropped above it used to fall through to a whole-file
/// draft that declared the line's values as columns; the fit then found no
/// block passing, read the file whole, and returned the real header as a
/// data row with no note and no review.
fn draft_then_fit(content: &str) -> (tempfile::TempDir, PathBuf, String) {
    let (dir, _) = pile(content, "");
    // As a person runs it: in the directory, `tdy draft report.csv`.
    let out = tdy_in(dir.path(), &["draft", "report.csv"]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let t = dir.path().join("d.tdy.sql");
    std::fs::write(&t, &out.stdout).unwrap();
    let out = tdy_in(dir.path(), &["fit", "d.tdy.sql"]);
    let text = String::from_utf8_lossy(&out.stdout).into_owned();
    assert!(out.status.success(), "{text}{}", String::from_utf8_lossy(&out.stderr));
    (dir, t, text)
}

#[test]
fn the_unedited_draft_of_a_headed_table_under_a_line_binds_by_name() {
    let (_d, t, text) = draft_then_fit("Meier;Bern\n\nName;City\nMuster;Zürich\nHuber;Genf\nKeller;Basel\n");
    assert!(text.contains("name<-\"Name\"") && text.contains("city<-\"City\""), "{text}");
    assert!(text.contains("REVIEW: a run of 1 line(s) at lines 1–1 was not read"), "{text}");
    let out = tdy(&["query", &format!("SELECT count(*) AS n FROM dataset('{}')", t.display())]);
    assert!(!out.status.success(), "unaccepted");
    let out = tdy_in(_d.path(), &["fit", "d.tdy.sql", "--accept", "report.csv"]);
    assert!(out.status.success(), "{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    let q = query(&t, "SELECT name FROM DS ORDER BY name");
    assert!(q.contains("| Huber ") && q.contains("| Keller ") && q.contains("| Muster "), "{q}");
    assert!(!q.contains("| Name ") && !q.contains("Meier"), "{q}");
    assert!(query(&t, "SELECT count(*) AS n FROM DS").contains("| 3 |"));
}

#[test]
fn the_unedited_draft_of_a_titled_report_binds_by_name() {
    let (_d, t, text) = draft_then_fit("Sales report;Q1 2025\n\nState;Amount\nBern;100.00\nZug;200.00\nUri;300.00\n\nSource: FSO\n");
    assert!(text.contains("state<-\"State\""), "{text}");
    assert!(text.contains("REVIEW: a run of 1 line(s) at lines 1–1 was not read"), "{text}");
    let out = tdy(&["query", &format!("SELECT count(*) AS n FROM dataset('{}')", t.display())]);
    assert!(!out.status.success(), "unaccepted");
    let out = tdy_in(_d.path(), &["fit", "d.tdy.sql", "--accept", "report.csv"]);
    assert!(out.status.success(), "{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    assert!(query(&t, "SELECT count(*) AS n, sum(amount) AS total FROM DS").contains("| 3 | 600.00 |"));
}

/// No block passes, so the file is read whole — but the split saw a table
/// with its own header there, and reading past it is a judgement: the
/// whole-file member says so and waits on a person.
#[test]
fn a_headed_block_that_does_not_fit_makes_the_whole_file_read_a_question() {
    let ddl = "CREATE TABLE q (meier TEXT NOT NULL OPTIONS(matches='Meier'), bern TEXT NOT NULL OPTIONS(matches='Bern')) WITH (files='*.csv');";
    let (_d, t) = pile("Meier;Bern\n\nName;City\nMuster;Zürich\nHuber;Genf\nKeller;Basel\n", ddl);
    let out = tdy(&["fit", t.to_str().unwrap()]);
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "{text}{}", String::from_utf8_lossy(&out.stderr));
    assert!(!text.contains("report.csv#"), "{text}");
    assert!(
        text.contains("REVIEW: the split found a table with its own header at lines 3–6 that does not fit the declared table; this file is read whole — accept only if that is intended"),
        "{text}"
    );
    let out = tdy(&["query", &format!("SELECT count(*) AS n FROM dataset('{}')", t.display())]);
    assert!(!out.status.success(), "unaccepted");
    assert!(tdy(&["fit", t.to_str().unwrap(), "--accept", "report.csv"]).status.success());
    assert!(query(&t, "SELECT count(*) AS n FROM DS").contains("| 4 |"));
}
