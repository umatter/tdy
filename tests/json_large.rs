//! A JSON document over 4 MiB.
//!
//! A capped read (the sniffer's probe, `fit`'s gates, `preview`, `dry_run`,
//! `profile --head`) reads a 4 MiB prefix of a text file and drops the torn
//! last line. That is right for delimited text and NDJSON, whose records are
//! lines. A JSON *document* has no records until it is parsed whole, and a
//! prefix of one is malformed JSON — so every capped path failed with "EOF
//! while parsing" on a document a plain query read without complaint. A
//! document is now parsed whole under a cap, as a workbook is, bounded by
//! `[limits].max_file_bytes`; NDJSON keeps the prefix.
//!
//! The documents are generated here, in a temp dir: five megabytes do not
//! belong in `testdata/`.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use tempfile::TempDir;

use tdy::config::Limits;
use tdy::engine::{self, ExtractOpts};
use tdy::spec::*;

const MIB: usize = 1024 * 1024;

/// Records in the generated array: enough for ~5.5 MiB.
const N: u64 = 50_000;

/// `{"id":i,"region":"…","amount":i % 100,"memo":"…"}` — the sums are
/// what a wrong reading would get wrong.
fn record(i: u64) -> String {
    format!(
        r#"{{"id":{i},"region":"{}","amount":{},"memo":"padding padding padding padding padding padding padding {i:08}"}}"#,
        ["ZH", "BE", "TI"][(i % 3) as usize],
        i % 100
    )
}

fn amount_sum() -> u64 {
    (0..N).map(|i| i % 100).sum()
}

fn array_text() -> String {
    let mut s = String::from("[\n");
    for i in 0..N {
        if i > 0 {
            s.push_str(",\n");
        }
        s.push_str(&record(i));
    }
    s.push_str("\n]\n");
    assert!(s.len() > 5 * MIB, "the array is only {} bytes", s.len());
    s
}

/// The same records under an envelope that also holds a short array: the
/// frame is found by elimination over the record arrays and the record.
fn enveloped_text() -> String {
    format!(r#"{{"meta":{{"source":"export"}},"notes":[{{"k":"a"}},{{"k":"b"}}],"rows":{}}}"#, array_text())
}

/// One object whose one long string pushes it past 4 MiB, beside a short
/// array of objects the record frame has to eliminate.
fn record_text() -> String {
    let long = "x".repeat(5 * MIB);
    format!(r#"{{"id":"big-1","n":7,"blob":"{long}","parts":[{{"p":1}},{{"p":2}}]}}"#)
}

fn write(dir: &Path, name: &str, body: &str) -> PathBuf {
    let p = dir.join(name);
    std::fs::write(&p, body).unwrap();
    p
}

/// tdy with a config directory of the test's own, so neither the machine's
/// config nor a sibling test's limits reach it.
fn tdy(config: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_tdy"))
        .env("XDG_CONFIG_HOME", config)
        .env("TDY_BACKEND", "none")
        .args(args)
        .output()
        .expect("run tdy")
}

fn ok(out: &Output) -> String {
    let text = String::from_utf8_lossy(&out.stdout).to_string();
    assert!(out.status.success(), "{text}{}", String::from_utf8_lossy(&out.stderr));
    text
}

/// The cells of the first result row that contains `needle`.
fn row_with(text: &str, needle: &str) -> Vec<String> {
    let row = text.lines().find(|l| l.contains(needle)).unwrap_or_else(|| panic!("{text}"));
    row.split('|').map(str::trim).filter(|c| !c.is_empty()).map(String::from).collect()
}

fn col(name: &str, dtype: DType) -> ColumnSpec {
    ColumnSpec { name: name.into(), source: None, dtype, nullable: true, parse: ValueParsing::default(), pointer: None }
}

fn spec(extraction: Extraction, columns: Vec<ColumnSpec>) -> ParseSpec {
    ParseSpec { extraction, transforms: vec![], columns, confidence: None, notes: vec![] }
}

fn array_spec(pointer: Option<&str>) -> ParseSpec {
    spec(
        Extraction::Json { lines: false, pointer: pointer.map(String::from), record: false },
        vec![col("id", DType::Int64), col("region", DType::Utf8), col("amount", DType::Int64)],
    )
}

fn record_spec() -> ParseSpec {
    spec(
        Extraction::Json { lines: false, pointer: None, record: true },
        vec![col("id", DType::Utf8), col("n", DType::Int64)],
    )
}

const ARRAY_TARGET: &str = "CREATE TABLE rows (\n  id BIGINT NOT NULL,\n  region TEXT NOT NULL,\n  amount BIGINT NOT NULL\n)\nWITH (files = '*.json*');\n";
const RECORD_TARGET: &str = "CREATE TABLE docs (\n  id TEXT NOT NULL,\n  n BIGINT NOT NULL\n)\nWITH (files = '*.json');\n";

// ---------------------------------------------------------------------------
// The capped reads, in the library
// ---------------------------------------------------------------------------

#[test]
fn a_capped_read_of_an_array_document_parses_it_whole_and_says_it_cut() {
    let dir = TempDir::new().unwrap();
    let p = write(dir.path(), "big.json", &array_text());
    let t = engine::extract(&array_spec(None).extraction, &p, &ExtractOpts::capped(Limits::default(), 50)).unwrap();
    assert_eq!(t.rows.len(), 50);
    assert!(t.truncated, "a cut array is a truncated table");
    // A cap the array fits under is not a cut.
    let all = engine::extract(&array_spec(None).extraction, &p, &ExtractOpts::capped(Limits::default(), N as usize)).unwrap();
    assert_eq!(all.rows.len(), N as usize);
    assert!(!all.truncated);

    let b = engine::preview(&array_spec(None), &p, Limits::default(), 10).unwrap();
    assert_eq!(b.num_rows(), 10);
    engine::dry_run(&array_spec(None), &p, Limits::default()).unwrap();
}

#[test]
fn a_capped_read_of_an_array_under_a_pointer_works_past_the_prefix() {
    let dir = TempDir::new().unwrap();
    let p = write(dir.path(), "env.json", &enveloped_text());
    let b = engine::preview(&array_spec(Some("/rows")), &p, Limits::default(), 10).unwrap();
    assert_eq!(b.num_rows(), 10);
    engine::dry_run(&array_spec(Some("/rows")), &p, Limits::default()).unwrap();
}

#[test]
fn a_capped_read_of_a_record_document_reads_the_one_row() {
    let dir = TempDir::new().unwrap();
    let p = write(dir.path(), "doc.json", &record_text());
    let t = engine::extract(&record_spec().extraction, &p, &ExtractOpts::capped(Limits::default(), 50)).unwrap();
    assert_eq!(t.rows.len(), 1);
    assert!(!t.truncated, "one record under a cap of 50 is the whole table");
    let b = engine::preview(&record_spec(), &p, Limits::default(), 10).unwrap();
    assert_eq!(b.num_rows(), 1);
    engine::dry_run(&record_spec(), &p, Limits::default()).unwrap();
}

/// NDJSON's records are lines, and a preview still reads a prefix of them:
/// a malformed last megabyte is past the probe, and only the whole read
/// meets it — and names its line.
#[test]
fn ndjson_keeps_the_prefix() {
    let dir = TempDir::new().unwrap();
    let mut body = String::new();
    let mut lines = 0u64;
    while body.len() < 5 * MIB {
        body.push_str(&record(lines));
        body.push('\n');
        lines += 1;
    }
    let bad_line = lines + 1;
    while body.len() < 6 * MIB {
        body.push_str("{\"id\": oops\n");
    }
    let p = write(dir.path(), "big.ndjson", &body);
    let s = spec(
        Extraction::Json { lines: true, pointer: None, record: false },
        vec![col("id", DType::Int64), col("region", DType::Utf8), col("amount", DType::Int64)],
    );
    let b = engine::preview(&s, &p, Limits::default(), 10).unwrap();
    assert_eq!(b.num_rows(), 10);
    engine::dry_run(&s, &p, Limits::default()).unwrap();
    let err = format!("{:#}", engine::execute(&s, &p, Limits::default()).expect_err("the malformed tail was not read"));
    assert!(err.contains(&format!("line {bad_line}")), "{err}");
}

/// A document over `max_file_bytes` is refused by name under a cap as on
/// the whole read — never parsed as a prefix that then fails as malformed.
#[test]
fn a_document_over_the_limit_is_refused_naming_it_in_the_probe_as_in_the_query() {
    let dir = TempDir::new().unwrap();
    let p = write(dir.path(), "big.json", &array_text());
    let tiny = Limits { max_file_bytes: MIB as u64, ..Limits::default() };
    for (what, err) in [
        ("preview", engine::preview(&array_spec(None), &p, tiny, 10).expect_err("preview")),
        ("dry run", engine::dry_run(&array_spec(None), &p, tiny).expect_err("dry run")),
        ("query", engine::execute(&array_spec(None), &p, tiny).expect_err("query")),
    ] {
        let msg = format!("{err:#}");
        assert!(msg.contains("max_file_bytes"), "{what}: {msg}");
        assert!(!msg.contains("EOF"), "{what}: {msg}");
    }
    let rec = write(dir.path(), "doc.json", &record_text());
    let msg = format!("{:#}", engine::preview(&record_spec(), &rec, tiny, 10).expect_err("record preview"));
    assert!(msg.contains("max_file_bytes"), "{msg}");
}

// ---------------------------------------------------------------------------
// The commands
// ---------------------------------------------------------------------------

/// fit (pile and single file) → lock → `dataset()`: every record, the
/// right sum. The enveloped copy is found by elimination over its arrays.
#[test]
fn an_array_document_over_4_mib_fits_and_queries() {
    let cfg = TempDir::new().unwrap();
    let dir = TempDir::new().unwrap();
    write(dir.path(), "big.json", &array_text());
    write(dir.path(), "env.json", &enveloped_text());
    let t = write(dir.path(), "rows.tdy.sql", ARRAY_TARGET);
    let ts = t.to_str().unwrap();

    let single = ok(&tdy(cfg.path(), &["fit", ts, dir.path().join("env.json").to_str().unwrap()]));
    assert!(single.contains("/rows"), "{single}");

    let fit = ok(&tdy(cfg.path(), &["fit", ts]));
    assert!(!fit.contains("GAP") && !fit.contains("REVIEW"), "{fit}");
    let sql = format!("SELECT count(*) n, sum(amount) s FROM dataset('{ts}')");
    let text = ok(&tdy(cfg.path(), &["query", &sql]));
    assert_eq!(row_with(&text, &(2 * N).to_string()), [(2 * N).to_string(), (2 * amount_sum()).to_string()], "{text}");
}

/// The materialised copy of a gzip document is what is read, and it is
/// read whole.
#[test]
fn a_gzipped_array_document_over_4_mib_fits_and_queries() {
    let cfg = TempDir::new().unwrap();
    let dir = TempDir::new().unwrap();
    let gz = dir.path().join("big.json.gz");
    let mut enc = flate2::write::GzEncoder::new(std::fs::File::create(&gz).unwrap(), flate2::Compression::fast());
    enc.write_all(array_text().as_bytes()).unwrap();
    enc.finish().unwrap();
    let t = write(dir.path(), "rows.tdy.sql", ARRAY_TARGET);
    let ts = t.to_str().unwrap();
    ok(&tdy(cfg.path(), &["fit", ts]));
    let sql = format!("SELECT count(*) n, sum(amount) s FROM dataset('{ts}')");
    let text = ok(&tdy(cfg.path(), &["query", &sql]));
    assert_eq!(row_with(&text, &N.to_string()), [N.to_string(), amount_sum().to_string()], "{text}");
}

#[test]
fn a_record_document_over_4_mib_fits_and_queries() {
    let cfg = TempDir::new().unwrap();
    let dir = TempDir::new().unwrap();
    write(dir.path(), "doc.json", &record_text());
    let t = write(dir.path(), "docs.tdy.sql", RECORD_TARGET);
    let ts = t.to_str().unwrap();
    let fit = ok(&tdy(cfg.path(), &["fit", ts]));
    assert!(!fit.contains("GAP") && !fit.contains("REVIEW"), "{fit}");
    let text = ok(&tdy(cfg.path(), &["query", &format!("SELECT count(*) c, max(id) i, sum(n) s FROM dataset('{ts}')")]));
    assert_eq!(row_with(&text, "big-1"), ["1", "big-1", "7"], "{text}");
}

#[test]
fn draft_and_profile_read_a_document_over_4_mib() {
    let cfg = TempDir::new().unwrap();
    let dir = TempDir::new().unwrap();
    let big = write(dir.path(), "big.json", &array_text());
    let doc = write(dir.path(), "doc.json", &record_text());
    let draft = ok(&tdy(cfg.path(), &["draft", big.to_str().unwrap()]));
    assert!(draft.contains("CREATE TABLE") && draft.contains("amount"), "{draft}");
    // A document that also holds an array is drafted through the array
    // unless asked for the record.
    let draft = ok(&tdy(cfg.path(), &["draft", doc.to_str().unwrap()]));
    assert!(draft.contains("/parts"), "{draft}");
    let draft = ok(&tdy(cfg.path(), &["draft", "--records", doc.to_str().unwrap()]));
    assert!(draft.contains("CREATE TABLE") && draft.contains("blob"), "{draft}");

    for args in [vec!["profile", big.to_str().unwrap(), "--head", "100"], vec!["profile", big.to_str().unwrap()]] {
        let out = ok(&tdy(cfg.path(), &args));
        assert!(out.contains("amount"), "{out}");
    }
    // The sniffer's frame for it is the array, which sits past the blob: read
    // only because the document was parsed whole.
    let out = ok(&tdy(cfg.path(), &["profile", doc.to_str().unwrap(), "--head", "100"]));
    assert!(out.contains("record array /parts: 2 rows"), "{out}");
}

/// The probe of a document over the limit names the limit, through the
/// command as through the library.
#[test]
fn fit_names_the_limit_for_a_document_over_it() {
    let cfg = TempDir::new().unwrap();
    std::fs::create_dir_all(cfg.path().join("tdy")).unwrap();
    std::fs::write(cfg.path().join("tdy").join("config.toml"), format!("[limits]\nmax_file_bytes = {}\n", MIB)).unwrap();
    let dir = TempDir::new().unwrap();
    write(dir.path(), "big.json", &array_text());
    let t = write(dir.path(), "rows.tdy.sql", ARRAY_TARGET);
    let out = tdy(cfg.path(), &["fit", t.to_str().unwrap()]);
    let all = format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    assert!(!out.status.success(), "{all}");
    assert!(all.contains("max_file_bytes"), "{all}");
    assert!(!all.contains("EOF while parsing"), "{all}");
}
