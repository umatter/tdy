//! `dataset()` — the members as one relation, and the lock that makes it
//! reproducible.
//!
//! The whole point of a lock is that membership is *recorded* rather than
//! discovered. A `dataset()` that expanded its globs at query time would
//! return a different number the morning after an export landed, with nothing
//! to point at and nothing to diff. So the tests here are mostly about what
//! makes the query **fail**: a new file, a changed file, a changed
//! declaration. Each has to be loud, and each has to name the file.

use std::path::{Path, PathBuf};

use tempfile::TempDir;

fn corpus() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("testdata").join("drifting_exports")
}

/// A private copy of the corpus, so a test can break it.
fn staged() -> TempDir {
    let dir = TempDir::new().unwrap();
    for e in std::fs::read_dir(corpus()).unwrap().flatten() {
        let p = e.path();
        if p.is_file() {
            // Sidecars and locks are rebuilt by the test that needs them.
            let n = e.file_name().to_string_lossy().to_string();
            if n.ends_with(".tdy.toml") || n.ends_with(".tdy.lock") {
                continue;
            }
            std::fs::copy(&p, dir.path().join(&n)).unwrap();
        }
    }
    dir
}

fn tdy(args: &[&str]) -> std::process::Output {
    std::process::Command::new(env!("CARGO_BIN_EXE_tdy"))
        .args(args)
        .output()
        .expect("run tdy")
}

fn target_of(dir: &TempDir) -> PathBuf {
    dir.path().join("sales_ok.tdy.sql")
}

fn fit_all(dir: &TempDir) {
    let t = target_of(dir);
    let out = tdy(&["fit", t.to_str().unwrap()]);
    assert!(
        out.status.success(),
        "fitting the corpus failed:\n{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}

fn query(dir: &TempDir, sql: &str) -> std::process::Output {
    let t = target_of(dir);
    tdy(&["query", &sql.replace("@", t.to_str().unwrap())])
}

/// The vision, as one assertion. Nine files: CSV and XLSX, windows-1252 and
/// UTF-8, German and English headers, day-first and ISO dates, one with a
/// merged band above its real header — read as a single relation whose total
/// is the generator's independently computed ground truth.
#[test]
fn nine_heterogeneous_files_are_one_relation_with_the_right_total() {
    let dir = staged();
    fit_all(&dir);

    let out = query(
        &dir,
        "SELECT count(*) rows, sum(amount_chf) total FROM dataset('@')",
    );
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert!(text.contains(" 36 "), "wrong row count:\n{text}");
    assert!(text.contains("57340.00"), "wrong total:\n{text}");
}

/// Row order must not depend on which member finished first, or `--frozen`
/// stops meaning "same files, same answer".
#[test]
fn the_row_order_of_a_dataset_is_deterministic() {
    let dir = staged();
    fit_all(&dir);
    let sql = "SELECT month, region, amount_chf FROM dataset('@')";
    let first = query(&dir, sql);
    assert!(first.status.success());
    for i in 0..3 {
        let again = query(&dir, sql);
        assert_eq!(first.stdout, again.stdout, "run {i} returned a different order");
    }
    // Members are read in sorted order, so January comes first.
    let text = String::from_utf8_lossy(&first.stdout);
    let body: Vec<&str> = text.lines().filter(|l| l.contains("2025-")).collect();
    assert!(body[0].contains("2025-01-31"), "not in member order:\n{text}");
}

/// A dataset is queryable under `--frozen`: nothing is planned, nothing is
/// written, and the lock is the membership.
#[test]
fn a_dataset_is_queryable_frozen() {
    let dir = staged();
    fit_all(&dir);
    let t = target_of(&dir);
    let out = tdy(&[
        "query",
        "-f",
        &format!("SELECT count(*) FROM dataset('{}')", t.display()),
    ]);
    assert!(
        out.status.success(),
        "frozen query failed:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(String::from_utf8_lossy(&out.stdout).contains("36"));
}

/// THE REASON THE LOCK EXISTS. A new export lands in the folder; the query
/// must stop rather than quietly include or exclude it. Either behaviour
/// would change a total with nothing to point at.
#[test]
fn a_new_file_is_drift_not_a_silently_different_answer() {
    let dir = staged();
    fit_all(&dir);
    std::fs::copy(dir.path().join("2025-01.csv"), dir.path().join("2025-13.csv")).unwrap();

    let out = query(&dir, "SELECT count(*) FROM dataset('@')");
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success(), "a new export was silently absorbed");
    assert!(err.contains("2025-13.csv"), "the drifting file is not named:\n{err}");
    assert!(err.contains("tdy fit"), "no remedy offered:\n{err}");
}

/// A member edited in place is drift too — its spec was proved against bytes
/// that no longer exist.
#[test]
fn an_edited_member_is_drift() {
    let dir = staged();
    fit_all(&dir);
    let p = dir.path().join("2025-01.csv");
    let mut body = std::fs::read(&p).unwrap();
    body.extend_from_slice(b"31.01.2025;Ost;9999.00\n");
    std::fs::write(&p, body).unwrap();

    let out = query(&dir, "SELECT sum(amount_chf) FROM dataset('@')");
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success(), "an edited member changed the total silently");
    assert!(err.contains("2025-01.csv"), "{err}");
}

/// A removed member is drift rather than a quietly shorter dataset.
#[test]
fn a_removed_member_is_drift() {
    let dir = staged();
    fit_all(&dir);
    std::fs::remove_file(dir.path().join("2025-01.csv")).unwrap();

    let out = query(&dir, "SELECT count(*) FROM dataset('@')");
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success(), "a missing member was silently dropped");
    assert!(err.contains("2025-01.csv"), "{err}");
}

/// Changing the declaration voids every member's proof — but a comment must
/// not. The point of writing the target in SQL is that it reads like
/// documentation, and punishing people for clarifying one would train them to
/// ignore the invalidation.
#[test]
fn the_declaration_invalidates_on_meaning_not_on_comments() {
    let dir = staged();
    fit_all(&dir);
    let t = target_of(&dir);

    let mut sql = std::fs::read_to_string(&t).unwrap();
    sql.push_str("\n-- reviewed 2026-08-31, still correct\n");
    std::fs::write(&t, &sql).unwrap();
    let out = query(&dir, "SELECT count(*) FROM dataset('@')");
    assert!(
        out.status.success(),
        "a comment invalidated the lock:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );

    // A real change does invalidate it.
    let sql = sql.replace("DECIMAL(14,2)", "DECIMAL(16,2)");
    std::fs::write(&t, sql).unwrap();
    let out = query(&dir, "SELECT count(*) FROM dataset('@')");
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success(), "a changed column type did not invalidate the lock");
    assert!(err.contains("declaration changed"), "{err}");
}

/// No partial lock. A dataset that silently omits the months that would not
/// fit is the aggregate-laundering failure the whole design refuses, so a run
/// with any gap writes nothing at all.
#[test]
fn a_dataset_with_an_unfittable_member_writes_no_lock() {
    let dir = staged();
    // `sales.tdy.sql` is the target *without* the exclusions, so three of its
    // twelve members cannot reach it.
    let t = dir.path().join("sales.tdy.sql");
    let out = tdy(&["fit", t.to_str().unwrap()]);
    let text = String::from_utf8_lossy(&out.stdout);

    assert!(!out.status.success(), "a dataset with three gaps was accepted");
    assert!(text.contains("9 of 12"), "{text}");
    assert!(
        !dir.path().join("sales.tdy.lock").exists(),
        "a partial lock was written"
    );
    // Every gap in one pass, not just the first.
    for name in ["2025-07.csv", "2025-08.csv", "2025-11.csv"] {
        assert!(text.contains(name), "{name} was not reported:\n{text}");
    }
}

/// Querying a dataset that was never fitted must say so, not fail obscurely.
#[test]
fn a_dataset_with_no_lock_says_which_command_makes_one() {
    let dir = staged();
    let out = query(&dir, "SELECT count(*) FROM dataset('@')");
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success());
    assert!(err.contains("no lock"), "{err}");
    assert!(err.contains("tdy fit"), "{err}");
}

/// A member's sidecar edited to produce something else must not be trusted
/// just because the lock lists it. A sidecar is hand-editable, so it is
/// re-proved against the target on every load — which costs no I/O.
#[test]
fn a_members_sidecar_is_reproved_against_the_target_on_every_query() {
    let dir = staged();
    fit_all(&dir);

    let sc = dir.path().join("2025-01.csv.tdy.toml");
    let text = std::fs::read_to_string(&sc).unwrap();
    // Rename an output column: the file still parses, and no longer conforms.
    let tampered = text.replace("name = \"region\"", "name = \"gebiet\"");
    assert_ne!(tampered, text, "the fixture changed shape; update this test");
    std::fs::write(&sc, tampered).unwrap();
    // Re-stamp so the fingerprint is fresh and only conformance can catch it.
    let p = dir.path().join("2025-01.csv");
    assert!(tdy(&["validate", p.to_str().unwrap(), "--stamp"]).status.success());

    let out = query(&dir, "SELECT count(*) FROM dataset('@')");
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success(), "a tampered sidecar was trusted");
    assert!(err.contains("2025-01.csv"), "{err}");
    assert!(err.contains("region") || err.contains("gebiet"), "{err}");
}

/// `dataset()` inside a comment or a string is not a reference — the same
/// property `messy()` has, for the same reason.
#[test]
fn a_dataset_reference_in_a_comment_is_not_a_reference() {
    assert!(tdy::sqlscan::find_dataset_refs("-- dataset('x.tdy.sql')\nSELECT 1").is_empty());
    assert!(tdy::sqlscan::find_dataset_refs("SELECT 'dataset(''x'')'").is_empty());
    assert_eq!(
        tdy::sqlscan::find_dataset_refs("SELECT * FROM dataset('a.tdy.sql')"),
        vec!["a.tdy.sql".to_string()]
    );
}
