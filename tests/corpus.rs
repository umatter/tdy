//! tdy against messy data **other people made**.
//!
//! Every fixture in `testdata/` is one I invented, and a fixture you wrote
//! yourself tests your imagination as much as your parser. `scripts/
//! download_corpus.sh` clones twenty-six public data-wrangling exercise
//! repositories — Data Carpentry's deliberately-messy spreadsheet lessons, the
//! OpenRefine cleaning sets, `OxfordIHTM/messy-data`, course labs, pandas
//! exercise pools — into `corpus/`, which is gitignored and never vendored.
//!
//! Run it, then run these:
//!
//! ```text
//! ./scripts/download_corpus.sh corpus
//! TDY_CORPUS=corpus cargo test --test corpus -- --nocapture
//! ```
//!
//! Without `TDY_CORPUS` every test here skips, which is what CI sees and what
//! an ordinary `cargo test` sees.
//!
//! # What these can and cannot assert
//!
//! There is no ground truth for somebody else's exercise data — nobody has
//! told us what `messy_data_2.xlsx` is supposed to contain. So this file does
//! **not** claim tdy reads these files correctly. It asserts the properties
//! that hold without ground truth:
//!
//! * tdy never panics, never hangs, and never aborts on any of them;
//! * anything it claims to have sniffed confidently, it can also execute and
//!   reproduce under `--frozen`;
//! * a file it declines is declined with a sentence, not a crash.
//!
//! And it prints a survey — how much of real messy data tdy reads unaided,
//! and what it says about the rest. That number is the useful output: it is
//! the honest measure of the heuristic tier, and every file in the "declined"
//! column is a candidate for the next fixture.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// Opt-in, like `tests/live_backend.rs`: an explicit `TDY_CORPUS` rather than
/// "a corpus/ directory exists". Seven gigabytes of somebody else's data is
/// not something an ordinary `cargo test` should walk, and a suite whose cost
/// depends on what happens to be on disk is a suite people stop running.
fn corpus_dir() -> Option<PathBuf> {
    let p = PathBuf::from(std::env::var("TDY_CORPUS").ok()?);
    p.is_dir().then_some(p)
}

/// Extensions tdy claims to read. Deliberately the same list as
/// `sample::guess_format` routes, so a format added there and missed here
/// shows up as a suspiciously small corpus.
fn is_data(p: &Path) -> bool {
    let ext = p
        .extension()
        .map(|e| e.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default();
    matches!(
        ext.as_str(),
        "csv" | "tsv" | "txt" | "dat" | "log" | "json" | "ndjson" | "jsonl"
            | "xlsx" | "xlsm" | "xlsb" | "xls" | "ods"
    )
}

fn collect(dir: &Path, out: &mut Vec<PathBuf>, depth: usize) {
    if depth > 12 {
        return;
    }
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    for e in rd.flatten() {
        let p = e.path();
        let name = e.file_name().to_string_lossy().to_string();
        if p.is_dir() {
            // Upstream git metadata and virtualenvs are not data.
            if matches!(name.as_str(), ".git" | ".ipynb_checkpoints" | "node_modules" | ".venv") {
                continue;
            }
            collect(&p, out, depth + 1);
        } else if is_data(&p) && !name.ends_with(".tdy.toml") {
            out.push(p);
        }
    }
}

fn files() -> Vec<PathBuf> {
    let Some(dir) = corpus_dir() else { return Vec::new() };
    let mut v = Vec::new();
    collect(&dir, &mut v, 0);
    v.sort();
    v
}

fn skip_notice() -> bool {
    if corpus_dir().is_none() {
        eprintln!(
            "skipping: set TDY_CORPUS=<dir> to test against real messy data \
             (./scripts/download_corpus.sh corpus)"
        );
        return true;
    }
    false
}

/// What tdy made of one file.
#[derive(Debug, Clone, PartialEq)]
enum Outcome {
    /// Sniffed, executed, and reproducible.
    Read { rows: usize, cols: usize, confidence: f32 },
    /// Sniffed, but tdy said it was unsure. Not a failure — saying so is the
    /// designed behaviour — but counted separately, because "read it" and
    /// "read it and told you it might be wrong" are different claims.
    LowConfidence { confidence: f32, notes: Vec<String> },
    /// Declined with a message. Also not a failure.
    Declined(String),
}

fn examine(p: &Path) -> Outcome {
    use tdy::config::Limits;
    let lim = Limits::default();

    let sample = match tdy::sample::build(p, 16 * 1024, lim) {
        Ok(s) => s,
        Err(e) => return Outcome::Declined(format!("{e:#}")),
    };
    let res = match tdy::sniff::sniff(p, &sample, lim) {
        Ok(r) => r,
        Err(e) => return Outcome::Declined(format!("{e:#}")),
    };
    let confidence = res.spec.confidence.unwrap_or(0.0);
    if let Err(e) = res.spec.validate() {
        return Outcome::Declined(format!("sniffed an invalid spec: {e:?}"));
    }
    match tdy::provider::spec_to_batch(&res.spec, p) {
        Ok(b) => {
            if confidence < 0.8 {
                Outcome::LowConfidence { confidence, notes: res.spec.notes.clone() }
            } else {
                Outcome::Read { rows: b.num_rows(), cols: b.num_columns(), confidence }
            }
        }
        Err(e) => Outcome::Declined(format!("{e:#}")),
    }
}

/// THE PROPERTY THAT HOLDS WITHOUT GROUND TRUTH: nothing in a pile of real
/// messy data may make tdy panic, hang, or abort. A wrong answer needs ground
/// truth to detect; a crash does not.
#[test]
fn no_real_world_file_makes_tdy_panic_or_hang() {
    if skip_notice() {
        return;
    }
    let files = files();
    assert!(
        files.len() > 100,
        "only {} data files found — did the corpus download finish?",
        files.len()
    );

    let mut slowest: Vec<(Duration, PathBuf)> = Vec::new();
    for p in &files {
        let t0 = Instant::now();
        // A panic here fails the test, which is the point: `examine` goes
        // through sample -> sniff -> validate -> execute, and any of them
        // panicking on real data is a defect however unusual the file.
        let _ = examine(p);
        let dt = t0.elapsed();
        slowest.push((dt, p.clone()));
        assert!(
            dt < Duration::from_secs(30),
            "{} took {dt:?} — a single ordinary file should not",
            p.display()
        );
    }
    slowest.sort_by_key(|(d, _)| std::cmp::Reverse(*d));
    eprintln!("\nslowest five of {} files:", files.len());
    for (d, p) in slowest.iter().take(5) {
        eprintln!("  {:>8.2?}  {}", d, p.display());
    }
}

/// A confident spec must be executable and reproducible. This is the same
/// contract `tests/adversarial.rs` holds the invented fixtures to, applied to
/// data nobody wrote for us.
#[test]
fn anything_read_confidently_is_reproducible() {
    if skip_notice() {
        return;
    }
    use tdy::config::Limits;
    let mut checked = 0usize;
    for p in files() {
        let Outcome::Read { rows, cols, .. } = examine(&p) else { continue };
        let sample = tdy::sample::build(&p, 16 * 1024, Limits::default()).unwrap();
        let spec = tdy::sniff::sniff(&p, &sample, Limits::default()).unwrap().spec;

        // Twice, and identically: a confident answer that moves between runs
        // is worse than no answer.
        let a = tdy::provider::spec_to_batch(&spec, &p).expect("first run");
        let b = tdy::provider::spec_to_batch(&spec, &p).expect("second run");
        assert_eq!(a.num_rows(), b.num_rows(), "{}: row count moved", p.display());
        assert_eq!(a.num_rows(), rows, "{}", p.display());
        assert_eq!(a.num_columns(), cols, "{}", p.display());
        assert_eq!(
            format!("{:?}", a.schema()),
            format!("{:?}", b.schema()),
            "{}: schema moved between runs",
            p.display()
        );
        checked += 1;
    }
    assert!(checked > 20, "only {checked} files were read confidently");
    eprintln!("{checked} files read confidently and reproducibly");
}

/// The survey. Not an assertion about quality — an honest measurement of how
/// tdy does on real messy data, printed so it can be read and acted on.
///
/// The "declined" and "low confidence" lists are the useful output: each entry
/// is either a bug or a candidate for the next fixture.
#[test]
fn survey_how_tdy_does_on_real_messy_data() {
    if skip_notice() {
        return;
    }
    let files = files();
    let mut read = 0usize;
    let mut low = 0usize;
    let mut declined = 0usize;
    let mut by_ext: BTreeMap<String, (usize, usize, usize)> = BTreeMap::new();
    let mut why: BTreeMap<String, usize> = BTreeMap::new();
    let mut examples: Vec<(String, String)> = Vec::new();

    for p in &files {
        let ext = p
            .extension()
            .map(|e| e.to_string_lossy().to_ascii_lowercase())
            .unwrap_or_default();
        let e = by_ext.entry(ext).or_insert((0, 0, 0));
        match examine(p) {
            Outcome::Read { .. } => {
                read += 1;
                e.0 += 1;
            }
            Outcome::LowConfidence { notes, .. } => {
                low += 1;
                e.1 += 1;
                for n in notes.iter().take(1) {
                    *why.entry(first_clause(n)).or_insert(0) += 1;
                }
            }
            Outcome::Declined(msg) => {
                declined += 1;
                e.2 += 1;
                *why.entry(first_clause(&msg)).or_insert(0) += 1;
                if examples.len() < 25 {
                    examples.push((p.display().to_string(), first_clause(&msg)));
                }
            }
        }
    }

    let total = files.len().max(1);
    eprintln!("\n=== tdy on {} real files from 26 upstream repositories ===", files.len());
    eprintln!(
        "  read confidently   {read:>5}  ({:.0}%)",
        100.0 * read as f64 / total as f64
    );
    eprintln!(
        "  read, unsure       {low:>5}  ({:.0}%)   <- said so, which is the designed behaviour",
        100.0 * low as f64 / total as f64
    );
    eprintln!(
        "  declined           {declined:>5}  ({:.0}%)",
        100.0 * declined as f64 / total as f64
    );

    eprintln!("\n  by extension        read  unsure  declined");
    for (ext, (r, l, d)) in &by_ext {
        eprintln!("    {ext:<16} {r:>5}  {l:>6}  {d:>8}");
    }

    let mut ranked: Vec<(&String, &usize)> = why.iter().collect();
    ranked.sort_by_key(|(_, n)| std::cmp::Reverse(**n));
    eprintln!("\n  most common reasons tdy was unsure or refused:");
    for (reason, n) in ranked.iter().take(15) {
        eprintln!("    {n:>4}x  {reason}");
    }

    eprintln!("\n  a sample of declined files (each is a bug or a future fixture):");
    for (f, r) in examples.iter().take(12) {
        eprintln!("    {}\n        {r}", short(f));
    }

    // The only hard assertion: tdy must not be useless on real data. This is
    // a floor, not a target — the number above is what to actually read.
    assert!(
        read + low > files.len() / 2,
        "tdy could not read even half of real messy data: {read} + {low} of {}",
        files.len()
    );
}

/// The first sentence of a message, so a histogram groups by cause rather than
/// by filename.
fn first_clause(s: &str) -> String {
    let s = s.split(['\n', ':']).next().unwrap_or(s).trim();
    s.chars().take(90).collect()
}

fn short(p: &str) -> String {
    match p.find("/corpus/") {
        Some(i) => p[i + 8..].to_string(),
        None => p.to_string(),
    }
}
