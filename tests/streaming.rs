//! The streaming executor must agree with the materialising one, everywhere.
//!
//! `src/stream.rs` exists to keep peak memory off the size of the file, not
//! to change any answer. So its specification is not a list of cases someone
//! thought of — it is equality with `engine::execute_batches` over every
//! delimited fixture in the tree, including the adversarial ones, and over
//! the transform combinations that are easy to get subtly wrong at a chunk
//! boundary.
//!
//! A failure here means the two paths disagree about what a file says, which
//! is the failure tdy is built to make impossible. There is no "close
//! enough": the batches are compared cell by cell.

use std::path::{Path, PathBuf};

use datafusion::arrow::record_batch::RecordBatch;
use datafusion::arrow::util::pretty::pretty_format_batches;
use tempfile::TempDir;

use tdy::config::Limits;
use tdy::spec::*;
use tdy::{engine, sample, sniff, stream};

fn testdata() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("testdata")
}

/// Everything a query would see, as text, so a mismatch prints readably.
fn render(batches: &[RecordBatch]) -> String {
    pretty_format_batches(batches).map(|d| d.to_string()).unwrap_or_default()
}

/// Run both executors and require they agree.
fn assert_paths_agree(spec: &ParseSpec, path: &Path, what: &str) {
    assert!(stream::can_stream(spec), "{what}: spec was not streamable");
    let materialised =
        engine::execute_batches(spec, path, Limits::default()).map(|b| render(&b));
    let streamed = stream::execute_batches(spec, path, Limits::default()).map(|b| render(&b));

    match (materialised, streamed) {
        (Ok(a), Ok(b)) => assert_eq!(a, b, "{what}: the two paths disagree"),
        (Err(a), Err(b)) => {
            // Both refused. The messages need not be identical, but a file
            // that one path rejects must not be one the other accepts.
            let _ = (a, b);
        }
        (Ok(a), Err(b)) => panic!("{what}: streaming refused a file the engine read:\n{b:#}\n{a}"),
        (Err(a), Ok(b)) => panic!("{what}: streaming read a file the engine refused:\n{a:#}\n{b}"),
    }
}

fn delimited_fixtures() -> Vec<PathBuf> {
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
                if matches!(ext.as_str(), "csv" | "tsv" | "txt" | "dat" | "log") {
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

/// The sweep. Every text fixture, sniffed, then run down both paths —
/// delimited, log lines and fixed-width reports alike.
#[test]
fn both_paths_agree_on_every_delimited_fixture() {
    let files = delimited_fixtures();
    assert!(files.len() > 20, "expected a corpus, found {} files", files.len());

    let mut compared = 0usize;
    for p in &files {
        let Ok(s) = sample::build(p, 64 * 1024, Limits::default()) else { continue };
        let Ok(res) = sniff::sniff(p, &s, Limits::default()) else { continue };
        if !stream::can_stream(&res.spec) {
            continue;
        }
        assert_paths_agree(&res.spec, p, &p.display().to_string());
        compared += 1;
    }
    assert!(
        compared > 20,
        "only {compared} fixtures were actually streamed — the sweep proves little"
    );
    eprintln!("compared {compared} fixtures down both paths");
}

/// The line-oriented sources have to stream too: logs are where the genuinely
/// large files are, and they were the ones still being materialised.
///
/// Their headers come from the extraction — capture-group names — so unlike a
/// delimited file there is no width to discover, and with no `skip_rows` tail
/// they are read exactly once.
#[test]
fn log_fixtures_stream_and_agree() {
    let mut seen = 0usize;
    for p in delimited_fixtures() {
        let Ok(s) = sample::build(&p, 64 * 1024, Limits::default()) else { continue };
        let Ok(res) = sniff::sniff(&p, &s, Limits::default()) else { continue };
        if !matches!(res.spec.extraction, Extraction::Lines { .. }) {
            continue;
        }
        seen += 1;
        assert!(
            stream::can_stream(&res.spec),
            "{}: a `lines` spec was not streamable",
            p.display()
        );
        assert_paths_agree(&res.spec, &p, &p.display().to_string());
    }
    assert!(seen > 0, "no `lines` fixture was exercised");
    eprintln!("compared {seen} log fixtures down both paths");
}

/// Fixed-width, against the committed reports and with the character offsets
/// `testdata/gen/04_logs_fixedwidth.py` documents.
///
/// No fixture sniffs as fixed-width — the decorated reports defeat tier 1, by
/// design and on the record — so the spec is hand-written here, which is how
/// a user would reach this extraction anyway. The UTF-8 and windows-1252
/// reports are the interesting pair: offsets are *character* positions, so an
/// implementation that sliced bytes would truncate "Müller" mid-sequence and
/// slide every later field. Both executors must make the same choice.
#[test]
fn fixed_width_reports_stream_and_agree() {
    let fields: Vec<FixedField> = [
        ("kunde", 0u32, 24u32),
        ("land", 24, 26),
        ("menge", 27, 34),
        ("betrag_chf", 35, 48),
        ("abweichung", 49, 60),
        ("marge_pct", 61, 68),
        ("bemerkung", 69, 9999),
    ]
    .into_iter()
    .map(|(name, start, end)| FixedField { name: name.into(), start, end })
    .collect();

    let mut seen = 0usize;
    for (name, encoding) in [
        ("logs_fixed_width_report_utf8.txt", None),
        ("logs_fixed_width_report_ascii.txt", None),
        ("logs_fixed_width_report_cp1252.txt", Some("windows-1252".to_string())),
    ] {
        let p = testdata().join(name);
        if !p.exists() {
            continue;
        }
        let s = ParseSpec {
            extraction: Extraction::FixedWidth { encoding, fields: fields.clone() },
            transforms: vec![Transform::DropRowsMatching {
                pattern: "^(-+|=+)?$".into(),
                column: None,
            }],
            columns: vec![
                col("kunde", DType::Utf8),
                col("land", DType::Utf8),
                col("bemerkung", DType::Utf8),
            ],
            confidence: Some(1.0),
            notes: vec![],
        };
        assert!(stream::can_stream(&s), "{name}: fixed_width was not streamable");
        assert_paths_agree(&s, &p, name);
        seen += 1;
    }
    assert!(seen > 0, "no fixed-width report fixture was found");
    eprintln!("compared {seen} fixed-width reports down both paths");
}

// ---------------------------------------------------------------------------
// The places a chunked pipeline goes wrong
// ---------------------------------------------------------------------------

fn write(dir: &TempDir, name: &str, body: &str) -> PathBuf {
    let p = dir.path().join(name);
    std::fs::write(&p, body).unwrap();
    p
}

fn delimited() -> Extraction {
    Extraction::Delimited {
        delimiter: ',',
        quote: Some('"'),
        escape: None,
        encoding: None,
        comment: None,
        ragged: RaggedPolicy::PadNulls,
    }
}

fn spec(transforms: Vec<Transform>, columns: Vec<ColumnSpec>) -> ParseSpec {
    ParseSpec {
        extraction: delimited(),
        transforms,
        columns,
        confidence: Some(1.0),
        notes: vec![],
    }
}

fn col(name: &str, dtype: DType) -> ColumnSpec {
    ColumnSpec {
        name: name.into(),
        source: None,
        dtype,
        nullable: true,
        parse: ValueParsing::default(),
    }
}

/// `fill_down` carries a value from one row to the next. Across a batch
/// boundary the carry has to survive, and a chunked implementation that
/// starts each batch with an empty carry loses exactly one value per
/// 65,536 rows — a corruption rare enough to survive any small test.
#[test]
fn fill_down_carries_across_a_batch_boundary() {
    let dir = TempDir::new().unwrap();
    let mut body = String::from("grp,val\n");
    // Well past BATCH_ROWS, with the group named once and never again.
    body.push_str("Ost,1\n");
    for i in 2..70_000 {
        body.push_str(&format!(",{i}\n"));
    }
    let p = write(&dir, "carry.csv", &body);

    let s = spec(
        vec![
            Transform::PromoteHeader { rows: 1, join: " ".into() },
            Transform::FillDown { columns: vec!["grp".into()] },
        ],
        vec![col("grp", DType::Utf8), col("val", DType::Int64)],
    );
    assert_paths_agree(&s, &p, "fill_down across a batch boundary");

    // And say what the right answer actually is, not just that both agree.
    let batches = stream::execute_batches(&s, &p, Limits::default()).unwrap();
    assert!(batches.len() > 1, "test did not span more than one batch");
    let text = render(&batches);
    assert!(!text.contains("|  |"), "a blank group survived the fill");
}

/// A `skip_rows` tail is the end of the file, which a streaming reader has
/// not seen yet when it starts. Dropping the wrong rows here would silently
/// delete data or keep a total in.
#[test]
fn a_skip_rows_tail_drops_the_end_not_the_middle() {
    let dir = TempDir::new().unwrap();
    let mut body = String::from("Report\nk,v\n");
    for i in 1..1000 {
        body.push_str(&format!("r{i},{i}\n"));
    }
    body.push_str("Total,999999\n");
    let p = write(&dir, "tail.csv", &body);

    let s = spec(
        vec![
            Transform::SkipRows { head: 1, tail: 1 },
            Transform::PromoteHeader { rows: 1, join: " ".into() },
        ],
        vec![col("k", DType::Utf8), col("v", DType::Int64)],
    );
    assert_paths_agree(&s, &p, "skip_rows tail");

    let text = render(&stream::execute_batches(&s, &p, Limits::default()).unwrap());
    assert!(!text.contains("999999"), "the Total row survived");
    assert!(text.contains("r999"), "the last real row was dropped instead");
}

/// `unpivot` turns one input row into several output rows, so input and
/// output row counts diverge and the batch boundary must follow the output.
#[test]
fn unpivot_expands_rows_without_losing_any_at_a_boundary() {
    let dir = TempDir::new().unwrap();
    let mut body = String::from("region,jan,feb\n");
    for i in 0..40_000 {
        body.push_str(&format!("r{i},{i},{}\n", i + 1));
    }
    let p = write(&dir, "wide.csv", &body);

    let s = spec(
        vec![
            Transform::PromoteHeader { rows: 1, join: " ".into() },
            Transform::Unpivot {
                id_columns: vec!["region".into()],
                value_columns: vec!["jan".into(), "feb".into()],
                variable_name: "monat".into(),
                value_name: "betrag".into(),
            },
        ],
        vec![
            col("region", DType::Utf8),
            col("monat", DType::Utf8),
            col("betrag", DType::Int64),
        ],
    );
    assert_paths_agree(&s, &p, "unpivot across a batch boundary");

    let batches = stream::execute_batches(&s, &p, Limits::default()).unwrap();
    let rows: usize = batches.iter().map(|b| b.num_rows()).sum();
    assert_eq!(rows, 80_000, "unpivot lost or duplicated rows");
}

/// A row dropped by `drop_rows_matching` must not take the fill_down carry
/// with it: the value has to reach the row *after* the dropped one, across
/// the gap the drop leaves.
#[test]
fn a_dropped_row_does_not_break_the_fill_down_carry() {
    let dir = TempDir::new().unwrap();
    let body = "grp,val\nOst,1\nZwischensumme,99\n,2\n,3\n";
    let p = write(&dir, "drop.csv", body);

    // Drop first, then fill: the subtotal row is gone before the carry runs,
    // so "Ost" reaches the rows beneath it.
    let s = spec(
        vec![
            Transform::PromoteHeader { rows: 1, join: " ".into() },
            Transform::DropRowsMatching {
                pattern: "^Zwischensumme$".into(),
                column: Some("grp".into()),
            },
            Transform::FillDown { columns: vec!["grp".into()] },
        ],
        vec![col("grp", DType::Utf8), col("val", DType::Int64)],
    );
    assert_paths_agree(&s, &p, "drop before fill_down");

    let text = render(&stream::execute_batches(&s, &p, Limits::default()).unwrap());
    assert!(!text.contains("99"), "the subtotal row survived");
    assert_eq!(text.matches("Ost").count(), 3, "the carry did not reach past the drop");
}

/// Transform order is not cosmetic, and the streaming driver must honour the
/// order the spec gives rather than a convenient fixed one.
///
/// The same two transforms in the two orders mean different things: filling
/// first propagates the subtotal's own label into the blank rows beneath it,
/// so the drop then matches those too and takes three rows instead of one.
/// That is a real footgun in spec authoring; what matters here is that both
/// executors fall into it identically.
#[test]
fn fill_then_drop_and_drop_then_fill_are_different_and_both_paths_know_it() {
    let dir = TempDir::new().unwrap();
    let body = "grp,val\nOst,1\nZwischensumme,99\n,2\n,3\n";
    let p = write(&dir, "order.csv", body);

    let fill = Transform::FillDown { columns: vec!["grp".into()] };
    let drop = Transform::DropRowsMatching {
        pattern: "^Zwischensumme$".into(),
        column: Some("grp".into()),
    };
    let head = Transform::PromoteHeader { rows: 1, join: " ".into() };
    let cols = || vec![col("grp", DType::Utf8), col("val", DType::Int64)];

    let fill_first = spec(vec![head.clone(), fill.clone(), drop.clone()], cols());
    let drop_first = spec(vec![head, drop, fill], cols());
    assert_paths_agree(&fill_first, &p, "fill then drop");
    assert_paths_agree(&drop_first, &p, "drop then fill");

    let a = render(&stream::execute_batches(&fill_first, &p, Limits::default()).unwrap());
    let b = render(&stream::execute_batches(&drop_first, &p, Limits::default()).unwrap());
    assert_ne!(a, b, "the two orders produced the same table — order was ignored");
    assert_eq!(a.matches("Ost").count(), 1, "fill-then-drop should keep only Ost");
    assert_eq!(b.matches("Ost").count(), 3, "drop-then-fill should carry Ost down");
}

/// An empty file still has to produce a schema, or a query over it fails
/// with a shape error instead of returning no rows.
#[test]
fn a_header_only_file_still_produces_one_empty_batch() {
    let dir = TempDir::new().unwrap();
    let p = write(&dir, "empty.csv", "k,v\n");
    let s = spec(
        vec![Transform::PromoteHeader { rows: 1, join: " ".into() }],
        vec![col("k", DType::Utf8), col("v", DType::Int64)],
    );
    assert_paths_agree(&s, &p, "header-only file");
    let batches = stream::execute_batches(&s, &p, Limits::default()).unwrap();
    assert_eq!(batches.len(), 1);
    assert_eq!(batches[0].num_rows(), 0);
    assert_eq!(batches[0].num_columns(), 2);
}

/// A ragged file under `ragged = "error"` must be refused by both paths, and
/// the streaming one must still name the offending row rather than shrugging.
#[test]
fn a_ragged_file_is_refused_with_the_offending_row() {
    let dir = TempDir::new().unwrap();
    let p = write(&dir, "ragged.csv", "k,v\na,1\nb,2,3\nc,3\n");
    let s = ParseSpec {
        extraction: Extraction::Delimited {
            delimiter: ',',
            quote: Some('"'),
            escape: None,
            encoding: None,
            comment: None,
            ragged: RaggedPolicy::Error,
        },
        transforms: vec![Transform::PromoteHeader { rows: 1, join: " ".into() }],
        columns: vec![col("k", DType::Utf8), col("v", DType::Int64)],
        confidence: Some(1.0),
        notes: vec![],
    };
    let err = stream::execute_batches(&s, &p, Limits::default())
        .expect_err("a ragged file was accepted under ragged = error");
    let msg = format!("{err:#}");
    assert!(msg.contains("row 3"), "error does not name the offending row: {msg}");
    assert!(msg.contains("3 field"), "error does not give the arity: {msg}");
}

/// The cell limit has to apply on this path too, and before the work is done.
#[test]
fn the_cell_limit_still_applies_when_streaming() {
    let dir = TempDir::new().unwrap();
    let mut body = String::from("k,v\n");
    for i in 0..1000 {
        body.push_str(&format!("r{i},{i}\n"));
    }
    let p = write(&dir, "limited.csv", &body);
    let s = spec(
        vec![Transform::PromoteHeader { rows: 1, join: " ".into() }],
        vec![col("k", DType::Utf8), col("v", DType::Int64)],
    );
    let tight = Limits { max_streamed_cells: 100, ..Limits::default() };
    let err = stream::execute_batches(&s, &p, tight).expect_err("the cell limit did not apply");
    assert!(format!("{err:#}").contains("max_streamed_cells"), "{err:#}");
}

/// The streaming limit must also bite on a source that needs no measuring
/// pass — a log with no `skip_rows` tail is read exactly once, so a check
/// that lived only in the counting pass would never run.
#[test]
fn the_streaming_limit_applies_to_a_source_with_no_counting_pass() {
    let dir = TempDir::new().unwrap();
    let mut body = String::new();
    for i in 0..500 {
        body.push_str(&format!("2026-02-10 03:14:0{} INFO thing happened n={i}\n", i % 10));
    }
    let p = write(&dir, "app.log", &body);
    let s = ParseSpec {
        extraction: Extraction::Lines {
            pattern: r"^(?P<ts>\S+ \S+) (?P<level>\w+) (?P<msg>.*)$".into(),
            encoding: None,
            on_no_match: NoMatchPolicy::Skip,
        },
        transforms: vec![],
        columns: vec![col("level", DType::Utf8)],
        confidence: Some(1.0),
        notes: vec![],
    };
    // No tail and a header from the pattern: nothing counted this in advance.
    assert!(stream::can_stream(&s));
    let tight = Limits { max_streamed_cells: 30, ..Limits::default() };
    let err = stream::execute_batches(&s, &p, tight)
        .expect_err("the streaming limit did not apply without a counting pass");
    assert!(format!("{err:#}").contains("max_streamed_cells"), "{err:#}");
}

/// Specs the streaming driver does not implement must be *declined*, not
/// mis-executed — the caller falls back to the materialising path.
#[test]
fn unstreamable_shapes_are_declined_rather_than_guessed() {
    // A JSON document has to be parsed whole before its records exist.
    let json = ParseSpec {
        extraction: Extraction::Json { lines: false, pointer: None },
        transforms: vec![],
        columns: vec![],
        confidence: Some(1.0),
        notes: vec![],
    };
    assert!(!stream::can_stream(&json));

    // Excel is materialised by its reader whatever we do.
    let excel = ParseSpec {
        extraction: Extraction::Excel { sheet_name: None, sheet_index: None, range: None },
        transforms: vec![],
        columns: vec![],
        confidence: Some(1.0),
        notes: vec![],
    };
    assert!(!stream::can_stream(&excel));

    // skip_rows after promote_header addresses a different set of rows than
    // the driver's prologue assumes.
    let late_skip = spec(
        vec![
            Transform::PromoteHeader { rows: 1, join: " ".into() },
            Transform::SkipRows { head: 1, tail: 0 },
        ],
        vec![],
    );
    assert!(!stream::can_stream(&late_skip));

    // A transform after unpivot addresses the rewritten header.
    let after_unpivot = spec(
        vec![
            Transform::Unpivot {
                id_columns: vec!["a".into()],
                value_columns: vec!["b".into()],
                variable_name: "k".into(),
                value_name: "v".into(),
            },
            Transform::FillDown { columns: vec!["a".into()] },
        ],
        vec![],
    );
    assert!(!stream::can_stream(&after_unpivot));

    // A line-oriented source already has a header; promoting one over it
    // replaces it with data rows, which the driver does not implement.
    let promote_over_lines = ParseSpec {
        extraction: Extraction::Lines {
            pattern: "(?P<a>.+)".into(),
            encoding: None,
            on_no_match: NoMatchPolicy::Skip,
        },
        transforms: vec![Transform::PromoteHeader { rows: 1, join: " ".into() }],
        columns: vec![],
        confidence: Some(1.0),
        notes: vec![],
    };
    assert!(!stream::can_stream(&promote_over_lines));
}

// ---------------------------------------------------------------------------
// The lazy table provider
// ---------------------------------------------------------------------------
//
// Above a size threshold `messy()` returns a StreamingTable rather than a
// MemTable: the file is re-read on each scan and no batch outlives its
// consumer, so peak memory follows the batch rather than the file. These run
// with TDY_LAZY_ABOVE_BYTES=0, which forces every streamable file down that
// path without needing a 64 MB fixture.

fn tdy_env(args: &[&str], env: &[(&str, &str)]) -> std::process::Output {
    let mut c = std::process::Command::new(env!("CARGO_BIN_EXE_tdy"));
    c.args(args);
    for (k, v) in env {
        c.env(k, v);
    }
    c.output().expect("run tdy")
}

fn sample_file(dir: &TempDir) -> PathBuf {
    let mut body = String::from("region,menge\n");
    for i in 0..5000 {
        body.push_str(&format!("{},{i}\n", ["Ost", "West"][i % 2]));
    }
    write(dir, "lazy.csv", &body)
}

/// The lazy provider must answer exactly what the cached one answers.
#[test]
fn the_lazy_provider_gives_the_same_answers_as_the_cached_one() {
    let dir = TempDir::new().unwrap();
    let p = sample_file(&dir);
    let sql = format!(
        "SELECT region, count(*) n, sum(menge) s FROM messy('{}') GROUP BY region ORDER BY region",
        p.display()
    );

    let eager = tdy_env(&["query", &sql], &[("TDY_LAZY_ABOVE_BYTES", "999999999")]);
    let lazy = tdy_env(&["query", &sql], &[("TDY_LAZY_ABOVE_BYTES", "0")]);
    assert!(eager.status.success(), "{}", String::from_utf8_lossy(&eager.stderr));
    assert!(lazy.status.success(), "{}", String::from_utf8_lossy(&lazy.stderr));
    assert_eq!(
        String::from_utf8_lossy(&eager.stdout),
        String::from_utf8_lossy(&lazy.stdout),
        "the lazy provider disagreed with the cached one"
    );
}

/// Row order has to survive being produced by a background task: `--frozen`
/// promising "same file, same answer" is worth nothing if a scan can reorder.
#[test]
fn the_lazy_provider_keeps_file_order() {
    let dir = TempDir::new().unwrap();
    let p = sample_file(&dir);
    let sql = format!("SELECT menge FROM messy('{}')", p.display());
    let first = tdy_env(&["query", &sql], &[("TDY_LAZY_ABOVE_BYTES", "0")]);
    assert!(first.status.success());
    for _ in 0..3 {
        let again = tdy_env(&["query", &sql], &[("TDY_LAZY_ABOVE_BYTES", "0")]);
        assert_eq!(first.stdout, again.stdout, "the lazy scan reordered rows between runs");
    }
    let eager = tdy_env(&["query", &sql], &[("TDY_LAZY_ABOVE_BYTES", "999999999")]);
    assert_eq!(first.stdout, eager.stdout, "lazy and cached disagreed on row order");
}

/// The failure that would be worst: a parse error raised inside the producer
/// task, after some batches have already been handed over. If it were
/// swallowed, the query would return the rows read so far and look like a
/// short file — a silently wrong answer, which is the one thing tdy must not
/// do. It has to fail loudly instead.
#[test]
fn an_error_after_the_first_batch_fails_the_query_rather_than_truncating() {
    let dir = TempDir::new().unwrap();
    // Well over one batch, with an unparseable integer far into the file.
    let mut body = String::from("k,v\n");
    for i in 0..70_000 {
        body.push_str(&format!("r{i},{i}\n"));
    }
    body.push_str("bad,not-a-number\n");
    let p = write(&dir, "late_error.csv", &body);

    // A spec that insists v is an integer, so the bad row is fatal.
    let s = spec(
        vec![Transform::PromoteHeader { rows: 1, join: " ".into() }],
        vec![col("k", DType::Utf8), col("v", DType::Int64)],
    );
    let sidecar = p.with_file_name("late_error.csv.tdy.toml");
    std::fs::write(
        &sidecar,
        toml::to_string(&serde_json::json!({
            "spec_version": 1,
            "source": { "path": "late_error.csv", "blake3": "0", "bytes": 0 },
            "provenance": {
                "method": "manual",
                "tool_version": "0.1.0",
                "created_at": "2026-01-01T00:00:00Z"
            },
            "spec": serde_json::to_value(&s).unwrap(),
        }))
        .unwrap(),
    )
    .unwrap();
    let stamp = tdy_env(&["validate", p.to_str().unwrap(), "--stamp"], &[]);
    assert!(
        stamp.status.success(),
        "could not stamp the hand-written spec: {}",
        String::from_utf8_lossy(&stamp.stderr)
    );

    let out = tdy_env(
        &["query", &format!("SELECT count(*) FROM messy('{}')", p.display())],
        &[("TDY_LAZY_ABOVE_BYTES", "0")],
    );
    assert!(
        !out.status.success(),
        "a mid-file parse error was swallowed; the query returned:\n{}",
        String::from_utf8_lossy(&out.stdout)
    );
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        err.contains("not-a-number") || err.contains("row"),
        "the error does not say what went wrong:\n{err}"
    );
}

/// A LIMIT closes the receiver early. The producer must notice and stop
/// rather than block forever on a full channel or report a spurious failure.
#[test]
fn a_limit_stops_the_producer_without_an_error() {
    let dir = TempDir::new().unwrap();
    let mut body = String::from("k,v\n");
    for i in 0..200_000 {
        body.push_str(&format!("r{i},{i}\n"));
    }
    let p = write(&dir, "limited_scan.csv", &body);
    let out = tdy_env(
        &["query", &format!("SELECT k FROM messy('{}') LIMIT 3", p.display())],
        &[("TDY_LAZY_ABOVE_BYTES", "0")],
    );
    assert!(
        out.status.success(),
        "a LIMIT over the lazy provider failed:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("r0"), "no rows came back:\n{text}");
    assert!(!text.contains("r100"), "LIMIT was not applied:\n{text}");
}

/// A batch is bounded by cells, not rows, because a row is as wide as the
/// file. 65,536 rows of a 1,000-column file is 65 million strings — a 134 MB
/// fixture measured at 4.2 GB before this was fixed, since width was the one
/// dimension nothing bounded.
///
/// Memory cannot be asserted from inside a test, so what is pinned here is
/// the mechanism: a wide file must come back in many small batches rather
/// than a few enormous ones, and must still say exactly what a narrow file
/// says.
#[test]
fn a_wide_file_is_cut_into_batches_by_cells_not_rows() {
    let dir = TempDir::new().unwrap();
    const COLS: usize = 512;
    const ROWS: usize = 9_000;
    let mut body = (0..COLS).map(|i| format!("c{i}")).collect::<Vec<_>>().join(",");
    body.push('\n');
    let row = vec!["7"; COLS].join(",");
    for _ in 0..ROWS {
        body.push_str(&row);
        body.push('\n');
    }
    let p = write(&dir, "wide.csv", &body);

    let s = spec(
        vec![Transform::PromoteHeader { rows: 1, join: " ".into() }],
        vec![col("c0", DType::Int64), col("c511", DType::Int64)],
    );
    let batches = stream::execute_batches(&s, &p, Limits::default()).unwrap();

    let rows: usize = batches.iter().map(|b| b.num_rows()).sum();
    assert_eq!(rows, ROWS, "rows were lost or duplicated");

    // 9,000 rows is well under BATCH_ROWS, so a row-bounded implementation
    // would return exactly one batch holding 4.6M cells.
    assert!(
        batches.len() > 1,
        "a {COLS}-column file came back in one batch — the bound is still on rows"
    );
    let widest = batches.iter().map(|b| b.num_rows()).max().unwrap_or(0);
    assert!(
        widest * COLS <= 1 << 21,
        "a batch held {} cells, far past the cell bound",
        widest * COLS
    );

    // And the values are still right.
    assert_paths_agree(&s, &p, "a wide file");
}
