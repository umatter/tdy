//! Profiling: what a column holds, read over the framed raw table and the
//! whole file. See docs/design/2026-10-01-profiling.md §7.

use std::path::{Path, PathBuf};

use tdy::config::Limits;
use tdy::profile::{profile_file, ColumnProfile, Distinct, Profile, Request, Shape};

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("testdata").join(name)
}

fn column<'a>(p: &'a Profile, name: &str) -> &'a ColumnProfile {
    p.columns.iter().find(|c| c.name == name).unwrap_or_else(|| panic!("no column {name}: {p:?}"))
}

fn shape(pattern: &str, count: u64, example: &str) -> Shape {
    Shape { pattern: pattern.into(), count, example: example.into() }
}

/// A scratch copy, so nothing a profile might write could land in testdata.
fn scratch(name: &str) -> (tempfile::TempDir, PathBuf) {
    let d = tempfile::tempdir().unwrap();
    let p = d.path().join(name);
    std::fs::copy(fixture(name), &p).unwrap();
    (d, p)
}

/// The six dotted dates sit after row 60, where no head shows them; the
/// whole-file profile names both shapes with their exact counts.
#[test]
fn the_whole_file_shows_the_shapes_the_head_hides() {
    let (_d, p) = scratch("profile_mixed_dates.csv");
    let prof = profile_file(&p, &Request::default(), Limits::default()).unwrap();
    assert!(prof.complete);
    assert_eq!(prof.rows, 100);
    let names: Vec<&str> = prof.columns.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(names, ["Datum", "Region", "Betrag"]);
    let datum = column(&prof, "Datum");
    assert_eq!(datum.position, 1);
    assert_eq!((datum.non_empty, datum.empty), (100, 0));
    assert_eq!(datum.distinct, Distinct::Exact(100));
    assert_eq!(
        datum.shapes,
        [shape("9999-99-99", 94, "2025-01-01"), shape("99.99.9999", 6, "01.03.2025")]
    );
    assert_eq!(datum.min.as_deref(), Some("01.03.2025"));
    assert_eq!(datum.max.as_deref(), Some("2025-04-10"));

    let region = column(&prof, "Region");
    assert_eq!(region.distinct, Distinct::Exact(4));
    assert_eq!(
        region.top,
        [("Nord".to_string(), 25), ("Ost".to_string(), 25), ("Sued".to_string(), 25), ("West".to_string(), 25)]
    );
    assert_eq!(region.shapes, [shape("Aa+", 100, "Ost")]);

    let betrag = column(&prof, "Betrag");
    assert_eq!((betrag.non_empty, betrag.empty), (98, 2));
    assert_eq!(betrag.shapes, [shape("9'999.99", 98, "1'201.50")]);
}

/// `--head 10` reads ten rows, says it did, and sees one shape.
#[test]
fn a_head_profile_says_it_is_not_the_whole_file() {
    let (_d, p) = scratch("profile_mixed_dates.csv");
    let req = Request { head: Some(10), ..Request::default() };
    let prof = profile_file(&p, &req, Limits::default()).unwrap();
    assert!(!prof.complete);
    assert_eq!(prof.rows, 10);
    assert_eq!(column(&prof, "Datum").shapes, [shape("9999-99-99", 10, "2025-01-01")]);
    let text = tdy::commands::profile_text("profile_mixed_dates.csv", &prof, None).unwrap();
    let first = text.lines().next().unwrap();
    assert!(first.contains("first 10 rows only"), "the first line says so: {text}");
}

/// Past 10,000 distinct values the count is a floor and no top five is
/// offered: an approximate top five is a number nobody can check.
#[test]
fn distinct_is_a_floor_past_the_cap_and_top_is_withheld() {
    let (_d, p) = scratch("profile_many_ids.csv");
    let prof = profile_file(&p, &Request::default(), Limits::default()).unwrap();
    assert_eq!(prof.rows, 12_000);
    let id = column(&prof, "id");
    assert_eq!(id.distinct, Distinct::AtLeast(10_000));
    assert!(id.top.is_empty(), "{:?}", id.top);
    assert_eq!(id.non_empty, 12_000);
    assert_eq!(id.shapes, [shape("A+999999", 12_000, "ID000001")]);
    assert_eq!((id.min.as_deref(), id.max.as_deref()), (Some("ID000001"), Some("ID012000")));
    let kind = column(&prof, "kind");
    assert_eq!(kind.distinct, Distinct::Exact(3));
    assert_eq!(
        kind.top,
        [("a".to_string(), 4000), ("b".to_string(), 4000), ("c".to_string(), 4000)]
    );
}

/// The member that most needs a profile is the refused one, and it has no
/// sidecar. It is profiled in the sniffer's frame, under the file's own
/// spelling — two columns called `Betrag`, not `Betrag` and `Betrag_2` — and
/// nothing is written beside it.
#[test]
fn a_refused_member_is_profiled_without_a_sidecar_and_none_is_written() {
    let d = tempfile::tempdir().unwrap();
    let p = d.path().join("2025-08.csv");
    std::fs::copy(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("testdata/drifting_exports/2025-08.csv"),
        &p,
    )
    .unwrap();
    let prof = profile_file(&p, &Request::default(), Limits::default()).unwrap();
    let names: Vec<&str> = prof.columns.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(names, ["Datum", "Region", "Betrag", "Betrag"]);
    assert_eq!(prof.columns[3].position, 4);
    assert_eq!(prof.columns[3].min.as_deref(), Some("1'945.80"));
    let left: Vec<String> =
        std::fs::read_dir(d.path()).unwrap().flatten().map(|e| e.file_name().to_string_lossy().into()).collect();
    assert_eq!(left, ["2025-08.csv"], "a profile writes nothing");
}

/// A fresh sidecar's frame is the one profiled, and a stale one is not
/// trusted — the sniffer's frame is used instead, and the profile says
/// which.
#[test]
fn a_fresh_sidecars_frame_is_used() {
    let (_d, p) = scratch("profile_mixed_dates.csv");
    let mut spec = tdy::sniff::sniff_opts(
        &p,
        &tdy::sample::build(&p, 16 * 1024, Limits::default()).unwrap(),
        Limits::default(),
        tdy::sniff::SniffOpts { verify: false },
    )
    .unwrap()
    .spec;
    // A hand-made frame: the first two data rows counted as title lines.
    spec.transforms.insert(0, tdy::spec::Transform::SkipRows { head: 2, tail: 0 });
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
    let prof = profile_file(&p, &Request::default(), Limits::default()).unwrap();
    assert_eq!(prof.frame, "sidecar");
    let names: Vec<&str> = prof.columns.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(names, ["2025-01-02", "West", "1'202.50"], "the sidecar's frame, not a re-sniff");
    assert_eq!(prof.rows, 98);
}

/// A sheet is profiled in its own frame: the title row above the header is
/// skipped, as `fit` frames that sheet.
#[test]
fn a_sheet_is_profiled_in_its_own_frame() {
    let (_d, p) = scratch("sheet_frames_one_fits.xlsx");
    let req = Request { sheet: Some("Daten".into()), ..Request::default() };
    let prof = profile_file(&p, &req, Limits::default()).unwrap();
    assert_eq!(prof.sheet.as_deref(), Some("Daten"));
    let names: Vec<&str> = prof.columns.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(names, ["Datum", "Region", "Betrag"]);
    assert_eq!(prof.rows, 4);
    assert_eq!(column(&prof, "Betrag").max.as_deref(), Some("310.00"));
}

/// A region member's profile is of its block: `--rows 6-9` is the second
/// table of `regions_three.csv`, and only its three rows are counted.
#[test]
fn a_region_is_profiled_inside_its_window() {
    let (_d, p) = scratch("regions_three.csv");
    let req = Request { rows: Some((6, 9)), ..Request::default() };
    let prof = profile_file(&p, &req, Limits::default()).unwrap();
    assert_eq!(prof.rows, 3);
    let betrag = column(&prof, "Betrag");
    assert_eq!((betrag.min.as_deref(), betrag.max.as_deref()), (Some("490.00"), Some("510.00")));
    let w = prof.window.expect("the window is reported");
    assert_eq!((w.start, w.end), (5, 9));
}

/// A `--column` that names nothing is refused, naming what exists.
#[test]
fn an_unknown_column_is_refused_by_name() {
    let (_d, p) = scratch("profile_mixed_dates.csv");
    let prof = profile_file(&p, &Request::default(), Limits::default()).unwrap();
    let e = tdy::commands::profile_text("x.csv", &prof, Some("Datun")).unwrap_err().to_string();
    assert!(e.contains("Datun") && e.contains("Datum"), "{e}");
    let text = tdy::commands::profile_text("x.csv", &prof, Some("Datum")).unwrap();
    assert!(text.contains("99.99.9999") && text.contains("01.03.2025"), "{text}");
}

/// Memory: one streamed pass, O(columns x caps), not O(file). Ignored by
/// default (it writes ~50 MB); run by hand under `/usr/bin/time` as the
/// regions test is, and record the peak in CLAUDE.md:
///
/// ```text
/// cargo test --release --test profile --no-run
/// BIN=$(ls -t target/release/deps/profile-* | grep -v '\.d$' | head -1)
/// /usr/bin/time -f "wall %es peak_rss %MkB" "$BIN" --ignored --exact \
///   profile_streams_a_large_file
/// ```
#[test]
#[ignore]
fn profile_streams_a_large_file() {
    use std::io::Write;
    const TARGET_BYTES: usize = 50 * 1024 * 1024;
    let dir = tempfile::TempDir::new().unwrap();
    let p = dir.path().join("big.csv");
    let mut f = std::io::BufWriter::new(std::fs::File::create(&p).unwrap());
    writeln!(f, "id,city,amount").unwrap();
    let cities = ["Bern", "Zürich", "Basel", "Genf"];
    let (mut written, mut i) = (0usize, 0usize);
    while written < TARGET_BYTES {
        let line = format!("ID{i:09},{},{}.{:02}\n", cities[i % 4], i % 9973, i % 100);
        written += line.len();
        f.write_all(line.as_bytes()).unwrap();
        i += 1;
    }
    drop(f);
    let prof = profile_file(&p, &Request::default(), Limits::default()).unwrap();
    assert_eq!(prof.rows, i as u64);
    assert_eq!(column(&prof, "id").distinct, Distinct::AtLeast(10_000));
    assert_eq!(column(&prof, "city").distinct, Distinct::Exact(4));
}
