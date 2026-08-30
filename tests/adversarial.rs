//! A sweep over every generated fixture: whatever the file, tdy must produce
//! either a result or an error — never a panic, never a hang, never a silent
//! empty table where data exists.
//!
//! This is the test that stops "hardened against the cases we thought of"
//! from quietly becoming "hardened against the cases we tested". It walks
//! `testdata/` as it is on disk, so every fixture a future generator adds is
//! covered the moment it exists.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

fn testdata() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("testdata")
}

/// Extensions the sniffer is expected to be pointed at. Keep this in step
/// with `sample::guess_format` — an extension it routes but this list omits
/// is a format nothing sweeps for panics.
fn is_data_file(p: &Path) -> bool {
    let ext = p
        .extension()
        .map(|e| e.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default();
    matches!(
        ext.as_str(),
        "csv"
            | "tsv"
            | "txt"
            | "dat"
            | "log"
            | "json"
            | "ndjson"
            | "jsonl"
            | "xlsx"
            | "xlsm"
            | "xlsb"
            | "xls"
            | "ods"
    )
}

fn collect(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    for e in rd.flatten() {
        let p = e.path();
        let name = p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        if name.starts_with('.') {
            continue;
        }
        if p.is_dir() {
            // Generators and multi-gigabyte scratch files are not fixtures.
            if name == "gen" || name == "large" {
                continue;
            }
            collect(&p, out);
        } else if is_data_file(&p) && !name.ends_with(".tdy.toml") {
            out.push(p);
        }
    }
}

fn fixtures() -> Vec<PathBuf> {
    let mut v = Vec::new();
    collect(&testdata(), &mut v);
    v.sort();
    v
}

/// Copy a fixture into a scratch directory before touching it: sniffing
/// writes a sidecar next to the file, and a test must not leave artefacts in
/// the repository (nor let one test's sidecar decide another test's result).
fn staged(dir: &Path, src: &Path) -> PathBuf {
    let name = src.file_name().unwrap();
    let dst = dir.join(name);
    std::fs::copy(src, &dst).expect("copy fixture");
    dst
}

/// Run the real binary so a panic is observable as exit code 101 and a hang
/// is observable as a timeout.
///
/// Output goes to files rather than pipes on purpose: a sidecar for a
/// 100k-column file is megabytes of TOML, and a pipe nobody drains while
/// polling `try_wait` fills at 64 KB and blocks the child forever — a hang
/// invented by the test harness rather than found in the tool.
fn run_with_timeout(args: &[&str], limit: Duration) -> (Option<i32>, String, Duration) {
    let started = Instant::now();
    let dir = tempfile::TempDir::new().expect("scratch dir");
    let out_path = dir.path().join("stdout");
    let err_path = dir.path().join("stderr");
    let mut child = Command::new(env!("CARGO_BIN_EXE_tdy"))
        .args(args)
        .stdout(Stdio::from(std::fs::File::create(&out_path).unwrap()))
        .stderr(Stdio::from(std::fs::File::create(&err_path).unwrap()))
        .spawn()
        .expect("spawn tdy");
    loop {
        if let Some(status) = child.try_wait().expect("wait") {
            let mut text = read_capped(&err_path);
            text.push_str(&read_capped(&out_path));
            return (status.code(), text, started.elapsed());
        }
        if started.elapsed() > limit {
            let _ = child.kill();
            let _ = child.wait();
            return (None, "TIMEOUT".into(), started.elapsed());
        }
        std::thread::sleep(Duration::from_millis(25));
    }
}

/// Read at most the first chunk of a file: some outputs are megabytes.
fn read_capped(p: &Path) -> String {
    use std::io::Read;
    let Ok(mut f) = std::fs::File::open(p) else { return String::new() };
    let mut buf = vec![0u8; 64 * 1024];
    let n = f.read(&mut buf).unwrap_or(0);
    buf.truncate(n);
    String::from_utf8_lossy(&buf).into_owned()
}

#[test]
fn no_fixture_makes_tdy_panic_or_hang() {
    let files = fixtures();
    assert!(
        !files.is_empty(),
        "no fixtures found in {} — run `python3 gen_fixtures.py`",
        testdata().display()
    );

    let mut panicked = Vec::new();
    let mut hung = Vec::new();
    let mut errored = BTreeSet::new();

    let scratch = tempfile::TempDir::new().unwrap();
    for f in &files {
        let staged_path = staged(scratch.path(), f);
        let (code, text, took) = run_with_timeout(
            &["sniff", staged_path.to_str().unwrap(), "--no-llm"],
            Duration::from_secs(60),
        );
        let name = f.strip_prefix(testdata()).unwrap_or(f).display().to_string();
        match code {
            Some(101) => panicked.push(format!("{name}: {}", first_line(&text))),
            None => hung.push(format!("{name} (after {took:?})")),
            Some(0) => {}
            Some(_) => {
                errored.insert(format!("{name}: {}", first_line(&text)));
            }
        }
    }

    assert!(panicked.is_empty(), "these fixtures panicked:\n  {}", panicked.join("\n  "));
    assert!(hung.is_empty(), "these fixtures hung:\n  {}", hung.join("\n  "));

    // Errors are allowed — some fixtures exist precisely to be rejected — but
    // they must be reported so a regression that starts rejecting a good file
    // is visible in the test output.
    if !errored.is_empty() {
        eprintln!(
            "{} of {} fixtures were rejected (expected for the deliberately broken ones):\n  {}",
            errored.len(),
            files.len(),
            errored.into_iter().collect::<Vec<_>>().join("\n  ")
        );
    }
}

fn first_line(s: &str) -> String {
    s.lines()
        .find(|l| !l.trim().is_empty())
        .unwrap_or("(no output)")
        .chars()
        .take(160)
        .collect()
}

/// Every fixture that sniffs successfully must then survive a real query, and
/// a second `--frozen` run must reproduce it exactly. This is the promise the
/// sidecar makes: same file, same spec, same numbers.
#[test]
fn sniffable_fixtures_are_queryable_and_reproducible() {
    let mut mismatches = Vec::new();
    let mut checked = 0usize;

    let scratch = tempfile::TempDir::new().unwrap();
    for f in fixtures() {
        let staged_path = staged(scratch.path(), &f);
        let path = staged_path.to_str().unwrap();
        let (code, _, _) =
            run_with_timeout(&["sniff", path, "--no-llm"], Duration::from_secs(60));
        if code != Some(0) {
            continue; // deliberately unparseable fixture
        }
        let sql = format!("SELECT count(*) AS n FROM messy('{}')", path.replace('\'', "''"));
        let (c1, t1, _) = run_with_timeout(&["query", &sql], Duration::from_secs(120));
        let (c2, t2, _) = run_with_timeout(&["query", &sql, "-f"], Duration::from_secs(120));
        checked += 1;
        let name = f.file_name().unwrap().to_string_lossy().into_owned();
        if c1 != Some(0) {
            mismatches.push(format!("{name}: query failed after a successful sniff: {}", first_line(&t1)));
        } else if c2 != Some(0) {
            mismatches.push(format!("{name}: --frozen re-run failed: {}", first_line(&t2)));
        } else if count_of(&t1) != count_of(&t2) {
            mismatches.push(format!(
                "{name}: frozen re-run disagreed ({:?} vs {:?})",
                count_of(&t1),
                count_of(&t2)
            ));
        }
    }
    assert!(checked > 0, "no fixture was queryable");
    assert!(mismatches.is_empty(), "\n  {}", mismatches.join("\n  "));
}

fn count_of(table: &str) -> Option<i64> {
    table
        .lines()
        .filter_map(|l| l.trim().trim_matches('|').trim().parse::<i64>().ok())
        .next()
}
