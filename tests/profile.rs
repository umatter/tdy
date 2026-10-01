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

// --- fix wave ---------------------------------------------------------------

fn text_of(p: &Profile, column: Option<&str>) -> String {
    tdy::commands::profile_text(&p.path, p, column).unwrap()
}

fn sniffed(p: &Path) -> tdy::spec::ParseSpec {
    tdy::sniff::sniff_opts(
        p,
        &tdy::sample::build(p, 16 * 1024, Limits::default()).unwrap(),
        Limits::default(),
        tdy::sniff::SniffOpts { verify: false },
    )
    .unwrap()
    .spec
}

fn save(p: &Path, spec: &tdy::spec::ParseSpec) {
    tdy::sidecar::save(
        p,
        spec,
        tdy::sidecar::ProvenanceInfo {
            method: tdy::spec::InferenceMethod::Manual,
            model: None,
            prompt_version: None,
            sampled_bytes: None,
        },
    )
    .unwrap();
}

/// `--sheet S --rows A-B` are the sheet's own row numbers — what Excel and a
/// sheet block's sidecar `range` show — not rows of the used range. The
/// used range of `regions_three_offset.xlsx` is C5:E18 and block 2 is
/// C10:E13; the heading says which range was read.
#[test]
fn sheet_rows_are_the_sheets_own_row_numbers() {
    let (_d, p) = scratch("regions_three_offset.xlsx");
    let req = Request { sheet: Some("Data".into()), rows: Some((10, 13)), ..Request::default() };
    let prof = profile_file(&p, &req, Limits::default()).unwrap();
    assert_eq!(prof.rows, 3);
    let betrag = column(&prof, "Betrag");
    assert_eq!((betrag.min.as_deref(), betrag.max.as_deref()), (Some("490.00"), Some("510.00")));
    assert_eq!(prof.range.as_deref(), Some("C10:E13"));
    assert!(text_of(&prof, None).lines().next().unwrap().contains("range C10:E13"), "{}", text_of(&prof, None));

    let req = Request { sheet: Some("Data".into()), rows: Some((2, 4)), ..Request::default() };
    let e = profile_file(&p, &req, Limits::default()).unwrap_err().to_string();
    assert!(e.contains("5") && e.contains("18"), "names the used range: {e}");
}

/// Past 10,000 distinct shapes the shapes are not all tracked: the column
/// says so, and no renderer claims a "most frequent shape" it cannot know.
/// 10,000 punctuation-only values, each its own shape, then 20,000 ISO
/// dates: the true answer (`9999-99-99`, 66.7%) arrived after the bound.
#[test]
fn an_untracked_shape_is_said_not_guessed() {
    let d = tempfile::tempdir().unwrap();
    let p = d.path().join("v.csv");
    let mut s = String::from("v;n\n");
    let marks = ['.', '-', '/'];
    for i in 0..10_000u32 {
        let mut k = i;
        let v: String = (0..9).map(|_| { let c = marks[(k % 3) as usize]; k /= 3; c }).collect();
        s.push_str(&format!("{v};1\n"));
    }
    for _ in 0..20_000 {
        s.push_str("2025-01-01;1\n");
    }
    std::fs::write(&p, s).unwrap();
    let prof = profile_file(&p, &Request::default(), Limits::default()).unwrap();
    let v = column(&prof, "v");
    assert!(!v.shapes_complete);
    let summary = text_of(&prof, None);
    let row = summary.lines().find(|l| l.contains("  v  ")).unwrap();
    assert!(row.contains("(shapes past 10000 not tracked)"), "{row}");
    assert!(!row.contains("0.0%"), "no false most-frequent shape: {row}");
    let detail = text_of(&prof, Some("v"));
    assert!(detail.contains("not tracked"), "{detail}");
    assert!(column(&prof, "n").shapes_complete);
}

/// 65 shapes: the 64 most frequent are listed, the least frequent is the
/// one `(other)` row, and the shapes are still complete — every one was
/// counted.
#[test]
fn past_64_shapes_the_rest_are_one_other_row() {
    let d = tempfile::tempdir().unwrap();
    let p = d.path().join("s.csv");
    let mut s = String::from("v,n\n");
    for k in 0..65usize {
        // `x` then k dots: one shape each; shape k appears 66 - k times.
        for _ in 0..(66 - k) {
            s.push_str(&format!("x{},1\n", ".".repeat(k)));
        }
    }
    std::fs::write(&p, s).unwrap();
    let prof = profile_file(&p, &Request::default(), Limits::default()).unwrap();
    let v = column(&prof, "v");
    assert!(v.shapes_complete);
    assert_eq!(v.shapes.len(), 65);
    assert_eq!(v.shapes[0], shape("a", 66, "x"));
    assert_eq!(v.shapes[64].pattern, "(other)");
    assert_eq!(v.shapes[64].count, 2, "the 65th shape, two values");
    assert_eq!(v.shapes.iter().map(|s| s.count).sum::<u64>(), v.non_empty);
}

/// A column's `na_values` count as empty, by the column that reads that
/// position — case-folded, as the executor reads them.
#[test]
fn na_values_count_as_empty() {
    let d = tempfile::tempdir().unwrap();
    let p = d.path().join("na.csv");
    std::fs::write(&p, "Datum;Region;Betrag\n2025-01-01;  ost ;1\n2025-01-02;N/A;2\n2025-01-03;West;3\n2025-01-04;  ;4\n")
        .unwrap();
    let mut spec = sniffed(&p);
    for c in &mut spec.columns {
        if c.source_name() == "Region" {
            c.parse.na_values = vec!["n/a".into()];
        }
    }
    save(&p, &spec);
    let prof = profile_file(&p, &Request::default(), Limits::default()).unwrap();
    assert_eq!(prof.frame, "sidecar");
    let region = column(&prof, "Region");
    assert_eq!((region.non_empty, region.empty), (2, 2), "`N/A` and a blank are empty");
    assert_eq!(region.min.as_deref(), Some("West"));
    assert_eq!(region.max.as_deref(), Some("ost"), "trimmed");
}

/// Two columns with one name: `--column NAME` is refused offering both
/// positions, quoted for a shell; `#4` picks one; `\#3` names a column
/// literally called `#3`.
#[test]
fn columns_are_picked_by_name_by_position_or_literally() {
    let d = tempfile::tempdir().unwrap();
    let p = d.path().join("2025-08.csv");
    std::fs::copy(Path::new(env!("CARGO_MANIFEST_DIR")).join("testdata/drifting_exports/2025-08.csv"), &p).unwrap();
    let prof = profile_file(&p, &Request::default(), Limits::default()).unwrap();
    let e = tdy::commands::profile_text("x", &prof, Some("Betrag")).unwrap_err().to_string();
    assert!(e.contains("--column '#3'") && e.contains("--column '#4'"), "{e}");
    let t = text_of(&prof, Some("#4"));
    assert!(t.contains("column `Betrag` (position 4)") && t.contains("1'945.80"), "{t}");

    let p = d.path().join("hash.csv");
    std::fs::write(&p, "a,#3,c\n1,x,3\n2,y,4\n").unwrap();
    let prof = profile_file(&p, &Request::default(), Limits::default()).unwrap();
    assert!(text_of(&prof, Some("#3")).contains("column `c` (position 3)"));
    assert!(text_of(&prof, Some("\\#3")).contains("column `#3` (position 2)"));
}

/// A member reference reads the block its fresh sidecar names; without one
/// it is refused with the designed sentence, not "no such file"; a sheet
/// reference needs no sidecar.
#[test]
fn member_references_resolve_or_are_refused_by_name() {
    let d = tempfile::tempdir().unwrap();
    let report = d.path().join("report.csv");
    std::fs::copy(fixture("regions_three.csv"), &report).unwrap();
    let e = profile_file(&d.path().join("report.csv#2"), &Request::default(), Limits::default())
        .unwrap_err()
        .to_string();
    assert!(e.contains("no fresh sidecar for") && e.contains("report.csv#2") && e.contains("--rows"), "{e}");

    std::fs::write(
        d.path().join("q.tdy.sql"),
        "CREATE TABLE q (month DATE NOT NULL OPTIONS(matches='Datum'), \
         region TEXT NOT NULL OPTIONS(matches='Region'), \
         amount DECIMAL(14,2) NOT NULL OPTIONS(matches='Betrag')) \
         WITH (files = '*.csv', date_order = 'dmy');",
    )
    .unwrap();
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_tdy"))
        .args(["fit", "q.tdy.sql"])
        .current_dir(d.path())
        .env("TDY_BACKEND", "none")
        .output()
        .unwrap();
    assert!(d.path().join("report.csv#2.tdy.toml").exists(), "{}", String::from_utf8_lossy(&out.stdout));
    let prof = profile_file(&d.path().join("report.csv#2"), &Request::default(), Limits::default()).unwrap();
    assert_eq!(prof.frame, "sidecar");
    assert_eq!(prof.rows, 3);
    assert_eq!(column(&prof, "Betrag").min.as_deref(), Some("490.00"));

    // A workbook block with no sidecar: `#Data#2` and `#2` read as a block
    // of sheet `Data` (a sheet called `Data#2` or `2` does not exist), and
    // reach the designed refusal rather than an ambiguity.
    let (_d3, offset) = scratch("regions_three_offset.xlsx");
    for reference in ["regions_three_offset.xlsx#Data#2", "regions_three_offset.xlsx#2"] {
        let e = profile_file(&offset.with_file_name(reference), &Request::default(), Limits::default())
            .unwrap_err()
            .to_string();
        assert!(e.contains("no fresh sidecar for") && e.contains("--rows") && e.contains("--sheet"), "{reference}: {e}");
        assert!(!e.contains("could mean"), "{reference}: {e}");
    }
    // A workbook that really has a sheet named `2` and a block 2: both
    // readings are true, and the refusal names both.
    let (_d4, named2) = scratch("profile_sheet_named_2.xlsx");
    let e = profile_file(&named2.with_file_name("profile_sheet_named_2.xlsx#2"), &Request::default(), Limits::default())
        .unwrap_err()
        .to_string();
    assert!(e.contains("could mean") && e.contains("sheet \"2\"") && e.contains("block 2"), "{e}");

    let (_d2, book) = scratch("sheet_frames_one_fits.xlsx");
    let sheet_ref = book.with_file_name("sheet_frames_one_fits.xlsx#Daten");
    let prof = profile_file(&sheet_ref, &Request::default(), Limits::default()).unwrap();
    assert_eq!((prof.sheet.as_deref(), prof.rows), (Some("Daten"), 4));
}

/// A stale sidecar is not trusted: the sniffer's frame is read, and the
/// profile says why.
#[test]
fn a_stale_sidecar_is_not_trusted() {
    let (_d, p) = scratch("profile_mixed_dates.csv");
    let mut spec = sniffed(&p);
    spec.transforms.insert(0, tdy::spec::Transform::SkipRows { head: 2, tail: 0 });
    save(&p, &spec);
    let mut text = std::fs::read_to_string(&p).unwrap();
    text.push_str("2025-05-01;Ost;1'301.50\n");
    std::fs::write(&p, text).unwrap();
    let prof = profile_file(&p, &Request::default(), Limits::default()).unwrap();
    assert!(prof.frame.starts_with("sniffed (the sidecar is stale"), "{}", prof.frame);
    assert_eq!(prof.columns[0].name, "Datum");
    assert_eq!(prof.rows, 101);
}

/// A title line wider than the table adds no phantom columns (the streamed
/// width is measured as the engine measures it).
#[test]
fn a_wide_title_adds_no_columns() {
    let d = tempfile::tempdir().unwrap();
    let p = d.path().join("title.csv");
    std::fs::write(&p, "Bericht Q1, Zuerich, final, v2\n\nDatum,Betrag\n2025-01-01,10\n2025-01-02,11\n2025-01-03,12\n")
        .unwrap();
    let prof = profile_file(&p, &Request::default(), Limits::default()).unwrap();
    let names: Vec<&str> = prof.columns.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(names, ["Datum", "Betrag"]);
}

/// A document with several record arrays: the heading names the array
/// profiled and the candidates; `--pointer` picks another.
#[test]
fn several_record_arrays_are_named_and_pickable() {
    let (_d, p) = scratch("json_frames_two_fit.json");
    let prof = profile_file(&p, &Request::default(), Limits::default()).unwrap();
    let head = text_of(&prof, None).lines().next().unwrap().to_string();
    assert!(head.contains("record array /q1") && head.contains("one of 2 candidates (/q1, /q2)"), "{head}");
    assert!(head.contains("--pointer"), "{head}");
    let req = Request { pointer: Some("/q2".into()), ..Request::default() };
    let prof = profile_file(&p, &req, Limits::default()).unwrap();
    assert_eq!(prof.pointer.as_deref(), Some("/q2"));
    assert_eq!(column(&prof, "amount").min.as_deref(), Some("490.00"));
    let head = text_of(&prof, None).lines().next().unwrap().to_string();
    assert!(head.contains("record array /q2") && !head.contains("candidates"), "{head}");
}

/// The smaller sentences: rows past the end name the file and its length;
/// a stacked file read whole says so; "no sidecar" names the members'
/// sidecars that do exist; a newline in a value draws as `↵`; a long shape
/// is elided in the summary and whole in the detail; `profile()` called
/// directly says its frame came from the caller.
#[test]
fn the_smaller_sentences() {
    let (d, p) = scratch("regions_three.csv");
    let req = Request { rows: Some((20, 30)), ..Request::default() };
    let e = profile_file(&p, &req, Limits::default()).unwrap_err().to_string();
    assert!(e.contains("regions_three.csv") && e.contains("14 lines"), "{e}");

    let prof = profile_file(&p, &Request::default(), Limits::default()).unwrap();
    let t = text_of(&prof, None);
    assert!(t.contains("3 stacked tables") && t.contains("--rows"), "{t}");
    // Under `--head` the read is not whole, and the note's own pass over
    // the whole file is not paid.
    let headed = profile_file(&p, &Request { head: Some(2), ..Request::default() }, Limits::default()).unwrap();
    assert!(headed.notes.is_empty(), "{:?}", headed.notes);
    assert!(!text_of(&headed, None).contains("stacked"));

    let mut spec = sniffed(&p);
    if let tdy::spec::Extraction::Delimited { region, .. } = &mut spec.extraction {
        *region = Some(tdy::spec::RowWindow { start: 5, end: 9, ordinal: 2 });
    }
    tdy::sidecar::save_member(
        &p,
        None,
        Some(2),
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
    assert!(prof.frame.contains("region sidecars: #2"), "{}", prof.frame);
    drop(d);

    let d = tempfile::tempdir().unwrap();
    let p = d.path().join("nl.csv");
    let long = "x.".repeat(30);
    std::fs::write(&p, format!("a,b\n\"one\ntwo\",{long}\n")).unwrap();
    let prof = profile_file(&p, &Request::default(), Limits::default()).unwrap();
    let t = text_of(&prof, None);
    assert!(t.contains("one↵two"), "{t}");
    assert!(!t.contains(&shape_of(&long)), "elided in the summary: {t}");
    assert!(text_of(&prof, Some("b")).contains(&shape_of(&long)), "whole in the detail");

    let spec = sniffed(&p);
    let direct = tdy::profile::profile(&p, &spec, Limits::default(), tdy::profile::ProfileOpts::default()).unwrap();
    assert!(!direct.frame.is_empty());
}

fn shape_of(v: &str) -> String {
    tdy::profile::shape(v)
}

/// A pile's report says which rows each block member is — set from the
/// split, in a dry run too — and a whole-file member carries no `rows` key,
/// so the JSON a script already reads is unchanged.
#[test]
fn a_pile_report_names_each_blocks_rows() {
    let d = tempfile::tempdir().unwrap();
    std::fs::copy(fixture("regions_three.csv"), d.path().join("report.csv")).unwrap();
    std::fs::copy(fixture("drifting_exports/2025-01.csv"), d.path().join("plain.csv")).unwrap();
    std::fs::write(
        d.path().join("q.tdy.sql"),
        "CREATE TABLE q (month DATE NOT NULL OPTIONS(matches='Datum'), \
         region TEXT NOT NULL OPTIONS(matches='Region'), \
         amount DECIMAL(14,2) NOT NULL OPTIONS(matches='Betrag')) \
         WITH (files = '*.csv', date_order = 'dmy');",
    )
    .unwrap();
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_tdy"))
        .args(["--json", "fit", "q.tdy.sql", "--dry-run"])
        .current_dir(d.path())
        .env("TDY_BACKEND", "none")
        .output()
        .unwrap();
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let rows: Vec<(String, serde_json::Value)> = v["members"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| {
            let name = format!("{}{}", m["path"].as_str().unwrap(), m["region"].as_u64().map(|r| format!("#{r}")).unwrap_or_default());
            (name, m.get("rows").cloned().unwrap_or(serde_json::Value::Null))
        })
        .collect();
    assert_eq!(
        rows,
        [
            ("plain.csv".to_string(), serde_json::Value::Null),
            ("report.csv#1".to_string(), serde_json::json!([1, 4])),
            ("report.csv#2".to_string(), serde_json::json!([6, 9])),
            ("report.csv#3".to_string(), serde_json::json!([11, 14])),
        ]
    );
    let sidecars = std::fs::read_dir(d.path()).unwrap().flatten().filter(|e| e.file_name().to_string_lossy().ends_with(".tdy.toml")).count();
    assert_eq!(sidecars, 0, "a dry run writes nothing");
}
