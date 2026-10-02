//! `plans = 'lock'`: one spec table in the lock instead of a sidecar per
//! member (docs/design/2026-10-02-plans-in-the-lock.md).
//!
//! Nothing about what a plan is, or how it is proved, changes — so every
//! test here holds the lock-held pile to the same answer the sidecar pile
//! gives, and to the same refusals.

use std::path::{Path, PathBuf};

use tempfile::TempDir;

fn corpus() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("testdata").join("drifting_exports")
}

/// A private copy of the drifting exports, and `sales_ok` declared with
/// `plans = 'lock'` (or as committed, with `lock = false`).
fn staged(lock: bool) -> (TempDir, PathBuf) {
    let dir = TempDir::new().unwrap();
    for e in std::fs::read_dir(corpus()).unwrap().flatten() {
        let n = e.file_name().to_string_lossy().to_string();
        if e.path().is_file() && !n.ends_with(".tdy.toml") && !n.ends_with(".tdy.lock") {
            std::fs::copy(e.path(), dir.path().join(&n)).unwrap();
        }
    }
    let t = dir.path().join("sales_ok.tdy.sql");
    if lock {
        let sql = std::fs::read_to_string(&t).unwrap();
        let with = sql.replace("date_order = 'dmy'\n);", "date_order = 'dmy',\n  plans      = 'lock'\n);");
        assert_ne!(with, sql, "the fixture changed shape; update this test");
        std::fs::write(&t, with).unwrap();
    }
    (dir, t)
}

fn tdy(args: &[&str]) -> std::process::Output {
    std::process::Command::new(env!("CARGO_BIN_EXE_tdy")).args(args).output().expect("run tdy")
}

fn ok(out: &std::process::Output) -> String {
    let text = String::from_utf8_lossy(&out.stdout).to_string();
    assert!(out.status.success(), "{text}{}", String::from_utf8_lossy(&out.stderr));
    text
}

fn fit(t: &Path) -> String {
    ok(&tdy(&["fit", t.to_str().unwrap()]))
}

fn query(t: &Path, sql: &str) -> std::process::Output {
    tdy(&["query", &sql.replace('@', t.to_str().unwrap())])
}

fn sidecars(dir: &Path) -> Vec<String> {
    let mut v: Vec<String> = std::fs::read_dir(dir)
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().to_string())
        .filter(|n| n.ends_with(".tdy.toml"))
        .collect();
    v.sort();
    v
}

fn lock_text(t: &Path) -> String {
    std::fs::read_to_string(tdy::lockfile::lock_path(t)).unwrap()
}

const ROWS: &str = "SELECT month, region, amount FROM dataset('@') ORDER BY month, region";

/// The vision under the new storage: nine members, no sidecar, every plan
/// once in the lock — and the same 57,340.00 over 36 rows, row for row the
/// answer the sidecar pile gives.
#[test]
fn the_drifting_exports_fit_with_no_sidecar_and_give_the_same_answer() {
    let (dir, t) = staged(true);
    let text = fit(&t);
    assert!(text.contains("9 of 9 file(s) fit"), "{text}");
    assert_eq!(sidecars(dir.path()), Vec::<String>::new(), "a lock-held plan writes no sidecar");

    let lock = lock_text(&t);
    assert!(lock.contains("lock_version = 2"), "{lock}");
    let entries = lock.matches("[[spec]]").count();
    assert!((1..9).contains(&entries), "nine members share fewer than nine plans: {entries}\n{lock}");
    assert_eq!(lock.matches("\nspec = \"b3:").count(), 9, "every member names its plan:\n{lock}");
    assert!(!lock.contains("spec_digest"), "a lock-held plan's digest is its id:\n{lock}");
    assert!(
        text.contains(&format!("plans: 9 member(s) share {entries} plan(s), held in the lock")),
        "{text}"
    );

    let out = query(&t, "SELECT count(*) AS n, sum(amount) AS total FROM dataset('@')");
    let q = ok(&out);
    assert!(q.contains(" 36 ") && q.contains("57340.00"), "{q}");

    let (sdir, st) = staged(false);
    fit(&st);
    assert_eq!(sidecars(sdir.path()).len(), 9);
    assert!(lock_text(&st).contains("lock_version = 1"), "a target that does not opt in writes version 1");
    assert_eq!(ok(&query(&t, ROWS)), ok(&query(&st, ROWS)), "both storages, one answer");
}

/// A refit reads each plan from the lock, re-proves it and dry-runs the
/// member — and writes nothing new beside the data.
#[test]
fn a_refit_takes_the_plans_from_the_lock() {
    let (dir, t) = staged(true);
    fit(&t);
    let before = lock_text(&t);
    let text = fit(&t);
    assert_eq!(text.matches("(existing spec)").count(), 9, "{text}");
    assert_eq!(sidecars(dir.path()), Vec::<String>::new());
    let strip = |s: &str| s.lines().filter(|l| !l.starts_with("created_at")).collect::<Vec<_>>().join("\n");
    assert_eq!(strip(&lock_text(&t)), strip(&before), "an untouched pile refits to the same lock");
}

/// A `manual` sidecar is how one member gets a different plan: it is read
/// instead of the lock's, proved like any other, and never overwritten.
#[test]
fn a_manual_sidecar_overrides_the_lock_for_one_member_and_is_kept() {
    let (dir, t) = staged(true);
    fit(&t);
    let jan = dir.path().join("2025-01.csv");
    let lock = tdy::lockfile::Lock::load(&t).unwrap().unwrap();
    let entry = lock.member("2025-01.csv", None, None).unwrap();
    let mut spec = lock.spec(entry.spec.as_deref().unwrap()).unwrap().spec.clone();
    // Observable and still conforming: January's region read from its date.
    let month_source = spec.columns.iter().find(|c| c.name == "month").unwrap().source_name().to_string();
    spec.columns.iter_mut().find(|c| c.name == "region").unwrap().source = Some(month_source);
    tdy::sidecar::save(
        &jan,
        &spec,
        tdy::sidecar::ProvenanceInfo {
            method: tdy::spec::InferenceMethod::Manual,
            model: None,
            prompt_version: None,
            sampled_bytes: None,
        },
    )
    .unwrap();
    let written = std::fs::read_to_string(tdy::sidecar::sidecar_path(&jan)).unwrap();

    let text = fit(&t);
    assert!(text.contains("2025-01.csv") && text.contains("(hand-written spec)"), "{text}");
    let lock = lock_text(&t);
    let jan_entry = lock.split("[[member]]").find(|m| m.contains("path = \"2025-01.csv\"")).unwrap();
    assert!(!jan_entry.contains("\nspec = "), "a sidecar-held member names no lock plan:\n{jan_entry}");
    assert!(jan_entry.contains("spec_digest = \"b3:"), "{jan_entry}");
    assert_eq!(std::fs::read_to_string(tdy::sidecar::sidecar_path(&jan)).unwrap(), written, "never overwritten");
    assert_eq!(sidecars(dir.path()), vec!["2025-01.csv.tdy.toml"]);

    let q = ok(&query(&t, "SELECT DISTINCT region FROM dataset('@') WHERE month = DATE '2025-01-31'"));
    assert!(q.contains("31.01.2025"), "the manual plan is the one read:\n{q}");
}

/// A changed file is drift exactly as before: the query names it, and the
/// refit plans that member afresh and takes every other plan from the lock.
#[test]
fn drift_on_one_file_refuses_the_query_and_the_refit_replans_only_it() {
    let (dir, t) = staged(true);
    fit(&t);
    let march = dir.path().join("2025-03.csv");
    let mut body = std::fs::read(&march).unwrap();
    body.extend_from_slice(b"31.03.2025;Mitte;1'000.00\n");
    std::fs::write(&march, body).unwrap();

    let out = query(&t, "SELECT count(*) FROM dataset('@')");
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success(), "a changed member was queried");
    assert!(err.contains("2025-03.csv has changed since it was fitted"), "{err}");

    let out = tdy(&["--json", "fit", t.to_str().unwrap()]);
    let r: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert!(out.status.success(), "{r}");
    for m in r["members"].as_array().unwrap() {
        let want = if m["path"] == "2025-03.csv" { "heuristic" } else { "existing" };
        assert_eq!(m["via"], want, "{m}");
    }
    assert_eq!(sidecars(dir.path()), Vec::<String>::new());
    let q = ok(&query(&t, "SELECT count(*) AS n, sum(amount) AS total FROM dataset('@')"));
    assert!(q.contains(" 37 ") && q.contains("58340.00"), "{q}");
}

/// A pile whose third month is in cents: March waits on a person.
fn cents_pile() -> (TempDir, PathBuf) {
    let dir = TempDir::new().unwrap();
    let month = |m: &str, base: u32, factor: u32| {
        let mut s = String::from("Datum;Region;Betrag\n");
        for (i, region) in ["Ost", "West", "Nord", "Sued", "Mitte"].iter().enumerate() {
            s.push_str(&format!("28.{m}.2025;{region};{}.00\n", (base + 10 * i as u32) * factor));
        }
        s
    };
    std::fs::write(dir.path().join("2025-01.csv"), month("01", 1100, 1)).unwrap();
    std::fs::write(dir.path().join("2025-02.csv"), month("02", 1200, 1)).unwrap();
    std::fs::write(dir.path().join("2025-03.csv"), month("03", 1300, 100)).unwrap();
    let t = dir.path().join("sales.tdy.sql");
    std::fs::write(
        &t,
        "CREATE TABLE sales (month DATE NOT NULL OPTIONS(matches='Datum'), region TEXT NOT NULL \
         OPTIONS(matches='Region'), amount DECIMAL(14,2) NOT NULL OPTIONS(matches='Betrag')) \
         WITH (files = '*.csv', date_order = 'dmy', plans = 'lock');",
    )
    .unwrap();
    (dir, t)
}

/// An acceptance is a judgement about a plan. It carries over a refit that
/// leaves the plan alone, and expires when the member's plan changes — here
/// by a sidecar written over the lock's plan for that member.
#[test]
fn an_acceptance_survives_a_refit_and_expires_when_the_plan_changes() {
    let (dir, t) = cents_pile();
    let text = fit(&t);
    assert!(text.contains("REVIEW"), "{text}");
    ok(&tdy(&["fit", t.to_str().unwrap(), "--accept", "2025-03.csv"]));
    ok(&query(&t, "SELECT count(*) FROM dataset('@')"));

    let again = fit(&t);
    assert!(!again.contains("REVIEW"), "the acceptance was not carried over:\n{again}");
    assert_eq!(lock_text(&t).matches("accepted = true").count(), 1);

    // The plan for March changes: a sidecar now holds it.
    let march = dir.path().join("2025-03.csv");
    let lock = tdy::lockfile::Lock::load(&t).unwrap().unwrap();
    let id = lock.member("2025-03.csv", None, None).unwrap().spec.clone().unwrap();
    let spec = lock.spec(&id).unwrap().spec.clone();
    tdy::sidecar::save(
        &march,
        &spec,
        tdy::sidecar::ProvenanceInfo {
            method: tdy::spec::InferenceMethod::Manual,
            model: None,
            prompt_version: None,
            sampled_bytes: None,
        },
    )
    .unwrap();
    let out = query(&t, "SELECT count(*) FROM dataset('@')");
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success(), "an acceptance outlived its plan");
    assert!(err.contains("2025-03.csv") && err.contains("accepted"), "{err}");

    let refit = fit(&t);
    assert!(refit.contains("REVIEW"), "the acceptance was carried onto a different plan:\n{refit}");
    assert_eq!(lock_text(&t).matches("accepted = true").count(), 0);
}

/// The lock is text. A spec table edited into one that no longer produces
/// the target is refused by `dataset()`, naming the member — and one edited
/// in a way that still conforms is refused too, because the lock is not
/// where a plan is changed.
#[test]
fn an_edited_spec_table_is_refused_by_the_dataset() {
    let (_dir, t) = staged(true);
    fit(&t);
    let text = lock_text(&t);

    let broken = text.replace("name = \"amount\"", "name = \"betrag\"");
    assert_ne!(broken, text);
    std::fs::write(tdy::lockfile::lock_path(&t), &broken).unwrap();
    let out = query(&t, "SELECT count(*) FROM dataset('@')");
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success(), "a non-conforming lock plan was queried");
    assert!(err.contains("no longer produces `sales_ok`"), "{err}");
    assert!(err.contains("2025-01.csv"), "{err}");

    // Still the same schema, a different meaning: "1'100.00" is now a null
    // wherever it appears. Conformance cannot see it; the plan's id can.
    let edited = text.replacen("\"keine\",\n]\nthousands_separator", "\"keine\",\n    \"1'100.00\",\n]\nthousands_separator", 1);
    assert_ne!(edited, text);
    std::fs::write(tdy::lockfile::lock_path(&t), &edited).unwrap();
    let out = query(&t, "SELECT count(*) FROM dataset('@')");
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success(), "a hand-edited lock plan was queried");
    assert!(err.contains("edited by hand"), "{err}");
}

/// A lock written before plans could live in one is version 1 and reads
/// exactly as it did.
#[test]
fn a_version_1_lock_still_reads() {
    let (_dir, t) = staged(false);
    fit(&t);
    let lock = lock_text(&t);
    assert!(lock.contains("lock_version = 1") && !lock.contains("[[spec]]"), "{lock}");
    let q = ok(&query(&t, "SELECT sum(amount) AS total FROM dataset('@')"));
    assert!(q.contains("57340.00"), "{q}");
}
